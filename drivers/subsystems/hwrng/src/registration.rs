// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Subsystem-owned registration, operation admission, and synchronous drain.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::fmt;

use driver_model::PublicationSource;
use kerrno::{KError, KResult};
use kspin::SpinNoPreempt;

use super::{HwrngDevice, completion::RngDrain};

/// Driver-supplied runtime; never exposed by consumer handles.
pub type HwrngDeviceImpl = Arc<dyn HwrngDevice + Send + Sync>;
type Subscriber = Arc<dyn Fn(HwrngHandle) + Send + Sync>;

struct RegistryState {
    entries: Vec<Arc<Entry>>,
    available: Vec<Subscriber>,
    removed: Vec<Subscriber>,
}

struct Admission {
    is_closed: bool,
    in_flight: usize,
}

struct Entry {
    source: Arc<dyn PublicationSource>,
    runtime: HwrngDeviceImpl,
    drain: Arc<dyn RngDrain>,
    admission: SpinNoPreempt<Admission>,
}

/// Hardware RNG registry. Registration does not imply entropy trust.
///
/// Retains runtimes until explicit unregistration, independently of credential
/// Drop. Callbacks and runtime methods run outside registry/admission locks.
/// Source names and paths do not impose business uniqueness.
/// Registry and handle methods require task context; read and unregister
/// additionally require sleepable context. They are not IRQ entry points.
pub struct HwrngRegistry {
    state: SpinNoPreempt<RegistryState>,
}

impl HwrngRegistry {
    /// Construct an empty registry for static initialization.
    pub const fn new() -> Self {
        Self {
            state: SpinNoPreempt::new(RegistryState {
                entries: Vec::new(),
                available: Vec::new(),
                removed: Vec::new(),
            }),
        }
    }

    /// Register a runtime with an already constructed reliable drain provider.
    ///
    /// Call in sleepable task context. Metadata comes from the source, not a
    /// cached copy. Admission is independent of the source's device-online bit.
    /// Available callbacks must restore context and not wait for publisher
    /// progress. Runtime, source and drain final destructors must not require
    /// sleep; drivers explicitly release hardware during teardown.
    /// Returns BadState if the source has no driver identity.
    pub fn register(
        &'static self,
        source: Arc<dyn PublicationSource>,
        runtime: HwrngDeviceImpl,
        drain: Arc<dyn RngDrain>,
    ) -> KResult<HwrngRegistration> {
        if source.driver_name().is_none() {
            return Err(KError::BadState);
        }
        let entry = Arc::new(Entry {
            source,
            runtime,
            drain,
            admission: SpinNoPreempt::new(Admission {
                is_closed: false,
                in_flight: 0,
            }),
        });
        let subscribers = {
            let mut state = self.state.lock();
            state.entries.push(entry.clone());
            state.available.clone()
        };
        for subscriber in subscribers {
            subscriber(HwrngHandle {
                entry: entry.clone(),
            });
        }
        Ok(HwrngRegistration {
            registry: self,
            entry,
            is_unregistered: false,
        })
    }

    /// Transitional identity adapter for the legacy boot-only publisher.
    #[doc(hidden)]
    pub fn attach_legacy(
        &'static self,
        driver_name: &'static str,
        path: String,
        runtime: HwrngDeviceImpl,
        drain: Arc<dyn RngDrain>,
    ) -> KResult<HwrngRegistration> {
        self.register(Arc::new(LegacySource { driver_name, path }), runtime, drain)
    }

    /// Snapshot available handles; a later unregister may close them.
    pub fn devices(&self) -> Vec<HwrngHandle> {
        let entries = self.state.lock().entries.clone();
        entries
            .into_iter()
            .map(|entry| HwrngHandle { entry })
            .filter(HwrngHandle::is_available)
            .collect()
    }

    /// Subscribe before enumerating to avoid missing an available provider.
    /// The registry retains callbacks for its lifetime; avoid strong cycles.
    pub fn subscribe_available(&self, callback: Subscriber) {
        self.state.lock().available.push(callback);
    }

    /// Observe removal after drain and before unregister returns, without locks.
    ///
    /// This does not wake blocked reads or certify hardware/IRQ teardown.
    /// Callbacks must restore context and not wait for work requiring the
    /// publisher's management lock.
    pub fn subscribe_removed(&self, callback: Subscriber) {
        self.state.lock().removed.push(callback);
    }
}

impl Default for HwrngRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// System-wide hardware RNG capability registry.
pub static HWRNG: HwrngRegistry = HwrngRegistry::new();

/// Unique explicit unregistration credential, not a consumer handle.
///
/// Drop does not unregister, wait, panic, or invoke callbacks. Omission leaves
/// an observable registration retaining its runtime.
#[must_use = "retain the credential and explicitly unregister before driver teardown"]
pub struct HwrngRegistration {
    registry: &'static HwrngRegistry,
    entry: Arc<Entry>,
    is_unregistered: bool,
}

impl HwrngRegistration {
    /// Obtain a consumer handle without transferring registration rights.
    pub fn publication(&self) -> HwrngHandle {
        HwrngHandle {
            entry: self.entry.clone(),
        }
    }

    /// Close admission and discoverability, then wait for admitted calls.
    ///
    /// Stop the driver and wake its blocked readers first. Requires sleepable
    /// context without locks needed by readers. Preparation errors leave the
    /// registration unchanged. Once preparation succeeds, RngDrain cannot
    /// return a normal error. Removed callbacks run after drain, before return.
    /// Retained consumers do not delay drain. Successful repeats are no-ops.
    pub fn unregister(&mut self) -> KResult {
        if self.is_unregistered {
            return Ok(());
        }
        let mut removed = None;
        let mut has_closed = false;
        self.entry.drain.wait_until(&mut || {
            if !has_closed {
                self.entry.admission.lock().is_closed = true;
                let mut state = self.registry.state.lock();
                if let Some(index) = state
                    .entries
                    .iter()
                    .position(|entry| Arc::ptr_eq(entry, &self.entry))
                {
                    removed = Some(state.entries.remove(index));
                }
                has_closed = true;
            }
            self.entry.admission.lock().in_flight == 0
        })?;
        self.is_unregistered = true;
        drop(removed);
        let subscribers = self.registry.state.lock().removed.clone();
        for subscriber in subscribers {
            subscriber(self.publication());
        }
        Ok(())
    }
}

impl fmt::Debug for HwrngRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HwrngRegistration")
            .field("is_unregistered", &self.is_unregistered)
            .finish_non_exhaustive()
    }
}

/// Cloneable operation-only handle tied permanently to its original provider.
/// Methods require task context, even for non-sleeping metadata/failure queries.
#[derive(Clone)]
pub struct HwrngHandle {
    entry: Arc<Entry>,
}

impl fmt::Debug for HwrngHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HwrngHandle")
            .finish_non_exhaustive()
    }
}

struct Operation<'a>(&'a Entry);

impl Drop for Operation<'_> {
    fn drop(&mut self) {
        let should_notify = {
            let mut admission = self.0.admission.lock();
            admission.in_flight -= 1;
            admission.is_closed && admission.in_flight == 0
        };
        if should_notify {
            self.0.drain.notify();
        }
    }
}

impl HwrngHandle {
    /// Query source identity without granting runtime access.
    pub fn driver_name(&self) -> Option<&'static str> {
        self.entry.source.driver_name()
    }

    /// Snapshot admission state; not an operation permit.
    pub fn is_available(&self) -> bool {
        !self.entry.admission.lock().is_closed
    }

    /// Compare provider instances without a public numeric identifier.
    pub fn same_publication(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.entry, &other.entry)
    }

    fn acquire(&self) -> KResult<Operation<'_>> {
        let mut admission = self.entry.admission.lock();
        if admission.is_closed {
            return Err(KError::BadState);
        }
        admission.in_flight += 1;
        Ok(Operation(&self.entry))
    }

    /// Read under a scoped permit. Returns BadState after admission closes.
    pub fn read(&self, buf: &mut [u8]) -> KResult<usize> {
        let _operation = self.acquire()?;
        self.entry.runtime.read(buf)
    }

    /// Query hardware failure under a permit; closure itself is not failure.
    pub fn is_failed(&self) -> KResult<bool> {
        let _operation = self.acquire()?;
        Ok(self.entry.runtime.is_failed())
    }
}

struct LegacySource {
    driver_name: &'static str,
    path: String,
}
impl PublicationSource for LegacySource {
    fn driver_name(&self) -> Option<&'static str> {
        Some(self.driver_name)
    }

    fn physical_path(&self) -> &str {
        &self.path
    }

    fn is_online(&self) -> bool {
        true
    }
}

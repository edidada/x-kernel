// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Registration ownership, consumer identity, and in-flight teardown.

use alloc::{boxed::Box, string::String, sync::Arc};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kerrno::{KError, KResult};
use kspin::SpinNoPreempt;

use crate::PublicationSource;

/// Class-side teardown run once, after the last in-flight use (or
/// immediately when there was none).
type Finalize<T> = Box<dyn FnOnce(&T) + Send + Sync>;

struct Inner<T> {
    id: u64,
    /// The device this publication belongs to — THE shared identity: name,
    /// path and liveness all come from here, never from a snapshot.
    owner: Arc<dyn PublicationSource>,
    runtime: T,
    name: String,
    /// In-flight operation count. `class_remove` flips `draining` BEFORE the
    /// removal callbacks run — new acquires fail from that point. Operations
    /// already past the gate complete against the still-live runtime, and the
    /// class-side teardown waits for the last of them (`finish_or_defer`).
    uses: AtomicUsize,
    draining: AtomicBool,
    /// Deferred class-side teardown, installed by the withdrawal when
    /// operations are still in flight.
    finalize: SpinNoPreempt<Option<Finalize<T>>>,
}

/// The unique registration credential returned to the publisher.
///
/// Dropping it withdraws the publication without waiting for existing uses.
/// Resources needed by those uses must remain owned by the payload or be
/// released only after the driver's quiesce/join protocol completes.
/// This type deliberately implements neither Clone nor Deref.
#[must_use = "dropping the registration immediately withdraws the capability"]
pub struct Registration<T> {
    publication: Publication<T>,
    withdraw: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl<T> Registration<T> {
    pub(crate) fn new(
        publication: Publication<T>,
        withdraw: impl FnOnce() + Send + Sync + 'static,
    ) -> Self {
        Self {
            publication,
            withdraw: Some(Box::new(withdraw)),
        }
    }

    /// Obtain a consumer handle without transferring registration ownership.
    pub fn publication(&self) -> Publication<T> {
        self.publication.clone()
    }
}

impl<T> Drop for Registration<T> {
    fn drop(&mut self) {
        if let Some(withdraw) = self.withdraw.take() {
            withdraw();
        }
    }
}

impl<T> core::fmt::Debug for Registration<T> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Registration")
            .field("publication", &self.publication)
            .finish()
    }
}

/// A cloneable consumer reference to one published capability.
///
/// References keep the payload's memory alive, not its registration or
/// permission to perform I/O. Obtain an InUse before accessing the payload.
pub struct Publication<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Clone for Publication<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Publication<T> {
    /// Build the shared state before installing it in a class.
    pub(crate) fn new(
        owner: Arc<dyn PublicationSource>,
        runtime: T,
        name: String,
        id: u64,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                id,
                owner,
                runtime,
                name,
                uses: AtomicUsize::new(0),
                draining: AtomicBool::new(false),
                finalize: SpinNoPreempt::new(None),
            }),
        }
    }

    /// Registry-local sequence, never exposed to consumers.
    pub(crate) fn id(&self) -> u64 {
        self.inner.id
    }

    /// Whether both handles refer to the same publication instance.
    ///
    /// Clones compare equal even after withdrawal. A new publication is
    /// distinct even when its source and name match.
    pub fn same_publication(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Whether both handles publish the same runtime name off the same
    /// owning device — the registry's duplicate-identity test.
    pub(crate) fn shares_identity_with(&self, other: &Self) -> bool {
        self.inner.name == other.inner.name && Arc::ptr_eq(&self.inner.owner, &other.inner.owner)
    }

    /// The runtime device name the driver published under.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// The driver that published this device.
    pub fn driver_name(&self) -> Option<&'static str> {
        self.inner.owner.driver_name()
    }

    /// Where the device lives on the bus topology (DEVPATH source).
    pub fn location(&self) -> &str {
        self.inner.owner.physical_path()
    }

    /// Whether this publication admits new operations and its source is online.
    /// This is a snapshot, not an operation permit.
    pub fn is_available(&self) -> bool {
        !self.inner.draining.load(Ordering::SeqCst) && self.inner.owner.is_online()
    }

    /// Access the runtime payload without the availability gate.
    ///
    /// For internal teardown paths and class hooks; data-plane consumers use
    /// [`Self::with_available`].
    pub(crate) fn with<'a, R>(&'a self, f: impl FnOnce(&'a T) -> R) -> R {
        f(&self.inner.runtime)
    }

    /// Run a data-plane operation, refusing once the device is offline.
    ///
    /// Every consumer-facing class operation that returns a [`KResult`] goes
    /// through here: after removal the operation fails with
    /// [`KError::BadState`] instead of touching a torn-down runtime object.
    ///
    /// Intended for trusted subsystem adapters. Do not retain cloned runtime
    /// capabilities beyond this call; expose operation-only facades to consumers.
    pub fn with_available<R>(&self, f: impl FnOnce(&T) -> KResult<R>) -> KResult<R> {
        if !self.is_available() {
            return Err(KError::BadState);
        }
        let _use = self.acquire_use()?;
        f(&self.inner.runtime)
    }

    /// Acquire a device-usage guard: counts one in-flight operation, refused
    /// once the removal closed the gate. The guard decrements on drop;
    /// long-running consumers (workers holding the device across awaits) hold
    /// THIS instead of a bare clone so removal can wait for them.
    pub fn acquire_use(&self) -> Result<InUse<T>, KError> {
        if !self.is_available() {
            return Err(KError::BadState);
        }
        // Count BEFORE re-checking the gate: a withdrawal that closes the
        // gate between our check and our increment must not read zero
        // in-flight and finalize under an operation that believes it
        // passed. If the gate closed, undo and fail.
        self.inner.uses.fetch_add(1, Ordering::SeqCst);
        if self.inner.draining.load(Ordering::SeqCst) || !self.is_available() {
            // Undo our count. If we were the last in-flight operation, the
            // finalizer parked in the slot by `finish_or_defer` is now
            // unclaimed — nobody else can reach zero after us. Run it here,
            // exactly as the last `InUse::drop` would, or the class-side
            // teardown (devnode/gendisk/transport) never executes.
            let previous = self.inner.uses.fetch_sub(1, Ordering::AcqRel);
            if previous == 1 {
                let finalize = self.inner.finalize.lock().take();
                if let Some(finalize) = finalize {
                    finalize(&self.inner.runtime);
                }
            }
            return Err(KError::BadState);
        }
        Ok(InUse {
            publication: self.clone(),
        })
    }

    /// In-flight operation count (diagnostics, removal orchestration).
    pub fn in_flight(&self) -> usize {
        self.inner.uses.load(Ordering::Acquire)
    }

    /// Close the usage gate: new `acquire_use` calls fail. Returns the
    /// in-flight count AT close time — operations already past the gate
    /// complete against the still-live runtime and the class-side teardown is
    /// deferred to the last of them (see [`Self::finish_or_defer`]).
    pub(crate) fn begin_drain(&self) -> usize {
        self.inner.draining.store(true, Ordering::SeqCst);
        self.in_flight()
    }

    /// Install the post-withdraw teardown and run it now when nothing is in
    /// flight; otherwise the last [`InUse`] drop runs it.
    ///
    /// Ordering: the finalizer is published BEFORE the count is read, so an
    /// operation dropping to zero either sees the finalizer or the reader
    /// sees the zero — never both missing it.
    pub(crate) fn finish_or_defer(&self, finalize: Finalize<T>) {
        let ready = {
            let mut slot = self.inner.finalize.lock();
            *slot = Some(finalize);
            if self.inner.uses.load(Ordering::SeqCst) == 0 {
                slot.take()
            } else {
                None
            }
        };
        if let Some(finalize) = ready {
            finalize(&self.inner.runtime);
        }
    }
}

impl<T> core::fmt::Debug for Publication<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Publication")
            .field("id", &self.id())
            .field("name", &self.name())
            .field("location", &self.location())
            .field("driver_name", &self.driver_name())
            .finish()
    }
}

/// One counted device use: dropping it decrements the in-flight count and, if
/// it was the last one during a withdrawal, runs the deferred class teardown.
pub struct InUse<T> {
    publication: Publication<T>,
}

impl<T> InUse<T> {
    /// Access the payload under this already-acquired permit, even after withdrawal.
    ///
    /// Trusted subsystem adapters must not let cloned runtime capabilities
    /// escape the permit's lifetime.
    pub fn with<R>(&self, operation: impl FnOnce(&T) -> R) -> R {
        operation(&self.publication.inner.runtime)
    }

    /// The publication this use belongs to.
    pub fn publication(&self) -> &Publication<T> {
        &self.publication
    }
}

impl<T> Drop for InUse<T> {
    fn drop(&mut self) {
        let previous = self.publication.inner.uses.fetch_sub(1, Ordering::AcqRel);
        if previous == 1 {
            let finalize = self.publication.inner.finalize.lock().take();
            if let Some(finalize) = finalize {
                finalize(&self.publication.inner.runtime);
            }
        }
    }
}

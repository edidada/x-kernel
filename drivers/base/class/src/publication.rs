// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Publication identity, owning and borrowed handles, and in-flight teardown.

use alloc::{boxed::Box, string::String, sync::Arc};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use kerrno::{KError, KResult};
use kspin::SpinNoPreempt;

use crate::PublicationSource;

/// Opaque handle of one class publication.
///
/// Allocated from a dedicated id space when a driver publishes. Consumers
/// treat it as an opaque capability handle (compare, store, pass back to
/// `find_*`/console APIs). It is distinct from `DeviceId` (the domain's
/// identity space) and from the devnode's node id, which reuses this value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublicationId(u64);

/// THE publication-id source: one process-wide counter, no tag range. The
/// legacy-model high-bit fence is gone — nothing else allocates a
/// `PublicationId`, and `from_raw` only round-trips values that already
/// came from here (devfs projection keys, tests).
static NEXT_PUBLICATION_ID: AtomicU64 = AtomicU64::new(1);

impl PublicationId {
    /// Allocate a fresh publication id.
    pub fn next() -> Self {
        Self(NEXT_PUBLICATION_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// Wrap an existing raw id without re-deriving it.
    ///
    /// For lookups where the caller already holds the id's owner (e.g. the
    /// devfs projection, whose node ids ARE publication ids): the value is
    /// preserved exactly — any re-derivation (hashing) could never match the
    /// registry entry it came from.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Class-side teardown run once, after the last in-flight use (or
/// immediately when there was none).
type Finalize<T> = Box<dyn FnOnce(&T) + Send + Sync>;

struct Inner<T> {
    id: PublicationId,
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
    /// The withdrawal action (registry removal + callbacks + projection +
    /// class teardown). Taken and run exactly once, by the owning handle's
    /// drop.
    withdraw: SpinNoPreempt<Option<Box<dyn FnOnce() + Send + Sync>>>,
}

/// One published class device.
///
/// `Drop` withdraws only for the owning handle returned by `publish`; clones
/// (consumers, registry, callbacks) are borrows.
pub struct Publication<T> {
    inner: Arc<Inner<T>>,
    owns_withdrawal: bool,
}

impl<T> Clone for Publication<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            owns_withdrawal: false,
        }
    }
}

impl<T> Drop for Publication<T> {
    fn drop(&mut self) {
        if !self.owns_withdrawal {
            return;
        }
        let withdraw = self.inner.withdraw.lock().take();
        if let Some(withdraw) = withdraw {
            withdraw();
        }
    }
}

impl<T> Publication<T> {
    /// Build a publication. The withdrawal action is installed separately by
    /// `Class::publish` once the owning class is known.
    pub(crate) fn new(
        owner: Arc<dyn PublicationSource>,
        runtime: T,
        name: String,
        id: PublicationId,
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
                withdraw: SpinNoPreempt::new(None),
            }),
            owns_withdrawal: true,
        }
    }

    pub(crate) fn install_withdraw(&self, withdraw: impl FnOnce() + Send + Sync + 'static) {
        *self.inner.withdraw.lock() = Some(Box::new(withdraw));
    }

    /// Unique class-publication handle.
    pub fn id(&self) -> PublicationId {
        self.inner.id
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

    /// Whether the owning device is still standing behind this publication.
    pub fn is_available(&self) -> bool {
        self.inner.owner.is_online()
    }

    /// Access the runtime payload without the availability gate.
    ///
    /// For internal teardown paths and class hooks; data-plane consumers use
    /// [`Self::with_available`].
    pub fn with<'a, R>(&'a self, f: impl FnOnce(&'a T) -> R) -> R {
        f(&self.inner.runtime)
    }

    /// Run a data-plane operation, refusing once the device is offline.
    ///
    /// Every consumer-facing class operation that returns a [`KResult`] goes
    /// through here: after removal the operation fails with
    /// [`KError::BadState`] instead of touching a torn-down runtime object.
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

    /// Flip the shared device identity offline. Removal-orchestration
    /// internal: one flip covers every publication of the device.
    pub(crate) fn mark_offline(&self) {
        self.inner.owner.mark_offline();
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
    /// The publication this use belongs to.
    pub fn publication(&self) -> &Publication<T> {
        &self.publication
    }
}

impl<T> core::ops::Deref for InUse<T> {
    type Target = Publication<T>;

    fn deref(&self) -> &Self::Target {
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

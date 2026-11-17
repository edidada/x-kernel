// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Prepared registration ownership and allocation-free notification transfer.

use alloc::sync::Arc;

use intrusive_collections::{LinkedList, LinkedListLink};
use kalloc_types::{AllocFlags, KBox};
use kerrno::KResult;

use crate::{DeliveryCtx, WakeDelivery, adapter::WakeAdapter};

pub(super) struct WakeEntry {
    pub(super) links: LinkedListLink,
    pub(super) target: WakeTarget,
}

/// An independently retained notification identity, without queue membership.
///
/// Clones share callback storage, but each prepared registration has its own
/// queue node. Separate wake sets never coalesce notifications implicitly.
#[derive(Clone)]
pub struct WakeTarget {
    wake: Arc<dyn WakeDelivery>,
}

impl WakeTarget {
    /// Retains an existing shared callback without allocating.
    ///
    /// The caller prepares callback storage and independently retains all of
    /// its dependencies. This constructor transfers the supplied Arc unchanged;
    /// cloning a prepared target only increments its reference count.
    pub fn new(wake: Arc<dyn WakeDelivery>) -> Self {
        Self { wake }
    }

    /// Allocates one independently selectable registration with KERNEL permissions.
    ///
    /// Requires sleepable, reclaim-safe context before source exclusion. The
    /// source retains this non-cloneable value until cancellation or selection.
    /// Each selection consumes its registration; reenrollment needs a fresh one.
    ///
    /// # Errors
    /// Returns NoMemory if node storage cannot be allocated. The target remains
    /// valid and no registration is published on failure.
    pub fn prepare_registration(&self) -> KResult<WakeRegistration> {
        self.prepare_registration_with_flags(AllocFlags::KERNEL)
    }

    /// Allocates a registration node with caller-selected permissions.
    ///
    /// Obeys KBox::new_with_flags's execution-context and allocator constraints.
    /// Prepare before source exclusion; flags do not constrain later destruction.
    ///
    /// # Errors
    /// Returns NoMemory on allocation failure or InvalidInput for unknown flags.
    /// The target remains valid and no registration is published on failure.
    pub fn prepare_registration_with_flags(&self, flags: AllocFlags) -> KResult<WakeRegistration> {
        Ok(WakeRegistration {
            entry: KBox::new_with_flags(
                WakeEntry {
                    links: LinkedListLink::new(),
                    target: self.clone(),
                },
                flags,
            )?,
        })
    }
}

/// Owned storage for one source registration and its eventual notification.
///
/// A source moves this value into a WakeSet when selected. It cannot be cloned
/// or selected twice. Dropping an unselected registration cancels it without
/// invoking the callback; freeing storage obeys the host's teardown constraints.
pub struct WakeRegistration {
    entry: KBox<WakeEntry>,
}

/// Independently owned notifications selected without callbacks or allocation.
///
/// Each entry belongs to exactly one set. Multiple entries may reference the
/// same target and are delivered separately. Dropping this set cancels only
/// its entries without executing delivery callbacks; it may strand waiters.
/// Final callback release may run a destructor, and entry storage is deallocated.
#[must_use = "deliver selected notifications after releasing conflicting locks"]
pub struct WakeSet {
    registrations: LinkedList<WakeAdapter>,
    len: usize,
}

impl WakeSet {
    /// Creates an empty set without allocating or requiring a current task.
    pub const fn new() -> Self {
        Self {
            registrations: LinkedList::new(WakeAdapter::NEW),
            len: 0,
        }
    }

    /// Transfers a selected registration into this set without allocating.
    ///
    /// Consuming ownership prevents the same registration entering another set.
    pub fn push(&mut self, registration: WakeRegistration) {
        self.registrations.push_back(registration.entry);
        self.len += 1;
    }

    /// Returns the number of independently owned notifications in this set.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Reports whether this set contains no selected notifications.
    pub fn is_empty(&self) -> bool {
        self.registrations.is_empty()
    }

    /// Moves all entries from another set into this one in constant time.
    ///
    /// This neither allocates nor merges entries that share a target.
    pub fn append(&mut self, mut other: Self) {
        self.registrations
            .back_mut()
            .splice_after(other.registrations.take());
        self.len += other.len;
    }

    /// Consumes this set, delivering each selected notification exactly once.
    ///
    /// Returns the number of delivered entries, including separate registrations
    /// for the same target. Callers release all conflicting outer locks first
    /// and preserve the capability's admission conditions throughout delivery.
    /// Each entry is unlinked before its callback, then its storage is released
    /// after the callback returns. Teardown must be valid in the host context.
    pub fn deliver(mut self, cx: &mut DeliveryCtx) -> usize {
        let count = self.len;
        while let Some(entry) = self.registrations.pop_front() {
            entry.target.wake.deliver(cx);
        }
        count
    }
}

impl Default for WakeSet {
    fn default() -> Self {
        Self::new()
    }
}

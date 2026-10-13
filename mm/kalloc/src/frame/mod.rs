// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Resident descriptors for allocator-managed base pages.
//!
//! A descriptor identifies a physical slot. Finding one does not acquire
//! ownership of the current allocation, and an unshared slot is not necessarily
//! free: legacy allocations and allocator caches still own unshared pages.

mod reference;
mod registry;

use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use alloc_engine::{AllocError, AllocResult};
pub use reference::FrameRef;
pub(crate) use registry::{FrameLayout, FrameRegistry, MAX_FRAME_REGIONS};

use crate::UsageKind;

const UNSHARED: usize = 0;
const PERMANENT: usize = usize::MAX;
const RETIRING: usize = usize::MAX - 1;
const INITIALIZING: usize = usize::MAX - 2;
const MAX_REFERENCES: usize = isize::MAX as usize;

/// Diagnostic snapshot of a resident descriptor's lifetime state.
///
/// An unshared page may be held by a raw allocation or PCP. These snapshots
/// are not allocation admission checks, and do not acquire a page reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameState {
    /// Lifetime is not managed by a `FrameRef` at this instant.
    Unshared,
    /// Descriptor or buddy metadata storage, never allocatable.
    Permanent,
    /// The allocator is publishing the new allocation's initial reference.
    Initializing,
    /// The current allocation has this many lifetime references.
    Shared {
        /// Snapshot of the number of live holders.
        references: usize,
    },
    /// The unique last holder is returning the page to allocator inventory.
    Retiring,
}

/// Resident metadata for one allocator-managed 4 KiB physical page.
///
/// The storage outlives all allocations of the physical page. This descriptor
/// is not itself a live-page reference; its existence never authorizes a free
/// or access to the page contents.
pub struct FrameMeta {
    refs: AtomicUsize,
    kind: AtomicU8,
}

impl FrameMeta {
    fn new(is_permanent: bool) -> Self {
        Self {
            refs: AtomicUsize::new(if is_permanent { PERMANENT } else { UNSHARED }),
            kind: AtomicU8::new(UsageKind::Global as u8),
        }
    }

    /// Snapshot of the current lifetime state, without acquiring ownership.
    pub fn state(&self) -> FrameState {
        match self.refs.load(Ordering::Acquire) {
            UNSHARED => FrameState::Unshared,
            PERMANENT => FrameState::Permanent,
            RETIRING => FrameState::Retiring,
            INITIALIZING => FrameState::Initializing,
            references => FrameState::Shared { references },
        }
    }

    fn claim(&self, kind: UsageKind) {
        self.refs
            .compare_exchange(UNSHARED, INITIALIZING, Ordering::Acquire, Ordering::Relaxed)
            .expect("allocator returned an already referenced or permanent frame");
        // Allocation ownership excludes old holders. Publishing the first
        // reference releases the category initialized for this incarnation.
        self.kind.store(kind as u8, Ordering::Relaxed);
        self.refs.store(1, Ordering::Release);
    }

    fn add_reference(&self) -> AllocResult {
        match self
            .refs
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |refs| {
                if (1..MAX_REFERENCES).contains(&refs) {
                    Some(refs + 1)
                } else {
                    None
                }
            }) {
            Ok(_) => Ok(()),
            Err(MAX_REFERENCES) => Err(AllocError::NoMemory),
            #[expect(
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: An existing borrowed frame \
                          reference keeps the count positive; special states require a violated \
                          allocation or raw-transfer invariant"
            )]
            Err(_) => panic!("only a live frame reference can be cloned"),
        }
    }

    fn remove_reference(&self) -> bool {
        let mut previous = self.refs.load(Ordering::Relaxed);
        loop {
            assert!(
                (1..=MAX_REFERENCES).contains(&previous),
                "invalid frame reference release"
            );
            let next = if previous == 1 {
                RETIRING
            } else {
                previous - 1
            };
            // The last holder acquires prior holders' release operations
            // before returning the storage for another allocation.
            let ordering = if previous == 1 {
                Ordering::AcqRel
            } else {
                Ordering::Release
            };
            match self
                .refs
                .compare_exchange_weak(previous, next, ordering, Ordering::Relaxed)
            {
                Ok(_) => return previous == 1,
                Err(current) => previous = current,
            }
        }
    }

    fn usage_kind(&self) -> UsageKind {
        use strum::VariantArray;
        // Only an existing holder (or the unique retiring holder) reads this
        // field, so the allocation's category cannot be replaced concurrently.
        UsageKind::VARIANTS[self.kind.load(Ordering::Relaxed) as usize]
    }

    fn finish_retirement(&self) {
        assert_eq!(self.refs.load(Ordering::Relaxed), RETIRING);
        // No new reference can be acquired from a bare descriptor. Inventory
        // is published only after this store, and nothing resets it afterward.
        self.refs.store(UNSHARED, Ordering::Release);
    }

    /// Whether this slot backs permanent allocator metadata.
    ///
    /// The flag is established before the registry is published. Such pages
    /// remain mapped but are excluded from allocatable memory.
    pub fn is_permanent(&self) -> bool {
        self.refs.load(Ordering::Relaxed) == PERMANENT
    }
}

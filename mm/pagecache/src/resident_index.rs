// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RCU resident lookup, with ordered enumeration retained under the writer lock.
//!
//! Only `ResidentEntries` can mutate the radix index. Its exclusive borrow comes
//! from the mapping state mutex. Readers borrow nodes through a kernel RCU scope,
//! clone the leaf's owning Arc, and recheck the slot before leaving that scope.
//! Removed leaves and empty branches each carry their own bounded GP callback.

use alloc::{boxed::Box, collections::BTreeMap, sync::Arc};
use core::{
    ops::Deref,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, Ordering},
};

use kfinalize::DeferredArc;
use krcu::{CallbackNode, PreparedCallback, ReadScope};

use crate::{Folio, PageIndex};

const RADIX_BITS: u32 = 6;
const RADIX_SLOTS: usize = 1 << RADIX_BITS;
type FolioRef = Arc<Folio>;

enum Contents {
    Branch {
        shift: u32,
        slots: Box<[AtomicPtr<Node>; RADIX_SLOTS]>,
    },
    Leaf {
        index: PageIndex,
        folio: DeferredArc<FolioRef>,
    },
}

#[repr(C)]
struct Node {
    callback: CallbackNode,
    contents: Contents,
}

const _: () = assert!(core::mem::offset_of!(Node, callback) == 0);

impl Node {
    fn branch(shift: u32) -> Box<Self> {
        Box::new(Self {
            callback: CallbackNode::new(reclaim_node),
            contents: Contents::Branch {
                shift,
                slots: Box::new(core::array::from_fn(|_| AtomicPtr::new(ptr::null_mut()))),
            },
        })
    }

    fn leaf(index: PageIndex, folio: FolioRef) -> Box<Self> {
        // Prepare in task context: the GP callback may release the last leaf
        // owner in softirq, but Folio::drop and its quota provider may sleep.
        let folio = match DeferredArc::new(folio) {
            Ok(folio) => folio,
            #[expect(
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: MappingPages requires sleepable \
                          task context; kruntime installs permanent normal finalization before \
                          exposing filesystems, so preparation errors violate that lifecycle"
            )]
            Err((error, _)) => {
                panic!("page-cache publication requires task finalization services: {error:?}")
            }
        };
        Box::new(Self {
            callback: CallbackNode::new(reclaim_node),
            contents: Contents::Leaf { index, folio },
        })
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Contents::Branch { slots, .. } = &mut self.contents {
            for slot in slots.iter_mut() {
                let child = *slot.get_mut();
                if !child.is_null() {
                    // SAFETY: only final exclusive index destruction reaches
                    // nonempty branches. Each slot owns a distinct Box; retired
                    // branches have had every slot cleared. At most 11 branch
                    // levels and one leaf are traversed during final teardown.
                    drop(unsafe { Box::from_raw(child) });
                }
            }
        }
    }
}

/// # Safety
/// `callback` is the zero-offset projection of one uniquely retired raw Box<Node>.
/// Invoke only after its GP. Branches must be empty; leaves transfer their
/// prepared folio owner to a normal worker without sleeping or allocating.
unsafe fn reclaim_node(callback: *mut CallbackNode) {
    // SAFETY: repr(C) and the offset assertion preserve the Box provenance;
    // submission transferred its sole reclamation right and the GP ended reads.
    drop(unsafe { Box::from_raw(callback.cast::<Node>()) });
}

/// # Safety
/// The callback was prepared from a retired node that may still have readers.
/// Cancellation deliberately retains the raw owner; it is not a grace period.
unsafe fn retain_on_rejected_submission(_callback: *mut CallbackNode) {}

/// # Safety
/// `node` is a non-null, uniquely withdrawn Box<Node>, never previously queued.
/// No live index slot points to it. A branch has no children. Call in watched
/// task context after the permanent RCU service starts, without RCU service locks.
unsafe fn retire(node: *mut Node) {
    // SAFETY: Node is repr(C), with callback at offset zero; the raw Box retains
    // the enclosing allocation's provenance and stable address.
    let callback = unsafe { NonNull::new_unchecked(node.cast::<CallbackNode>()) };
    // SAFETY: the withdrawn Box has one submission right, transferable contents
    // and a bounded callback: leaf payload destruction is deferred to a task.
    // Rejection cannot free active data.
    let prepared = unsafe { PreparedCallback::new(callback, retain_on_rejected_submission) };
    // SAFETY: all publication routes were withdrawn by the exclusive writer;
    // the embedded node/Arc survive submission and callback code is permanent.
    let accepted = unsafe { prepared.submit() }.is_ok();
    assert!(
        accepted,
        "page-cache retirement requires the running RCU service"
    );
}

fn slot_number(index: PageIndex, shift: u32) -> usize {
    ((index >> shift) & (RADIX_SLOTS as u64 - 1)) as usize
}

fn covers(index: PageIndex, shift: u32) -> bool {
    shift == 60 || index >> (shift + RADIX_BITS) == 0
}

pub(super) struct ResidentIndex {
    root: AtomicPtr<Node>,
}

impl ResidentIndex {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            root: AtomicPtr::new(ptr::null_mut()),
        })
    }

    pub(super) fn get(&self, index: PageIndex, _scope: &ReadScope<'_>) -> Option<FolioRef> {
        'retry: loop {
            let mut slot = &self.root;
            loop {
                let pointer = slot.load(Ordering::Acquire);
                if pointer.is_null() {
                    return None;
                }
                // SAFETY: acquire loads observe fully initialized immutable
                // contents. The caller's RCU scope retains all reached nodes,
                // including a node detached while this traversal was underway.
                let node = unsafe { &*pointer };
                match &node.contents {
                    Contents::Branch { shift, slots } => {
                        if ptr::eq(slot, &self.root) && !covers(index, *shift) {
                            return None;
                        }
                        slot = &slots[slot_number(index, *shift)];
                    }
                    Contents::Leaf {
                        index: found,
                        folio,
                    } => {
                        debug_assert_eq!(*found, index);
                        let reference = Arc::clone(folio);
                        if slot.load(Ordering::Acquire) != pointer {
                            // The node itself still owns an Arc through the GP,
                            // so dropping this speculative reference cannot free
                            // a folio inside the read scope.
                            drop(reference);
                            continue 'retry;
                        }
                        return Some(reference);
                    }
                }
            }
        }
    }

    // Only ResidentEntries' exclusive writer calls the following methods.
    fn insert(&self, index: PageIndex, folio: FolioRef) {
        let mut root = self.root.load(Ordering::Relaxed);
        let mut shift = if root.is_null() {
            0
        } else {
            // SAFETY: the exclusive writer owns all live nodes; callbacks only
            // own detached nodes. The root is always a branch.
            match &unsafe { &*root }.contents {
                Contents::Branch { shift, .. } => *shift,
                Contents::Leaf { .. } => unreachable!(),
            }
        };
        if root.is_null() {
            while !covers(index, shift) {
                shift += RADIX_BITS;
            }
            root = Box::into_raw(Node::branch(shift));
            self.root.store(root, Ordering::Release);
        }
        while !covers(index, shift) {
            shift += RADIX_BITS;
            let parent = Node::branch(shift);
            if let Contents::Branch { slots, .. } = &parent.contents {
                slots[0].store(root, Ordering::Relaxed);
            }
            root = Box::into_raw(parent);
            self.root.store(root, Ordering::Release);
        }
        let mut pointer = root;
        loop {
            // SAFETY: only this writer can detach a live node. No mutable
            // references are created; publication uses atomic child slots.
            let node = unsafe { &*pointer };
            let Contents::Branch { shift, slots } = &node.contents else {
                unreachable!();
            };
            let slot = &slots[slot_number(index, *shift)];
            if *shift == 0 {
                let incoming = Box::into_raw(Node::leaf(index, folio));
                let old = slot.swap(incoming, Ordering::AcqRel);
                if !old.is_null() {
                    // SAFETY: swap withdrew the sole publication of this leaf;
                    // the mapping mutex serializes writers and service is live.
                    unsafe { retire(old) };
                }
                return;
            }
            let mut child = slot.load(Ordering::Relaxed);
            if child.is_null() {
                child = Box::into_raw(Node::branch(*shift - RADIX_BITS));
                slot.store(child, Ordering::Release);
            }
            pointer = child;
        }
    }

    fn remove(&self, index: PageIndex) {
        Self::remove_from(&self.root, index, true);
    }

    fn remove_from(slot: &AtomicPtr<Node>, index: PageIndex, is_root: bool) {
        let pointer = slot.load(Ordering::Relaxed);
        if pointer.is_null() {
            return;
        }
        // SAFETY: this recursive traversal runs only under the exclusive
        // writer. Its live path cannot be detached by any other task.
        let node = unsafe { &*pointer };
        if let Contents::Branch { shift, slots } = &node.contents {
            if is_root && !covers(index, *shift) {
                return;
            }
            Self::remove_from(&slots[slot_number(index, *shift)], index, false);
            if slots
                .iter()
                .any(|child| !child.load(Ordering::Relaxed).is_null())
            {
                return;
            }
        }
        slot.store(ptr::null_mut(), Ordering::Release);
        // SAFETY: the slot was this node's only publication; children have all
        // been cleared and separately retired. Do not access node after submit.
        unsafe { retire(pointer) };
    }
}

impl Drop for ResidentIndex {
    fn drop(&mut self) {
        let root = *self.root.get_mut();
        if !root.is_null() {
            // SAFETY: final Arc ownership excludes readers/writers, which
            // borrow the index. Detached nodes are independently callback-owned.
            drop(unsafe { Box::from_raw(root) });
        }
    }
}

/// Writer-locked ordered view. There is intentionally no DerefMut: every
/// structural mutation must also publish/withdraw the RCU lookup entry.
pub(super) struct ResidentEntries {
    ordered: BTreeMap<PageIndex, FolioRef>,
    index: Arc<ResidentIndex>,
}

impl ResidentEntries {
    pub(super) fn new() -> (Self, Arc<ResidentIndex>) {
        let index = ResidentIndex::new();
        let entries = Self {
            ordered: BTreeMap::new(),
            index: Arc::clone(&index),
        };
        // There is exactly one writer owner. Read handles cannot manufacture
        // another ResidentEntries sharing this index and bypass serialization.
        (entries, index)
    }

    pub(super) fn insert(&mut self, index: PageIndex, folio: FolioRef) -> Option<FolioRef> {
        let old = self.ordered.insert(index, Arc::clone(&folio));
        self.index.insert(index, folio);
        old
    }

    pub(super) fn remove(&mut self, index: &PageIndex) -> Option<FolioRef> {
        let old = self.ordered.remove(index)?;
        self.index.remove(*index);
        Some(old)
    }

    pub(super) fn take(&mut self) -> BTreeMap<PageIndex, FolioRef> {
        let old = core::mem::take(&mut self.ordered);
        for index in old.keys() {
            self.index.remove(*index);
        }
        old
    }
}

impl Deref for ResidentEntries {
    type Target = BTreeMap<PageIndex, FolioRef>;

    fn deref(&self) -> &Self::Target {
        &self.ordered
    }
}

unittest::path_mod_test!(tests, "../ut-tests/resident_index.rs");

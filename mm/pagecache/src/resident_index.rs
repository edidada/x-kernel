// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RCU resident lookup, with ordered enumeration retained under the writer lock.
//!
//! Only `ResidentEntries` can mutate the radix index. Its exclusive borrow comes
//! from the mapping state mutex. Readers borrow nodes through a kernel RCU scope,
//! clone the leaf's owning Arc, and recheck the slot before leaving that scope.
//! Single removals use bounded node callbacks. Whole-index withdrawal submits
//! one callback and reclaims the detached tree in a yielding task worker.

use alloc::{boxed::Box, collections::BTreeMap, sync::Arc};
use core::{
    mem,
    ops::Deref,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, Ordering},
};

use kfinalize::DeferredArc;
use krcu::{CallbackNode, PreparedCallback, ReadScope};

use crate::{Folio, PageIndex};

const RADIX_BITS: u32 = 6;
const RADIX_SLOTS: usize = 1 << RADIX_BITS;
const RECLAIM_BATCH_NODES: usize = 64;
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
                    // branches have had every slot cleared. Recursion depth is
                    // at most 11 branch levels plus a leaf, while total work is
                    // proportional to all nodes in the subtree.
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
    let accepted = match unsafe { prepared.submit() } {
        Ok(()) => true,
        Err((_error, retained)) => {
            // No GP has completed. Preserve the returned submission right and
            // raw allocation rather than running cancellation on protected data.
            mem::forget(retained);
            false
        }
    };
    assert!(
        accepted,
        "page-cache retirement requires the running RCU service"
    );
}

/// Validates admission before changing either index. Mapping operations require
/// sleepable watched task context; kruntime starts permanent RCU and normal
/// worker services before filesystem registration and namespace construction.
pub(super) fn assert_retirement_context() {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: internal mapping mutations require \
                  sleepable watched task context after permanent RCU startup; check before \
                  mutation, while fallible public entry points return admission errors"
    )]
    krcu::preflight_callback_submission().expect("page-cache mutation requires RCU task context");
}

/// Owns an entire withdrawn tree; only the normal finalization worker drops it.
struct DetachedTree {
    root: AtomicPtr<Node>,
}

impl Drop for DetachedTree {
    fn drop(&mut self) {
        let root = *self.root.get_mut();
        if !root.is_null() {
            // SAFETY: the GP callback releases this owner only after every old
            // reader exits. The root was withdrawn once and never reused.
            unsafe { destroy_tree(root, &mut 0) };
        }
    }
}

/// # Safety
/// `root` owns a distinct Box tree with no readers or writers. Invoke in
/// sleepable task context without mapping/folio locks; children have one owner.
unsafe fn destroy_tree(root: *mut Node, released: &mut usize) {
    // SAFETY: the caller transfers this live allocation after all borrows ended.
    let mut node = unsafe { Box::from_raw(root) };
    if let Contents::Branch { slots, .. } = &mut node.contents {
        for slot in slots.iter_mut() {
            let child = mem::replace(slot.get_mut(), ptr::null_mut());
            if !child.is_null() {
                // SAFETY: taking the sole child edge transfers a distinct tree.
                // No publication or reader remains; the parent's slot is empty.
                unsafe { destroy_tree(child, released) };
            }
        }
    }
    // Branches are now empty, so Node::drop cannot recurse. Leaf payloads
    // transfer to the finalizer; never wait for another finalization here.
    drop(node);
    *released += 1;
    if *released == RECLAIM_BATCH_NODES {
        *released = 0;
        ktask::yield_now();
    }
}

#[repr(C)]
struct TreeRetirement {
    callback: CallbackNode,
    tree: DeferredArc<DetachedTree>,
}

const _: () = assert!(mem::offset_of!(TreeRetirement, callback) == 0);

impl TreeRetirement {
    fn prepare() -> Box<Self> {
        let tree = DeferredArc::new(DetachedTree {
            root: AtomicPtr::new(ptr::null_mut()),
        });
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: mapping teardown has passed task \
                      admission and permanent workers start before filesystem construction"
        )]
        let tree = tree
            .map_err(|(error, _)| error)
            .expect("normal finalization is ready");
        Box::new(Self {
            callback: CallbackNode::new(reclaim_tree),
            tree,
        })
    }

    fn submit(self: Box<Self>, root: *mut Node) {
        self.tree.root.store(root, Ordering::Relaxed);
        let raw = Box::into_raw(self);
        // SAFETY: the non-null Box allocation has a zero-offset callback field.
        let callback = unsafe { NonNull::new_unchecked(raw.cast::<CallbackNode>()) };
        // SAFETY: this private one-shot record owns its preparation. Cancellation
        // frees only the record, independently retaining the protected raw tree.
        let prepared = unsafe { PreparedCallback::new(callback, cancel_tree) };
        // SAFETY: root.swap withdrew the sole publication after task admission.
        // The callback only hands off destruction; it cannot traverse the tree.
        let accepted = unsafe { prepared.submit() }.is_ok();
        assert!(accepted, "admitted page-cache tree retirement was rejected");
    }
}

/// # Safety
/// callback owns a uniquely prepared, unsubmitted TreeRetirement Box. Rejection
/// is not a GP: retain any detached tree independently of the canceled record.
unsafe fn cancel_tree(callback: *mut CallbackNode) {
    // SAFETY: repr(C) and the offset assertion preserve the original Box pointer.
    let record = unsafe { Box::from_raw(callback.cast::<TreeRetirement>()) };
    // Preserve the raw tree owner on fatal admission failure. The deferred
    // payload is now empty, so dropping the preparation cannot free reader data.
    record.tree.root.store(ptr::null_mut(), Ordering::Relaxed);
    drop(record);
}

/// # Safety
/// callback is the first field of one submitted TreeRetirement whose GP ended.
unsafe fn reclaim_tree(callback: *mut CallbackNode) {
    // SAFETY: the one-shot callback owns the entire record after its GP. Dropping
    // DeferredArc transfers the detached tree to a sleepable normal worker.
    drop(unsafe { Box::from_raw(callback.cast::<TreeRetirement>()) });
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
            // borrow the index. MappingPages drops its fields in task context
            // after releasing its locks; detached nodes are callback-owned.
            unsafe { destroy_tree(root, &mut 0) };
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
        assert_retirement_context();
        let old = self.ordered.insert(index, Arc::clone(&folio));
        self.index.insert(index, folio);
        old
    }

    pub(super) fn remove(&mut self, index: &PageIndex) -> Option<FolioRef> {
        assert_retirement_context();
        let old = self.ordered.remove(index)?;
        self.index.remove(*index);
        Some(old)
    }

    pub(super) fn take(&mut self) -> BTreeMap<PageIndex, FolioRef> {
        assert_retirement_context();
        if self.ordered.is_empty() {
            return BTreeMap::new();
        }
        // Prepare before either index changes. Once withdrawn, the old subtree
        // stays immutable for readers until GP and is never recursively freed
        // by the softirq callback.
        let retirement = TreeRetirement::prepare();
        let old = mem::take(&mut self.ordered);
        let root = self.index.root.swap(ptr::null_mut(), Ordering::AcqRel);
        retirement.submit(root);
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

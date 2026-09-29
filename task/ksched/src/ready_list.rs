// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared intrusive FIFO linkage and checked constant-time removal.
//!
//! Each list has a nonzero identity that is never reused. Removal checks that
//! identity before `cursor_mut_from_ptr`, so a task queued on another list is
//! left alone. `is_linked` is not enough: a link can be acquired by a
//! different list.

use alloc::sync::Arc;
use core::{
    mem::ManuallyDrop,
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use intrusive_collections::{
    Adapter, DefaultLinkOps, DefaultPointerOps, LinkedList, LinkedListAtomicLink,
};

static NEXT_QUEUE_ID: AtomicUsize = AtomicUsize::new(1);

pub(crate) struct ReadyLinks {
    link: LinkedListAtomicLink,
    owner: AtomicUsize,
}

impl ReadyLinks {
    pub(crate) const fn new() -> Self {
        Self {
            link: LinkedListAtomicLink::new(),
            owner: AtomicUsize::new(0),
        }
    }
}

/// Identifies the one private linkage field in an entity.
///
/// # Safety
///
/// `OFFSET` must be the byte offset of the [`ReadyLinks`] returned by
/// [`Self::ready_links`]. That field must stay private. Only [`ReadyList`]
/// may read or write the link and the owner word.
pub(crate) unsafe trait ReadyItem: Sized {
    /// Offset of the [`ReadyLinks`] field inside `Self`.
    const OFFSET: usize;

    /// The linkage [`ReadyList`] uses for this entity.
    fn ready_links(&self) -> &ReadyLinks;
}

struct ReadyAdapter<T: ReadyItem> {
    link_ops: <LinkedListAtomicLink as DefaultLinkOps>::Ops,
    pointer_ops: DefaultPointerOps<Arc<T>>,
}

impl<T: ReadyItem> ReadyAdapter<T> {
    const NEW: Self = Self {
        link_ops: <LinkedListAtomicLink as DefaultLinkOps>::NEW,
        pointer_ops: DefaultPointerOps::new(),
    };
    const OFFSET: usize = T::OFFSET + core::mem::offset_of!(ReadyLinks, link);
}

// SAFETY: `ReadyItem::OFFSET` names the private `ReadyLinks` field.
// `get_link` and `get_value` invert that same field offset inside the live
// `Arc` allocation. `DefaultPointerOps` owns each list's `Arc`; the adapter
// does not change the strong count.
unsafe impl<T: ReadyItem> Adapter for ReadyAdapter<T> {
    type LinkOps = <LinkedListAtomicLink as DefaultLinkOps>::Ops;
    type PointerOps = DefaultPointerOps<Arc<T>>;

    /// # Safety
    ///
    /// `link` must have been produced by this adapter's `get_link` on a
    /// still-live `T`.
    unsafe fn get_value(&self, link: NonNull<LinkedListAtomicLink>) -> *const T {
        // SAFETY: Adapter requires a link previously projected by this
        // adapter. Subtracting the same field offset recovers the live
        // owning allocation. The link is aligned because it is a real field.
        unsafe { link.as_ptr().cast::<u8>().sub(Self::OFFSET).cast() }
    }

    /// # Safety
    ///
    /// `value` must point to a live, initialized `T`.
    unsafe fn get_link(&self, value: *const T) -> NonNull<LinkedListAtomicLink> {
        // SAFETY: Adapter supplies a live entity. OFFSET selects its
        // aligned, initialized `LinkedListAtomicLink` without creating an
        // exclusive entity reference.
        unsafe { NonNull::new_unchecked(value.cast::<u8>().add(Self::OFFSET).cast_mut().cast()) }
    }

    fn link_ops(&self) -> &Self::LinkOps {
        &self.link_ops
    }

    fn link_ops_mut(&mut self) -> &mut Self::LinkOps {
        &mut self.link_ops
    }

    fn pointer_ops(&self) -> &Self::PointerOps {
        &self.pointer_ops
    }
}

pub(crate) struct ReadyList<T: ReadyItem> {
    list: LinkedList<ReadyAdapter<T>>,
    id: usize,
}

impl<T: ReadyItem> ReadyList<T> {
    pub(crate) const fn new() -> Self {
        Self {
            list: LinkedList::new(ReadyAdapter::NEW),
            id: 0,
        }
    }

    fn ensure_identity(&mut self) {
        if self.id == 0 {
            self.id = NEXT_QUEUE_ID
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .expect("ready queue identity exhausted");
        }
    }

    pub(crate) fn push_back(&mut self, task: Arc<T>) {
        self.ensure_identity();
        self.list.push_back(task);
        // AtomicLink acquires exclusive linkage before publication. Assign
        // only after success, so a rejected duplicate cannot overwrite its owner.
        self.list
            .back()
            .get()
            .unwrap()
            .ready_links()
            .owner
            .store(self.id, Ordering::Relaxed);
    }

    pub(crate) fn push_front(&mut self, task: Arc<T>) {
        self.ensure_identity();
        self.list.push_front(task);
        self.list
            .front()
            .get()
            .unwrap()
            .ready_links()
            .owner
            .store(self.id, Ordering::Relaxed);
    }

    pub(crate) fn pop_front(&mut self) -> Option<Arc<T>> {
        // Clear before the library releases its atomic link: the next
        // insertion may acquire that link immediately after removal.
        self.list
            .front()
            .get()?
            .ready_links()
            .owner
            .store(0, Ordering::Relaxed);
        self.list.pop_front()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub(crate) fn remove(&mut self, task: &T) -> Option<Arc<T>> {
        if self.id == 0 || task.ready_links().owner.load(Ordering::Relaxed) != self.id {
            return None;
        }
        task.ready_links().owner.store(0, Ordering::Relaxed);
        // SAFETY: `cursor_mut_from_ptr` requires `task` to be linked in this
        // list. The nonzero, never-reused owner id was stored only after this
        // list acquired the link, and it is cleared before the link is
        // released. Exclusive `&mut self` is the only mutator of this list.
        // The list's `Arc` keeps the allocation alive for the cursor.
        unsafe {
            self.list
                .cursor_mut_from_ptr(core::ptr::from_ref(task))
                .remove()
        }
    }

    pub(crate) fn steal(&mut self, mut want: impl FnMut(&Arc<T>) -> bool) -> Option<Arc<T>> {
        let mut cursor = self.list.front_mut();
        while let Some(pointer) = cursor.as_cursor().get_ptr() {
            // SAFETY: `get_ptr` is the data pointer of the `Arc` this list
            // inserted through `DefaultPointerOps::into_raw`. The exclusive
            // cursor keeps that `Arc` alive for the predicate.
            // `ManuallyDrop` borrows the strong count: it does not release it
            // on return or panic. The view is not used after `cursor.remove`
            // transfers the list's `Arc`. The predicate receives `&Arc` and
            // cannot consume the borrowed count.
            let task = ManuallyDrop::new(unsafe { Arc::from_raw(pointer.as_ptr()) });
            if want(&task) {
                task.ready_links().owner.store(0, Ordering::Relaxed);
                return cursor.remove();
            }
            cursor.move_next();
        }
        None
    }
}

impl<T: ReadyItem> Drop for ReadyList<T> {
    fn drop(&mut self) {
        // Surviving external Arcs must be reusable after the queue is dropped.
        while self.pop_front().is_some() {}
    }
}

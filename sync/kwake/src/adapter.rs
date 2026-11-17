// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Private KBox ownership conversion and intrusive-link projection.

use core::{mem::offset_of, ptr::NonNull};

use intrusive_collections::{Adapter, DefaultLinkOps, LinkedListLink, PointerOps};
use kalloc_types::KBox;

use crate::value::WakeEntry;

// KBox owns registration nodes; the collection must recover KBox rather than
// standard Box so release retains the kernel allocator's ownership contract.
#[derive(Clone)]
pub(super) struct WakeAdapter {
    links: <LinkedListLink as DefaultLinkOps>::Ops,
}

impl WakeAdapter {
    pub(super) const NEW: Self = Self {
        links: <LinkedListLink as DefaultLinkOps>::NEW,
    };
}

// SAFETY: Both conversions transfer the same unique KBox<WakeEntry> owner.
// The collection retains the stable allocation and recovers ownership exactly
// once after unlinking or during cleanup. Recovery neither allocates nor panics.
unsafe impl PointerOps for WakeAdapter {
    type Pointer = KBox<WakeEntry>;
    type Value = WakeEntry;

    unsafe fn from_raw(&self, pointer: *const WakeEntry) -> KBox<WakeEntry> {
        // SAFETY: PointerOps returns one unrecovered pointer from into_raw with
        // the original allocation, type and unique ownership intact.
        unsafe { KBox::from_raw(pointer.cast_mut()) }
    }

    fn into_raw(&self, owner: KBox<WakeEntry>) -> *const WakeEntry {
        KBox::into_raw(owner)
    }
}

// SAFETY: Link projection and recovery use the same links field offset in a
// live WakeEntry. KBox ownership keeps that allocation stable while linked;
// private consuming APIs prevent duplicate insertion or external link access.
unsafe impl Adapter for WakeAdapter {
    type LinkOps = <LinkedListLink as DefaultLinkOps>::Ops;
    type PointerOps = Self;

    unsafe fn get_value(&self, link: NonNull<LinkedListLink>) -> *const WakeEntry {
        // SAFETY: Adapter supplies the links field previously obtained from
        // this live entry; subtracting its field offset recovers the allocation.
        unsafe {
            link.as_ptr()
                .cast::<u8>()
                .sub(offset_of!(WakeEntry, links))
                .cast()
        }
    }

    unsafe fn get_link(&self, value: *const WakeEntry) -> NonNull<LinkedListLink> {
        // SAFETY: Adapter supplies a live, aligned WakeEntry whose links field
        // stays in place until the collection returns its unique owner.
        let link = unsafe { core::ptr::addr_of!((*value).links) }.cast_mut();
        // SAFETY: A field address within the supplied live entry cannot be null.
        unsafe { NonNull::new_unchecked(link) }
    }

    fn link_ops(&self) -> &Self::LinkOps {
        &self.links
    }

    fn link_ops_mut(&mut self) -> &mut Self::LinkOps {
        &mut self.links
    }

    fn pointer_ops(&self) -> &Self::PointerOps {
        self
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Exercise the owned PTE mutation paths, with completion observed by each hold.

use alloc::{alloc::Layout, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};
use unittest::def_test;

use super::TestEntry;
use crate::{PageSize, PageTable64, PagingFlags, PagingHandler, PagingMetaData, PtError};

struct Lifetime {
    address: usize,
    bytes: usize,
    withdrawn_at: AtomicUsize,
    invalidated_at: AtomicUsize,
    drops: AtomicUsize,
}

struct Trace {
    pages: Vec<Lifetime>,
    sequence: AtomicUsize,
    singles: AtomicUsize,
    fulls: AtomicUsize,
    fences: AtomicUsize,
}

impl Trace {
    fn withdraw(&self, index: usize) {
        self.pages[index]
            .withdrawn_at
            .store(self.sequence.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    fn invalidate(&self, address: Option<VirtAddr>) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        if address.is_some() {
            self.singles.fetch_add(1, Ordering::Relaxed);
        } else {
            self.fulls.fetch_add(1, Ordering::Relaxed);
        }
        for page in &self.pages {
            if address
                .is_none_or(|address| address.as_usize().wrapping_sub(page.address) < page.bytes)
            {
                page.invalidated_at.store(sequence, Ordering::Relaxed);
            }
        }
    }

    fn assert_counts(&self, singles: usize, fulls: usize) {
        assert_eq!(self.singles.load(Ordering::Relaxed), singles);
        assert_eq!(self.fulls.load(Ordering::Relaxed), fulls);
    }

    fn assert_drops(&self, index: usize, count: usize) {
        assert_eq!(self.pages[index].drops.load(Ordering::Relaxed), count);
    }
}

// Only GatherMeta uses this hook. Each serial test installs a scoped fixture
// before constructing its inactive page table and drops that table first.
static ACTIVE_TRACE: AtomicPtr<Trace> = AtomicPtr::new(core::ptr::null_mut());

struct Fixture(Arc<Trace>);

impl Fixture {
    fn new(layout: impl IntoIterator<Item = (usize, usize)>) -> Self {
        let trace = Arc::new(Trace {
            pages: layout
                .into_iter()
                .map(|(address, bytes)| Lifetime {
                    address,
                    bytes,
                    withdrawn_at: AtomicUsize::new(usize::MAX),
                    invalidated_at: AtomicUsize::new(0),
                    drops: AtomicUsize::new(0),
                })
                .collect(),
            sequence: AtomicUsize::new(0),
            singles: AtomicUsize::new(0),
            fulls: AtomicUsize::new(0),
            fences: AtomicUsize::new(0),
        });
        assert!(
            ACTIVE_TRACE
                .compare_exchange(
                    core::ptr::null_mut(),
                    Arc::as_ptr(&trace).cast_mut(),
                    Ordering::Release,
                    Ordering::Relaxed,
                )
                .is_ok()
        );
        Self(trace)
    }

    fn reference(&self, index: usize) -> Reference {
        Reference {
            trace: Arc::clone(&self.0),
            index,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        ACTIVE_TRACE.store(core::ptr::null_mut(), Ordering::Release);
        assert_eq!(Arc::strong_count(&self.0), 1, "all mapping holds retired");
    }
}

fn with_active_trace(f: impl FnOnce(&Trace)) {
    let ptr = ACTIVE_TRACE.load(Ordering::Acquire);
    assert!(!ptr.is_null());
    // SAFETY: Only the serial tests below invoke GatherMeta. Their Fixture
    // owns this Arc allocation until all table operations and destructors
    // complete. The reference is used only during the synchronous hook call.
    let trace = unsafe { &*ptr };
    f(trace);
}

struct Reference {
    trace: Arc<Trace>,
    index: usize,
}

impl Drop for Reference {
    fn drop(&mut self) {
        let page = &self.trace.pages[self.index];
        let withdrawn = page.withdrawn_at.load(Ordering::Relaxed);
        if withdrawn != usize::MAX {
            assert!(
                page.invalidated_at.load(Ordering::Relaxed) > withdrawn,
                "hold ended before its PTE invalidation completed"
            );
        }
        page.drops.fetch_add(1, Ordering::Relaxed);
    }
}

struct GatherHandler;

impl PagingHandler for GatherHandler {
    type MappingReference = Reference;

    fn mapping_reference_paddr(reference: &Reference) -> PhysAddr {
        PhysAddr::from(0x4000_0000 + reference.index * (1 << 30))
    }

    fn mapping_reference_bytes(reference: &Reference) -> usize {
        reference.trace.pages[reference.index].bytes
    }

    fn try_clone_mapping_reference(reference: &Reference) -> crate::PtResult<Reference> {
        Ok(Reference {
            trace: Arc::clone(&reference.trace),
            index: reference.index,
        })
    }

    fn alloc_frame() -> Option<PhysAddr> {
        let layout = Layout::from_size_align(PAGE_SIZE_4K, PAGE_SIZE_4K).unwrap();
        // SAFETY: The nonzero page layout is valid; allocation failure is
        // returned as None. Successful storage is zeroed and 4 KiB aligned.
        let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
        (!ptr.is_null()).then_some(PhysAddr::from(ptr as usize))
    }

    fn dealloc_frame(address: PhysAddr) {
        let layout = Layout::from_size_align(PAGE_SIZE_4K, PAGE_SIZE_4K).unwrap();
        // SAFETY: PageTable64 returns each table from alloc_frame once, after
        // its last software use. These fixtures never activate hardware roots.
        unsafe { alloc::alloc::dealloc(address.as_usize() as *mut u8, layout) };
    }

    fn p2v(address: PhysAddr) -> VirtAddr {
        VirtAddr::from(address.as_usize())
    }
}

struct GatherMeta;

impl PagingMetaData for GatherMeta {
    type VirtAddr = VirtAddr;

    const INVALID_TO_VALID_NEEDS_TLBI: bool = false;
    const LEVELS: usize = 4;
    const PA_MAX_BITS: usize = usize::BITS as usize - 1;
    const VA_MAX_BITS: usize = usize::BITS as usize - 1;

    fn vaddr_is_valid(_address: usize) -> bool {
        true
    }

    fn flush_tlb(address: Option<VirtAddr>) {
        with_active_trace(|trace| trace.invalidate(address));
    }

    fn fence_fresh_map() {
        with_active_trace(|trace| {
            trace.fences.fetch_add(1, Ordering::Relaxed);
        });
    }
}

type Table = PageTable64<GatherMeta, TestEntry, GatherHandler>;

#[def_test(serial)]
fn owned_unmap_aggregates_across_retirement_boundaries() {
    for count in [1, 16, 17, 32, 33, 255, 256, 257, 1024] {
        let fixture = Fixture::new((0..count).map(|index| (index * PAGE_SIZE_4K, PAGE_SIZE_4K)));
        let mut table = Table::try_new_kernel().unwrap();
        let mut edit = table.modify();
        for index in 0..count {
            edit.map_owned(
                VirtAddr::from(index * PAGE_SIZE_4K),
                fixture.reference(index),
                PagingFlags::READ,
            )
            .unwrap();
        }
        assert!(!edit.finish().had_pending());
        assert_eq!(fixture.0.fences.load(Ordering::Relaxed), 1);
        for index in 0..count {
            fixture.0.withdraw(index);
            edit.unmap(VirtAddr::from(index * PAGE_SIZE_4K)).unwrap();
        }
        edit.finish();
        let tail = count % 256;
        fixture.0.assert_counts(
            if tail <= 16 { tail } else { 0 },
            count / 256 + usize::from(tail > 16),
        );
        for index in 0..count {
            fixture.0.assert_drops(index, 1);
        }
        assert!(!edit.finish().had_pending());
        drop(edit);
        drop(table);
    }
}

#[def_test(serial)]
fn sparse_owned_and_raw_unmap_share_one_invalidation_summary() {
    let count = 33;
    let fixture = Fixture::new((0..count).map(|index| (index * PAGE_SIZE_4K, PAGE_SIZE_4K)));
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    for index in 0..count {
        let address = VirtAddr::from(index * PAGE_SIZE_4K);
        if index % 2 == 0 {
            edit.map_owned(address, fixture.reference(index), PagingFlags::READ)
                .unwrap();
            fixture.0.withdraw(index);
        } else {
            edit.map(
                address,
                PhysAddr::from(0x8000_0000),
                PageSize::Size4K,
                PagingFlags::READ,
            )
            .unwrap();
        }
    }
    edit.finish();
    edit.unmap_sparse_region(VirtAddr::from(0), count * PAGE_SIZE_4K, PageSize::Size4K)
        .unwrap();
    edit.finish();
    fixture.0.assert_counts(0, 1);
    for index in 0..count {
        fixture.0.assert_drops(index, usize::from(index % 2 == 0));
    }
}

#[def_test(serial)]
fn unmap_error_finishes_only_the_successful_owned_prefix() {
    let fixture = Fixture::new((0..33).map(|index| (index * PAGE_SIZE_4K, PAGE_SIZE_4K)));
    let mut table = Table::try_new_kernel().unwrap();
    {
        let mut edit = table.modify();
        for index in 0..33 {
            if index == 32 {
                continue;
            }
            edit.map_owned(
                VirtAddr::from(index * PAGE_SIZE_4K),
                fixture.reference(index),
                PagingFlags::READ,
            )
            .unwrap();
            fixture.0.withdraw(index);
        }
        edit.finish();
        assert_eq!(
            edit.unmap_region(VirtAddr::from(0), 33 * PAGE_SIZE_4K),
            Err(PtError::NotMapped)
        );
        for index in 0..32 {
            fixture.0.assert_drops(index, 0);
        }
    }
    fixture.0.assert_counts(0, 1);
    for index in 0..32 {
        fixture.0.assert_drops(index, 1);
    }
    fixture.0.assert_drops(32, 0);
}

#[def_test(serial)]
fn retirement_reservation_failure_makes_progress_without_carrying_a_hold() {
    for count in [1, 16, 17, 32, 33, 1024] {
        let fixture = Fixture::new((0..count).map(|index| (index * PAGE_SIZE_4K, PAGE_SIZE_4K)));
        let mut table = Table::try_new_kernel().unwrap();
        let mut edit = table.modify();
        edit.retired_mapping_refs.should_fail_reservation = true;
        for index in 0..count {
            edit.map_owned(
                VirtAddr::from(index * PAGE_SIZE_4K),
                fixture.reference(index),
                PagingFlags::READ,
            )
            .unwrap();
        }
        edit.finish();
        for index in 0..count {
            fixture.0.withdraw(index);
            edit.unmap(VirtAddr::from(index * PAGE_SIZE_4K)).unwrap();
        }
        edit.finish();
        fixture.0.assert_counts(count % 17, count / 17);
        for index in 0..count {
            fixture.0.assert_drops(index, 1);
        }
    }
}

#[def_test(serial)]
fn owned_replacement_deduplicates_one_address_with_many_retired_holds() {
    let fixture = Fixture::new((0..257).map(|_| (0, PAGE_SIZE_4K)));
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    let address = VirtAddr::from(0);
    edit.map_owned(address, fixture.reference(0), PagingFlags::READ)
        .unwrap();
    edit.finish();
    for index in 1..257 {
        let snapshot = edit.query_entry(address).unwrap();
        fixture.0.withdraw(index - 1);
        edit.replace_owned_if_same(
            address,
            snapshot,
            fixture.reference(index),
            PagingFlags::READ,
        )
        .unwrap();
    }
    fixture.0.assert_counts(1, 0);
    for index in 0..256 {
        fixture.0.assert_drops(index, 1);
    }
    fixture.0.assert_drops(256, 0);
    assert!(!edit.finish().had_pending());
    fixture.0.withdraw(256);
    edit.protect(address, PagingFlags::empty()).unwrap();
    fixture.0.assert_drops(256, 0);
    drop(edit);
    fixture.0.assert_counts(2, 0);
    fixture.0.assert_drops(256, 1);
}

#[def_test(serial)]
fn sparse_owned_error_keeps_huge_leaf_and_retires_the_prefix() {
    let fixture = Fixture::new(
        (0..33)
            .map(|index| (index * PAGE_SIZE_4K, PAGE_SIZE_4K))
            .chain(core::iter::once((1 << 21, 1 << 21))),
    );
    let mut table = Table::try_new_kernel().unwrap();
    {
        let mut edit = table.modify();
        for index in 0..33 {
            edit.map_owned(
                VirtAddr::from(index * PAGE_SIZE_4K),
                fixture.reference(index),
                PagingFlags::READ,
            )
            .unwrap();
            fixture.0.withdraw(index);
        }
        edit.map_owned_sized(
            VirtAddr::from(1 << 21),
            fixture.reference(33),
            PageSize::Size2M,
            PagingFlags::READ,
        )
        .unwrap();
        edit.finish();
        assert_eq!(
            edit.unmap_sparse_region(VirtAddr::from(0), 2 << 21, PageSize::Size4K),
            Err(PtError::MappedToHugePage),
        );
        assert!(edit.query(VirtAddr::from(1 << 21)).is_ok());
        fixture.0.assert_drops(33, 0);
    }
    fixture.0.assert_counts(0, 1);
    for index in 0..33 {
        fixture.0.assert_drops(index, 1);
    }
    {
        let mut edit = table.modify();
        fixture.0.withdraw(33);
        edit.unmap(VirtAddr::from((1 << 21) + PAGE_SIZE_4K))
            .unwrap();
        fixture.0.assert_drops(33, 0);
    }
    fixture.0.assert_counts(1, 1);
    fixture.0.assert_drops(33, 1);
}

#[def_test(serial)]
fn small_range_covers_holes_reverse_order_and_top_address() {
    let last_page = usize::MAX & !(PAGE_SIZE_4K - 1);
    let fixture = Fixture::new([
        (last_page - 2 * PAGE_SIZE_4K, PAGE_SIZE_4K),
        (last_page, PAGE_SIZE_4K),
    ]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    // Raw leaves exercise inclusive range bounds at the top address without
    // a lifetime index affecting the arithmetic under test.
    for offset in [0, 2] {
        edit.map(
            VirtAddr::from(last_page - offset * PAGE_SIZE_4K),
            PhysAddr::from(0x4000_0000),
            PageSize::Size4K,
            PagingFlags::READ,
        )
        .unwrap();
    }
    edit.finish();
    for offset in [0, 2] {
        edit.unmap(VirtAddr::from(last_page - offset * PAGE_SIZE_4K))
            .unwrap();
    }
    edit.finish();
    fixture.0.assert_counts(3, 0);
}

#[def_test(serial)]
fn empty_owned_replacement_at_drain_releases_both_holds_in_the_same_batch() {
    let fixture = Fixture::new(
        (0..256)
            .map(|index| (index * PAGE_SIZE_4K, PAGE_SIZE_4K))
            .chain(core::iter::once((255 * PAGE_SIZE_4K, PAGE_SIZE_4K))),
    );
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    for index in 0..256 {
        edit.map_owned(
            VirtAddr::from(index * PAGE_SIZE_4K),
            fixture.reference(index),
            PagingFlags::READ,
        )
        .unwrap();
    }
    edit.finish();
    for index in 0..255 {
        fixture.0.withdraw(index);
        edit.unmap(VirtAddr::from(index * PAGE_SIZE_4K)).unwrap();
    }
    let address = VirtAddr::from(255 * PAGE_SIZE_4K);
    let snapshot = edit.query_entry(address).unwrap();
    fixture.0.withdraw(255);
    fixture.0.withdraw(256);
    edit.replace_owned_if_same(
        address,
        snapshot,
        fixture.reference(256),
        PagingFlags::empty(),
    )
    .unwrap();
    fixture.0.assert_counts(0, 1);
    for index in 0..257 {
        fixture.0.assert_drops(index, 1);
    }
    assert!(!edit.finish().had_pending());
}

#[def_test(serial)]
fn owned_permission_clears_and_failed_compare_preserve_lifetime_order() {
    let fixture = Fixture::new((0..34).map(|index| ((index % 33) * PAGE_SIZE_4K, PAGE_SIZE_4K)));
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    for index in 0..33 {
        edit.map_owned(
            VirtAddr::from(index * PAGE_SIZE_4K),
            fixture.reference(index),
            PagingFlags::READ,
        )
        .unwrap();
    }
    edit.finish();
    let address = VirtAddr::from(0);
    let stale = edit.query_entry(address).unwrap();
    edit.protect(address, PagingFlags::READ | PagingFlags::WRITE)
        .unwrap();
    assert!(matches!(
        edit.replace_owned_if_same(address, stale, fixture.reference(33), PagingFlags::READ),
        Err(crate::PteReplaceError::Changed { .. })
    ));
    fixture.0.assert_drops(0, 0);
    fixture.0.assert_drops(33, 1);
    for index in 0..33 {
        let address = VirtAddr::from(index * PAGE_SIZE_4K);
        fixture.0.withdraw(index);
        match index % 3 {
            0 => {
                edit.protect(address, PagingFlags::empty()).unwrap();
            }
            1 => {
                let (physical, ..) = edit.query(address).unwrap();
                edit.remap(address, physical, PagingFlags::empty()).unwrap();
            }
            _ => {
                let snapshot = edit.query_entry(address).unwrap();
                edit.replace_if_same(address, snapshot, snapshot.paddr(), PagingFlags::empty())
                    .unwrap();
            }
        }
        fixture.0.assert_drops(index, 0);
    }
    drop(edit);
    fixture.0.assert_counts(0, 1);
    for index in 0..34 {
        fixture.0.assert_drops(index, 1);
    }
}

#[def_test(serial)]
fn owned_huge_leaves_keep_whole_capabilities_until_full_completion() {
    let fixture = Fixture::new([(1 << 21, 1 << 21), (1 << 30, 1 << 30)]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    for (index, size) in [PageSize::Size2M, PageSize::Size1G].into_iter().enumerate() {
        edit.map_owned_sized(
            VirtAddr::from(size as usize),
            fixture.reference(index),
            size,
            PagingFlags::READ,
        )
        .unwrap();
    }
    edit.finish();
    fixture.0.withdraw(0);
    edit.unmap(VirtAddr::from((1 << 21) + 19 * PAGE_SIZE_4K))
        .unwrap();
    fixture.0.withdraw(1);
    edit.protect(
        VirtAddr::from((1 << 30) + 23 * PAGE_SIZE_4K),
        PagingFlags::empty(),
    )
    .unwrap();
    for index in 0..2 {
        fixture.0.assert_drops(index, 0);
    }
    drop(edit);
    fixture.0.assert_counts(0, 1);
    for index in 0..2 {
        fixture.0.assert_drops(index, 1);
    }
}

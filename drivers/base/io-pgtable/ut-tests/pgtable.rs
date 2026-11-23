// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Backend tests: 4 KiB / 2 MiB mapping, block splitting and unmap release.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use kspin::SpinNoPreempt;

use unittest::def_test;

use crate::{
    ArmLpae, ArmLpaeConfig, Granule, IoPageTable, IommuProt, Iova, PgTableError, RockchipV2,
    TableAllocator,
};

/// Fake allocator: hands out real zeroed pages from the unit-test allocator
/// and assigns linear fake physical addresses, remembering every live page
/// for leak checks. Addresses are tracked as `usize` so the fake stays
/// `Send + Sync`.
struct FakeAllocator {
    next_physical: AtomicU64,
    live: SpinNoPreempt<Vec<(usize, u64)>>,
}

impl Default for FakeAllocator {
    fn default() -> Self {
        // Fake physical addresses start above zero so the "root table is
        // present" checks can rely on nonzero roots.
        Self {
            next_physical: AtomicU64::new(0x1000_0000),
            live: SpinNoPreempt::new(Vec::new()),
        }
    }
}

impl FakeAllocator {
    fn live_count(&self) -> usize {
        self.live.lock().len()
    }
}

impl TableAllocator for FakeAllocator {
    fn alloc_zeroed_table_page(&self) -> Option<(*mut u8, u64)> {
        // SAFETY: a page-aligned zeroed allocation; the page-table backend
        // owns it until free_table_page.
        let layout = core::alloc::Layout::from_size_align(4096, 4096).ok()?;
        // SAFETY: the layout is a non-zero page-aligned request; the fake
        // owns the returned page until free_table_page.
        let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            return None;
        }
        let physical = self.next_physical.fetch_add(4096, Ordering::SeqCst);
        self.live.lock().push((ptr as usize, physical));
        Some((ptr, physical))
    }

    /// # Safety
    ///
    /// `(ptr, physical)` must be a live allocation from this allocator.
    unsafe fn free_table_page(&self, ptr: *mut u8, physical: u64) {
        let removed = {
            let mut live = self.live.lock();
            let before = live.len();
            live.retain(|(addr, phys)| !(*addr == ptr as usize && *phys == physical));
            before != live.len()
        };
        if removed {
            // SAFETY: the pointer is a live page-sized allocation.
            unsafe {
                alloc::alloc::dealloc(
                    ptr,
                    core::alloc::Layout::from_size_align(4096, 4096)
                        .expect("fixed table page layout is valid"),
                );
            }
        }
    }
}

fn rw() -> IommuProt {
    IommuProt {
        read: true,
        write: true,
        cacheable: true,
    }
}

fn ro() -> IommuProt {
    IommuProt {
        read: true,
        write: false,
        cacheable: true,
    }
}

const GIB: u64 = 1 << 30;
const MIB2: u64 = 2 << 20;

#[def_test(serial)]
fn arm_lpae_maps_4k_pages_and_translates() {
    let allocator = Arc::new(FakeAllocator::default());
    let mut table =
        ArmLpae::new(ArmLpaeConfig::SMMU_S1_DEFAULT, allocator.clone()).expect("table builds");
    assert_eq!(table.granule(), Granule::Size4K);
    assert_eq!(table.pgsize_bitmap() & (1 << 12), 1 << 12);
    assert_ne!(table.root_table_address(), 0);

    let iova = Iova::new(0x0000_1234_5000);
    let paddr = 0x0000_00ab_c000;
    let mapped = table
        .map_pages(iova, paddr, 4096, 4, rw())
        .expect("4 pages map");
    assert_eq!(mapped, 4);
    for step in 0..4u64 {
        assert_eq!(
            table.translate(Iova::new(iova.as_u64() + step * 4096)),
            Some(paddr + step * 4096),
            "each page translates back to its frame"
        );
    }
    assert_eq!(
        table.translate(Iova::new(iova.as_u64() - 4096)),
        None,
        "unmapped neighbour stays absent"
    );

    // Remapping the same range with the same attributes is idempotent.
    assert_eq!(
        table.map_pages(iova, paddr, 4096, 4, rw()).expect("remap"),
        4
    );
    // Remapping with different attributes conflicts.
    assert_eq!(
        table.map_pages(iova, paddr, 4096, 4, ro()),
        Err(PgTableError::Conflict)
    );

    // Unmap releases every entry.
    assert_eq!(table.unmap_pages(iova, 4096, 4).expect("unmap"), 4);
    assert_eq!(table.translate(iova), None);
}

#[def_test(serial)]
fn arm_lpae_maps_2m_blocks_and_splits_for_4k() {
    let allocator = Arc::new(FakeAllocator::default());
    let mut table =
        ArmLpae::new(ArmLpaeConfig::SMMU_S1_DEFAULT, allocator.clone()).expect("table builds");
    assert_eq!(table.pgsize_bitmap() & (1 << 21), 1 << 21);

    let block_iova = Iova::new(0x0000_4000_0000);
    let block_paddr = 0x0000_8000_0000;
    assert_eq!(
        table.map_pages(block_iova, block_paddr, MIB2, 1, rw()).ok(),
        Some(1)
    );
    assert_eq!(
        table.translate(Iova::new(block_iova.as_u64() + 0x123_456)),
        Some(block_paddr + 0x123_456),
        "any offset within the block translates"
    );

    // Mapping one 4 KiB page inside the block must split it down to pages.
    let head = Iova::new(block_iova.as_u64());
    assert_eq!(
        table.map_pages(head, block_paddr, 4096, 1, rw()).ok(),
        Some(1)
    );
    // The rest of the former block still translates (fanned-out entries).
    assert_eq!(
        table.translate(Iova::new(block_iova.as_u64() + MIB2 - 4096)),
        Some(block_paddr + MIB2 - 4096)
    );
    // And the split head now resolves through the page level.
    assert_eq!(table.translate(head), Some(block_paddr));

    // Unmapping the head page leaves the split remainder intact.
    assert_eq!(table.unmap_pages(head, 4096, 1).ok(), Some(1));
    assert_eq!(table.translate(head), None);
    assert_eq!(
        table.translate(Iova::new(block_iova.as_u64() + 4096)),
        Some(block_paddr + 4096)
    );
}

#[def_test(serial)]
fn arm_lpape_1g_block_maps_at_start_level() {
    let allocator = Arc::new(FakeAllocator::default());
    let mut table =
        ArmLpae::new(ArmLpaeConfig::SMMU_S1_DEFAULT, allocator.clone()).expect("table builds");
    assert_eq!(table.pgsize_bitmap() & GIB, GIB);
    let iova = Iova::new(GIB);
    let paddr = 0x2 * GIB;
    assert_eq!(table.map_pages(iova, paddr, GIB, 1, rw()).ok(), Some(1));
    // The block covers [1 GiB, 2 GiB); both ends translate.
    assert_eq!(table.translate(Iova::new(GIB)), Some(2 * GIB));
    assert_eq!(
        table.translate(Iova::new(2 * GIB - 1)),
        Some(3 * GIB - 1),
        "identity 1 GiB block"
    );
}

#[def_test(serial)]
fn arm_lpae_rejects_out_of_range_and_unaligned() {
    let allocator = Arc::new(FakeAllocator::default());
    let mut table = ArmLpae::new(
        ArmLpaeConfig {
            va_bits: 33,
            pa_bits: 32,
            coherent_walk: false,
        },
        allocator,
    )
    .expect("table builds");
    // Above the 33-bit IOVA window.
    assert_eq!(
        table.map_pages(Iova::new(1 << 34), 0, 4096, 1, rw()),
        Err(PgTableError::IovaOutOfRange)
    );
    // Above the 32-bit output window.
    assert_eq!(
        table.map_pages(Iova::new(0), 1 << 33, 4096, 1, rw()),
        Err(PgTableError::PhysOutOfRange)
    );
    // Not page aligned.
    assert_eq!(
        table.map_pages(Iova::new(4096), 4099, 4096, 1, rw()),
        Err(PgTableError::Unaligned)
    );
    // Unsupported granularity.
    assert_eq!(
        table.map_pages(Iova::new(0), 0, 8192, 1, rw()),
        Err(PgTableError::UnsupportedPageSize)
    );
}

#[def_test(serial)]
fn arm_lpae_frees_all_tables_on_drop() {
    let allocator = Arc::new(FakeAllocator::default());
    {
        let mut table = ArmLpae::new(ArmLpaeConfig::SMMU_S1_DEFAULT, allocator.clone())
            .expect("table builds");
        // Map sparse 4 KiB pages across distant regions: each allocates an
        // intermediate table.
        for region in 0..4u64 {
            let iova = Iova::new(region * (512 * MIB2));
            assert_eq!(
                table.map_pages(iova, region * (512 * MIB2), 4096, 1, rw()).ok(),
                Some(1)
            );
        }
        assert!(allocator.live_count() > 1, "intermediate tables exist");
    }
    assert_eq!(
        allocator.live_count(),
        0,
        "dropping the backend releases every table page"
    );
}

#[def_test(serial)]
fn rockchip_v2_maps_unmaps_and_translates() {
    let allocator = Arc::new(FakeAllocator::default());
    let mut table = RockchipV2::new(allocator.clone()).expect("table builds");
    assert_eq!(table.pgsize_bitmap(), 4096);
    assert_ne!(table.root_table_address(), 0);

    // Two pages in two different directory entries (far apart in the IOVA
    // space: different DTE indices).
    let first = Iova::new(0x0000_0000_1000);
    let second = Iova::new(0x0000_0100_3000);
    let phys_a = 0x0000_0012_4000;
    let phys_b = 0x0000_0034_5000;
    assert_eq!(
        table.map_pages(first, phys_a, 4096, 1, rw()).ok(),
        Some(1)
    );
    assert_eq!(
        table.map_pages(second, phys_b, 4096, 2, ro()).ok(),
        Some(2)
    );
    assert_eq!(table.translate(first), Some(phys_a));
    assert_eq!(table.translate(second), Some(phys_b));
    assert_eq!(
        table.translate(Iova::new(second.as_u64() + 4096)),
        Some(phys_b + 4096)
    );

    // Unmap one page of the two-page range; the neighbour survives.
    assert_eq!(table.unmap_pages(second, 4096, 1).ok(), Some(1));
    assert_eq!(table.translate(second), None);
    assert_eq!(
        table.translate(Iova::new(second.as_u64() + 4096)),
        Some(phys_b + 4096)
    );

    // Read-only flags survive a translate round-trip through map conflict
    // detection: rewriting the first page read-only conflicts with its RW
    // descriptor.
    assert_eq!(
        table.map_pages(first, phys_a, 4096, 1, ro()),
        Err(PgTableError::Conflict)
    );
}

#[def_test(serial)]
fn rockchip_v2_validates_ranges() {
    let allocator = Arc::new(FakeAllocator::default());
    let mut table = RockchipV2::new(allocator).expect("table builds");
    // 2 MiB "block" mappings are not a Rockchip v2 page size.
    assert_eq!(
        table.map_pages(Iova::new(0), 0, MIB2, 1, rw()),
        Err(PgTableError::UnsupportedPageSize)
    );
    // The window covers 4 GiB: anything above faults out.
    assert_eq!(
        table.map_pages(Iova::new(1 << 32), 0, 4096, 1, rw()),
        Err(PgTableError::IovaOutOfRange)
    );
    // v2 descriptors carry 40-bit output addresses: 41-bit PA is rejected.
    assert_eq!(
        table.map_pages(Iova::new(0), 1 << 40, 4096, 1, rw()),
        Err(PgTableError::PhysOutOfRange)
    );
}

#[def_test(serial)]
fn rockchip_v2_frees_all_tables_on_drop() {
    let allocator = Arc::new(FakeAllocator::default());
    {
        let mut table = RockchipV2::new(allocator.clone()).expect("table builds");
        let iova = Iova::new(0x1234_5000);
        assert_eq!(
            table.map_pages(iova, 0x9abc_d000, 4096, 1, rw()).ok(),
            Some(1)
        );
        assert!(allocator.live_count() >= 2, "PT page allocated on demand");
    }
    assert_eq!(allocator.live_count(), 0, "drop releases DT and PT pages");
}

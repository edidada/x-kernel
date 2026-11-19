// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use memaddr::VirtAddr;
use unittest::def_test;

use super::{PageSize, PagingFlags, PagingMetaData};

struct DummyMeta;

impl PagingMetaData for DummyMeta {
    type VirtAddr = VirtAddr;

    const LEVELS: usize = 4;
    const PA_MAX_BITS: usize = 36;
    const VA_MAX_BITS: usize = 39;

    fn flush_tlb(_vaddr: Option<Self::VirtAddr>) {}

    #[cfg(feature = "smp")]
    fn flush_tlb_all_cpus(_vaddr: Option<Self::VirtAddr>) {}
}

#[def_test]
fn test_paging_flags_bits() {
    let flags = PagingFlags::READ | PagingFlags::WRITE;
    assert!(flags.contains(PagingFlags::READ));
    assert!(flags.contains(PagingFlags::WRITE));
    assert!(!flags.contains(PagingFlags::EXECUTE));
}

#[def_test]
fn test_page_size_alignment() {
    assert!(PageSize::Size4K.is_aligned(0x2000));
    assert!(!PageSize::Size4K.is_aligned(0x2001));
    assert!(PageSize::Size2M.is_huge());
}

#[def_test]
fn test_paging_metadata_bounds() {
    assert!(DummyMeta::paddr_is_valid(0));
    assert!(DummyMeta::paddr_is_valid((1 << DummyMeta::PA_MAX_BITS) - 1));
    assert!(!DummyMeta::paddr_is_valid(1 << DummyMeta::PA_MAX_BITS));
}

#[cfg(feature = "smp")]
#[def_test]
fn default_smp_hooks_use_the_required_full_scope_provider() {
    use core::sync::atomic::{AtomicUsize, Ordering};
    static FULL_SCOPE_CALLS: AtomicUsize = AtomicUsize::new(0);
    struct FullScopeMeta;
    impl PagingMetaData for FullScopeMeta {
        type VirtAddr = VirtAddr;

        const LEVELS: usize = 4;
        const PA_MAX_BITS: usize = 36;
        const VA_MAX_BITS: usize = 39;

        fn flush_tlb(_address: Option<VirtAddr>) {}

        fn flush_tlb_all_cpus(_address: Option<VirtAddr>) {
            FULL_SCOPE_CALLS.fetch_add(1, Ordering::Relaxed);
        }
    }
    FULL_SCOPE_CALLS.store(0, Ordering::Relaxed);
    let mut targets = kcpu_id_map::KCpuMask::new();
    targets.set(0, true);
    FullScopeMeta::flush_tlb_process_mask(None, targets);
    FullScopeMeta::flush_tlb_process(None);
    FullScopeMeta::flush_tlb_all_cpus(None);
    assert_eq!(FULL_SCOPE_CALLS.load(Ordering::Relaxed), 3);
}

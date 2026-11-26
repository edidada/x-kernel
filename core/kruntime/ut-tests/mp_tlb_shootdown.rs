// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcpu_id_map::LogicalCpuId;
use khal::percpu::this_cpu_id;
use ktask::KCpuMask;
use memaddr::{PhysAddr, VirtAddr};
use unittest::{assert, assert_eq, def_test};

fn enable_tlb_shootdown_for_test() {
    // These integration tests exercise the runtime after SMP bring-up, so
    // they must establish the same "all APs are ready" precondition that
    // production TLB shootdowns require. Do not rely on other unit tests
    // leaving this global gate enabled.
    kipi::tlb::mark_all_cpus_started();
}

/// Test C: Proves flush_all_cpus sends IPIs to all online CPUs and waits
/// for remote completion.
#[def_test(serial)]
fn test_flush_all_targeted_shootdown() {
    let cpu_num = kcpu_id_map::nr_cpus();
    if cpu_num >= 2 {
        enable_tlb_shootdown_for_test();
        // Call flush_all — if it returns, the remote CPU received the IPI,
        // called handle_shootdown() → karch::flush_tlb(), and set COMPLETED.
        kipi::tlb::trigger_flush_all(None);
    }
}

/// Test D: Proves context switch updates on_cpu_mask.
#[def_test(serial)]
fn test_context_switch_updates_on_cpu_mask() {
    let cpu_num = kcpu_id_map::nr_cpus();
    if cpu_num >= 2 {
        let remote_cpu = if this_cpu_id() == LogicalCpuId::new(0) {
            1
        } else {
            0
        };

        static TASK_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);
        static TASK_MASK_BIT: AtomicBool = AtomicBool::new(false);

        let mut affinity = KCpuMask::new();
        affinity.set(remote_cpu, true);

        let task = ktask::TaskInner::new_kthread_with_entry(
            ktask::PreparedTaskEntry::new(move || {
                TASK_CPU.store(khal::percpu::this_cpu_id().as_usize(), Ordering::Release);
                if let Some(t) = ktask::current_may_uninit() {
                    TASK_MASK_BIT.store(t.on_cpu_mask().get(remote_cpu), Ordering::Release);
                }
            }).unwrap(),
            "test_ctx_mask".into(),
            0x2000,
        )
        .expect("test kernel thread identity allocation should succeed");
        task.set_cpumask(affinity);
        let task = ktask::spawn_task(task);

        task.join();
        assert_eq!(TASK_CPU.load(Ordering::Acquire), remote_cpu);
        assert!(TASK_MASK_BIT.load(Ordering::Acquire));
    }
}

/// Test E: Proves PageTableMut::finish() calls flush_tlb_all_cpus.
///
/// Creates a real page table, maps a page (setting ToFlush::Addresses),
/// then drops the PageTableMut which calls finish() → flush_tlb_all_cpus
/// → flush_remote → targeted IPI → completion.
/// Flush completion does not rebuild residency state.
#[def_test(serial)]
fn test_finish_triggers_cross_cpu_flush() {
    let cpu_num = kcpu_id_map::nr_cpus();
    if cpu_num >= 2 {
        enable_tlb_shootdown_for_test();
        // Create a new page table (allocates a root page via kernel allocator).
        let mut pt = khal::paging::PageTable::try_new().expect("test page table alloc");

        // Map a page to set ToFlush::Addresses.
        let test_vaddr = VirtAddr::from(0x7F00_0000usize);
        let mut pt_mut = pt.modify();
        pt_mut
            .map_raw(
                test_vaddr,
                PhysAddr::from(0x8000_0000usize),
                khal::paging::PageSize::Size4K,
                khal::paging::MappingFlags::READ,
            )
            .expect("fresh test page table must accept its first mapping");

        // Dropping pt_mut calls finish() → flush_tlb_all_cpus → flush_remote.
        // On architectures without hardware broadcast (x86_64, riscv64), this sends IPIs.
        // If the remote CPU doesn't respond, this hangs.
        drop(pt_mut);
    }
}

/// Test F: Proves TLB shootdown actually eliminates stale TLB entries.
///
/// Maps a virtual address to physical page P1 in the kernel page table,
/// has the remote CPU read it (populating its TLB with V→P1), then remaps
/// V to P2 with a shootdown. Verifies the remote CPU now reads P2's
/// content, proving the stale V→P1 TLB entry was invalidated.
#[def_test(serial)]
fn test_shootdown_clears_stale_tlb() {
    let cpu_num = kcpu_id_map::nr_cpus();
    if cpu_num >= 2 {
        enable_tlb_shootdown_for_test();
        let my_cpu = this_cpu_id();
        let remote_cpu = LogicalCpuId::new(if my_cpu == LogicalCpuId::new(0) { 1 } else { 0 });

        // Allocate two physical pages with different magic values.
        let v1 = kalloc::global_allocator()
            .alloc_pages(1, memaddr::PAGE_SIZE_4K, kalloc::UsageKind::PageTable)
            .expect("alloc page 1");
        let v2 = kalloc::global_allocator()
            .alloc_pages(1, memaddr::PAGE_SIZE_4K, kalloc::UsageKind::PageTable)
            .expect("alloc page 2");
        let p1 = khal::mem::v2p(VirtAddr::from(v1));
        let p2 = khal::mem::v2p(VirtAddr::from(v2));

        const MAGIC_A: u64 = 0xDEAD_BEEF_CAFE_BABE;
        const MAGIC_B: u64 = 0xCAFE_BABE_DEAD_BEEF;

        // SAFETY: v1 and v2 are valid kernel-virtual addresses backed by
        // freshly allocated physical pages owned by this test.  Writing
        // magic values through volatile pointers is safe because no other
        // code holds references to these pages.
        unsafe { core::ptr::write_volatile(v1 as *mut u64, MAGIC_A) };
        // SAFETY: v2 is the aligned kernel mapping of the second freshly
        // allocated page, exclusively owned by this test with no references.
        unsafe { core::ptr::write_volatile(v2 as *mut u64, MAGIC_B) };

        // Pick a kernel-high virtual address in the reserved region.
        // Must be valid across all architectures (SV39 needs ≥0xFFFFFFC0).
        let test_vaddr = VirtAddr::from(kaddr_layout::IOMAP_VADDR + kaddr_layout::IOMAP_VSIZE);

        // Phase 1: Map V → P1; remote CPU reads V → sees MAGIC_A.
        memspace::kernel_layout()
            .map_linear(
                test_vaddr,
                p1,
                memaddr::PAGE_SIZE_4K,
                khal::paging::MappingFlags::READ | khal::paging::MappingFlags::WRITE,
            )
            .expect("map V→P1");

        static REMOTE_READ: AtomicUsize = AtomicUsize::new(0);

        REMOTE_READ.store(0, Ordering::Relaxed);
        kipi::run_on_cpu(remote_cpu, move || {
            // SAFETY: test_vaddr is mapped read/write to p1 in the kernel page table
            // before this closure runs, and p1 backs an initialized u64 written above.
            let val = unsafe { core::ptr::read_volatile(test_vaddr.as_usize() as *const u64) };
            REMOTE_READ.store(val as usize, Ordering::Release);
        })
        .unwrap();
        while REMOTE_READ.load(Ordering::Acquire) == 0 {
            core::hint::spin_loop();
        }
        assert_eq!(REMOTE_READ.load(Ordering::Acquire), MAGIC_A as usize);

        // Kernel translations target all participating CPUs independently of
        // this task's residency mask.
        memspace::kernel_layout()
            .unmap(test_vaddr, memaddr::PAGE_SIZE_4K)
            .expect("unmap V");
        memspace::kernel_layout()
            .map_linear(
                test_vaddr,
                p2,
                memaddr::PAGE_SIZE_4K,
                khal::paging::MappingFlags::READ | khal::paging::MappingFlags::WRITE,
            )
            .expect("map V→P2");

        // Remote CPU reads V → must see MAGIC_B (proves stale TLB was flushed).
        REMOTE_READ.store(0, Ordering::Relaxed);
        kipi::run_on_cpu(remote_cpu, move || {
            // SAFETY: test_vaddr is remapped read/write to p2 in the kernel page table
            // before this closure runs, and p2 backs an initialized u64 written above.
            let val = unsafe { core::ptr::read_volatile(test_vaddr.as_usize() as *const u64) };
            REMOTE_READ.store(val as usize, Ordering::Release);
        })
        .unwrap();
        while REMOTE_READ.load(Ordering::Acquire) == 0 {
            core::hint::spin_loop();
        }
        assert_eq!(REMOTE_READ.load(Ordering::Acquire), MAGIC_B as usize);

        // Cleanup: unmap V, free pages.
        memspace::kernel_layout()
            .unmap(test_vaddr, memaddr::PAGE_SIZE_4K)
            .expect("test mapping must remain installed until cleanup");
        kalloc::global_allocator().dealloc_pages(v1, 1, kalloc::UsageKind::PageTable);
        kalloc::global_allocator().dealloc_pages(v2, 1, kalloc::UsageKind::PageTable);
    }
}

/// Test G: Proves that kernel page table TLB shootdown reaches ALL
/// online CPUs, regardless of the current task's CPU residency mask.
///
/// Kernel page table modifications (is_kernel=true) go through
/// `flush_all_cpus()`, which builds a full CPU mask via
/// `for_each_present_logical_cpu()`. Every online CPU receives a
/// shootdown IPI regardless of user-mm residency targeting.
///
/// ### What this test verifies
///
/// 1. **Correct shootdown**: after modifying the kernel page table, the
///    remote CPU still receives a TLB shootdown IPI and reads the new
///    mapping (MAGIC_B instead of stale MAGIC_A).
///
/// 2. **Task-local fallback irrelevant**: kernel page table shootdown does
///    not depend on the current task's local fallback mask.
#[def_test(serial)]
fn test_kernel_pt_miss_remote_without_mask() {
    let cpu_num = kcpu_id_map::nr_cpus();
    if cpu_num >= 2 {
        enable_tlb_shootdown_for_test();
        let my_cpu = this_cpu_id();
        let remote_cpu = LogicalCpuId::new(if my_cpu == LogicalCpuId::new(0) { 1 } else { 0 });

        // Ensure the current task's mask does NOT include the remote CPU.
        // This is the natural state for a kernel task that has only ever
        // run on its own CPU.
        let task = ktask::current_may_uninit().unwrap();
        task.reset_on_cpu_mask(my_cpu);

        // Allocate two physical pages with different magic values.
        let v1 = kalloc::global_allocator()
            .alloc_pages(1, memaddr::PAGE_SIZE_4K, kalloc::UsageKind::PageTable)
            .expect("alloc page 1");
        let v2 = kalloc::global_allocator()
            .alloc_pages(1, memaddr::PAGE_SIZE_4K, kalloc::UsageKind::PageTable)
            .expect("alloc page 2");
        let p1 = khal::mem::v2p(VirtAddr::from(v1));
        let p2 = khal::mem::v2p(VirtAddr::from(v2));

        const MAGIC_A: u64 = 0xDEAD_BEEF_CAFE_BABE;
        const MAGIC_B: u64 = 0xCAFE_BABE_DEAD_BEEF;

        // SAFETY: v1 and v2 are valid kernel-virtual addresses backed by
        // freshly allocated physical pages owned by this test.  Writing
        // magic values through volatile pointers is safe because no other
        // code holds references to these pages.
        unsafe { core::ptr::write_volatile(v1 as *mut u64, MAGIC_A) };
        // SAFETY: v2 is the aligned kernel mapping of the second freshly
        // allocated page, exclusively owned by this test with no references.
        unsafe { core::ptr::write_volatile(v2 as *mut u64, MAGIC_B) };

        let test_vaddr = VirtAddr::from(kaddr_layout::IOMAP_VADDR + kaddr_layout::IOMAP_VSIZE);

        // Phase 1: Map V → P1 in the kernel page table; remote CPU reads V.
        // This populates the remote CPU's TLB with V → P1.
        memspace::kernel_layout()
            .map_linear(
                test_vaddr,
                p1,
                memaddr::PAGE_SIZE_4K,
                khal::paging::MappingFlags::READ | khal::paging::MappingFlags::WRITE,
            )
            .expect("map V→P1");

        static REMOTE_READ_G: AtomicUsize = AtomicUsize::new(0);

        REMOTE_READ_G.store(0, Ordering::Relaxed);
        kipi::run_on_cpu(remote_cpu, move || {
            // SAFETY: test_vaddr is mapped read/write to p1 in the kernel page table
            // before this closure runs, and p1 backs an initialized u64 written above.
            let val = unsafe { core::ptr::read_volatile(test_vaddr.as_usize() as *const u64) };
            REMOTE_READ_G.store(val as usize, Ordering::Release);
        })
        .unwrap();
        while REMOTE_READ_G.load(Ordering::Acquire) == 0 {
            core::hint::spin_loop();
        }
        assert_eq!(
            REMOTE_READ_G.load(Ordering::Acquire),
            MAGIC_A as usize,
            "Phase 1: remote CPU should read MAGIC_A from V→P1 mapping"
        );

        // Phase 2: Remap V → P2 in the kernel page table.
        // `kernel_layout()` creates a kernel page table (is_kernel=true), so
        // `PageTableMut::finish()` calls `flush_all_cpus()`, which builds a
        // mask of ALL online CPUs via `for_each_present_logical_cpu()`.
        // The remote CPU WILL receive a shootdown IPI.
        memspace::kernel_layout()
            .unmap(test_vaddr, memaddr::PAGE_SIZE_4K)
            .expect("unmap V");
        memspace::kernel_layout()
            .map_linear(
                test_vaddr,
                p2,
                memaddr::PAGE_SIZE_4K,
                khal::paging::MappingFlags::READ | khal::paging::MappingFlags::WRITE,
            )
            .expect("map V→P2");

        // ---- Assertion: TLB shootdown must reach ALL online CPUs ----
        //
        // After modifying the global kernel page table, every online CPU
        // must see the new mapping.  We assert the CORRECT behavior:
        // the remote CPU should read MAGIC_B (P2's content).
        //
        // Kernel page table modifications (is_kernel=true) go through
        // `flush_all_cpus()`, which builds a full CPU mask via
        // `for_each_present_logical_cpu()`.  Every online CPU — including
        // the remote CPU — receives a shootdown IPI, so this assertion
        // PASSES on all architectures.
        REMOTE_READ_G.store(0, Ordering::Relaxed);
        kipi::run_on_cpu(remote_cpu, move || {
            // SAFETY: test_vaddr is remapped read/write to p2 in the kernel page table
            // before this closure runs, and p2 backs an initialized u64 written above.
            let val = unsafe { core::ptr::read_volatile(test_vaddr.as_usize() as *const u64) };
            REMOTE_READ_G.store(val as usize, Ordering::Release);
        })
        .unwrap();
        while REMOTE_READ_G.load(Ordering::Acquire) == 0 {
            core::hint::spin_loop();
        }
        let remote_val = REMOTE_READ_G.load(Ordering::Acquire);

        assert_eq!(
            remote_val, MAGIC_B as usize,
            "BUG (iomap TLB): remote CPU {remote_cpu:?} read {remote_val:#x}, expected MAGIC_B \
             ({MAGIC_B:#x}) from the V→P2 mapping.\nThe kernel page table was modified and \
             `flush_all_cpus()` should have targeted ALL online CPUs via \
             `for_each_present_logical_cpu()`, including the remote CPU.  If this assertion \
             fails, the remote CPU did not receive a TLB shootdown IPI or its TLB was not \
             properly invalidated.",
        );

        // Cleanup: unmap V, free pages.
        memspace::kernel_layout()
            .unmap(test_vaddr, memaddr::PAGE_SIZE_4K)
            .expect("test mapping must remain installed until cleanup");
        kalloc::global_allocator().dealloc_pages(v1, 1, kalloc::UsageKind::PageTable);
        kalloc::global_allocator().dealloc_pages(v2, 1, kalloc::UsageKind::PageTable);
    }
}

/// IRQ-off callers must fail before contending on the runtime mapping lock.
#[def_test(serial)]
fn irq_off_cached_kernel_root_is_readable_but_mutation_is_rejected() {
    let root = memspace::kernel_page_table_root();
    let irq = kspin::NoPreemptIrqSave::new();
    let observed_root = memspace::kernel_page_table_root();
    let is_rejected = matches!(
        memspace::kernel_layout().protect(
            VirtAddr::from(0),
            4096,
            khal::paging::MappingFlags::READ
        ),
        Err(kerrno::KError::WouldBlock)
    );
    let cached_root = memspace::kernel_page_table_root();
    let is_mmio_rejected = matches!(
        memspace::iomap_device(0usize.into(), 4096, "forbidden-irq-map"),
        Err(memspace::IoMapError::MappingFailed)
    );
    let has_preserved_irq_off = !karch::local_irq_enabled();
    drop(irq);
    assert!(is_rejected);
    assert!(is_mmio_rejected);
    assert!(has_preserved_irq_off);
    assert_eq!(root, cached_root);
    assert_eq!(root, observed_root);
}

/// Enabled hardware IRQs alone are insufficient inside a BH-excluded scope.
#[def_test(serial)]
fn bh_excluded_mapping_mutation_requires_task_completion_context() {
    let bh = kirq::context::local_bh_disable();
    let root = memspace::kernel_page_table_root();
    let is_rejected = matches!(
        memspace::kernel_layout().protect(
            VirtAddr::from(0),
            4096,
            khal::paging::MappingFlags::READ
        ),
        Err(kerrno::KError::WouldBlock)
    );
    let has_preserved_irq_on = karch::local_irq_enabled();
    drop(bh);
    assert!(is_rejected);
    assert!(has_preserved_irq_on);
    assert_eq!(root, memspace::kernel_page_table_root());
}

/// Prepared streaming maps remain usable inside device IRQ-safe locks.
#[def_test(serial)]
fn irq_off_dma_only_uses_the_prepared_streaming_pool() {
    use core::{alloc::Layout, ptr::NonNull};
    let mut bytes = [0x5a_u8; 257];
    let buffer = NonNull::from(bytes.as_mut_slice());
    let layout = Layout::from_size_align(4096, 4096).expect("page layout");
    let irq = kspin::NoPreemptIrqSave::new();
    // SAFETY: the private test allocation is never published to a device.
    let coherent = unsafe { kdma::allocate_dma_memory(layout) };
    // SAFETY: bytes stays exclusive until the matching unmap below.
    let mapping = unsafe { kdma::map_dma_buffer(buffer, kdma::DmaDirection::Bidirectional) };
    if let Ok(dma) = mapping {
        // SAFETY: this live mapping has the exact original buffer; no device owns it.
        unsafe { kdma::unmap_dma_buffer(dma.bus_addr, buffer, kdma::DmaDirection::Bidirectional) };
    }
    drop(irq);
    assert!(coherent.is_err());
    assert!(mapping.is_ok());
    assert!(bytes.iter().all(|byte| *byte == 0x5a));
}

/// Exercise coherent lifetime, bounced copy-back, and kernel mapping copies
/// concurrently without changing the unit-test runner's scheduling policy.
#[def_test(serial)]
fn concurrent_dma_lifetimes_and_kernel_mapping_copies() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    use alloc::{sync::Arc, vec::Vec};
    use core::{alloc::Layout, ptr::NonNull};
    let ready_count = Arc::new(AtomicUsize::new(0));
    let can_start = Arc::new(AtomicBool::new(false));
    let mut tasks = Vec::new();
    for worker in 0..3 {
        let ready_count = ready_count.clone();
        let can_start = can_start.clone();
        let task = ktask::TaskInner::new_kthread(
            move || {
                ready_count.fetch_add(1, Ordering::AcqRel);
                while !can_start.load(Ordering::Acquire) {
                    ktask::yield_now();
                }
                for iteration in 0..32 {
                    if worker == 0 {
                        let layout = Layout::from_size_align(4096, 4096).expect("test page layout");
                        // SAFETY: the allocation is private to this iteration,
                        // never published to a device, and freed once below.
                        let dma = unsafe { kdma::allocate_dma_memory(layout) }
                            .expect("test DMA allocation");
                        // SAFETY: this iteration owns all layout.size() bytes.
                        unsafe {
                            dma.cpu_addr
                                .as_ptr()
                                .write_bytes(iteration as u8, layout.size())
                        };
                        // SAFETY: the original address and matching layout are
                        // released exactly once without outstanding DMA.
                        unsafe { kdma::deallocate_dma_memory(dma, layout) };
                    } else if worker == 1 {
                        let mut bytes = [0x5a_u8; 257];
                        let buffer = NonNull::from(bytes.as_mut_slice());
                        // SAFETY: bytes remains exclusively owned until unmap;
                        // no real device receives the temporary bus address.
                        let dma = unsafe {
                            kdma::map_dma_buffer(buffer, kdma::DmaDirection::Bidirectional)
                        }
                        .expect("test streaming mapping");
                        // SAFETY: the returned bounce allocation contains 257
                        // writable bytes and has no concurrent device access.
                        unsafe {
                            dma.cpu_addr
                                .as_ptr()
                                .write_bytes(iteration as u8, bytes.len())
                        };
                        // SAFETY: this is the active mapping and exact original
                        // buffer, and the simulated device write has completed.
                        unsafe {
                            kdma::unmap_dma_buffer(
                                dma.bus_addr,
                                buffer,
                                kdma::DmaDirection::Bidirectional,
                            )
                        };
                        core::assert!(bytes.iter().all(|byte| *byte == iteration as u8));
                    } else {
                        drop(memspace::MmSpace::new_user_empty().expect("test user address space"));
                    }
                }
            },
            alloc::format!("dma-tlb-regression-{worker}"),
            0x40000,
        )
        .expect("test kernel thread allocation");
        let mut affinity = KCpuMask::new();
        affinity.set(worker % kcpu_id_map::nr_cpus(), true);
        task.set_cpumask(affinity);
        tasks.push(ktask::spawn_task(task));
    }
    while ready_count.load(Ordering::Acquire) != 3 {
        ktask::yield_now();
    }
    can_start.store(true, Ordering::Release);
    for task in tasks {
        task.join();
    }
}

static PRE_ACK_ROOT: AtomicUsize = AtomicUsize::new(0);
static PRE_ACK_READ: AtomicBool = AtomicBool::new(false);

fn read_kernel_metadata_before_tlb_ack() {
    let expected = PRE_ACK_ROOT.load(Ordering::Acquire);
    core::assert_eq!(memspace::kernel_page_table_root().as_usize(), expected);
    PRE_ACK_READ.store(true, Ordering::Release);
}

/// The IRQ-side cached-root accessor stays independent of the kernel mutex
/// retained by the initiating task through synchronous completion.
#[def_test(serial)]
fn shootdown_target_reads_cached_root_while_the_kernel_mutex_is_held() {
    if kcpu_id_map::nr_cpus() < 2 || cfg!(target_arch = "aarch64") {
        return unittest::TestResult::Ignored;
    }
    let pages = kalloc::alloc_page_ref(kalloc::UsageKind::Dma).expect("test backing");
    let address = khal::mem::p2v(pages.phys_addr());
    PRE_ACK_ROOT.store(
        memspace::kernel_page_table_root().as_usize(),
        Ordering::Release,
    );
    PRE_ACK_READ.store(false, Ordering::Release);
    let result =
        kipi::tlb::with_before_ack_hook_for_test(read_kernel_metadata_before_tlb_ack, || {
            memspace::kernel_layout().protect(
                address,
                memaddr::PAGE_SIZE_4K,
                khal::paging::MappingFlags::READ | khal::paging::MappingFlags::WRITE,
            )
        });
    result.expect("mapping completion while holding the kernel task mutex");
    assert!(PRE_ACK_READ.load(Ordering::Acquire));
}

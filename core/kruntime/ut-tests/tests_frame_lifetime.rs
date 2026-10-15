// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use kalloc::{FrameState, UsageKind, global_allocator};
use kcpu_id_map::KCpuMaskExt;
use kspin::NoPreemptIrqSave;
use unittest::def_test;

#[def_test]
fn frame_references_balance_across_cpus_without_clone_allocations() {
    let allocator = global_allocator();
    let owner = allocator.alloc_frame(UsageKind::Global).unwrap();
    let meta = allocator.frame_meta(owner.pfn()).unwrap();
    let mut tasks = Vec::new();
    for cpu in 0..kcpu_id_map::nr_cpus().min(4) {
        let peer = owner.try_clone_ref().unwrap();
        let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            move || {
                assert_eq!(khal::percpu::this_cpu_id().as_usize(), cpu);
                let guard = NoPreemptIrqSave::new();
                let before = kalloc::current_cpu_allocated_bytes_for_test(&guard);
                for _ in 0..64 {
                    let temporary = peer.try_clone_ref().unwrap();
                    assert_eq!(temporary.phys_addr(), peer.phys_addr());
                    drop(temporary);
                }
                assert_eq!(kalloc::current_cpu_allocated_bytes_for_test(&guard), before);
                drop(guard);
                drop(peer);
            },
            "frame_ref_worker".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        task.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu.into()));
        tasks.push(task);
    }
    // All peer references exist before any worker is activated. Completion is
    // driven by join, not elapsed time or assumptions about scheduler speed.
    for task in &tasks {
        ktask::activate_task(task);
    }
    for task in &tasks {
        assert_eq!(task.join(), 0);
    }
    assert_eq!(meta.state(), FrameState::Referenced { references: 1 });
    drop(owner);
}

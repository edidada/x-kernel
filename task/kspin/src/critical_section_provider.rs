// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Experimental SMP provider for the event-listener upgrade evaluation.
//! Only valid after CPU-local initialization; not callable from NMI context.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::{BaseGuard, NoPreemptIrqSave, guard::CurrentCpuIf};

const NO_OWNER: usize = usize::MAX;
static OWNER: AtomicUsize = AtomicUsize::new(NO_OWNER);
// Only the owning CPU accesses depth, with IRQs/preemption disabled. Atomic
// storage avoids a second unsafe data-access boundary; no RMW is needed.
static DEPTH: AtomicUsize = AtomicUsize::new(0);

struct KernelCriticalSection;
critical_section::set_impl!(KernelCriticalSection);

// SAFETY: Local guards prevent migration and ordinary IRQ reentry. OWNER
// serializes CPUs with Acquire/Release ordering. Same-CPU nested entries
// retain ownership until the outermost release; each release restores its
// matching guard token. NMI callers and use before CPU initialization are
// excluded by this experimental provider's integration contract.
unsafe impl critical_section::Impl for KernelCriticalSection {
    unsafe fn acquire() -> usize {
        let irq_state = NoPreemptIrqSave::acquire();
        let cpu = CurrentCpuIf::current_cpu().as_usize();
        if OWNER.load(Ordering::Relaxed) != cpu {
            while OWNER
                .compare_exchange_weak(NO_OWNER, cpu, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                core::hint::spin_loop();
            }
        }
        let depth = DEPTH.load(Ordering::Relaxed);
        DEPTH.store(depth + 1, Ordering::Relaxed);
        irq_state
    }

    unsafe fn release(irq_state: usize) {
        let depth = DEPTH.load(Ordering::Relaxed) - 1;
        DEPTH.store(depth, Ordering::Relaxed);
        if depth == 0 {
            OWNER.store(NO_OWNER, Ordering::Release);
        }
        // Drop global ownership before a guard release can reschedule.
        NoPreemptIrqSave::release(irq_state);
    }
}

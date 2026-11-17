// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Write-once snapshots that survive partial participation in a fatal report.

use core::sync::atomic::{AtomicUsize, Ordering};

use kcpu_id_map::for_each_present_logical_cpu;
use khal::{context::TrapFrame, percpu::this_cpu_id};
use klazy::Once;

/// Independent trap-frame storage for a terminal multi-CPU report.
///
/// Collection runs on the owning CPU in pinned IRQ/NMI context with ordinary
/// IRQs masked. Each CPU must retain its interrupted stack until the report
/// finishes, normally by parking until reset. Readers may run concurrently
/// with late collectors: each slot is published once and never cleared.
///
/// This storage does not reserve or reset an ordinary snapshot session. Only
/// interrupted running stacks are dumped; saved contexts of other tasks may
/// still change when some CPUs cannot be stopped.
pub struct TerminalSnapshot {
    frames: [Once<Option<TrapFrame>>; kbuild_config::NR_CPUS],
    claimed: AtomicUsize,
}

impl TerminalSnapshot {
    /// Creates empty permanent storage; construction does not collect frames.
    pub const fn new() -> Self {
        Self {
            frames: [const { Once::new() }; kbuild_config::NR_CPUS],
            claimed: AtomicUsize::new(0),
        }
    }

    /// Publishes this CPU's current exception frame once, without waiting.
    /// Requires the IRQ/NMI and stack-retention contract on [`Self`]. A nested
    /// collector returns immediately so it cannot wait for its interrupted
    /// publisher. The report coordinator publishes readiness after this call.
    pub fn collect_local(&self) {
        let cpu = this_cpu_id().as_usize();
        let Some(bit) = super::cpu_bit(cpu) else {
            return;
        };
        // Admission alone carries no payload. Once publishes the copied frame
        // with Release and get() acquires it; the first claimant is the only
        // caller of call_once, so it cannot wait on an interrupted initializer.
        if self.claimed.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
            self.frames[cpu].call_once(khal::context::active_exception_context);
        }
    }

    /// Prints the interrupted stacks selected by `mask`, using emergency output.
    /// Missing/unpublished slots are reported and never read; no saved
    /// task-context walk is attempted on a partial system stop.
    ///
    /// # Safety
    /// The caller must run in pinned IRQ/NMI context and retain every selected
    /// CPU's interrupted stack, mapped and unchanged, until this call returns.
    /// Publishing a copied frame alone does not retain its referenced stack.
    pub unsafe fn dump(&self, mask: usize) {
        for_each_present_logical_cpu(|_, cpu_id, _| {
            let cpu = cpu_id.as_usize();
            if mask & (1usize << cpu) == 0 {
                khal::kprint_atomic!("[snapshot] cpu={cpu} NOT COLLECTED\n");
                return;
            }
            match self.frames[cpu].get() {
                Some(Some(tf)) => super::dump::dump_terminal_backtrace(cpu_id, tf),
                Some(None) => khal::kprint_atomic!("[snapshot] cpu={cpu} no active trap frame\n"),
                None => khal::kprint_atomic!("[snapshot] cpu={cpu} frame not published\n"),
            }
        });
    }
}

impl Default for TerminalSnapshot {
    fn default() -> Self {
        Self::new()
    }
}

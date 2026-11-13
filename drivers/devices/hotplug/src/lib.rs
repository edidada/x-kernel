#![no_std]
// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU and memory hotplug policy.
//!
//! `kdevice::system` owns identity, presence, online state, and lifecycle
//! events. This crate owns the *policy* that validates a requested transition
//! and the platform backend that makes it real. `sysfs` projects both; it
//! never decides whether a request is legal.
//!
//! [`HotplugEventSource`] adapters feed hardware events into [`HotplugSink`],
//! which only installs or removes devices and submits online requests. The
//! state machines validate those requests again, so an event source cannot
//! bypass CPU0 or last-CPU protection.
//!
//! # Execution context
//!
//! Controllers run in task context. They take a `SpinNoPreempt` transition
//! lock and then call into `kdevice`, whose registry lock is also
//! `SpinNoPreempt`. Neither lock may be acquired from interrupt context.
//!
//! # Subsystem boundary
//!
//! CPU and memory hotplug is host-kernel policy, not a reusable device driver:
//! it needs the scheduler's CPU topology, the kernel direct map, and the global
//! page allocator. This crate therefore depends on host contracts (`kalloc`,
//! `memspace`, `kcpu_id_map`) instead of pretending to be hardware-agnostic.
//! There is no architecture-specific dependency; hardware access remains
//! behind `khal`.
//!
//! # Hardware boundary
//!
//! CPU bring-up and tear-down have no runtime architecture API in this tree:
//! `karch::stop_cpu()` only stops the calling CPU forever and secondary CPUs
//! start only during boot. The CPU state machine therefore rejects all illegal
//! requests and reports `EOPNOTSUPP` for the actual change instead of
//! pretending the CPU went offline. Memory hotplug does have a real allocator
//! and direct-map path and is implemented.

extern crate alloc;

mod cpu;
mod event;
mod memory;

use alloc::{collections::BTreeMap, format, string::String, vec::Vec};

pub use cpu::CpuHotplugController;
pub use event::{HotplugEventSource, HotplugSink, register_event_source, start_event_sources};
use kdevice::{SystemClass, SystemDeviceId, SystemDeviceState};
use kerrno::{KError, KErrorKind, KResult};
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;
pub use memory::{MEMORY_BLOCK_SIZE_BYTES, MemoryHotplugController};

/// Policy interface for one class of system device.
pub trait HotplugController: Send + Sync {
    /// Class handled by this controller.
    fn class(&self) -> SystemClass;

    /// Every present device of this class, in index order.
    fn present(&self) -> Vec<SystemDeviceId>;

    /// Requests an online-state transition.
    ///
    /// Validation belongs here, not in `sysfs`. Implementations must leave the
    /// device unchanged and publish no lifecycle event when they return an
    /// error.
    fn set_online(&self, id: SystemDeviceId, is_online: bool) -> KResult<()>;

    /// Returns the current state, or `None` when the device is not present.
    fn state(&self, id: SystemDeviceId) -> Option<SystemDeviceState>;
}

struct Controllers {
    by_class: BTreeMap<SystemClass, &'static dyn HotplugController>,
}

static CONTROLLERS: LazyInit<SpinNoPreempt<Controllers>> = LazyInit::new();

fn controllers() -> &'static SpinNoPreempt<Controllers> {
    CONTROLLERS.call_once(|| {
        SpinNoPreempt::new(Controllers {
            by_class: BTreeMap::new(),
        })
    });
    #[expect(
        clippy::expect_used,
        reason = "CG-021: call_once above initializes the controller table; failure is unreachable"
    )]
    CONTROLLERS.get().expect("controller table initialized")
}

/// Returns the controller for `class` once [`init`] has installed it.
pub fn controller_for(class: SystemClass) -> Option<&'static dyn HotplugController> {
    controllers().lock().by_class.get(&class).copied()
}

fn install_controller(controller: &'static dyn HotplugController) {
    let mut controllers = controllers().lock();
    let previous = controllers.by_class.insert(controller.class(), controller);
    debug_assert!(
        previous.is_none_or(|previous| core::ptr::eq(previous, controller)),
        "conflicting hotplug controller installed for one class"
    );
}

static CPU_CONTROLLER: CpuHotplugController = CpuHotplugController::new();
static MEMORY_CONTROLLER: MemoryHotplugController = MemoryHotplugController::new();

/// Installs boot CPU and memory-block devices and their controllers.
///
/// Idempotent: `sysfs` may request initialization independently of this
/// crate's init-array slot. Reinstalling a controller replaces the same static
/// instance, and installing an already-present device is ignored.
#[macros::register_init]
pub fn init() {
    install_controller(&CPU_CONTROLLER);
    install_controller(&MEMORY_CONTROLLER);
    cpu::install_boot_cpus();
    memory::install_boot_memory_blocks();
}

/// Formats device indices as a Linux cpulist: ranges for consecutive indices,
/// comma-separated otherwise, with a trailing newline.
pub fn format_cpulist(indices: impl IntoIterator<Item = u32>) -> String {
    let mut indices: Vec<u32> = indices.into_iter().collect();
    indices.sort_unstable();
    indices.dedup();

    let mut output = String::new();
    let mut position = 0;
    while position < indices.len() {
        let start = indices[position];
        let mut end = start;
        while position + 1 < indices.len() && indices[position + 1] == end.saturating_add(1) {
            position += 1;
            end = indices[position];
        }
        if !output.is_empty() {
            output.push(',');
        }
        if start == end {
            output.push_str(&format!("{start}"));
        } else {
            output.push_str(&format!("{start}-{end}"));
        }
        position += 1;
    }
    output.push('\n');
    output
}

/// Formats a block-index bitmap as lowercase hexadecimal, lowest index first.
///
/// Linux exposes the memory `online`/`possible` sets as a bitmap, unlike the
/// CPU sets, which are cpulists.
pub fn format_block_bitmap(indices: impl IntoIterator<Item = u32>) -> String {
    let mut indices: Vec<u32> = indices.into_iter().collect();
    indices.sort_unstable();
    let Some(highest) = indices.last().copied() else {
        return String::from("0\n");
    };
    let words = (highest as usize / 32) + 1;
    let mut bitmap = alloc::vec![0u32; words];
    for index in indices {
        bitmap[index as usize / 32] |= 1 << (index % 32);
    }
    let mut output = String::new();
    for (position, word) in bitmap.iter().enumerate().rev() {
        if position == bitmap.len() - 1 {
            output.push_str(&format!("{word:x}"));
        } else {
            output.push_str(&format!("{word:08x}"));
        }
    }
    output.push('\n');
    output
}

/// Converts an allocator failure into a kernel error.
///
/// `alloc-engine` has no `Busy` variant. [`kalloc::global_remove_memory`]
/// reports an in-use range as `NoMemory` because its reservation probe failed;
/// the hotplug contract requires `EBUSY`, so that case is translated here.
pub(crate) fn allocator_error(error: alloc_engine::AllocError) -> KError {
    use alloc_engine::AllocError;

    match error {
        AllocError::InvalidInput => KErrorKind::InvalidInput.into(),
        AllocError::AlreadyInitialized | AllocError::NotInitialized => KErrorKind::BadState.into(),
        AllocError::MemoryOverlap | AllocError::NoMemory => KErrorKind::ResourceBusy.into(),
        AllocError::NotAllocated | AllocError::NotFound => KErrorKind::BadState.into(),
    }
}

#[cfg(unittest)]
#[path = "../ut-tests/lib.rs"]
mod tests;

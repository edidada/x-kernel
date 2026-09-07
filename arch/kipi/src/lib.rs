// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel Inter-Processor Communication (IPC) API
//!
//! This module provides a lightweight abstraction for CPU-to-CPU communication
//! using Inter-Processor Interrupts (IPI). It maintains per-CPU callback queues,
//! dispatches generic callbacks from the shared IPI handler, hosts the TLB
//! shootdown protocol used by page-table updates, and the system-wide CPU
//! stop protocol used before entering a platform power terminal through
//! [`khal::power::SmpStopIf`]. It also owns the CPU-backtrace IPI lifecycle;
//! the optional `nmi` feature promotes delivery when supported. The irqchip,
//! not consumers or Kconfig, chooses each concrete vector.
//!
//! ## Safety
//!
//! All callbacks must be `Send` as they execute on different CPUs.

#![cfg_attr(not(test), no_std)]

#[macro_use]
extern crate log;
extern crate alloc;

use core::sync::atomic::{AtomicBool, Ordering};

use kcpu_id_map::{LogicalCpuId, for_each_present_logical_cpu, raw_cpu_id};
use khal::percpu::this_cpu_id;
use kirq::TargetCpu as IpiTarget;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;

mod backtrace;
mod event;
mod icache;
mod queue;
mod stop;
pub mod tlb;

pub use backtrace::{
    CpuBacktraceDelivery, cpu_backtrace_delivery, cpu_backtrace_ready_mask, enable_cpu_backtrace,
    register_cpu_backtrace_handler, send_cpu_backtrace,
};
pub use event::{Callback, MulticastCallback};
use queue::IpiEventQueue;

/// Result type for IPI operations
pub type Result<T> = core::result::Result<T, KipiError>;

/// Error types for IPI operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KipiError {
    /// Invalid CPU ID (exceeds system CPU count)
    InvalidCpuId,
    /// Target CPU exists but has not enabled IPI delivery or has begun terminal stop.
    TargetCpuNotReady,
    /// Queue full (too many pending callbacks)
    QueueFull,
    /// Callback execution failed
    CallbackFailed,
}

impl core::fmt::Display for KipiError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            Self::InvalidCpuId => write!(f, "Invalid CPU ID"),
            Self::TargetCpuNotReady => write!(f, "Target CPU is not ready for IPI"),
            Self::QueueFull => write!(f, "IPI queue full"),
            Self::CallbackFailed => write!(f, "Callback execution failed"),
        }
    }
}

#[percpu::def_percpu]
static IPI_EVENT_QUEUE: LazyInit<SpinNoIrq<IpiEventQueue>> = LazyInit::new();

static IPI_QUEUE_READY: [AtomicBool; kbuild_config::NR_CPUS] =
    [const { AtomicBool::new(false) }; kbuild_config::NR_CPUS];

#[inline]
fn is_ipi_queue_ready(cpu_id: LogicalCpuId) -> bool {
    IPI_QUEUE_READY[cpu_id.as_usize()].load(Ordering::Acquire)
}

#[inline]
fn send_generic_ipi(target: IpiTarget) -> bool {
    kirq::send_ipi(kirq::IpiKind::Generic, target)
}

/// Registers the platform-owned generic IPI vector during primary boot.
///
/// Returns `false` when the platform has no generic IPI descriptor or the
/// descriptor already has a handler.
pub fn register_ipi_handler(handler: fn()) -> bool {
    let Some(desc) = kirq::ipi_desc(kirq::IpiKind::Generic) else {
        return false;
    };
    kirq::register_disabled(
        desc.with_flags(kirq::IrqFlags::PER_CPU),
        alloc::sync::Arc::new(move |_| {
            handler();
            kirq::IrqEvent::HANDLED
        }),
    )
}

/// Enables the platform-owned generic IPI vector on the calling CPU.
///
/// Call after global registration and local [`init`], from ordinary kernel/IRQ
/// context, not NMI. Pins the CPU and masks ordinary IRQs through publication.
/// Delivery stays enabled until terminal stop; must not be called after that
/// CPU has withdrawn its readiness for stopping.
/// Returns `false` when the vector is unavailable or could not be enabled.
pub fn enable_ipi() -> bool {
    let _local = kspin::NoPreemptIrqSave::new();
    if !IPI_EVENT_QUEUE.with_current(|queue| queue.is_inited()) {
        return false;
    }
    let Some(desc) = kirq::ipi_desc(kirq::IpiKind::Generic) else {
        return false;
    };
    if kirq::try_enable_percpu_irq(desc).is_err() {
        return false;
    }
    IPI_QUEUE_READY[this_cpu_id().as_usize()].store(true, Ordering::Release);
    true
}

/// Withdraws local IPI readiness for the final CPU stop, retaining all storage.
///
/// The caller is pinned and has masked CPU-local interrupt delivery. This
/// terminal path only publishes software gates: it must not enter ordinary
/// per-CPU IRQ control, whose global locks may be held by a stopped peer.
/// Pending notifications and racing enqueues remain valid because handlers,
/// descriptors, and queues are never reclaimed or re-enabled after this point.
fn quiesce_local_ipis() {
    IPI_QUEUE_READY[this_cpu_id().as_usize()].store(false, Ordering::Release);
    backtrace::withdraw_local_readiness();
}

/// Initializes the current CPU's local IPI event queue.
///
/// This must run on every CPU before that CPU can receive callbacks through
/// [`run_on_cpu`] or either broadcast API. Readiness is published separately by
/// [`enable_ipi`], only after local controller setup is complete.
pub fn init() {
    IPI_EVENT_QUEUE.with_current(|ipi_queue| {
        ipi_queue.init_once(SpinNoIrq::new(IpiEventQueue::default()));
    });
}

/// Executes a callback on the specified destination CPU via IPI.
///
/// If `dest_cpu` is the current CPU, the callback runs synchronously in the
/// caller's current context. Otherwise it is queued onto the target CPU and
/// later runs from that CPU's shared IPI handler.
///
/// Callbacks must therefore be valid for the target execution context. In
/// particular, remotely executed callbacks must not rely on sleeping or other
/// process-context-only behavior unless the caller has arranged for that to be
/// safe in the target CPU's IPI handler.
///
/// # Notes
///
/// The callback must be `Send` because it may execute on a different CPU.
///
/// # Errors
///
/// Returns `KipiError::InvalidCpuId` if `dest_cpu` is outside the configured
/// range or does not belong to a present logical CPU.
///
/// Returns `KipiError::TargetCpuNotReady` if the destination CPU exists but
/// its local IPI queue/delivery is not enabled.
pub fn run_on_cpu<T: Into<Callback>>(dest_cpu: LogicalCpuId, callback: T) -> Result<()> {
    let cpu_num = kbuild_config::NR_CPUS;
    let dest_cpu_index = dest_cpu.as_usize();

    // Error handling: check CPU ID validity
    if dest_cpu_index >= cpu_num {
        error!("Invalid CPU ID: {} (max: {})", dest_cpu_index, cpu_num - 1);
        return Err(KipiError::InvalidCpuId);
    }

    if raw_cpu_id(dest_cpu).is_none() {
        error!(
            "CPU {} is not present in the logical CPU map",
            dest_cpu_index
        );
        return Err(KipiError::InvalidCpuId);
    }

    debug!("Send IPI event to CPU {}", dest_cpu_index);

    if dest_cpu == this_cpu_id() {
        // Execute callback on current CPU immediately
        callback.into().call();
    } else {
        if !is_ipi_queue_ready(dest_cpu) {
            error!("CPU {} IPI delivery is not ready", dest_cpu_index);
            return Err(KipiError::TargetCpuNotReady);
        }
        // SAFETY: `dest_cpu` was validated as present in the logical CPU map,
        // and `is_ipi_queue_ready(dest_cpu)` proves the target CPU has already
        // initialized its local per-CPU queue storage.
        unsafe { IPI_EVENT_QUEUE.remote_ref_raw(dest_cpu_index) }
            .lock()
            .push(this_cpu_id(), callback.into());
        // Once enqueued, a failed notification cannot be rolled back safely
        // because the target may already be draining it. Leave the callback
        // queued for a later generic IPI, matching the former fire-and-forget
        // notification contract.
        let _ = send_generic_ipi(IpiTarget::Specific(dest_cpu_index));
    }

    Ok(())
}

/// Executes a callback on all other CPUs via IPI.
///
/// The current CPU runs the callback immediately; all other CPUs receive it
/// through their IPI event queues.
///
/// Use this variant when the current CPU may execute the callback in ordinary
/// task context but the other CPUs may execute it in IPI interrupt context.
pub fn run_on_each_cpu<T: Into<MulticastCallback>>(callback: T) -> Result<()> {
    debug!("Send IPI event to all other CPUs");
    let current_cpu_id = this_cpu_id();
    let callback = callback.into();

    validate_broadcast_targets(current_cpu_id, false)?;

    // Execute callback on current CPU immediately
    callback.clone().call();

    // Push the callback to all other CPUs' IPI event queues
    enqueue_broadcast_to_others(current_cpu_id, &callback);

    // Send IPI to all other CPUs to trigger their callbacks
    let _ = send_generic_ipi(IpiTarget::AllButSelf {
        me: current_cpu_id.as_usize(),
        total: kcpu_id_map::nr_cpus(),
    });

    Ok(())
}

/// Executes a callback on every CPU from the IPI handler context.
///
/// Unlike [`run_on_each_cpu`], the current CPU also receives the callback
/// through its local IPI queue instead of running it immediately.
///
/// Use this variant when all CPUs, including the caller, must observe the same
/// IPI-handler execution context.
pub fn run_on_each_cpu_via_ipi<T: Into<MulticastCallback>>(callback: T) -> Result<()> {
    debug!("Send IPI event to every CPU, including self");
    let current_cpu_id = this_cpu_id();
    let callback = callback.into();

    validate_broadcast_targets(current_cpu_id, true)?;

    // Push to current CPU's queue so it runs from IPI context too
    // SAFETY: this runs on the current CPU, whose local per-CPU queue storage
    // has already been initialized before cross-CPU IPI use is allowed.
    unsafe { IPI_EVENT_QUEUE.current_ref_mut_raw() }
        .lock()
        .push(current_cpu_id, callback.clone().into_unicast());

    enqueue_broadcast_to_others(current_cpu_id, &callback);

    let _ = send_generic_ipi(IpiTarget::Self_);
    let _ = send_generic_ipi(IpiTarget::AllButSelf {
        me: current_cpu_id.as_usize(),
        total: kcpu_id_map::nr_cpus(),
    });

    Ok(())
}

fn validate_broadcast_targets(current_cpu_id: LogicalCpuId, include_self: bool) -> Result<()> {
    if include_self && !is_ipi_queue_ready(current_cpu_id) {
        error!(
            "CPU {} local IPI delivery is not ready",
            current_cpu_id.as_usize()
        );
        return Err(KipiError::TargetCpuNotReady);
    }

    let mut not_ready_cpu = None;
    for_each_present_logical_cpu(|_, cpu_id, _| {
        if not_ready_cpu.is_some() {
            return;
        }
        if cpu_id != current_cpu_id && !is_ipi_queue_ready(cpu_id) {
            not_ready_cpu = Some(cpu_id);
        }
    });
    if let Some(cpu_id) = not_ready_cpu {
        error!("CPU {} IPI delivery is not ready", cpu_id.as_usize());
        return Err(KipiError::TargetCpuNotReady);
    }
    Ok(())
}

fn enqueue_broadcast_to_others(current_cpu_id: LogicalCpuId, callback: &MulticastCallback) {
    for_each_present_logical_cpu(|_, cpu_id, _| {
        if cpu_id != current_cpu_id {
            // SAFETY: `validate_broadcast_targets` already ensured every
            // destination present CPU has initialized its local per-CPU queue
            // storage before any enqueue happens.
            unsafe { IPI_EVENT_QUEUE.remote_ref_raw(cpu_id.as_usize()) }
                .lock()
                .push(current_cpu_id, callback.clone().into_unicast());
        }
    });
}

/// The handler for IPI events. Retrieves events from the queue and executes callbacks.
///
/// This function is called in interrupt context. If a callback panics or fails,
/// the error is logged but other pending callbacks will still be processed.
///
/// A system-wide stop request is processed before anything else: once a stop
/// is in progress, every non-orchestrator CPU acknowledges and parks here and
/// never reaches the TLB shootdown or callback queues.
pub fn ipi_handler() {
    stop::handle_stop_request();

    if !is_ipi_queue_ready(this_cpu_id()) {
        return;
    }

    // Process TLB shootdown requests before handling generic callbacks.
    tlb::handle_shootdown();

    // SAFETY: the handler runs on the current CPU and only accesses that CPU's
    // already-initialized local IPI queue.
    while let Some((src_cpu_id, callback)) = unsafe { IPI_EVENT_QUEUE.current_ref_mut_raw() }
        .lock()
        .pop_one()
    {
        debug!("Received IPI event from CPU {}", src_cpu_id.as_usize());

        // use logging instead of silent failure
        callback.call();

        // If future needs to track failures, can add error handling inside Callback
    }
}

#[cfg(unittest)]
mod tests;

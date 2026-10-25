// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host-provided waiting and notification for RNG read completion.

use kerrno::KResult;

/// Reliable host wait for draining admitted RNG operations.
///
/// Construct this capability before registration becomes visible. It is
/// separate from fallible read completion: after invoking the predicate it
/// cannot return a normal error, time out, or stop for a signal. The predicate
/// only changes subsystem-private state under short locks and never sleeps.
pub trait RngDrain: Send + Sync {
    /// Prepare the current task, then wait until the private predicate is true.
    ///
    /// Invalid context must return `InvalidInput` before calling `drained`.
    /// In valid context, preparation must not fail for memory pressure.
    /// Register before checking, or check/register/recheck, to preserve early
    /// notifications. After the first invocation, return only when true.
    /// Never retain the predicate. The scoped waiter is removed before return.
    fn wait_until(&self, drained: &mut dyn FnMut() -> bool) -> KResult;

    /// Notify after publishing an in-flight count change, outside source locks.
    ///
    /// Non-sleeping and allocation-free; does not execute the predicate.
    fn notify(&self);
}

/// Host capability for waiting on driver-owned read completion state.
///
/// Notification prompts a recheck; it is not a stored completion count or a
/// successful DMA result. Offline/error transitions may also wake readers.
pub trait RngCompletion: Send + Sync {
    /// Sleep until `ready` returns true.
    ///
    /// Requires a schedulable task with IRQs enabled and no lock forbidding
    /// sleep. Validate context and finish fallible wait preparation before
    /// invoking `ready`. Task interruption does
    /// not end this wait; the driver must notify after publishing completion
    /// or a terminal error.
    ///
    /// `ready` runs synchronously on this task, is never retained, and must
    /// not sleep. It may briefly lock the device and consume data or record a
    /// terminal error. Registration after the first check must be infallible
    /// and allocation-free. Register before checking it, or check/register/recheck,
    /// so notification before parking cannot be lost. Recheck after spurious
    /// wakes; never poll, yield-loop or use a device-completion timer.
    ///
    /// # Errors
    /// Returns [`kerrno::KError::InvalidInput`] for a non-sleepable
    /// context, or a shared driver error for wait preparation failure. Neither
    /// error proves that the device failed. The registration is removed on
    /// return; an already detached late wake is allowed. A wait error does
    /// not cancel DMA started by an earlier check.
    fn wait_until(&self, ready: &mut dyn FnMut() -> bool) -> KResult;

    /// Wake registered readers after the driver publishes a state change.
    ///
    /// IRQ-safe, non-sleeping, allocation-free. Call outside device locks.
    /// Does not execute `ready`, consume data, or manufacture a read result.
    fn notify(&self);
}

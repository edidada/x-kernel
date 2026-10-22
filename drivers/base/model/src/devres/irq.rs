// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The IRQ family: handler callback vocabulary and the request guard.

use alloc::sync::Arc;
use core::fmt;

use super::ReleaseHook;
use crate::resource::Virq;

/// A registered device interrupt; released (handler freed, line freed) on
/// drop.
pub struct DevIrq {
    virq: Virq,
    release: Option<ReleaseHook>,
}

impl fmt::Debug for DevIrq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DevIrq")
            .field("virq", &self.virq)
            .finish_non_exhaustive()
    }
}

impl DevIrq {
    /// Constructed by [`DeviceServices`] implementations.
    pub fn new(virq: Virq, release: Option<ReleaseHook>) -> Self {
        Self { virq, release }
    }

    pub const fn virq(&self) -> Virq {
        self.virq
    }
}

impl Drop for DevIrq {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// Outcome of an interrupt handler invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqAction {
    /// The interrupt was ours and has been acked.
    Handled,
    /// Not ours (shared line).
    NotHandled,
}

// NOTE: there is deliberately no `WakeThread` variant. kirq has no
// threaded-handler vocabulary yet; a half-supported variant whose
// "wake the thread" half silently degrades to Handled at the runtime
// adapter would let drivers believe they run work in task context while
// actually running it in IRQ context. Re-add it together with a real
// threaded-IRQ path (DeviceServices::request_threaded_irq + a scheduler
// handoff), never as a silent degradation.

/// Type-erased interrupt handler registered by [`Device::request_irq`].
///
/// The handler carries its own context (the `dev_id` pattern): drivers close
/// over a `Weak` handle into their own `Data`; there is no global irq-to-
/// device routing table anywhere in the framework.
///
/// [`Device::request_irq`]: crate::Device::request_irq
pub type IrqHandlerFn = Arc<dyn Fn() -> IrqAction + Send + Sync>;

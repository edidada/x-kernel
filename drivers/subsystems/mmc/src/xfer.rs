// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Data-transfer completion signaling between IRQ context and waiting
//! tasks (the Linux `mmc_wait_data_done` completion analogue) — the
//! host-free contract half.
//!
//! Host drivers hold one shared signal, their IRQ handler publishes a
//! status word and calls [`XferSignal::notify`], and the data path parks
//! in [`XferSignal::wait_until`]. A non-task caller (card detection
//! during probe) gets `InvalidInput` from `wait_until` and the host falls
//! back to a bounded poll — one code path, two execution shapes. The
//! task-context implementation lives in `subsys-mmc-host` (the
//! `subsys-block-host` split); this crate never names a scheduler.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

/// Transfer-status word published by the host on completion.
///
/// Bit layout keeps the raw controller error vocabulary: `DONE` marks a
/// clean transfer; any other nonzero value is a host-specific error bitset
/// (timeout/CRC/DMA bits straight from the controller's status registers).
pub const XFER_DONE: u32 = 1;

/// Completion signal for data transfers (Linux `struct completion` role).
pub trait XferSignal: Send + Sync {
    /// Block the calling task until `ready` holds.
    ///
    /// Task context only (IRQs enabled, no lock forbidding sleep):
    /// registration happens before each recheck so a wake between check
    /// and park cannot be lost. Returns [`KError::InvalidInput`] when the
    /// caller is not a parkable task — callers treat that as "poll
    /// instead".
    fn wait_until(&self, ready: &mut dyn FnMut() -> bool) -> kerrno::KResult;

    /// Wake registered waiters after publishing a state change.
    ///
    /// IRQ-safe, non-sleeping, allocation-free. Call outside device locks.
    fn notify(&self);
}

/// The host-side status slot: a published status word plus the signal the
/// waiter sleeps on.
///
/// Split so the IRQ handler only touches lock-free atomics — the publish
/// (`store`) strictly precedes the wake, and the waiter rechecks the word
/// after every wake, so no state is lost to either ordering.
pub struct XferStatus {
    status: AtomicU32,
    signal: Arc<dyn XferSignal>,
}

impl XferStatus {
    /// A slot over `signal`, initially "busy" (0).
    pub fn new(signal: Arc<dyn XferSignal>) -> Self {
        Self {
            status: AtomicU32::new(0),
            signal,
        }
    }

    /// The IRQ-side publish: store `status`, then wake.
    pub fn publish(&self, status: u32) {
        self.status.store(status, Ordering::Release);
        self.signal.notify();
    }

    /// Read the current status word (0 = still busy).
    pub fn load(&self) -> u32 {
        self.status.load(Ordering::Acquire)
    }

    /// Reset to "busy" before starting a transfer.
    pub fn reset(&self) {
        self.status.store(0, Ordering::Release);
    }

    /// The underlying signal (for hosts that want custom wait shapes).
    pub fn signal(&self) -> &Arc<dyn XferSignal> {
        &self.signal
    }
}

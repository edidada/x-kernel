// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Two-phase wake values, independent of event sources and scheduler policy.
//!
//! Prepare a [`WakeRegistration`] for each wait in allocation-capable context.
//! Signaling moves selected registrations into a [`WakeSet`] without callbacks
//! or allocation; callbacks require explicit delivery with an admitted
//! [`DeliveryCtx`]. This crate defines the protocol mechanism.
//! Host queues, delivery checkpoints, source adapters and standard Waker bridges
//! are separate integration work.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

use core::marker::PhantomData;

mod adapter;
mod value;
pub use value::{WakeRegistration, WakeSet, WakeTarget};

/// Signal-side contract for event sources: publish state and select registrations.
///
/// Sources serialize selection with cancellation and move each selected
/// registration into the returned set. A selected registration cannot be
/// selected again; future enrollment supplies fresh prepared storage.
pub trait Signal {
    /// Returns owned selected registrations without callbacks, allocation or blocking.
    fn signal(&self) -> WakeSet;
}

/// An independently retained host notification operation.
///
/// # Safety
/// Delivery must be IRQ-safe, nonblocking and nonunwinding. It must obey host
/// lock ordering, preserve execution context and retain every referenced object
/// independently of a registering waiter's stack. It may acquire scheduler
/// locks, but must not sleep or directly switch tasks. Independent registrations
/// may deliver the same target concurrently; implementations synchronize their
/// own mutable state.
pub unsafe trait WakeDelivery: Send + Sync {
    /// Processes a captured notification at an admitted delivery boundary.
    fn deliver(&self, cx: &mut DeliveryCtx);
}

/// A current-context capability for synchronous wake delivery.
///
/// It is neither Send nor Sync. This protocol does not prove lock absence at
/// compile time: the host must establish admission and keep conflicting locks
/// released throughout delivery. Runtime admission is not implemented here.
pub struct DeliveryCtx {
    _local: PhantomData<*mut ()>,
}

impl DeliveryCtx {
    /// Admits a host-owned notification delivery boundary.
    ///
    /// # Safety
    /// The host must exclude conflicting outer locks and NMI, retain CPU/task
    /// identity for delivery, and prevent task switches during callbacks. Any
    /// IRQ/preemption exclusion must belong to this boundary itself. These
    /// conditions must remain true for every delivery using this capability.
    pub unsafe fn new_unchecked() -> Self {
        Self {
            _local: PhantomData,
        }
    }
}

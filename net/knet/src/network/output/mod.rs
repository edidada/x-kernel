// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 output: send transactions and device submission.
//!
//! Output has two entry points with separate ownership of the work:
//!
//! - `send` prepares a transaction. Preparation borrows routing, devices, and
//!   queue capacity for the lifetime of one send, the transport fills only its
//!   own bytes using the selected addresses, and submission consumes the
//!   transaction to complete IPv4 encapsulation and fragmentation.
//! - `xmit` hands finished datagrams to devices, either immediately for
//!   loopback or later when the poller dequeues them. Queued packets resolve
//!   routing again at dispatch.
//!
//! Route selection itself lives in [`super::output_route`] so that preparation,
//! queued dispatch, and source-address queries share one implementation.

mod send;
mod xmit;

#[cfg(unittest)]
pub(crate) use send::requires_broadcast;
pub(crate) use send::{
    Ipv4FragmentationPolicy, Ipv4OutputResources, Ipv4PacketBuilder, Ipv4SendOptions,
    PreparedIpv4Send,
};
pub(crate) use xmit::dispatch_queued;
#[cfg(unittest)]
pub(crate) use xmit::queue_packet;

/// Owns the datagram identification sequence for non-atomic IPv4 sends.
///
/// Resources are borrowed from the enclosing Router guard. Output never locks
/// Router itself and never retains a routing decision beyond its transaction.
pub(crate) struct Ipv4Output {
    next_identification: u16,
}

impl Ipv4Output {
    pub(crate) fn new() -> Self {
        Self {
            next_identification: 1,
        }
    }

    fn allocate_identification(&mut self) -> u16 {
        let identification = self.next_identification;
        self.next_identification = self.next_identification.wrapping_add(1).max(1);
        identification
    }
}

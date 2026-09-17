// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Persistent nonblocking UDP relay socket.

use alloc::sync::Arc;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use kerrno::{KError, KResult};

use super::UdpSocket;
use crate::{SocketAddrEx, SocketOps, netns, netns::NetNamespace};

/// Persistent UDP relay socket for request/reply datagram flows.
pub struct UdpDatagramRelay {
    socket: UdpSocket,
}

impl UdpDatagramRelay {
    /// Creates a UDP relay socket with an ephemeral local port.
    ///
    /// Relay sockets serve kernel flows (DNS, vsock bridging), so they always
    /// belong to the initial namespace regardless of the current process.
    pub fn new() -> KResult<Self> {
        Self::new_with_port(0)
    }

    /// Creates a UDP relay socket bound to `local_port`.
    pub fn new_with_port(local_port: u16) -> KResult<Self> {
        Self::new_in_namespace(netns::global_options_net_ns(), local_port)
    }

    /// Creates a UDP relay socket bound to `local_port` in `net_ns`.
    pub fn new_in_namespace(net_ns: Arc<NetNamespace>, local_port: u16) -> KResult<Self> {
        if !netns::initialized() {
            return Err(KError::OperationNotSupported);
        }

        let socket = UdpSocket::new(net_ns);
        socket.bind(SocketAddrEx::Ip(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            local_port,
        )))?;
        Ok(Self { socket })
    }

    /// Sends a UDP datagram through this relay socket.
    pub fn send_to(&self, dst: SocketAddr, payload: &[u8]) -> KResult<usize> {
        self.socket.send_datagram_now(dst, payload)
    }

    /// Receives a UDP datagram without blocking.
    pub fn try_recv(&self, buf: &mut [u8]) -> KResult<Option<(usize, SocketAddr)>> {
        self.socket.recv_datagram_now(buf)
    }
}

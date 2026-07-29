// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Raw IP socket implementation for ICMP-style traffic.

use alloc::vec;
use core::{
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::atomic::{AtomicBool, Ordering},
};

use kerrno::{KError, KResult, LinuxError};
use kio::prelude::*;
use kpoll::{IoEvents, PollContext, PollRegisterError, Pollable};
use ksync::RwLock;
pub use smoltcp::wire::{IpProtocol, IpVersion};
use smoltcp::{
    iface::SocketHandle,
    socket::raw as smol,
    storage::PacketMetadata,
    wire::{IpAddress, IpListenEndpoint, Ipv4Packet, Ipv4Repr, Ipv6Packet, Ipv6Repr},
};

use crate::{
    RecvFlags, RecvOptions, SERVICE, SOCKET_SET, SendOptions, Shutdown, SocketAddrEx, SocketOps,
    consts::{RAW_RX_BUF_LEN, RAW_TX_BUF_LEN},
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, OptionHandled, SetSocketOption},
    poll_interfaces,
};

const IPV4_HEADER_LEN: usize = 20;
const IPV6_HEADER_LEN: usize = 40;

pub(crate) fn new_raw_socket(
    ip_version: IpVersion,
    ip_protocol: IpProtocol,
) -> smol::Socket<'static> {
    smol::Socket::new(
        Some(ip_version),
        Some(ip_protocol),
        smol::PacketBuffer::new(vec![PacketMetadata::EMPTY; 256], vec![0; RAW_RX_BUF_LEN]),
        smol::PacketBuffer::new(vec![PacketMetadata::EMPTY; 256], vec![0; RAW_TX_BUF_LEN]),
    )
}

/// A raw IP socket used for ICMP and ICMPv6 traffic.
pub struct RawSocket {
    dispatch_irq: SocketHandle,
    ip_version: IpVersion,
    local_addr: RwLock<Option<IpAddress>>,
    peer_addr: RwLock<Option<IpAddress>>,
    ttl: RwLock<Option<u8>>,
    rx_closed: AtomicBool,
    tx_closed: AtomicBool,
    general: GeneralOptions,
}

impl RawSocket {
    pub fn new(ip_version: IpVersion, ip_protocol: IpProtocol) -> Self {
        let dispatch_irq = SOCKET_SET.add(new_raw_socket(ip_version, ip_protocol));
        let general = GeneralOptions::new();
        general.set_device_mask(u32::MAX);
        Self {
            dispatch_irq,
            ip_version,
            local_addr: RwLock::new(None),
            peer_addr: RwLock::new(None),
            ttl: RwLock::new(None),
            rx_closed: AtomicBool::new(false),
            tx_closed: AtomicBool::new(false),
            general,
        }
    }

    fn with_smol_socket<R>(&self, f: impl FnOnce(&mut smol::Socket) -> R) -> R {
        SOCKET_SET.with_socket_mut::<smol::Socket, _, _>(self.dispatch_irq, f)
    }

    fn check_ip_version(&self, addr: IpAddress) -> KResult<IpAddress> {
        match (self.ip_version, addr) {
            (IpVersion::Ipv4, IpAddress::Ipv4(_)) | (IpVersion::Ipv6, IpAddress::Ipv6(_)) => {
                Ok(addr)
            }
            _ => Err(KError::from(LinuxError::EAFNOSUPPORT)),
        }
    }

    fn remote_address(&self, options: &SendOptions) -> KResult<IpAddress> {
        match &options.to {
            Some(addr) => {
                let remote = addr.clone().into_ip()?;
                self.check_ip_version(remote.ip().into())
            }
            None => (*self.peer_addr.read()).ok_or(KError::NotConnected),
        }
    }

    fn local_address_for(&self, remote: IpAddress) -> KResult<IpAddress> {
        let service = SERVICE.lock();
        if let Some(local) = validate_explicit_bound_source(
            *self.local_addr.read(),
            |address| service.has_local_address(&address),
            |address| service.is_smoltcp_ipv4_broadcast(&address),
        )? {
            return Ok(local);
        }
        service.get_smoltcp_source_address(&remote)
    }

    fn parse_ip_packet<'a>(&self, packet: &'a [u8]) -> KResult<(IpAddress, &'a [u8])> {
        match self.ip_version {
            IpVersion::Ipv4 => {
                let packet = Ipv4Packet::new_checked(packet)
                    .map_err(|_| KError::from(LinuxError::EINVAL))?;
                Ok((IpAddress::Ipv4(packet.src_addr()), packet.into_inner()))
            }
            IpVersion::Ipv6 => {
                let packet = Ipv6Packet::new_checked(packet)
                    .map_err(|_| KError::from(LinuxError::EINVAL))?;
                Ok((IpAddress::Ipv6(packet.src_addr()), packet.into_inner()))
            }
        }
    }
}

impl Configurable for RawSocket {
    fn get_option_inner(&self, option: &mut GetSocketOption) -> KResult<OptionHandled> {
        use GetSocketOption as O;

        if self.general.get_option_inner(option)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }

        match option {
            O::Ttl(ttl) => {
                **ttl = (*self.ttl.read()).unwrap_or(64);
            }
            O::SendBuffer(size) => {
                **size = RAW_TX_BUF_LEN;
            }
            O::ReceiveBuffer(size) => {
                **size = RAW_RX_BUF_LEN;
            }
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }

    fn set_option_inner(&self, option: SetSocketOption) -> KResult<OptionHandled> {
        use SetSocketOption as O;

        if self.general.set_option_inner(option)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }

        match option {
            O::Ttl(ttl) => {
                if *ttl == 0 {
                    return Err(KError::InvalidInput);
                }
                *self.ttl.write() = Some(*ttl);
            }
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }
}

impl SocketOps for RawSocket {
    fn bind(&self, local_addr: SocketAddrEx) -> KResult {
        let local_addr = local_addr.into_ip()?;
        let local = self.check_ip_version(local_addr.ip().into())?;
        let service = SERVICE.lock();
        validate_raw_bind_address(
            local,
            |address| service.has_local_address(&address),
            |address| service.is_smoltcp_ipv4_bindable_broadcast(&address),
        )?;
        *self.local_addr.write() = Some(local);
        let device_mask = if is_dynamic_bound_source(local, |address| {
            service.is_smoltcp_ipv4_broadcast(&address)
        }) {
            u32::MAX
        } else {
            service.smoltcp_device_mask_for(&IpListenEndpoint {
                addr: Some(local),
                port: 0,
            })
        };
        self.general.set_device_mask(device_mask);
        Ok(())
    }

    fn connect(&self, remote_addr: SocketAddrEx) -> KResult {
        let remote_addr = remote_addr.into_ip()?;
        let remote = self.check_ip_version(remote_addr.ip().into())?;
        let local = self.local_address_for(remote)?;
        if self.local_addr.read().is_none() {
            *self.local_addr.write() = Some(local);
        }
        *self.peer_addr.write() = Some(remote);
        self.general
            .set_device_mask(SERVICE.lock().smoltcp_device_mask_for(&IpListenEndpoint {
                addr: Some(remote),
                port: 0,
            }));
        Ok(())
    }

    fn send(&self, mut src: impl Read + IoBuf, options: SendOptions) -> KResult<usize> {
        if self.tx_closed.load(Ordering::Acquire) {
            return Err(KError::BrokenPipe);
        }

        let remote = self.remote_address(&options)?;
        let local = self.local_address_for(remote)?;
        let payload_len = src.remaining();
        let egress_mtu = SERVICE.lock().egress_mtu(&remote)?;
        validate_raw_payload_len_for_mtu(payload_len, self.ip_version, egress_mtu)?;
        let mut payload = vec![0u8; payload_len];
        src.read_exact(&mut payload)?;

        self.general
            .send_poller_with_nonblocking(self, options.flags.nonblocking(), || {
                poll_interfaces();
                self.with_smol_socket(|socket| {
                    if !socket.can_send() {
                        return Err(KError::WouldBlock);
                    }
                    let next_header = socket.ip_protocol().expect("raw socket protocol");
                    let hop_limit = (*self.ttl.read()).unwrap_or(64);

                    let header_len = match self.ip_version {
                        IpVersion::Ipv4 => Ipv4Repr {
                            src_addr: match local {
                                IpAddress::Ipv4(addr) => addr,
                                _ => unreachable!(),
                            },
                            dst_addr: match remote {
                                IpAddress::Ipv4(addr) => addr,
                                _ => unreachable!(),
                            },
                            next_header,
                            payload_len,
                            hop_limit,
                        }
                        .buffer_len(),
                        IpVersion::Ipv6 => Ipv6Repr {
                            src_addr: match local {
                                IpAddress::Ipv6(addr) => addr,
                                _ => unreachable!(),
                            },
                            dst_addr: match remote {
                                IpAddress::Ipv6(addr) => addr,
                                _ => unreachable!(),
                            },
                            next_header,
                            payload_len,
                            hop_limit,
                        }
                        .buffer_len(),
                    };

                    let buf = socket
                        .send(header_len + payload_len)
                        .map_err(|_| KError::WouldBlock)?;
                    match self.ip_version {
                        IpVersion::Ipv4 => {
                            let header = Ipv4Repr {
                                src_addr: match local {
                                    IpAddress::Ipv4(addr) => addr,
                                    _ => unreachable!(),
                                },
                                dst_addr: match remote {
                                    IpAddress::Ipv4(addr) => addr,
                                    _ => unreachable!(),
                                },
                                next_header,
                                payload_len,
                                hop_limit,
                            };
                            header.emit(
                                &mut Ipv4Packet::new_unchecked(&mut *buf),
                                &smoltcp::phy::ChecksumCapabilities::ignored(),
                            );
                        }
                        IpVersion::Ipv6 => {
                            let header = Ipv6Repr {
                                src_addr: match local {
                                    IpAddress::Ipv6(addr) => addr,
                                    _ => unreachable!(),
                                },
                                dst_addr: match remote {
                                    IpAddress::Ipv6(addr) => addr,
                                    _ => unreachable!(),
                                },
                                next_header,
                                payload_len,
                                hop_limit,
                            };
                            header.emit(&mut Ipv6Packet::new_unchecked(&mut *buf));
                        }
                    }

                    buf[header_len..].copy_from_slice(&payload);
                    Ok(payload_len)
                })
            })
    }

    fn recv(&self, mut dst: impl Write + IoBufMut, options: RecvOptions<'_>) -> KResult<usize> {
        if self.rx_closed.load(Ordering::Acquire) {
            return Err(KError::NotConnected);
        }
        let mut options = options;

        self.general
            .recv_poller_with_nonblocking(self, options.flags.nonblocking(), || {
                poll_interfaces();
                self.with_smol_socket(|socket| {
                    loop {
                        let packet = if options.flags.contains(RecvFlags::PEEK) {
                            let packet = socket.peek().map_err(|_| KError::WouldBlock)?;
                            let (source, _) = self.parse_ip_packet(packet)?;
                            if let Some(peer) = *self.peer_addr.read()
                                && source != peer
                            {
                                let _ = socket.recv().map_err(|_| KError::WouldBlock)?;
                                continue;
                            }
                            packet
                        } else {
                            socket.recv().map_err(|_| KError::WouldBlock)?
                        };
                        let (source, packet) = self.parse_ip_packet(packet)?;

                        if let Some(peer) = *self.peer_addr.read()
                            && source != peer
                        {
                            continue;
                        }

                        if let Some(from) = options.from.as_deref_mut() {
                            *from = SocketAddrEx::Ip(SocketAddr::new(source.into(), 0));
                        }

                        let written = dst.write(packet)?;
                        return Ok(if options.flags.contains(RecvFlags::TRUNCATE) {
                            packet.len()
                        } else {
                            written
                        });
                    }
                })
            })
    }

    fn local_addr(&self) -> KResult<SocketAddrEx> {
        let local = (*self.local_addr.read()).unwrap_or(match self.ip_version {
            IpVersion::Ipv4 => IpAddress::Ipv4(Ipv4Addr::UNSPECIFIED),
            IpVersion::Ipv6 => IpAddress::Ipv6(Ipv6Addr::UNSPECIFIED),
        });
        Ok(SocketAddrEx::Ip(SocketAddr::new(local.into(), 0)))
    }

    fn peer_addr(&self) -> KResult<SocketAddrEx> {
        let peer = (*self.peer_addr.read()).ok_or(KError::NotConnected)?;
        Ok(SocketAddrEx::Ip(SocketAddr::new(peer.into(), 0)))
    }

    fn shutdown(&self, how: Shutdown) -> KResult {
        if how.has_read() {
            self.rx_closed.store(true, Ordering::Release);
        }
        if how.has_write() {
            self.tx_closed.store(true, Ordering::Release);
        }
        Ok(())
    }
}

impl Pollable for RawSocket {
    fn poll(&self) -> IoEvents {
        poll_interfaces();
        let mut events = IoEvents::empty();
        self.with_smol_socket(|socket| {
            events.set(
                IoEvents::IN,
                !self.rx_closed.load(Ordering::Acquire) && socket.can_recv(),
            );
            events.set(
                IoEvents::OUT,
                !self.tx_closed.load(Ordering::Acquire) && socket.can_send(),
            );
        });
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.intersects(IoEvents::IN | IoEvents::OUT) {
            let source_waker = self.general.register_rx_waker(context)?;
            self.with_smol_socket(|socket| {
                if events.contains(IoEvents::IN) {
                    socket.register_recv_waker(&source_waker);
                }
                if events.contains(IoEvents::OUT) {
                    socket.register_send_waker(&source_waker);
                }
            });
        }
        Ok(())
    }
}

impl Drop for RawSocket {
    fn drop(&mut self) {
        let _ = self.shutdown(Shutdown::Both);
        SOCKET_SET.remove(self.dispatch_irq);
    }
}

fn validate_explicit_bound_source(
    local: Option<IpAddress>,
    is_local: impl FnOnce(IpAddress) -> bool,
    is_broadcast: impl FnOnce(IpAddress) -> bool,
) -> Result<Option<IpAddress>, LinuxError> {
    let Some(address) = local.filter(|address| !is_dynamic_bound_source(*address, is_broadcast))
    else {
        return Ok(None);
    };
    if !is_local(address) {
        return Err(LinuxError::ENETUNREACH);
    }
    Ok(Some(address))
}

fn validate_raw_bind_address(
    address: IpAddress,
    is_local: impl FnOnce(IpAddress) -> bool,
    is_broadcast: impl FnOnce(IpAddress) -> bool,
) -> Result<(), LinuxError> {
    if !is_dynamic_bound_source(address, is_broadcast) && !is_local(address) {
        return Err(LinuxError::EADDRNOTAVAIL);
    }
    Ok(())
}

fn is_dynamic_bound_source(
    address: IpAddress,
    is_broadcast: impl FnOnce(IpAddress) -> bool,
) -> bool {
    address.is_unspecified()
        || address.is_multicast()
        || matches!(address, IpAddress::Ipv4(address) if address.is_broadcast())
        || is_broadcast(address)
}

fn validate_raw_payload_len_for_mtu(
    payload_len: usize,
    ip_version: IpVersion,
    mtu: usize,
) -> Result<(), LinuxError> {
    let header_len = match ip_version {
        IpVersion::Ipv4 => IPV4_HEADER_LEN,
        IpVersion::Ipv6 => IPV6_HEADER_LEN,
    };
    let packet_len = payload_len
        .checked_add(header_len)
        .ok_or(LinuxError::EMSGSIZE)?;
    let exceeds_ip_length = match ip_version {
        IpVersion::Ipv4 => packet_len > u16::MAX as usize,
        IpVersion::Ipv6 => payload_len > u16::MAX as usize,
    };
    if packet_len > mtu || exceeds_ip_length {
        return Err(LinuxError::EMSGSIZE);
    }
    Ok(())
}

#[cfg(unittest)]
mod tests {
    use alloc::{vec, vec::Vec};
    use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    use kio::Cursor;
    use smoltcp::wire::{Ipv4Packet, Ipv4Repr, Ipv6Packet, Ipv6Repr};
    use unittest::def_test;

    use super::*;
    use crate::options::{Configurable, GetSocketOption, SetSocketOption};

    #[def_test]
    fn raw_ipv4_payload_respects_egress_mtu() {
        const MTU: usize = 1500;

        assert_eq!(
            validate_raw_payload_len_for_mtu(MTU - IPV4_HEADER_LEN, IpVersion::Ipv4, MTU),
            Ok(())
        );
        assert_eq!(
            validate_raw_payload_len_for_mtu(MTU - IPV4_HEADER_LEN + 1, IpVersion::Ipv4, MTU),
            Err(LinuxError::EMSGSIZE)
        );
    }

    #[def_test]
    fn raw_ipv4_payload_respects_total_length_field() {
        const LOOPBACK_MTU: usize = 65_536;
        const MAX_PAYLOAD: usize = u16::MAX as usize - IPV4_HEADER_LEN;

        assert_eq!(
            validate_raw_payload_len_for_mtu(MAX_PAYLOAD, IpVersion::Ipv4, LOOPBACK_MTU),
            Ok(())
        );
        assert_eq!(
            validate_raw_payload_len_for_mtu(MAX_PAYLOAD + 1, IpVersion::Ipv4, LOOPBACK_MTU),
            Err(LinuxError::EMSGSIZE)
        );
    }

    #[def_test]
    fn raw_ipv6_payload_respects_egress_mtu() {
        const MTU: usize = 1500;

        assert_eq!(
            validate_raw_payload_len_for_mtu(MTU - IPV6_HEADER_LEN, IpVersion::Ipv6, MTU),
            Ok(())
        );
        assert_eq!(
            validate_raw_payload_len_for_mtu(MTU - IPV6_HEADER_LEN + 1, IpVersion::Ipv6, MTU),
            Err(LinuxError::EMSGSIZE)
        );
    }

    #[def_test]
    fn raw_unspecified_bound_address_defers_source_selection() {
        assert_eq!(
            validate_explicit_bound_source(
                Some(IpAddress::Ipv4(Ipv4Addr::UNSPECIFIED)),
                |_| false,
                |_| false
            ),
            Ok(None)
        );
        assert_eq!(
            validate_explicit_bound_source(
                Some(IpAddress::Ipv6(Ipv6Addr::UNSPECIFIED)),
                |_| false,
                |_| false
            ),
            Ok(None)
        );
        assert_eq!(
            validate_explicit_bound_source(
                Some(IpAddress::Ipv4(Ipv4Addr::BROADCAST)),
                |_| false,
                |_| false
            ),
            Ok(None)
        );
        assert_eq!(
            validate_explicit_bound_source(
                Some(IpAddress::Ipv4(Ipv4Addr::new(224, 0, 0, 1))),
                |_| false,
                |_| false
            ),
            Ok(None)
        );
        assert_eq!(
            validate_explicit_bound_source(
                Some(IpAddress::Ipv4(Ipv4Addr::new(192, 0, 2, 255))),
                |_| false,
                |_| true
            ),
            Ok(None)
        );
    }

    #[def_test]
    fn raw_bind_accepts_dynamic_source_addresses() {
        assert_eq!(
            validate_raw_bind_address(IpAddress::Ipv4(Ipv4Addr::UNSPECIFIED), |_| false, |_| false),
            Ok(())
        );
        assert_eq!(
            validate_raw_bind_address(IpAddress::Ipv4(Ipv4Addr::BROADCAST), |_| false, |_| false),
            Ok(())
        );
        assert_eq!(
            validate_raw_bind_address(
                IpAddress::Ipv4(Ipv4Addr::new(224, 0, 0, 1)),
                |_| false,
                |_| false
            ),
            Ok(())
        );
        assert_eq!(
            validate_raw_bind_address(
                IpAddress::Ipv4(Ipv4Addr::new(192, 0, 2, 255)),
                |_| false,
                |_| true
            ),
            Ok(())
        );
    }

    #[def_test]
    fn raw_explicit_source_must_remain_local() {
        let source = IpAddress::Ipv4(Ipv4Addr::new(192, 0, 2, 2));
        assert_eq!(
            validate_explicit_bound_source(Some(source), |_| true, |_| false),
            Ok(Some(source))
        );
        assert_eq!(
            validate_explicit_bound_source(Some(source), |_| false, |_| false),
            Err(LinuxError::ENETUNREACH)
        );
    }

    fn ipv4_packet_bytes(src: Ipv4Addr, dst: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
        let repr = Ipv4Repr {
            src_addr: src,
            dst_addr: dst,
            next_header: IpProtocol::Icmp,
            payload_len: payload.len(),
            hop_limit: 32,
        };
        let mut bytes = vec![0; repr.buffer_len() + payload.len()];
        repr.emit(
            &mut Ipv4Packet::new_unchecked(&mut bytes),
            &smoltcp::phy::ChecksumCapabilities::ignored(),
        );
        bytes[repr.buffer_len()..].copy_from_slice(payload);
        bytes
    }

    fn ipv6_packet_bytes(src: Ipv6Addr, dst: Ipv6Addr, payload: &[u8]) -> Vec<u8> {
        let repr = Ipv6Repr {
            src_addr: src,
            dst_addr: dst,
            next_header: IpProtocol::Icmpv6,
            payload_len: payload.len(),
            hop_limit: 48,
        };
        let mut bytes = vec![0; repr.buffer_len() + payload.len()];
        repr.emit(&mut Ipv6Packet::new_unchecked(&mut bytes));
        bytes[repr.buffer_len()..].copy_from_slice(payload);
        bytes
    }

    #[def_test(serial)]
    fn raw_socket_ip_version_checks_and_defaults() {
        let socket = RawSocket::new(IpVersion::Ipv4, IpProtocol::Icmp);

        assert_eq!(
            socket
                .check_ip_version(IpAddress::Ipv4(Ipv4Addr::new(192, 0, 2, 10)))
                .unwrap(),
            IpAddress::Ipv4(Ipv4Addr::new(192, 0, 2, 10))
        );
        assert_eq!(
            socket.check_ip_version(IpAddress::Ipv6(Ipv6Addr::LOCALHOST)),
            Err(KError::from(LinuxError::EAFNOSUPPORT))
        );
        assert_eq!(
            match socket.local_addr().unwrap() {
                SocketAddrEx::Ip(addr) => addr,
                other => panic!("expected IP socket addr, got {other:?}"),
            },
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
        );
        assert!(matches!(socket.peer_addr(), Err(KError::NotConnected)));
    }

    #[def_test(serial)]
    fn raw_socket_parses_ipv4_and_ipv6_packets() {
        let ipv4 = RawSocket::new(IpVersion::Ipv4, IpProtocol::Icmp);
        let ipv4_packet = ipv4_packet_bytes(
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(10, 0, 0, 2),
            &[1, 2, 3, 4],
        );
        let (source, packet) = ipv4.parse_ip_packet(&ipv4_packet).unwrap();
        assert_eq!(source, IpAddress::Ipv4(Ipv4Addr::new(10, 0, 0, 1)));
        assert_eq!(packet, ipv4_packet.as_slice());

        let ipv6 = RawSocket::new(IpVersion::Ipv6, IpProtocol::Icmpv6);
        let ipv6_packet = ipv6_packet_bytes(
            Ipv6Addr::LOCALHOST,
            Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2),
            &[9, 8, 7],
        );
        let (source, packet) = ipv6.parse_ip_packet(&ipv6_packet).unwrap();
        assert_eq!(source, IpAddress::Ipv6(Ipv6Addr::LOCALHOST));
        assert_eq!(packet, ipv6_packet.as_slice());
        assert_eq!(
            ipv4.parse_ip_packet(&[1, 2, 3]),
            Err(KError::from(LinuxError::EINVAL))
        );
    }

    #[def_test(serial)]
    fn raw_socket_ttl_remote_and_local_shortcuts() {
        let socket = RawSocket::new(IpVersion::Ipv4, IpProtocol::Icmp);
        let mut ttl = 0u8;

        socket
            .get_option_inner(&mut GetSocketOption::Ttl(&mut ttl))
            .unwrap();
        assert_eq!(ttl, 64);

        let ttl_value = 7u8;
        socket
            .set_option_inner(SetSocketOption::Ttl(&ttl_value))
            .unwrap();
        socket
            .get_option_inner(&mut GetSocketOption::Ttl(&mut ttl))
            .unwrap();
        assert_eq!(ttl, 7);

        assert_eq!(
            socket
                .set_option_inner(SetSocketOption::Ttl(&0))
                .unwrap_err(),
            KError::InvalidInput
        );

        *socket.peer_addr.write() = Some(IpAddress::Ipv4(Ipv4Addr::new(198, 51, 100, 9)));
        assert_eq!(
            socket.remote_address(&SendOptions::default()).unwrap(),
            IpAddress::Ipv4(Ipv4Addr::new(198, 51, 100, 9))
        );

        *socket.local_addr.write() = Some(IpAddress::Ipv4(Ipv4Addr::new(192, 0, 2, 44)));
        assert_eq!(
            socket
                .local_address_for(IpAddress::Ipv4(Ipv4Addr::new(198, 51, 100, 9)))
                .unwrap_err(),
            KError::from(LinuxError::ENETUNREACH)
        );
    }

    #[def_test(serial)]
    fn raw_socket_bind_and_shutdown_gate_fast_paths() {
        let socket = RawSocket::new(IpVersion::Ipv4, IpProtocol::Icmp);

        socket
            .bind(SocketAddrEx::Ip(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                0,
            )))
            .unwrap();
        assert_eq!(
            *socket.local_addr.read(),
            Some(IpAddress::Ipv4(Ipv4Addr::UNSPECIFIED))
        );

        assert_eq!(
            socket.bind(SocketAddrEx::Ip(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::LOCALHOST),
                0
            ))),
            Err(KError::from(LinuxError::EAFNOSUPPORT))
        );

        socket.shutdown(Shutdown::Both).unwrap();

        let mut recv_buf = [0u8; 8];
        assert_eq!(
            socket.recv(Cursor::new(recv_buf.as_mut_slice()), RecvOptions::default()),
            Err(KError::NotConnected)
        );
        assert_eq!(
            socket.send(Cursor::new(&[1u8, 2, 3][..]), SendOptions::default()),
            Err(KError::BrokenPipe)
        );
    }
}

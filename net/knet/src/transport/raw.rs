// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Raw IPv4 socket implementation.

use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::atomic::{AtomicBool, Ordering},
};

use kerrno::{KError, KResult, LinuxError};
use kio::prelude::*;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::{Mutex, RwLock};
use lazyinit::LazyInit;

use crate::{
    RecvFlags, RecvOptions, SERVICE, SendOptions, Shutdown, SocketAddrEx, SocketOps,
    consts::{RAW_RX_BUF_LEN, RAW_TX_BUF_LEN},
    general::GeneralOptions,
    ip::{IpAddress, IpListenEndpoint, Ipv4Address},
    ipv4::{self, Ipv4Header},
    options::{Configurable, GetSocketOption, OptionHandled, SetSocketOption},
    poll_interfaces,
};

const RAW_RX_QUEUE_LIMIT: usize = 128;

static RAW_SOCKET_REGISTRY: LazyInit<RawSocketRegistry> = LazyInit::new();

struct RawSocketRegistry {
    sockets: Mutex<Vec<Weak<RawSocketInner>>>,
}

impl RawSocketRegistry {
    fn new() -> Self {
        Self {
            sockets: Mutex::new(Vec::new()),
        }
    }

    fn register(&self, socket: &Arc<RawSocketInner>) {
        let mut sockets = self.sockets.lock();
        sockets.retain(|socket| socket.strong_count() > 0);
        sockets.push(Arc::downgrade(socket));
    }

    fn active_sockets(&self) -> Vec<Arc<RawSocketInner>> {
        let mut active_sockets = Vec::new();
        let mut sockets = self.sockets.lock();
        sockets.retain(|socket| {
            let Some(socket) = socket.upgrade() else {
                return false;
            };
            active_sockets.push(socket);
            true
        });
        active_sockets
    }
}

#[derive(Clone)]
struct RawPacket {
    from: SocketAddr,
    data: Vec<u8>,
}

struct RawSocketInner {
    ip_version: IpVersion,
    ip_protocol: IpProtocol,
    local_addr: RwLock<Option<IpAddress>>,
    peer_addr: RwLock<Option<IpAddress>>,
    ttl: RwLock<Option<u8>>,
    rx_queue: Mutex<VecDeque<RawPacket>>,
    poll_rx: PollSet,
    rx_shutdown: AtomicBool,
    tx_closed: AtomicBool,
    general: GeneralOptions,
}

struct RawRecvMsg<'a> {
    from: Option<&'a mut SocketAddrEx>,
    flags: RecvFlags,
    out_flags: Option<&'a mut RecvFlags>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpVersion {
    Ipv4,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpProtocol {
    Icmp,
    Number(u8),
}

impl IpProtocol {
    pub fn from_ipv4_number(protocol: u8) -> Self {
        if protocol == ipv4::PROTOCOL_ICMP {
            Self::Icmp
        } else {
            Self::Number(protocol)
        }
    }

    fn ipv4_number(self) -> u8 {
        match self {
            Self::Icmp => ipv4::PROTOCOL_ICMP,
            Self::Number(protocol) => protocol,
        }
    }
}

/// A raw IPv4 socket carrying packets for one IPv4 protocol number.
pub struct RawSocket {
    inner: Arc<RawSocketInner>,
}

impl RawSocket {
    pub fn new(ip_version: IpVersion, ip_protocol: IpProtocol) -> Self {
        init_raw_registry();
        let general = GeneralOptions::new();
        general.set_device_mask(u32::MAX);
        let socket = Self {
            inner: Arc::new(RawSocketInner {
                ip_version,
                ip_protocol,
                local_addr: RwLock::new(None),
                peer_addr: RwLock::new(None),
                ttl: RwLock::new(None),
                rx_queue: Mutex::new(VecDeque::new()),
                poll_rx: PollSet::new(),
                rx_shutdown: AtomicBool::new(false),
                tx_closed: AtomicBool::new(false),
                general,
            }),
        };
        RAW_SOCKET_REGISTRY.register(&socket.inner);
        socket
    }

    fn check_ip_version(&self, addr: IpAddress) -> KResult<IpAddress> {
        match (self.inner.ip_version, addr) {
            (IpVersion::Ipv4, IpAddress::Ipv4(_)) => Ok(addr),
            _ => Err(KError::from(LinuxError::EAFNOSUPPORT)),
        }
    }

    fn remote_address(&self, options: &SendOptions) -> KResult<IpAddress> {
        match &options.to {
            Some(addr) => {
                let remote = addr.clone().into_ip()?;
                self.check_ip_version(remote.ip().into())
            }
            None => (*self.inner.peer_addr.read()).ok_or(KError::NotConnected),
        }
    }

    fn local_address_for(&self, remote: IpAddress) -> KResult<IpAddress> {
        let service = SERVICE.lock();
        if let Some(local) = validate_explicit_bound_source(
            *self.inner.local_addr.read(),
            |address| service.has_ipv4_address(address),
            |address| service.is_ipv4_broadcast(address),
        )? {
            return Ok(local);
        }
        service.get_source_address(&remote)
    }

    fn recv_packet(
        &self,
        dst: &mut (impl Write + IoBufMut),
        msg: &mut RawRecvMsg<'_>,
    ) -> KResult<usize> {
        let packet = {
            let mut rx_queue = self.inner.rx_queue.lock();
            let packet = if msg.flags.contains(RecvFlags::PEEK) {
                rx_queue.front().cloned()
            } else {
                rx_queue.pop_front()
            };
            match packet {
                Some(packet) => packet,
                None if self.inner.rx_shutdown.load(Ordering::Acquire) => return Ok(0),
                None => return Err(KError::WouldBlock),
            }
        };

        if let Some(from) = msg.from.as_deref_mut() {
            *from = SocketAddrEx::Ip(packet.from);
        }
        let write_len = packet.data.len().min(dst.remaining_mut());
        dst.write_all(&packet.data[..write_len])?;
        if write_len < packet.data.len()
            && let Some(out_flags) = msg.out_flags.as_deref_mut()
        {
            *out_flags |= RecvFlags::TRUNCATE;
        }
        if msg.flags.contains(RecvFlags::TRUNCATE) {
            Ok(packet.data.len())
        } else {
            Ok(write_len)
        }
    }
}

impl Configurable for RawSocket {
    fn get_option_inner(&self, option: &mut GetSocketOption) -> KResult<OptionHandled> {
        use GetSocketOption as O;

        if self.inner.general.get_option_inner(option)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }

        match option {
            O::Ttl(ttl) => {
                **ttl = (*self.inner.ttl.read()).unwrap_or(64);
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

        if self.inner.general.set_option_inner(option)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }

        match option {
            O::Ttl(ttl) => {
                if *ttl == 0 {
                    return Err(KError::InvalidInput);
                }
                *self.inner.ttl.write() = Some(*ttl);
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
            |address| service.has_ipv4_address(address),
            |address| service.is_ipv4_bindable_broadcast(address),
        )?;
        *self.inner.local_addr.write() = Some(local);
        let device_mask =
            if is_dynamic_bound_source(local, |address| service.is_ipv4_broadcast(address)) {
                u32::MAX
            } else {
                service.device_mask_for(&IpListenEndpoint {
                    addr: Some(local),
                    port: 0,
                })
            };
        self.inner.general.set_device_mask(device_mask);
        Ok(())
    }

    fn connect(&self, remote_addr: SocketAddrEx) -> KResult {
        let remote_addr = remote_addr.into_ip()?;
        let remote = self.check_ip_version(remote_addr.ip().into())?;
        let local = self.local_address_for(remote)?;
        if self.inner.local_addr.read().is_none() {
            *self.inner.local_addr.write() = Some(local);
        }
        *self.inner.peer_addr.write() = Some(remote);
        self.inner
            .general
            .set_device_mask(SERVICE.lock().device_mask_for(&IpListenEndpoint {
                addr: Some(remote),
                port: 0,
            }));
        Ok(())
    }

    fn send(&self, mut src: impl Read + IoBuf, options: SendOptions) -> KResult<usize> {
        if self.inner.tx_closed.load(Ordering::Acquire) {
            return Err(KError::BrokenPipe);
        }

        let remote = self.remote_address(&options)?;
        let local = self.local_address_for(remote)?;
        let IpAddress::Ipv4(remote) = remote else {
            return Err(KError::from(LinuxError::EAFNOSUPPORT));
        };
        let IpAddress::Ipv4(local) = local else {
            return Err(KError::from(LinuxError::EAFNOSUPPORT));
        };
        let payload_len = src.remaining();
        let mut payload = Some(alloc::vec![0u8; payload_len]);
        src.read_exact(payload.as_mut().ok_or(KError::InvalidInput)?)?;
        let packet_len = ipv4::IPV4_MIN_HEADER_LEN + payload_len;

        self.inner
            .general
            .send_poller_with_nonblocking(self, options.flags.nonblocking(), || {
                let remote_addr = IpAddress::Ipv4(remote);
                let local_addr = IpAddress::Ipv4(local);
                let mut service = SERVICE.lock();
                let source_addr =
                    service.prepare_ipv4_packet_send(Some(local_addr), &remote_addr, packet_len)?;

                let packet = ipv4::build_ipv4_packet(
                    match source_addr {
                        IpAddress::Ipv4(addr) => addr,
                        IpAddress::Ipv6(_) => return Err(KError::from(LinuxError::EAFNOSUPPORT)),
                    },
                    remote,
                    self.inner.ip_protocol.ipv4_number(),
                    (*self.inner.ttl.read()).unwrap_or(64),
                    payload.as_deref().ok_or(KError::InvalidInput)?,
                )
                .ok_or(KError::InvalidInput)?;
                service.send_ipv4_packet(packet)?;
                payload.take().ok_or(KError::InvalidInput)?;
                drop(service);
                poll_interfaces();
                Ok(payload_len)
            })
    }

    fn recv(&self, mut dst: impl Write + IoBufMut, options: RecvOptions<'_>) -> KResult<usize> {
        let RecvOptions {
            from,
            flags,
            ancillary: _,
            out_flags,
        } = options;
        let mut msg = RawRecvMsg {
            from,
            flags,
            out_flags,
        };
        self.inner
            .general
            .recv_poller_with_nonblocking(self, flags.nonblocking(), || {
                poll_interfaces();
                self.recv_packet(&mut dst, &mut msg)
            })
    }

    fn local_addr(&self) -> KResult<SocketAddrEx> {
        let local = (*self.inner.local_addr.read()).unwrap_or(match self.inner.ip_version {
            IpVersion::Ipv4 => IpAddress::Ipv4(Ipv4Address::from(Ipv4Addr::UNSPECIFIED)),
        });
        Ok(SocketAddrEx::Ip(SocketAddr::new(local.into(), 0)))
    }

    fn peer_addr(&self) -> KResult<SocketAddrEx> {
        let peer = (*self.inner.peer_addr.read()).ok_or(KError::NotConnected)?;
        Ok(SocketAddrEx::Ip(SocketAddr::new(peer.into(), 0)))
    }

    fn shutdown(&self, how: Shutdown) -> KResult {
        if how.has_read() {
            self.inner.rx_shutdown.store(true, Ordering::Release);
            self.inner.poll_rx.wake();
        }
        if how.has_write() {
            self.inner.tx_closed.store(true, Ordering::Release);
        }
        Ok(())
    }
}

impl Pollable for RawSocket {
    fn poll(&self) -> IoEvents {
        poll_interfaces();
        let mut events = IoEvents::empty();
        events.set(
            IoEvents::IN,
            !self.inner.rx_queue.lock().is_empty()
                || self.inner.rx_shutdown.load(Ordering::Acquire),
        );
        events.set(
            IoEvents::OUT,
            !self.inner.tx_closed.load(Ordering::Acquire) && SERVICE.lock().can_send_ip_packet(),
        );
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.intersects(IoEvents::IN | IoEvents::RDNORM | IoEvents::RDBAND) {
            self.inner.general.register_rx_waker(context)?;
            if self.inner.rx_queue.lock().is_empty()
                && !self.inner.rx_shutdown.load(Ordering::Acquire)
            {
                context.register(&self.inner.poll_rx)?;
            }
        }
        if events.intersects(IoEvents::OUT | IoEvents::WRNORM | IoEvents::WRBAND) {
            self.inner.general.register_tx_waker(context)?;
        }
        Ok(())
    }
}

impl Drop for RawSocket {
    fn drop(&mut self) {
        let _ = self.shutdown(Shutdown::Both);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputDisposition {
    Accepted,
    NoSocket,
}

pub(crate) fn init_raw_registry() {
    RAW_SOCKET_REGISTRY.call_once(RawSocketRegistry::new);
}

pub(crate) fn handle_ipv4_input(header: Ipv4Header, ip_packet: &[u8]) -> InputDisposition {
    if !RAW_SOCKET_REGISTRY.is_inited() {
        return InputDisposition::NoSocket;
    }
    let mut delivered = false;
    for socket in RAW_SOCKET_REGISTRY.active_sockets() {
        if raw_socket_matches(&socket, header) {
            delivered = true;
            deliver_raw_packet(&socket, header, ip_packet);
        }
    }
    if delivered {
        InputDisposition::Accepted
    } else {
        InputDisposition::NoSocket
    }
}

fn raw_socket_matches(socket: &RawSocketInner, header: Ipv4Header) -> bool {
    if socket.ip_version != IpVersion::Ipv4 || socket.ip_protocol.ipv4_number() != header.protocol()
    {
        return false;
    }
    if let Some(IpAddress::Ipv4(local)) = *socket.local_addr.read()
        && !local.is_unspecified()
        && local != header.dst_addr()
    {
        return false;
    }
    if let Some(IpAddress::Ipv4(peer)) = *socket.peer_addr.read()
        && peer != header.src_addr()
    {
        return false;
    }
    true
}

fn deliver_raw_packet(socket: &RawSocketInner, header: Ipv4Header, ip_packet: &[u8]) {
    let from = SocketAddr::V4(SocketAddrV4::new(ipv4_to_core(header.src_addr()), 0));
    let packet = RawPacket {
        from,
        data: Vec::from(ip_packet),
    };
    let mut rx_queue = socket.rx_queue.lock();
    if rx_queue.len() >= RAW_RX_QUEUE_LIMIT {
        debug!("raw socket receive queue is full, dropping packet");
        return;
    }
    rx_queue.push_back(packet);
    drop(rx_queue);
    socket.poll_rx.wake();
}

fn ipv4_to_core(addr: Ipv4Address) -> Ipv4Addr {
    let octets = addr.octets();
    Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3])
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

#[cfg(unittest)]
mod tests {
    use alloc::{vec, vec::Vec};
    use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    use kio::Cursor;
    use unittest::def_test;

    use super::*;
    use crate::options::{Configurable, GetSocketOption, SetSocketOption};

    fn ipv4_packet_bytes(src: Ipv4Addr, dst: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
        ipv4::build_ipv4_packet(
            Ipv4Address::from(src),
            Ipv4Address::from(dst),
            IpProtocol::Icmp.ipv4_number(),
            32,
            payload,
        )
        .unwrap()
    }

    #[def_test]
    fn raw_unspecified_bound_address_defers_source_selection() {
        assert_eq!(
            validate_explicit_bound_source(
                Some(IpAddress::Ipv4(Ipv4Addr::UNSPECIFIED.into())),
                |_| false,
                |_| false,
            ),
            Ok(None)
        );
        assert_eq!(
            validate_explicit_bound_source(
                Some(IpAddress::Ipv4(Ipv4Addr::BROADCAST.into())),
                |_| false,
                |_| false,
            ),
            Ok(None)
        );
        assert_eq!(
            validate_explicit_bound_source(
                Some(IpAddress::Ipv4(Ipv4Addr::new(224, 0, 0, 1).into())),
                |_| false,
                |_| false,
            ),
            Ok(None)
        );
    }

    #[def_test]
    fn raw_bind_accepts_dynamic_source_addresses() {
        assert_eq!(
            validate_raw_bind_address(
                IpAddress::Ipv4(Ipv4Addr::UNSPECIFIED.into()),
                |_| false,
                |_| false,
            ),
            Ok(())
        );
        assert_eq!(
            validate_raw_bind_address(
                IpAddress::Ipv4(Ipv4Addr::BROADCAST.into()),
                |_| false,
                |_| false,
            ),
            Ok(())
        );
        assert_eq!(
            validate_raw_bind_address(
                IpAddress::Ipv4(Ipv4Addr::new(224, 0, 0, 1).into()),
                |_| false,
                |_| false,
            ),
            Ok(())
        );
    }

    #[def_test]
    fn raw_explicit_source_must_remain_local() {
        let source = IpAddress::Ipv4(Ipv4Addr::new(192, 0, 2, 2).into());
        assert_eq!(
            validate_explicit_bound_source(Some(source), |_| true, |_| false),
            Ok(Some(source))
        );
        assert_eq!(
            validate_explicit_bound_source(Some(source), |_| false, |_| false),
            Err(LinuxError::ENETUNREACH)
        );
    }

    #[def_test(serial)]
    fn raw_socket_ip_version_checks_and_defaults() {
        let socket = RawSocket::new(IpVersion::Ipv4, IpProtocol::Icmp);

        assert_eq!(
            socket
                .check_ip_version(IpAddress::Ipv4(Ipv4Addr::new(192, 0, 2, 10).into()))
                .unwrap(),
            IpAddress::Ipv4(Ipv4Addr::new(192, 0, 2, 10).into())
        );
        assert_eq!(
            socket.check_ip_version(IpAddress::Ipv6(Ipv6Addr::LOCALHOST.into())),
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
    fn raw_socket_parses_ipv4_packets() {
        let ipv4_packet = ipv4_packet_bytes(
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(10, 0, 0, 2),
            &[1, 2, 3, 4],
        );
        let header = Ipv4Header::parse_input(&ipv4_packet).unwrap();
        assert_eq!(header.src_addr(), Ipv4Addr::new(10, 0, 0, 1).into());
        assert_eq!(header.protocol(), IpProtocol::Icmp.ipv4_number());
        assert_eq!(ipv4::payload(&ipv4_packet, &header).unwrap(), &[1, 2, 3, 4]);
        assert_eq!(
            Ipv4Header::parse_input(&[1, 2, 3]),
            Err(ipv4::Ipv4Error::Malformed)
        );
    }

    #[def_test(serial)]
    fn raw_socket_matches_configured_protocol_number() {
        let socket = RawSocket::new(IpVersion::Ipv4, IpProtocol::Number(ipv4::PROTOCOL_TCP));
        assert_eq!(socket.inner.ip_protocol.ipv4_number(), ipv4::PROTOCOL_TCP);

        let header = Ipv4Header::parse_input(
            &ipv4::build_ipv4_packet(
                Ipv4Address::from(Ipv4Addr::new(10, 0, 0, 1)),
                Ipv4Address::from(Ipv4Addr::new(10, 0, 0, 2)),
                ipv4::PROTOCOL_TCP,
                32,
                &[1, 2, 3, 4],
            )
            .unwrap(),
        )
        .unwrap();
        assert!(raw_socket_matches(&socket.inner, header));
    }

    #[def_test(serial)]
    fn raw_socket_read_shutdown_drains_received_packets() {
        let socket = RawSocket::new(IpVersion::Ipv4, IpProtocol::Icmp);
        let packet = ipv4_packet_bytes(
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(10, 0, 0, 2),
            &[1, 2, 3, 4],
        );
        let header = Ipv4Header::parse_input(&packet).unwrap();

        socket.shutdown(Shutdown::Read).unwrap();
        assert!(raw_socket_matches(&socket.inner, header));
        deliver_raw_packet(&socket.inner, header, &packet);

        let mut recv_buf = vec![0u8; packet.len()];
        let mut msg = RawRecvMsg {
            from: None,
            flags: RecvFlags::empty(),
            out_flags: None,
        };
        assert_eq!(
            socket.recv_packet(&mut Cursor::new(recv_buf.as_mut_slice()), &mut msg),
            Ok(packet.len())
        );
        assert_eq!(
            socket.recv_packet(&mut Cursor::new(recv_buf.as_mut_slice()), &mut msg),
            Ok(0)
        );
    }

    #[def_test(serial)]
    fn raw_socket_receives_full_ipv4_packet() {
        let socket = RawSocket::new(IpVersion::Ipv4, IpProtocol::Number(99));
        let packet = ipv4::build_ipv4_packet(
            Ipv4Address::from(Ipv4Addr::new(10, 0, 0, 1)),
            Ipv4Address::from(Ipv4Addr::new(10, 0, 0, 2)),
            99,
            32,
            &[5, 4, 3, 2, 1],
        )
        .unwrap();
        let header = Ipv4Header::parse_input(&packet).unwrap();

        assert_eq!(
            handle_ipv4_input(header, &packet),
            InputDisposition::Accepted
        );

        let mut output = [0u8; 64];
        let mut dst = Cursor::new(output.as_mut_slice());
        let mut msg = RawRecvMsg {
            from: None,
            flags: RecvFlags::empty(),
            out_flags: None,
        };
        assert_eq!(
            socket.recv_packet(&mut dst, &mut msg).unwrap(),
            packet.len()
        );
        assert_eq!(&output[..packet.len()], packet.as_slice());
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

        *socket.inner.peer_addr.write() =
            Some(IpAddress::Ipv4(Ipv4Addr::new(198, 51, 100, 9).into()));
        assert_eq!(
            socket.remote_address(&SendOptions::default()).unwrap(),
            IpAddress::Ipv4(Ipv4Addr::new(198, 51, 100, 9).into())
        );

        assert_eq!(
            validate_explicit_bound_source(
                Some(IpAddress::Ipv4(Ipv4Addr::new(192, 0, 2, 44).into())),
                |_| true,
                |_| false,
            ),
            Ok(Some(IpAddress::Ipv4(Ipv4Addr::new(192, 0, 2, 44).into())))
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
            *socket.inner.local_addr.read(),
            Some(IpAddress::Ipv4(Ipv4Addr::UNSPECIFIED.into()))
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
            Ok(0)
        );
        assert_eq!(
            socket.send(Cursor::new(&[1u8, 2, 3][..]), SendOptions::default()),
            Err(KError::BrokenPipe)
        );
    }
}

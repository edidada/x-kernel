// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::task::Wake;

use ::core::{
    net::{Ipv4Addr, SocketAddrV4},
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Waker},
};
use kpoll::PollRegistrations;
use unittest::def_test;

use super::{super::output::has_valid_udp_checksum, *};
use crate::{
    buf::{PacketBuf, PacketOwner},
    udp::{InputDisposition, PreparedUdpPacket, deliver_ipv4_packet},
};

#[derive(Default)]
struct WakeCounter(AtomicUsize);

impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn received_packet(ifindex: i32, local: IpEndpoint) -> PreparedUdpPacket {
    let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 2), 4321).into();
    let packet = PacketBuf::from_ip_packet_vec(ifindex, vec![42], PacketOwner::Ipv4Stack);
    PreparedUdpPacket::new(packet, 0, 1, local.into(), remote).unwrap()
}

// Serial: exercises the shared UDP registry; socket Drop unregisters both PCBs.
#[def_test(serial)]
fn receive_waiter_isolated_from_other_sockets_and_device_binding_changes() {
    let socket = UdpSocket::new();
    let other = UdpSocket::new();
    let local_addr = Ipv4Address::new(192, 0, 2, 1).into();
    let local =
        registry::bind_udp_auto_ephemeral_pcb(socket.pcb.clone(), local_addr, false).unwrap();
    let other_local =
        registry::bind_udp_auto_ephemeral_pcb(other.pcb.clone(), local_addr, false).unwrap();
    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let context = Context::from_waker(&waker);
    let mut registrations = PollRegistrations::new();
    socket
        .register(&mut registrations.context(&context), IoEvents::IN)
        .unwrap();

    assert_eq!(
        deliver_ipv4_packet(received_packet(2, other_local)).0,
        InputDisposition::Accepted
    );
    assert!(other.pcb.has_recv_data());
    assert!(!socket.pcb.has_recv_data());
    assert_eq!(counter.0.load(Ordering::Relaxed), 0);

    socket.state().set_bound_dev_if_for_test(1);
    let (disposition, rejected) = deliver_ipv4_packet(received_packet(2, local));
    assert_eq!(disposition, InputDisposition::NoSocket);
    drop(rejected);
    socket.state().set_bound_dev_if_for_test(0);
    assert_eq!(counter.0.load(Ordering::Relaxed), 0);

    // The original read subscription remains valid after unbinding, even
    // when a matching datagram arrives through a different interface.
    assert_eq!(
        deliver_ipv4_packet(received_packet(2, local)).0,
        InputDisposition::Accepted
    );
    assert!(socket.pcb.has_recv_data());
    assert_eq!(counter.0.load(Ordering::Relaxed), 1);

    socket
        .register(&mut registrations.context(&context), IoEvents::IN)
        .unwrap();
    drop(registrations);
    assert_eq!(
        deliver_ipv4_packet(received_packet(1, local)).0,
        InputDisposition::Accepted
    );
    assert_eq!(counter.0.load(Ordering::Relaxed), 1);
}

#[def_test]
fn receive_waiter_observes_errors_and_shutdown() {
    let socket = UdpSocket::new();
    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let context = Context::from_waker(&waker);
    let mut registrations = PollRegistrations::new();
    for events in [IoEvents::IN, IoEvents::RDNORM, IoEvents::RDBAND] {
        socket
            .register(&mut registrations.context(&context), events)
            .unwrap();
        socket.state().record_socket_error(LinuxError::ECONNREFUSED);
        assert_eq!(counter.0.swap(0, Ordering::Relaxed), 1);
        assert!(socket.state().take_socket_error().is_some());
    }

    socket
        .register(&mut registrations.context(&context), IoEvents::IN)
        .unwrap();
    socket.state().shutdown(Shutdown::Read);
    assert!(socket.state().is_read_shutdown());
    assert_eq!(counter.0.load(Ordering::Relaxed), 1);
}

#[def_test]
fn error_msg_trunc_returns_original_payload_len() {
    let mut data = [0u8; 2];
    let mut flags = RecvFlags::empty();
    let mut msg = RecvErrorMsg {
        data: &mut data,
        from: None,
        flags: RecvFlags::TRUNCATE,
        ancillary: None,
        out_flags: Some(&mut flags),
    };

    let copied = copy_udp_error_payload(&[1, 2, 3, 4], &mut msg);

    assert_eq!(copied, 4);
    assert_eq!(msg.data, &[1, 2]);
    assert!(flags.contains(RecvFlags::TRUNCATE));
}

#[def_test]
fn udp_datagram_fills_only_transport_bytes_before_ipv4_submission() {
    use crate::{
        device::{DeviceRegistry, LoopbackDevice},
        network::{
            interface_address::Ipv4AddrEntry,
            output::Ipv4Output,
            router::{ROUTE_SCOPE_HOST, RouteTable},
        },
        runtime::tx_queue::DataTxQueue,
    };

    let socket = UdpSocket::new();
    let src_addr = Ipv4Address::new(192, 0, 2, 1);
    let dst_addr = Ipv4Address::BROADCAST;
    socket
        .state()
        .set_local_endpoint(Some(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 1234).into()));
    let mut devices = DeviceRegistry::new(4);
    let dev = devices
        .register(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap())
        .unwrap();
    // Keep routing empty to exercise bound broadcast source selection without an MTU.
    let _change = devices
        .add_ipv4_addr(
            dev,
            Ipv4AddrEntry {
                addr: crate::ip::Ipv4Cidr::new(src_addr, 24).unwrap(),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
                no_prefix_route: false,
                label: None,
            },
        )
        .unwrap();
    let routes = RouteTable::new();
    let mut queue = DataTxQueue::with_capacity(1);
    let mut output = Ipv4Output::new();
    let mut builder = Ipv4PacketBuilder::new(UDP_HEADER_LEN + 4).unwrap();
    builder.transport_mut()[UDP_HEADER_LEN..].copy_from_slice(&[1, 2, 3, 4]);
    let allocation = builder.transport_mut().as_ptr();
    let options = Ipv4SendOptions {
        bound_src: None,
        dst_addr,
        oif: dev.ifindex(),
        allow_broadcast: true,
        protocol: ipv4::PROTOCOL_UDP,
        ttl: 37,
        fragmentation: Ipv4FragmentationPolicy::PreferDontFragment,
    };
    let mut send = output
        .prepare(&routes, &mut devices, &mut queue, options, builder)
        .unwrap();
    let source = send.source_addr();
    let destination = send.destination_addr();
    assert_eq!(source, src_addr);
    assert_eq!(destination, dst_addr);
    socket
        .write_datagram(send.transport_mut(), source, destination, 4321)
        .unwrap();
    send.submit().unwrap();

    let packet = queue.pop_front().unwrap();
    let bytes = packet.network_packet().unwrap();
    let header = ipv4::Ipv4Header::parse_input(bytes).unwrap();
    let udp_packet = &bytes[header.header_len()..];
    assert_eq!(udp_packet.as_ptr(), allocation);
    assert_eq!(header.src_addr(), src_addr);
    assert_eq!(header.dst_addr(), dst_addr);
    assert_eq!(header.total_len(), bytes.len());
    assert_eq!(header.protocol(), ipv4::PROTOCOL_UDP);
    assert_eq!(header.bytes()[8], 37);
    assert!(header.dont_fragment());
    assert!(has_valid_udp_checksum(src_addr, dst_addr, udp_packet));
    assert_eq!(&udp_packet[..4], &[4, 210, 16, 225]);
    assert_eq!(&udp_packet[UDP_HEADER_LEN..], &[1, 2, 3, 4]);
}

#[def_test]
fn mtu_discovery_maps_socket_options_to_ipv4_fragmentation_policy() {
    use Ipv4FragmentationPolicy::{Allow, Forbid, PreferDontFragment};
    for (option, expected) in [
        (IP_PMTUDISC_DONT, Allow),
        (IP_PMTUDISC_WANT, PreferDontFragment),
        (IP_PMTUDISC_DO, Forbid),
        (IP_PMTUDISC_PROBE, Forbid),
        (IP_PMTUDISC_INTERFACE, Allow),
        (IP_PMTUDISC_OMIT, Allow),
    ] {
        assert_eq!(fragmentation_policy(option), expected);
    }
}

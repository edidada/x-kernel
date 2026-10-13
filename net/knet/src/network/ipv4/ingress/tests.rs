// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, task::Wake};
use core::{
    net::SocketAddrV4,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::{Context, Waker},
};

use khal::time::monotonic_time;
use kpoll::{IoEvents, PollRegistrations, Pollable};
use ktime_types::TimeSpan;
use unittest::def_test;

use super::*;
use crate::{SocketAddrEx, SocketOps, buf::PacketOwner, ip::Ipv4Address, network::ipv4, udp};

struct IngressWakeProbe {
    ingress: Arc<Ipv4Ingress>,
    wakes: AtomicUsize,
    has_locked_wake: AtomicBool,
}

impl Wake for IngressWakeProbe {
    fn wake(self: Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::Relaxed);
        if self.ingress.state.try_lock().is_none() {
            self.has_locked_wake.store(true, Ordering::Relaxed);
        }
    }
}

fn input_packet(bytes: Vec<u8>) -> PacketBuf {
    PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx)
}

fn udp_packet(port: u16, payload: &[u8]) -> Vec<u8> {
    let mut udp = etherparse::UdpHeader::without_ipv4_checksum(4321, port, payload.len())
        .unwrap()
        .to_bytes()
        .to_vec();
    udp.extend_from_slice(payload);
    ipv4::build_ipv4_packet(
        Ipv4Address::new(192, 0, 2, 1),
        Ipv4Address::new(192, 0, 2, 2),
        ipv4::PROTOCOL_UDP,
        64,
        &udp,
    )
    .unwrap()
}

// Serial: the socket owns a shared UDP registry entry and unregisters on Drop.
#[def_test(serial)]
fn udp_delivery_releases_input_lock() {
    let ingress = Arc::new(Ipv4Ingress::new());
    let local_addr = Ipv4Address::new(192, 0, 2, 2);
    ingress.set_ipv4_addrs(&[Ipv4Cidr::new(local_addr, 24).unwrap()]);
    let socket = udp::UdpSocket::new();
    socket
        .bind(SocketAddrEx::Ip(
            SocketAddrV4::new(local_addr.into(), 0).into(),
        ))
        .unwrap();
    let port = socket.local_addr().unwrap().into_ip().unwrap().port();
    let probe = Arc::new(IngressWakeProbe {
        ingress: ingress.clone(),
        wakes: AtomicUsize::new(0),
        has_locked_wake: AtomicBool::new(false),
    });
    let waker = Waker::from(probe.clone());
    let context = Context::from_waker(&waker);
    let mut registrations = PollRegistrations::new();
    socket
        .register(&mut registrations.context(&context), IoEvents::IN)
        .unwrap();

    let mut rx_batch = IngressBatch::with_capacity(2);
    rx_batch
        .packets
        .push(input_packet(udp_packet(port, b"first")));
    rx_batch
        .packets
        .push(input_packet(udp_packet(port, b"second")));
    let mut accepted = Vec::new();
    let mut control = Vec::new();
    assert!(ingress.try_process_rx_batch(
        monotonic_time(),
        &mut rx_batch,
        &mut accepted,
        &mut control,
    ));
    assert!(rx_batch.packets.is_empty());
    assert!(accepted.is_empty());
    assert!(control.is_empty());
    assert!(probe.wakes.load(Ordering::Relaxed) > 0);
    assert!(!probe.has_locked_wake.load(Ordering::Relaxed));
    for expected in [b"first".as_slice(), b"second".as_slice()] {
        let mut bytes = [0; 32];
        let (len, _) = socket.recv_datagram_now(&mut bytes).unwrap().unwrap();
        assert_eq!(&bytes[..len], expected);
    }
}

#[def_test]
fn received_fragment_publishes_expiration_deadline() {
    let ingress = Ipv4Ingress::new();
    ingress.set_ipv4_addrs(&[Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 2), 24).unwrap()]);
    let mut fragments = ipv4::fragment_output_packet(&udp_packet(9, b"fragment"), 28, 1)
        .unwrap()
        .into_iter();
    let mut rx_batch = IngressBatch::with_capacity(1);
    rx_batch
        .packets
        .push(input_packet(fragments.next().unwrap()));
    let mut accepted_packets = Vec::new();
    let mut control_packets = Vec::new();
    let timestamp = monotonic_time();

    let was_processed = ingress.with_state_locked_for_test(|| {
        ingress.try_process_rx_batch(
            timestamp,
            &mut rx_batch,
            &mut accepted_packets,
            &mut control_packets,
        )
    });
    assert!(!was_processed);
    assert_eq!(rx_batch.packets.len(), 1);
    assert!(accepted_packets.is_empty());
    assert!(control_packets.is_empty());

    assert!(ingress.try_process_rx_batch(
        timestamp,
        &mut rx_batch,
        &mut accepted_packets,
        &mut control_packets,
    ));
    assert!(rx_batch.packets.is_empty());
    assert!(accepted_packets.is_empty());
    assert!(control_packets.is_empty());
    let after_expiration = timestamp + TimeSpan::from_secs(60);
    assert!(ingress.handle_timer_tick(
        u64::try_from(after_expiration.span_since_origin().as_micros()).unwrap()
    ));
    let progress = ingress.expire_fragments(after_expiration, 1, &mut control_packets);
    assert_eq!(progress.timer_events, 1);
    assert!(!progress.has_more);
    assert_eq!(control_packets.len(), 1);
    assert!(!ingress.has_pending_timer());
}

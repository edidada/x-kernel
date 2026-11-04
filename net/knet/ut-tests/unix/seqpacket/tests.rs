// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktask::future::block_on;
use unittest::{assert_eq, def_test};

use super::*;
use crate::unix::BindEntry;

fn test_peer_credentials() -> PeerCredentials {
    PeerCredentials::new(kidentity::PidHandle::fixed_root(7), 1000, 1001)
}

#[def_test]
fn socketpair_keeps_message_boundaries() {
    let (sender, receiver) = SeqpacketTransport::new_pair(test_peer_credentials());
    assert_eq!(sender.send(&b"hello"[..], SendOptions::default()), Ok(5));
    assert_eq!(sender.send(&b"world!"[..], SendOptions::default()), Ok(6));

    let mut buf = [0u8; 16];
    assert_eq!(receiver.recv(&mut buf[..], RecvOptions::default()), Ok(5));
    assert_eq!(&buf[..5], b"hello");
    assert_eq!(receiver.recv(&mut buf[..], RecvOptions::default()), Ok(6));
    assert_eq!(&buf[..6], b"world!");
}

#[def_test]
fn short_receive_truncates_the_whole_message() {
    let (sender, receiver) = SeqpacketTransport::new_pair(test_peer_credentials());
    assert_eq!(sender.send(&b"abcdef"[..], SendOptions::default()), Ok(6));

    let mut buf = [0u8; 3];
    assert_eq!(receiver.recv(&mut buf[..], RecvOptions::default()), Ok(3));
    assert_eq!(&buf, b"abc");
    // The remainder is discarded rather than left for the next receive.
    let nonblocking = RecvOptions {
        flags: RecvFlags::DONT_WAIT,
        ..RecvOptions::default()
    };
    assert_eq!(receiver.recv(&mut buf[..], nonblocking), Err(KError::WouldBlock));
}

#[def_test]
fn listener_accepts_a_queued_connection() {
    let listener = SeqpacketTransport::new();
    let entry = BindEntry::default();
    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(1, &test_peer_credentials()).unwrap();

    let client = SeqpacketTransport::new();
    client
        .connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        )
        .unwrap();

    let UnixTransport::Seqpacket(accepted) = block_on(listener.accept(true)).unwrap().0 else {
        panic!("seqpacket listener returned another transport");
    };

    assert_eq!(client.send(&b"ping"[..], SendOptions::default()), Ok(4));
    let mut buf = [0u8; 16];
    assert_eq!(accepted.recv(&mut buf[..], RecvOptions::default()), Ok(4));
    assert_eq!(&buf[..4], b"ping");

    assert_eq!(accepted.send(&b"pong"[..], SendOptions::default()), Ok(4));
    assert_eq!(client.recv(&mut buf[..], RecvOptions::default()), Ok(4));
    assert_eq!(&buf[..4], b"pong");
}

#[def_test]
fn accept_would_block_without_a_connection() {
    let listener = SeqpacketTransport::new();
    let entry = BindEntry::default();
    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(1, &test_peer_credentials()).unwrap();

    assert_eq!(
        block_on(listener.accept(true)).err(),
        Some(KError::WouldBlock)
    );
}

#[def_test]
fn connecting_to_a_bound_but_unlistened_socket_is_refused() {
    let listener = SeqpacketTransport::new();
    let entry = BindEntry::default();
    listener.bind(&entry, &UnixAddr::Unbound).unwrap();

    let client = SeqpacketTransport::new();
    assert_eq!(
        client
            .connect(
                &entry,
                &UnixAddr::Unbound,
                ConnectOptions::default(),
                &test_peer_credentials(),
            )
            .err(),
        Some(KError::ConnectionRefused)
    );
}

#[def_test]
fn shutdown_write_makes_the_peer_see_eof() {
    let (sender, receiver) = SeqpacketTransport::new_pair(test_peer_credentials());
    sender.shutdown(Shutdown::Write).unwrap();

    let mut buf = [0u8; 8];
    let nonblocking = RecvOptions {
        flags: RecvFlags::DONT_WAIT,
        ..RecvOptions::default()
    };
    // The peer's receive side is closed: it reports EOF instead of blocking.
    assert_eq!(receiver.recv(&mut buf[..], nonblocking), Ok(0));
    // The closed direction refuses further sends.
    assert_eq!(
        sender.send(&b"late"[..], SendOptions::default()),
        Err(KError::BrokenPipe)
    );
}

#[def_test]
fn accepted_socket_inherits_listener_pass_credentials() {
    use crate::options::GetSocketOption;

    let listener = SeqpacketTransport::new();
    listener
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    let entry = BindEntry::default();
    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(1, &test_peer_credentials()).unwrap();

    let client = SeqpacketTransport::new();
    client
        .connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        )
        .unwrap();
    let UnixTransport::Seqpacket(accepted) = block_on(listener.accept(true)).unwrap().0 else {
        panic!("seqpacket listener returned another transport");
    };

    let mut enabled = false;
    accepted
        .get_option(GetSocketOption::PassCredentials(&mut enabled))
        .unwrap();
    assert!(enabled);
}

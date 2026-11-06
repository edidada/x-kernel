// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kcred::Cred;
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{
    SocketAddrEx,
    unix::{DgramTransport, StreamTransport, UnixDomainSocket, UnixTransportOps},
};

#[def_test]
fn unix_missing_abstract_address_returns_connection_refused() {
    let addr = UnixAddr::Abstract(Arc::from(&b"xkernel-missing-abstract-socket"[..]));
    assert_eq!(
        lookup(&addr, UnixSocketType::Stream, &Cred::root()).err(),
        Some(KError::ConnectionRefused)
    );
}

#[def_test]
fn abstract_binding_lifetime_tracks_bound_sockets_and_failed_binds() {
    let name: Arc<[u8]> = Arc::from(&b"xkernel-abstract-binding-lifetime"[..]);
    let addr = UnixAddr::Abstract(name.clone());
    let bind = |socket: &UnixDomainSocket| {
        socket.bind_with_cred(
            SocketAddrEx::Unix(addr.clone()),
            &Cred::root(),
            NodePermission::from_bits_truncate(0o777),
        )
    };
    let stream = UnixDomainSocket::new(StreamTransport::new(), UnixSocketType::Stream);
    let dgram = UnixDomainSocket::new(DgramTransport::new(), UnixSocketType::Datagram);
    bind(&stream).unwrap();
    bind(&dgram).unwrap();
    let conflict = UnixDomainSocket::new(DgramTransport::new(), UnixSocketType::Datagram);
    assert_eq!(bind(&conflict), Err(KError::AddrInUse));
    drop(stream);
    assert!(lookup(&addr, UnixSocketType::Datagram, &Cred::root()).is_ok());
    assert_eq!(
        lookup(&addr, UnixSocketType::Stream, &Cred::root()).err(),
        Some(KError::ConnectionRefused)
    );
    drop(dgram);
    assert_eq!(
        lookup(&addr, UnixSocketType::Datagram, &Cred::root()).err(),
        Some(KError::ConnectionRefused)
    );
    bind(&conflict).unwrap();
    drop(conflict);
    assert!(
        !ABSTRACT_BINDINGS
            .lock()
            .contains_key(&(name, UnixSocketType::Datagram))
    );
}

#[def_test]
fn reserved_abstract_address_is_exclusive_and_invisible_until_published() {
    let addr = UnixAddr::Abstract(Arc::from(&b"xkernel-reserved-abstract-socket"[..]));
    let mode = NodePermission::from_bits_truncate(0o777);
    let guard = reserve(&addr, UnixSocketType::Stream, mode, &Cred::root()).unwrap();
    assert_eq!(
        reserve(&addr, UnixSocketType::Stream, mode, &Cred::root())
            .err()
            .map(|e| e.canonicalize()),
        Some(KError::AddrInUse)
    );
    assert_eq!(
        lookup(&addr, UnixSocketType::Stream, &Cred::root()).err(),
        Some(KError::ConnectionRefused)
    );
    drop(guard);
    assert!(reserve(&addr, UnixSocketType::Stream, mode, &Cred::root()).is_ok());
}

#[def_test]
fn endpoint_snapshot_does_not_keep_abstract_address_reserved() {
    let addr = UnixAddr::Abstract(Arc::from(&b"xkernel-abstract-lookup-snapshot"[..]));
    let mode = NodePermission::from_bits_truncate(0o777);
    let socket = UnixDomainSocket::new(StreamTransport::new(), UnixSocketType::Stream);
    socket
        .bind_with_cred(SocketAddrEx::Unix(addr.clone()), &Cred::root(), mode)
        .unwrap();
    let endpoint = lookup(&addr, UnixSocketType::Stream, &Cred::root()).unwrap();
    drop(socket);
    let replacement = UnixDomainSocket::new(StreamTransport::new(), UnixSocketType::Stream);
    replacement
        .bind_with_cred(SocketAddrEx::Unix(addr.clone()), &Cred::root(), mode)
        .unwrap();
    drop(endpoint);
    assert!(lookup(&addr, UnixSocketType::Stream, &Cred::root()).is_ok());
}

#[def_test]
fn transport_bind_failure_releases_abstract_reservation() {
    let addr = UnixAddr::Abstract(Arc::from(&b"xkernel-bind-setup-failure"[..]));
    let mode = NodePermission::from_bits_truncate(0o777);
    let (connected, _peer) = StreamTransport::new_pair(crate::options::PeerCredentials::none());
    let socket = UnixDomainSocket::new(connected, UnixSocketType::Stream);
    assert_eq!(
        socket.bind_with_cred(SocketAddrEx::Unix(addr.clone()), &Cred::root(), mode),
        Err(KError::InvalidInput)
    );
    let replacement = UnixDomainSocket::new(StreamTransport::new(), UnixSocketType::Stream);
    assert_eq!(
        replacement.bind_with_cred(SocketAddrEx::Unix(addr), &Cred::root(), mode),
        Ok(())
    );
}

#[def_test]
fn accepted_socket_keeps_addresses_without_owning_listener_registration() {
    use crate::{
        AcceptOptions, ConnectOptions, SocketOps, options::PeerCredentials,
        unix::SeqpacketTransport,
    };

    for kind in [UnixSocketType::Stream, UnixSocketType::Seqpacket] {
        let new_socket = || match kind {
            UnixSocketType::Stream => UnixDomainSocket::new(StreamTransport::new(), kind),
            _ => UnixDomainSocket::new(SeqpacketTransport::new(), kind),
        };
        let server_name: Arc<[u8]> =
            Arc::from(alloc::format!("xkernel-address-server-{}", kind.as_raw()).into_bytes());
        let client_name: Arc<[u8]> =
            Arc::from(alloc::format!("xkernel-address-client-{}", kind.as_raw()).into_bytes());
        let server_addr = UnixAddr::Abstract(server_name.clone());
        let client_addr = UnixAddr::Abstract(client_name.clone());
        let mode = NodePermission::from_bits_truncate(0o777);
        let server = new_socket();
        let client = new_socket();
        server
            .bind_with_cred(SocketAddrEx::Unix(server_addr.clone()), &Cred::root(), mode)
            .unwrap();
        client
            .bind_with_cred(SocketAddrEx::Unix(client_addr), &Cred::root(), mode)
            .unwrap();
        server
            .listen_with_credentials(1, &PeerCredentials::none())
            .unwrap();
        client
            .connect_with_credentials(
                SocketAddrEx::Unix(server_addr.clone()),
                crate::ConnectOptions::default(),
                &Cred::root(),
                &PeerCredentials::none(),
            )
            .unwrap();
        assert!(
            matches!(client.peer_addr().unwrap(), SocketAddrEx::Unix(UnixAddr::Abstract(name)) if name == server_name)
        );
        let accepted = server.accept(AcceptOptions::default()).unwrap();
        assert!(
            matches!(accepted.local_addr().unwrap(), SocketAddrEx::Unix(UnixAddr::Abstract(name)) if name == server_name)
        );
        assert!(
            matches!(accepted.peer_addr().unwrap(), SocketAddrEx::Unix(UnixAddr::Abstract(name)) if name == client_name)
        );
        let endpoint = lookup(&server_addr, kind, &Cred::root()).unwrap();
        drop(server);
        assert_eq!(
            lookup(&server_addr, kind, &Cred::root()).err(),
            Some(KError::ConnectionRefused)
        );
        let replacement = new_socket();
        replacement
            .bind_with_cred(SocketAddrEx::Unix(server_addr.clone()), &Cred::root(), mode)
            .unwrap();
        drop(accepted);
        let client = new_socket();
        // A snapshot acquired before close cannot reach the replacement.
        assert_eq!(
            client.transport.connect(
                &endpoint,
                &server_addr,
                ConnectOptions::default(),
                &PeerCredentials::none()
            ),
            Err(KError::ConnectionRefused)
        );
        assert!(lookup(&server_addr, kind, &Cred::root()).is_ok());
    }
}

#[def_test]
fn datagram_snapshot_cannot_connect_after_receiver_close() {
    use crate::{ConnectOptions, options::PeerCredentials};

    let addr = UnixAddr::Abstract(Arc::from(&b"xkernel-dgram-closed-snapshot"[..]));
    let receiver = UnixDomainSocket::new(DgramTransport::new(), UnixSocketType::Datagram);
    receiver
        .bind_with_cred(
            SocketAddrEx::Unix(addr.clone()),
            &Cred::root(),
            NodePermission::from_bits_truncate(0o777),
        )
        .unwrap();
    let endpoint = lookup(&addr, UnixSocketType::Datagram, &Cred::root()).unwrap();
    drop(receiver);
    let sender = DgramTransport::new();
    assert_eq!(
        sender.connect(
            &endpoint,
            &addr,
            ConnectOptions::default(),
            &PeerCredentials::none()
        ),
        Err(KError::NotConnected)
    );
}

#[def_test]
fn unbound_bind_is_rejected_before_and_after_explicit_binding() {
    use crate::SocketOps;

    let name: Arc<[u8]> = Arc::from(&b"xkernel-explicit-bind-only"[..]);
    let socket = UnixDomainSocket::new(DgramTransport::new(), UnixSocketType::Datagram);
    let mode = NodePermission::from_bits_truncate(0o777);
    assert_eq!(
        socket.bind_with_cred(SocketAddrEx::Unix(UnixAddr::Unbound), &Cred::root(), mode),
        Err(KError::InvalidInput)
    );
    socket
        .bind_with_cred(
            SocketAddrEx::Unix(UnixAddr::Abstract(name.clone())),
            &Cred::root(),
            mode,
        )
        .unwrap();
    assert_eq!(
        socket.bind_with_cred(SocketAddrEx::Unix(UnixAddr::Unbound), &Cred::root(), mode),
        Err(KError::InvalidInput)
    );
    assert!(
        matches!(socket.local_addr().unwrap(), SocketAddrEx::Unix(UnixAddr::Abstract(local)) if local == name)
    );
}

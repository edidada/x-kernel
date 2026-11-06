// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Transport dispatch and addressable communication endpoints.

use alloc::{boxed::Box, sync::Arc};

use async_trait::async_trait;
use enum_dispatch::enum_dispatch;
use kerrno::{KError, KResult};
use kio::{IoBuf, IoBufMut, Read, Write};
use kpoll::{IoEvents, PollContext, PollRegisterError, Pollable};

use super::{
    DgramTransport, SeqpacketTransport, StreamTransport, UnixAddr, UnixSocketType, dgram,
    seqpacket, stream,
};
use crate::{
    ConnectOptions, RecvOptions, SendOptions, Shutdown,
    options::{Configurable, GetSocketOption, OptionHandled, PeerCredentials, SetSocketOption},
};

/// A published rendezvous endpoint, independent of its address registration.
#[derive(Clone)]
pub(crate) enum BoundEndpoint {
    Stream(Arc<stream::ListenerQueue>),
    Dgram(Arc<dgram::Endpoint>),
    Seqpacket(Arc<seqpacket::ListenerEndpoint>),
}

impl UnixTransport {
    pub(super) fn kind(&self) -> UnixSocketType {
        match self {
            Self::Stream(_) => UnixSocketType::Stream,
            Self::Dgram(_) => UnixSocketType::Datagram,
            Self::Seqpacket(_) => UnixSocketType::Seqpacket,
        }
    }
}

/// Transport interface for Unix-domain sockets.
#[async_trait]
#[enum_dispatch]
pub(crate) trait UnixTransportOps: Configurable + Pollable + Send + Sync {
    fn bind(&self, local_endpoint: &UnixAddr) -> KResult<BoundEndpoint>;
    fn local_addr(&self) -> UnixAddr;
    fn peer_addr(&self) -> UnixAddr;
    /// Starts listening and records the listener's own peer identity.
    ///
    /// Stream clients copy this snapshot when they connect, matching Linux
    /// `unix_listen` + `unix_stream_connect`.
    fn listen(&self, _backlog: usize, _credentials: &PeerCredentials) -> KResult {
        Err(KError::OperationNotSupported)
    }

    /// Connects to a bound peer.
    ///
    /// For streams, `credentials` is the connector's snapshot, which the
    /// accepted endpoint reports. Datagram transports ignore it because Linux
    /// does not establish peer credentials on `connect`.
    fn connect(
        &self,
        endpoint: &BoundEndpoint,
        remote_addr: &UnixAddr,
        options: ConnectOptions,
        _credentials: &PeerCredentials,
    ) -> KResult;

    async fn accept(&self, nonblocking: bool) -> KResult<UnixTransport>;

    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> KResult<usize>;
    fn recv(&self, dst: impl Write + IoBufMut, options: RecvOptions<'_>) -> KResult<usize>;

    fn shutdown(&self, _how: Shutdown) -> KResult {
        Ok(())
    }
}

#[enum_dispatch(Configurable, UnixTransportOps)]
pub enum UnixTransport {
    Stream(StreamTransport),
    Dgram(DgramTransport),
    Seqpacket(SeqpacketTransport),
}
impl Pollable for UnixTransport {
    fn poll(&self) -> IoEvents {
        match self {
            UnixTransport::Stream(stream) => stream.poll(),
            UnixTransport::Dgram(dgram) => dgram.poll(),
            UnixTransport::Seqpacket(seqpacket) => seqpacket.poll(),
        }
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        match self {
            UnixTransport::Stream(stream) => stream.register(context, events),
            UnixTransport::Dgram(dgram) => dgram.register(context, events),
            UnixTransport::Seqpacket(seqpacket) => seqpacket.register(context, events),
        }
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unix seqpacket socket transport.
//!
//! A seqpacket socket uses connections like a stream socket and keeps message
//! boundaries like a datagram socket. Both endpoints are local, so messages
//! pass through `async_channel` queues, as in the datagram transport.
//! `bind` creates the listener endpoint, `listen` enables it, `connect` queues
//! a connection, and `accept` takes one from the queue.

use alloc::{boxed::Box, sync::Arc, vec::Vec};

use async_channel::TryRecvError;
use async_trait::async_trait;
use kerrno::{KError, KResult};
use kio::{IoBufMut, Read, Write};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::{Mutex, RwLock};

use super::dgram::Datagram;
use crate::{
    ConnectOptions, RecvFlags, RecvOptions, SendOptions, Shutdown, SocketAddrEx,
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, OptionHandled, PeerCredentials, SetSocketOption},
    unix::{UnixAddr, UnixTransport, UnixTransportOps},
};

/// Maximum number of connections waiting for accept.
///
/// This limit bounds the memory held by connections that have not been accepted.
const LISTEN_BACKLOG: usize = 128;

/// One connection waiting in a listener's queue.
///
/// Both client channels are ready when the connection is queued, so connect
/// can return without waiting for accept.
struct Pending {
    /// What the server reads.
    to_server: async_channel::Receiver<Datagram>,
    /// What the server writes.
    from_server: async_channel::Sender<Datagram>,
    /// Poll set the server registers for readability of `to_server`.
    server_poll: Arc<PollSet>,
    /// Poll set the client registers for readability of `from_server`.
    client_poll: Arc<PollSet>,
    client_addr: UnixAddr,
    client_credentials: PeerCredentials,
}

/// Whether a bound socket accepts connections.
///
/// `Unlistened` is distinct from `AcceptShutdown`: the first must fail `accept`
/// immediately, while the second still lets `accept` drain connections queued
/// before `shutdown(SHUT_RD)`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ListenerPhase {
    Unlistened,
    Listening,
    AcceptShutdown,
}

impl ListenerPhase {
    fn is_listening(self) -> bool {
        self == Self::Listening
    }

    fn is_accept_shutdown(self) -> bool {
        self == Self::AcceptShutdown
    }
}

struct ListenerState {
    phase: ListenerPhase,
    credentials: PeerCredentials,
}

/// Accept side of a bound socket, shared with the transport while it is bound.
#[derive(Clone)]
struct Listener {
    pending: async_channel::Receiver<Pending>,
    endpoint: Arc<ListenerEndpoint>,
}

/// Listener endpoint used by connecting seqpacket sockets.
pub(crate) struct ListenerEndpoint {
    queue: async_channel::Sender<Pending>,
    poll: Arc<PollSet>,
    /// Listener state shared with the transport.
    ///
    /// A bound socket must also be listening before it can accept a connection.
    /// Otherwise, connect returns ECONNREFUSED.
    state: Mutex<ListenerState>,
}

impl ListenerEndpoint {
    fn enqueue(&self, pending: Pending) -> KResult<PeerCredentials> {
        let state = self.state.lock();
        if !state.phase.is_listening() {
            return Err(KError::ConnectionRefused);
        }
        self.queue
            .try_send(pending)
            .map_err(|_| KError::ConnectionRefused)?;
        let credentials = state.credentials.clone();
        drop(state);
        self.poll.wake();
        Ok(credentials)
    }
}

/// Which directions `shutdown` has closed.
#[derive(Clone, Copy, Default)]
struct ShutdownState {
    read: bool,
    write: bool,
}

/// The outgoing direction of a connected socket and the peer's identity.
///
/// `tx` sends into the peer's receive queue. `shutdown(SHUT_WR)` clears `tx`
/// but keeps the connection record. A second connect still returns EISCONN,
/// and SO_PEERCRED still reports the peer's credentials. The peer's receive
/// channel closes once the last sender is dropped.
struct PeerChannel {
    tx: Option<async_channel::Sender<Datagram>>,
    /// Poll set the peer registers for readability of its receive channel.
    read_poll: Arc<PollSet>,
    peer_credentials: PeerCredentials,
    peer_addr: UnixAddr,
}

pub struct SeqpacketTransport {
    /// Incoming messages from the peer.
    rx: Mutex<Option<(async_channel::Receiver<Datagram>, Arc<PollSet>)>>,
    /// Outgoing direction and the peer's identity.
    peer: RwLock<Option<PeerChannel>>,
    /// Accepted-connection queue of a listening socket.
    listener: Mutex<Option<Listener>>,
    /// Directions closed by `shutdown`.
    shutdown: Mutex<ShutdownState>,
    local_addr: RwLock<UnixAddr>,
    poll_state: Arc<PollSet>,
    options: GeneralOptions,
}

impl Default for SeqpacketTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl SeqpacketTransport {
    pub fn new() -> Self {
        Self {
            rx: Mutex::new(None),
            peer: RwLock::new(None),
            listener: Mutex::new(None),
            shutdown: Mutex::new(ShutdownState::default()),
            local_addr: RwLock::new(UnixAddr::Unbound),
            poll_state: Arc::default(),
            options: GeneralOptions::default(),
        }
    }

    /// Creates a connected seqpacket pair, matching `socketpair`.
    pub fn new_pair(credentials: PeerCredentials) -> (Self, Self) {
        let (tx1, rx1) = async_channel::unbounded();
        let (tx2, rx2) = async_channel::unbounded();
        let poll1 = Arc::new(PollSet::new());
        let poll2 = Arc::new(PollSet::new());
        let transport1 = Self::new();
        let transport2 = Self::new();
        *transport1.rx.lock() = Some((rx1, poll1.clone()));
        *transport1.peer.write() = Some(PeerChannel {
            tx: Some(tx2),
            read_poll: poll2.clone(),
            peer_credentials: credentials.clone(),
            peer_addr: UnixAddr::Unbound,
        });
        *transport2.rx.lock() = Some((rx2, poll2.clone()));
        *transport2.peer.write() = Some(PeerChannel {
            tx: Some(tx1),
            read_poll: poll1,
            peer_credentials: credentials,
            peer_addr: UnixAddr::Unbound,
        });
        (transport1, transport2)
    }

    /// Stops a listening socket from queueing new connections.
    ///
    /// Marking the state `AcceptShutdown` stops new connections. Closing the
    /// queue's sender wakes a blocked `accept` once the pending connections
    /// already queued have been drained; `accept` then observes
    /// `TryRecvError::Closed` and returns `EINVAL`, matching Linux after
    /// `shutdown(SHUT_RD)` on a listener.
    fn close_listener_accepts(&self) {
        let Some(listener) = self.listener.lock().clone() else {
            return;
        };
        let bind = listener.endpoint;
        let poll = {
            let mut state = bind.state.lock();
            if !state.phase.is_listening() {
                return;
            }
            state.phase = ListenerPhase::AcceptShutdown;
            bind.queue.close();
            drop(state);
            bind.poll.clone()
        };
        poll.wake();
    }
}

impl Configurable for SeqpacketTransport {
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<OptionHandled> {
        use GetSocketOption as O;

        if self.options.get_option_inner(opt)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }

        match opt {
            O::PassCredentials(_) => {}
            O::PeerCredentials(cred) => {
                **cred = self
                    .peer
                    .read()
                    .as_ref()
                    .map(|peer| peer.peer_credentials.clone())
                    .unwrap_or_default();
            }
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> KResult<OptionHandled> {
        use SetSocketOption as O;

        if self.options.set_option_inner(opt)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }

        match opt {
            O::PassCredentials(_) => {}
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }
}

#[async_trait]
impl UnixTransportOps for SeqpacketTransport {
    fn local_addr(&self) -> UnixAddr {
        self.local_addr.read().clone()
    }

    fn peer_addr(&self) -> UnixAddr {
        self.peer
            .read()
            .as_ref()
            .map(|peer| peer.peer_addr.clone())
            .unwrap_or_default()
    }

    fn bind(&self, local_addr: &UnixAddr) -> KResult<super::transport::BoundEndpoint> {
        let mut guard = self.listener.lock();
        if guard.is_some() {
            return Err(KError::InvalidInput);
        }
        let (tx, rx) = async_channel::bounded(LISTEN_BACKLOG);
        let poll = Arc::new(PollSet::new());
        let bind = Arc::new(ListenerEndpoint {
            queue: tx,
            poll: poll.clone(),
            state: Mutex::new(ListenerState {
                phase: ListenerPhase::Unlistened,
                credentials: PeerCredentials::none(),
            }),
        });
        *guard = Some(Listener {
            pending: rx,
            endpoint: bind.clone(),
        });
        self.local_addr.write().clone_from(local_addr);
        self.poll_state.wake();
        Ok(super::transport::BoundEndpoint::Seqpacket(bind))
    }

    fn listen(&self, _backlog: usize, credentials: &PeerCredentials) -> KResult {
        let listener = self.listener.lock().clone().ok_or(KError::InvalidInput)?;
        let bind = listener.endpoint;
        let mut state = bind.state.lock();
        if state.phase.is_listening() || state.phase.is_accept_shutdown() {
            return Err(KError::InvalidInput);
        }
        state.phase = ListenerPhase::Listening;
        state.credentials = credentials.clone();
        drop(state);
        self.poll_state.wake();
        Ok(())
    }

    fn connect(
        &self,
        endpoint: &super::transport::BoundEndpoint,
        remote_addr: &UnixAddr,
        _options: ConnectOptions,
        credentials: &PeerCredentials,
    ) -> KResult {
        // The connected-or-not test and the installation of the connection are
        // one critical section: a second `connect` must observe either the
        // complete connection or nothing, never a half-installed one.
        let mut peer = self.peer.write();
        if peer.is_some() {
            return Err(KError::AlreadyConnected);
        }
        let client_addr = self.local_addr.read().clone();
        let client_credentials = credentials.clone();
        let (to_server, server_rx) = async_channel::unbounded();
        let (from_server, client_rx) = async_channel::unbounded();
        let server_poll = Arc::new(PollSet::new());
        let client_poll = Arc::new(PollSet::new());

        let pending = Pending {
            to_server: server_rx,
            from_server,
            server_poll: server_poll.clone(),
            client_poll: client_poll.clone(),
            client_addr,
            client_credentials,
        };
        let super::transport::BoundEndpoint::Seqpacket(bind) = endpoint else {
            return Err(KError::NotConnected);
        };
        let listener_credentials = bind.enqueue(pending)?;

        // A shutdown may have run before this connection existed. Hold the
        // shutdown lock until both directions are installed so a concurrent
        // shutdown sees either no connection yet or the finished one, never a
        // half-installed connection that misses its state.
        let shutdown = self.shutdown.lock();
        if shutdown.read {
            client_rx.close();
        }
        *self.rx.lock() = Some((client_rx, client_poll));
        *peer = Some(PeerChannel {
            tx: (!shutdown.write).then_some(to_server),
            read_poll: server_poll,
            // Linux connects seqpacket sockets through the stream path, so the
            // client reports the listening socket's identity as its peer.
            peer_credentials: listener_credentials,
            peer_addr: remote_addr.clone(),
        });
        drop(shutdown);
        self.poll_state.wake();
        Ok(())
    }

    async fn accept(&self, nonblocking: bool) -> KResult<UnixTransport> {
        let listener = self.listener.lock().clone().ok_or(KError::InvalidInput)?;
        if listener.endpoint.state.lock().phase == ListenerPhase::Unlistened {
            return Err(KError::InvalidInput);
        }

        let pending = if nonblocking || self.options.nonblocking() {
            match listener.pending.try_recv() {
                Ok(pending) => pending,
                Err(TryRecvError::Empty) => {
                    let state = listener.endpoint.state.lock().phase;
                    return if state.is_listening() {
                        Err(KError::WouldBlock)
                    } else {
                        Err(KError::InvalidInput)
                    };
                }
                Err(TryRecvError::Closed) => return Err(KError::InvalidInput),
            }
        } else {
            listener
                .pending
                .recv()
                .await
                .map_err(|_| KError::InvalidInput)?
        };

        let accepted = Self::new();
        if self.options.pass_credentials() {
            accepted.set_option(SetSocketOption::PassCredentials(&true))?;
        }
        *accepted.rx.lock() = Some((pending.to_server, pending.server_poll));
        *accepted.peer.write() = Some(PeerChannel {
            tx: Some(pending.from_server),
            read_poll: pending.client_poll,
            peer_credentials: pending.client_credentials,
            peer_addr: pending.client_addr,
        });
        accepted
            .local_addr
            .write()
            .clone_from(&self.local_addr.read().clone());
        Ok(UnixTransport::Seqpacket(accepted))
    }

    fn send(&self, mut src: impl Read, options: SendOptions) -> KResult<usize> {
        if options.to.is_some() {
            return Err(KError::InvalidInput);
        }
        let mut data = Vec::new();
        src.read_to_end(&mut data)?;
        let len = data.len();
        let packet = Datagram {
            data,
            ancillary: options.ancillary,
            sender: self.local_addr.read().clone(),
            sender_credentials: options.sender_credentials,
        };

        if self.shutdown.lock().write {
            return Err(KError::BrokenPipe);
        }
        let connected = self.peer.read();
        let Some(peer) = connected.as_ref() else {
            return Err(KError::NotConnected);
        };
        let Some(tx) = peer.tx.as_ref() else {
            return Err(KError::BrokenPipe);
        };
        tx.try_send(packet).map_err(|_| KError::BrokenPipe)?;
        peer.read_poll.wake();
        Ok(len)
    }

    fn recv(&self, mut dst: impl Write + IoBufMut, mut options: RecvOptions) -> KResult<usize> {
        self.options
            .recv_poller_with_nonblocking(self, options.flags.nonblocking(), move || {
                // `shutdown(SHUT_RD)` closes the receiver, but `async_channel`
                // still yields records queued before the close. Draining them
                // first matches Linux, where a read shutdown does not discard
                // data already received. `state.read` also covers a shutdown
                // issued before `connect` installed the receiver.
                let is_read_shutdown = self.shutdown.lock().read;
                let mut guard = self.rx.lock();
                let Some((rx, _)) = guard.as_mut() else {
                    return Err(KError::NotConnected);
                };

                let Datagram {
                    data,
                    ancillary,
                    sender,
                    sender_credentials,
                } = match rx.try_recv() {
                    Ok(packet) => packet,
                    Err(TryRecvError::Empty) if is_read_shutdown => return Ok(0),
                    Err(TryRecvError::Empty) => return Err(KError::WouldBlock),
                    Err(TryRecvError::Closed) => return Ok(0),
                };
                let count = dst.write(&data)?;

                if let Some(from) = options.from.as_mut() {
                    **from = SocketAddrEx::Unix(sender);
                }
                match options.ancillary.as_mut() {
                    Some(dst) => {
                        if self.options.pass_credentials()
                            && let Some(credentials) = sender_credentials
                        {
                            dst.push(Arc::new(crate::KernelAncillaryData::Credentials(
                                credentials,
                            )));
                        }
                        dst.extend(ancillary);
                    }
                    None => {
                        let exposes_credentials =
                            self.options.pass_credentials() && sender_credentials.is_some();
                        if (!ancillary.is_empty() || exposes_credentials)
                            && let Some(discarded) = options.ancillary_discarded.as_deref_mut()
                        {
                            *discarded = true;
                        }
                    }
                }

                // A seqpacket receiver always consumes the whole message; a
                // short buffer truncates it and the remainder is discarded.
                Ok(if options.flags.contains(RecvFlags::TRUNCATE) {
                    data.len()
                } else {
                    count
                })
            })
    }

    fn shutdown(&self, how: Shutdown) -> KResult {
        // A seqpacket connection is a pair of message channels. Closing the
        // write direction drops this side's sender, so the peer observes EOF;
        // the connected record stays so `connect` still reports `EISCONN` and
        // `SO_PEERCRED` survives the half-close. Closing the read direction
        // closes this side's receiver, which makes the peer's sends fail while
        // leaving records already queued here available for `recv`.
        let mut state = self.shutdown.lock();
        state.read |= how.has_read();
        state.write |= how.has_write();
        drop(state);

        if how.has_write()
            && let Some(peer) = self.peer.write().as_mut()
        {
            peer.tx.take();
            peer.read_poll.wake();
        }

        if how.has_read() {
            let rx_poll = self.rx.lock().as_ref().map(|(rx, poll)| {
                rx.close();
                poll.clone()
            });
            if let Some(poll) = rx_poll {
                poll.wake();
            }
            self.close_listener_accepts();
        }
        Ok(())
    }
}

impl Pollable for SeqpacketTransport {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::OUT;
        let state = *self.shutdown.lock();

        // Read the peer before taking `rx`: `connect` holds the peer write
        // lock and then installs `rx`, so nesting the locks in the opposite
        // order here would deadlock.
        let is_peer_rx_closed = self
            .peer
            .read()
            .as_ref()
            .is_none_or(|peer| peer.tx.as_ref().is_none_or(|tx| tx.is_closed()));

        if let Some((rx, _)) = self.rx.lock().as_ref() {
            let is_read_shutdown = state.read || rx.is_closed();
            let is_write_shutdown = state.write || is_peer_rx_closed;
            events.set(IoEvents::IN, !rx.is_empty() || is_read_shutdown);
            events.set(IoEvents::RDHUP, is_read_shutdown);
            // A half-close only ends one direction. `HUP` is reserved for a
            // socket whose read and write directions are both gone, matching
            // `stream` and Linux rather than reporting the peer's `SHUT_WR`
            // as a fully dead connection.
            events.set(IoEvents::HUP, is_read_shutdown && is_write_shutdown);
        } else if let Some(listener) = self.listener.lock().as_ref() {
            let listener_state = listener.endpoint.state.lock().phase;
            events.set(IoEvents::IN, !listener.pending.is_empty() || state.read);
            events.set(IoEvents::RDHUP, state.read);
            events.set(IoEvents::HUP, state.read && state.write);
            // Linux does not report POLLOUT while the socket is in TCP_LISTEN,
            // including after a read shutdown that stops new accepts. A bound
            // socket that never called `listen` is not listening and stays
            // writable.
            if listener_state != ListenerPhase::Unlistened {
                events.set(IoEvents::OUT, false);
            }
        }
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if !events.intersects(IoEvents::IN | IoEvents::RDHUP | IoEvents::HUP) {
            return Ok(());
        }
        if let Some((_, poll)) = self.rx.lock().as_ref() {
            context.register(poll)?;
        } else if let Some(listener) = self.listener.lock().as_ref() {
            context.register(&listener.endpoint.poll)?;
        }
        Ok(())
    }
}

impl Drop for SeqpacketTransport {
    fn drop(&mut self) {
        if let Some(listener) = self.listener.lock().take() {
            let mut state = listener.endpoint.state.lock();
            state.phase = ListenerPhase::AcceptShutdown;
            listener.endpoint.queue.close();
            drop(state);
            listener.endpoint.poll.wake();
        }
        if let Some(peer) = self.peer.write().take() {
            peer.read_poll.wake();
        }
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/unix/seqpacket/tests.rs");

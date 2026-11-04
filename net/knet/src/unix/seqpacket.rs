// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unix seqpacket socket transport.
//!
//! A seqpacket socket is connection-oriented like a stream socket but keeps
//! message boundaries like a datagram socket. Both ends of every connection
//! live in this kernel, so the transport moves whole messages through the same
//! `async_channel` queues the datagram transport uses and adds the connection
//! setup a listener needs: `bind` creates the rendezvous, `listen` enables it,
//! `connect` queues a connection, and `accept` takes one out.

use alloc::{boxed::Box, sync::Arc, vec::Vec};

use async_channel::TryRecvError;
use async_trait::async_trait;
use kerrno::{KError, KResult};
use kio::{IoBufMut, Read, Write};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::{Mutex, RwLock};

use super::dgram::{Channel, Datagram};
use crate::{
    ConnectOptions, RecvFlags, RecvOptions, SendOptions, Shutdown, SocketAddrEx,
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, OptionHandled, PeerCredentials, SetSocketOption},
    unix::{UnixAddr, UnixTransport, UnixTransportOps},
};

/// Connections a listener may queue before `connect` starts failing.
///
/// Linux applies the `listen` backlog here. The queue is bounded like a listen
/// queue rather than unbounded so an unaccepted connection cannot pin memory.
const LISTEN_BACKLOG: usize = 128;

/// One connection waiting in a listener's queue.
///
/// The client's end of both directions is already wired when the entry is
/// queued, so `connect` returns without waiting for `accept`, exactly like
/// Linux queues the connection and returns.
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

/// Rendezvous a bound seqpacket socket publishes to `connect`.
pub(crate) struct Bind {
    queue: async_channel::Sender<Pending>,
    poll: Arc<PollSet>,
    /// Whether `listen` has enabled the rendezvous.
    ///
    /// Connecting to a bound-but-not-listening socket fails with
    /// `ECONNREFUSED`, so the address alone cannot decide acceptance. It is a
    /// plain field because every access happens under the `BindEntry` lock.
    is_listening: bool,
    /// Identity `listen` recorded; a connecting client reports it as its peer.
    listener_credentials: Mutex<PeerCredentials>,
}

impl Bind {
    fn enqueue(&self, pending: Pending) -> KResult<()> {
        if !self.is_listening {
            return Err(KError::ConnectionRefused);
        }
        self.queue
            .try_send(pending)
            .map_err(|_| KError::ConnectionRefused)?;
        self.poll.wake();
        Ok(())
    }
}

/// Which directions `shutdown` has closed.
#[derive(Clone, Copy, Default)]
struct ShutdownState {
    read: bool,
    write: bool,
}

pub struct SeqpacketTransport {
    /// Incoming messages from the peer.
    rx: Mutex<Option<(async_channel::Receiver<Datagram>, Arc<PollSet>)>>,
    /// Outgoing direction and the peer's identity.
    peer: RwLock<Option<Channel>>,
    /// Accepted-connection queue of a listening socket.
    listener: Mutex<Option<(async_channel::Receiver<Pending>, Arc<PollSet>)>>,
    /// Directions closed by `shutdown`.
    shutdown: Mutex<ShutdownState>,
    bind_slot: Mutex<Option<Arc<Mutex<Option<Bind>>>>>,
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
            bind_slot: Mutex::new(None),
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
        *transport1.peer.write() = Some(Channel {
            tx: tx2,
            poll: poll2.clone(),
            peer_credentials: credentials.clone(),
        });
        *transport2.rx.lock() = Some((rx2, poll2.clone()));
        *transport2.peer.write() = Some(Channel {
            tx: tx1,
            poll: poll1,
            peer_credentials: credentials,
        });
        (transport1, transport2)
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
                    .map(|channel| channel.peer_credentials.clone())
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
    fn bind(&self, slot: &super::BindEntry, local_addr: &UnixAddr) -> KResult {
        let bind_slot_handle = slot.seqpacket.clone();
        let mut bind = bind_slot_handle.lock();
        if bind.is_some() {
            return Err(KError::AddrInUse);
        }
        let mut guard = self.listener.lock();
        if guard.is_some() {
            return Err(KError::InvalidInput);
        }
        let (tx, rx) = async_channel::bounded(LISTEN_BACKLOG);
        let poll = Arc::new(PollSet::new());
        *bind = Some(Bind {
            queue: tx,
            poll: poll.clone(),
            is_listening: false,
            listener_credentials: Mutex::new(PeerCredentials::none()),
        });
        drop(bind);
        *guard = Some((rx, poll));
        self.local_addr.write().clone_from(local_addr);
        *self.bind_slot.lock() = Some(bind_slot_handle);
        self.poll_state.wake();
        Ok(())
    }

    fn listen(&self, _backlog: usize, credentials: &PeerCredentials) -> KResult {
        let handle = self.bind_slot.lock().clone().ok_or(KError::InvalidInput)?;
        let mut bind = handle.lock();
        let bind = bind.as_mut().ok_or(KError::InvalidInput)?;
        if bind.is_listening {
            return Err(KError::InvalidInput);
        }
        *bind.listener_credentials.lock() = credentials.clone();
        bind.is_listening = true;
        self.poll_state.wake();
        Ok(())
    }

    fn connect(
        &self,
        slot: &super::BindEntry,
        _local_addr: &UnixAddr,
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
        // The bind entry's lock is held across the listening test and the
        // enqueue, so a concurrent `listen` cannot change the verdict between
        // them.
        let bind_slot = slot.seqpacket.lock();
        let bind = bind_slot.as_ref().ok_or(KError::NotConnected)?;
        let listener_credentials = bind.listener_credentials.lock().clone();
        bind.enqueue(pending)?;
        drop(bind_slot);

        *self.rx.lock() = Some((client_rx, client_poll));
        *peer = Some(Channel {
            tx: to_server,
            poll: server_poll,
            // Linux connects seqpacket sockets through the stream path, so the
            // client reports the listening socket's identity as its peer.
            peer_credentials: listener_credentials,
        });
        self.poll_state.wake();
        Ok(())
    }

    async fn accept(&self, nonblocking: bool) -> KResult<(UnixTransport, UnixAddr)> {
        let (pending_rx, _) = self.listener.lock().clone().ok_or(KError::InvalidInput)?;
        let pending = if nonblocking || self.options.nonblocking() {
            match pending_rx.try_recv() {
                Ok(pending) => pending,
                Err(TryRecvError::Empty) => return Err(KError::WouldBlock),
                Err(TryRecvError::Closed) => return Err(KError::InvalidInput),
            }
        } else {
            pending_rx.recv().await.map_err(|_| KError::InvalidInput)?
        };

        let accepted = Self::new();
        if self.options.pass_credentials() {
            accepted.set_option(SetSocketOption::PassCredentials(&true))?;
        }
        *accepted.rx.lock() = Some((pending.to_server, pending.server_poll));
        *accepted.peer.write() = Some(Channel {
            tx: pending.from_server,
            poll: pending.client_poll,
            peer_credentials: pending.client_credentials,
        });
        accepted
            .local_addr
            .write()
            .clone_from(&self.local_addr.read().clone());
        Ok((UnixTransport::Seqpacket(accepted), pending.client_addr))
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
        let Some(chan) = connected.as_ref() else {
            return Err(KError::NotConnected);
        };
        chan.tx.try_send(packet).map_err(|_| KError::BrokenPipe)?;
        chan.poll.wake();
        Ok(len)
    }

    fn recv(&self, mut dst: impl Write + IoBufMut, mut options: RecvOptions) -> KResult<usize> {
        self.options
            .recv_poller_with_nonblocking(self, options.flags.nonblocking(), move || {
                if self.shutdown.lock().read {
                    return Ok(0);
                }
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
        // A seqpacket connection is a pair of message channels: closing the
        // write direction drops the peer's sender, so the peer's receive side
        // reports EOF. The read direction only has to make this side's
        // `recv` return EOF without discarding messages already queued.
        let mut state = self.shutdown.lock();
        if how.has_write() {
            state.write = true;
            if let Some(chan) = self.peer.write().take() {
                chan.poll.wake();
            }
        }
        if how.has_read() {
            state.read = true;
            if let Some((_, poll)) = self.rx.lock().as_ref() {
                poll.wake();
            }
        }
        Ok(())
    }
}

impl Pollable for SeqpacketTransport {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::OUT;
        let read_closed = self.shutdown.lock().read;
        if let Some((rx, _)) = self.rx.lock().as_ref() {
            events.set(
                IoEvents::IN,
                !rx.is_empty() || read_closed || rx.is_closed(),
            );
            // A closed channel means every sender is gone: a reader must see
            // the disconnect instead of blocking forever.
            events.set(IoEvents::HUP, rx.is_closed());
        }
        if let Some((pending, _)) = self.listener.lock().as_ref() {
            events.set(IoEvents::IN, !pending.is_empty());
        }
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if !events.contains(IoEvents::IN) {
            return Ok(());
        }
        if let Some((_, poll)) = self.rx.lock().as_ref() {
            context.register(poll)?;
        }
        if let Some((_, poll)) = self.listener.lock().as_ref() {
            context.register(poll)?;
        }
        Ok(())
    }
}

impl Drop for SeqpacketTransport {
    fn drop(&mut self) {
        if let Some(slot) = self.bind_slot.lock().take() {
            *slot.lock() = None;
        }
        if let Some(chan) = self.peer.write().take() {
            chan.poll.wake();
        }
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/unix/seqpacket/tests.rs");

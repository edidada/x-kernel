// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unix connection-oriented seqpacket socket transport.
//!
//! `SOCK_SEQPACKET` combines the connection lifecycle of `SOCK_STREAM` with the
//! message-boundary delivery of `SOCK_DGRAM`: each `send` becomes exactly one
//! `recv`, and the channel exists only between connected peers.
use alloc::{boxed::Box, sync::Arc, vec::Vec};

use async_channel::TryRecvError;
use async_trait::async_trait;
use kerrno::{KError, KResult};
use kio::{Read, Write};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::{Mutex, RwLock};

use crate::{
    AncillaryData, RecvFlags, RecvOptions, SendOptions, SocketAddrEx,
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, OptionHandled, SetSocketOption, UnixCredentials},
    unix::{UnixAddr, UnixTransport, UnixTransportOps},
};

struct Packet {
    data: Vec<u8>,
    ancillary: Vec<AncillaryData>,
    sender: UnixAddr,
    credentials: UnixCredentials,
}

struct Channel {
    tx: async_channel::Sender<Packet>,
    poll: Arc<PollSet>,
    /// PID of the process that owns the peer endpoint of this channel.
    pid: u32,
}

/// Registered listener state for a pathname or abstract seqpacket address.
pub struct Bind {
    tx: async_channel::Sender<Packet>,
    poll: Arc<PollSet>,
    pid: u32,
}

impl Bind {
    fn connect(&self, _pid: u32) -> Channel {
        // The listener-side endpoint of an accepted connection reports the
        // connecting peer's PID through SO_PEERCRED.
        Channel {
            tx: self.tx.clone(),
            poll: self.poll.clone(),
            pid: self.pid,
        }
    }
}

pub struct SeqPacketTransport {
    rx: Mutex<Option<(async_channel::Receiver<Packet>, Arc<PollSet>)>>,
    peer: RwLock<Option<Channel>>,
    local_addr: RwLock<UnixAddr>,
    bind_slot: Mutex<Option<Arc<Mutex<Option<Bind>>>>>,
    poll_state: Arc<PollSet>,
    options: GeneralOptions,
    pid: u32,
}

impl SeqPacketTransport {
    pub fn new(pid: u32) -> Self {
        SeqPacketTransport {
            rx: Mutex::new(None),
            peer: RwLock::new(None),
            local_addr: RwLock::new(UnixAddr::Unbound),
            bind_slot: Mutex::new(None),
            poll_state: Arc::default(),
            options: GeneralOptions::default(),
            pid,
        }
    }

    fn new_connected(
        rx: (async_channel::Receiver<Packet>, Arc<PollSet>),
        peer: Channel,
        pid: u32,
    ) -> Self {
        SeqPacketTransport {
            rx: Mutex::new(Some(rx)),
            peer: RwLock::new(Some(peer)),
            local_addr: RwLock::new(UnixAddr::Unbound),
            bind_slot: Mutex::new(None),
            poll_state: Arc::default(),
            options: GeneralOptions::default(),
            pid,
        }
    }

    /// Creates a connected socketpair, as used by `socketpair(SOCK_SEQPACKET)`.
    pub fn new_pair(pid: u32) -> (Self, Self) {
        let (tx1, rx1) = async_channel::unbounded();
        let (tx2, rx2) = async_channel::unbounded();
        let poll1 = Arc::new(PollSet::new());
        let poll2 = Arc::new(PollSet::new());
        let transport1 = SeqPacketTransport::new_connected(
            (rx1, poll1.clone()),
            Channel {
                tx: tx2,
                poll: poll2.clone(),
                pid,
            },
            pid,
        );
        let transport2 = SeqPacketTransport::new_connected(
            (rx2, poll2.clone()),
            Channel {
                tx: tx1,
                poll: poll1.clone(),
                pid,
            },
            pid,
        );
        (transport1, transport2)
    }
}

impl Configurable for SeqPacketTransport {
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<OptionHandled> {
        use GetSocketOption as O;

        if self.options.get_option_inner(opt)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }

        match opt {
            O::PassCredentials(_) => {}
            O::PeerCredentials(cred) => {
                // Report the PID of the process that created the peer endpoint
                // of this connection, falling back to the creator of this
                // socket when no connection exists yet.
                **cred =
                    UnixCredentials::new(self.peer.read().as_ref().map_or(self.pid, |c| c.pid));
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
impl UnixTransportOps for SeqPacketTransport {
    fn bind(&self, slot: &super::BindEntry, local_addr: &UnixAddr) -> KResult<()> {
        let bind_slot_handle = slot.seqpacket.clone();
        let mut slot = slot.seqpacket.lock();
        if slot.is_some() {
            return Err(KError::AddrInUse);
        }
        let mut guard = self.rx.lock();
        if guard.is_some() {
            return Err(KError::InvalidInput);
        }
        let (tx, rx) = async_channel::unbounded();
        let poll = Arc::new(PollSet::new());
        *slot = Some(Bind {
            tx,
            poll: poll.clone(),
            pid: self.pid,
        });
        drop(slot);
        *guard = Some((rx, poll));
        self.local_addr.write().clone_from(local_addr);
        *self.bind_slot.lock() = Some(bind_slot_handle);
        self.poll_state.wake();
        Ok(())
    }

    fn connect(&self, slot: &super::BindEntry, _local_addr: &UnixAddr) -> KResult<()> {
        let mut guard = self.peer.write();
        if guard.is_some() {
            return Err(KError::AlreadyConnected);
        }
        *guard = Some(
            slot.seqpacket
                .lock()
                .as_ref()
                .ok_or(KError::NotConnected)?
                .connect(self.pid),
        );
        self.poll_state.wake();
        Ok(())
    }

    async fn accept(&self, _nonblocking: bool) -> KResult<(UnixTransport, UnixAddr)> {
        // Seqpacket connections are established synchronously at connect()
        // time with a shared message queue, so there is no pending connection
        // to accept.
        Err(KError::InvalidInput)
    }

    fn send(&self, mut src: impl Read, options: SendOptions) -> KResult<usize> {
        if options.to.is_some() {
            return Err(KError::InvalidInput);
        }
        let mut message = Vec::new();
        src.read_to_end(&mut message)?;
        let len = message.len();
        let packet = Packet {
            data: message,
            ancillary: options.ancillary,
            sender: self.local_addr.read().clone(),
            credentials: options
                .credentials
                .unwrap_or_else(|| UnixCredentials::new(self.pid)),
        };

        let connected = self.peer.read();
        let Some(chan) = connected.as_ref() else {
            return Err(KError::NotConnected);
        };
        chan.tx.try_send(packet).map_err(|_| KError::BrokenPipe)?;
        chan.poll.wake();
        Ok(len)
    }

    fn recv(&self, mut dst: impl Write, mut options: RecvOptions) -> KResult<usize> {
        self.options.recv_poller(self, move || {
            let mut guard = self.rx.lock();
            let Some((rx, _)) = guard.as_mut() else {
                return Err(KError::NotConnected);
            };

            let Packet {
                data,
                ancillary,
                sender,
                credentials,
            } = match rx.try_recv() {
                Ok(packet) => packet,
                Err(TryRecvError::Empty) => {
                    return Err(KError::WouldBlock);
                }
                Err(TryRecvError::Closed) => {
                    return Ok(0);
                }
            };
            let count = dst.write(&data)?;

            if let Some(from) = options.from.as_mut() {
                **from = SocketAddrEx::Unix(sender);
            }
            if let Some(dst) = options.ancillary.as_mut() {
                if self.options.pass_credentials() {
                    dst.push(Box::new(crate::KernelAncillaryData::UnixCredentials(
                        credentials,
                    )));
                }
                dst.extend(ancillary);
            }

            Ok(if options.flags.contains(RecvFlags::TRUNCATE) {
                data.len()
            } else {
                count
            })
        })
    }
}

impl Pollable for SeqPacketTransport {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::OUT;
        if let Some((rx, _)) = self.rx.lock().as_ref() {
            events.set(IoEvents::IN, !rx.is_empty());
        }
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if let Some((_, poll)) = self.rx.lock().as_ref()
            && events.contains(IoEvents::IN)
        {
            context.register(poll)?;
        }
        Ok(())
    }
}

impl Drop for SeqPacketTransport {
    fn drop(&mut self) {
        if let Some(slot) = self.bind_slot.lock().take() {
            *slot.lock() = None;
        }
        if let Some(chan) = self.peer.write().take() {
            chan.poll.wake();
        }
    }
}

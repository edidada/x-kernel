// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unix datagram socket transport.
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::task::Context;

use async_channel::{TryRecvError, TrySendError};
use async_trait::async_trait;
use kerrno::{KError, KResult, LinuxError};
use kio::{IoBuf, Read, Write};
use kpoll::{IoEvents, PollSet, Pollable};
use ksync::{Mutex, RwLock};
use ktask::current;
use kthread::AsThread;

use crate::{
    AncillaryData, RecvFlags, RecvOptions, SendOptions, SocketAddrEx,
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, OptionHandled, SetSocketOption, UnixCredentials},
    unix::{UnixAddr, UnixTransport, UnixTransportOps, lookup_bind_entry},
};

/// Cap a single datagram so `send` cannot `read_to_end` an unbounded user
/// buffer into the kernel. 64KiB matches the `/dev/log` recv buffer and is
/// enough for syslog; larger payloads return `EMSGSIZE`.
const DGRAM_MAX_BYTES: usize = 64 * 1024;

/// Bound the in-kernel receive queue. An unbounded channel plus world-writable
/// `/dev/log` would let any process exhaust heap. 32 × 64KiB is 2MiB worst case.
const DGRAM_QUEUE_LEN: usize = 32;

fn new_dgram_queue() -> (
    async_channel::Sender<Datagram>,
    async_channel::Receiver<Datagram>,
) {
    async_channel::bounded(DGRAM_QUEUE_LEN)
}

/// Credentials of the sending task, recorded at `send` time (Linux SCM_CREDENTIALS).
///
/// Kernel tasks are not user threads, so they must not look like `euid == 0`.
/// [`UnixCredentials::default`] is pid/uid/gid 0 (root).
fn current_send_credentials() -> UnixCredentials {
    current()
        .try_as_thread()
        .map(|thread| {
            let credentials = thread.process_state().credentials.read();
            UnixCredentials {
                pid: thread.pid(),
                uid: credentials.euid(),
                gid: credentials.egid(),
            }
        })
        .unwrap_or(UnixCredentials {
            pid: 0,
            uid: u32::MAX,
            gid: u32::MAX,
        })
}

fn enqueue_dgram(
    tx: &async_channel::Sender<Datagram>,
    poll: &PollSet,
    packet: Datagram,
) -> KResult {
    match tx.try_send(packet) {
        Ok(()) => {
            poll.wake();
            Ok(())
        }
        Err(TrySendError::Full(_)) => Err(KError::WouldBlock),
        Err(TrySendError::Closed(_)) => Err(KError::BrokenPipe),
    }
}

struct Datagram {
    data: Vec<u8>,
    ancillary: Vec<AncillaryData>,
    sender: UnixAddr,
    cred: UnixCredentials,
}

struct Channel {
    tx: async_channel::Sender<Datagram>,
    poll: Arc<PollSet>,
}

pub struct Bind {
    tx: async_channel::Sender<Datagram>,
    poll: Arc<PollSet>,
}
impl Bind {
    fn connect(&self) -> Channel {
        let tx = self.tx.clone();
        Channel {
            tx,
            poll: self.poll.clone(),
        }
    }
}

pub struct DgramTransport {
    rx: Mutex<Option<(async_channel::Receiver<Datagram>, Arc<PollSet>)>>,
    peer: RwLock<Option<Channel>>,
    local_addr: RwLock<UnixAddr>,
    poll_state: Arc<PollSet>,
    options: GeneralOptions,
    pid: u32,
}
impl DgramTransport {
    pub fn new(pid: u32) -> Self {
        DgramTransport {
            rx: Mutex::new(None),
            peer: RwLock::new(None),
            local_addr: RwLock::new(UnixAddr::Unbound),
            poll_state: Arc::default(),
            options: GeneralOptions::default(),
            pid,
        }
    }

    fn new_connected(
        rx: (async_channel::Receiver<Datagram>, Arc<PollSet>),
        peer: Channel,
        pid: u32,
    ) -> Self {
        DgramTransport {
            rx: Mutex::new(Some(rx)),
            peer: RwLock::new(Some(peer)),
            local_addr: RwLock::new(UnixAddr::Unbound),
            poll_state: Arc::default(),
            options: GeneralOptions::default(),
            pid,
        }
    }

    pub fn new_pair(pid: u32) -> (Self, Self) {
        let (tx1, rx1) = new_dgram_queue();
        let (tx2, rx2) = new_dgram_queue();
        let poll1 = Arc::new(PollSet::new());
        let poll2 = Arc::new(PollSet::new());
        let transport1 = DgramTransport::new_connected(
            (rx1, poll1.clone()),
            Channel {
                tx: tx2,
                poll: poll2.clone(),
            },
            pid,
        );
        let transport2 = DgramTransport::new_connected(
            (rx2, poll2.clone()),
            Channel {
                tx: tx1,
                poll: poll1.clone(),
            },
            pid,
        );
        (transport1, transport2)
    }
}

impl Configurable for DgramTransport {
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<OptionHandled> {
        use GetSocketOption as O;

        if self.options.get_option_inner(opt)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }

        match opt {
            O::PassCredentials(_) => {}
            O::PeerCredentials(cred) => {
                // Datagram sockets are stateless and do not have a peer, so we
                // return the credentials of the process that created the
                // socket.
                **cred = UnixCredentials::new(self.pid);
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
impl UnixTransportOps for DgramTransport {
    fn bind(&self, slot: &super::BindEntry, local_addr: &UnixAddr) -> KResult {
        let mut slot = slot.dgram.lock();
        if slot.is_some() {
            return Err(KError::AddrInUse);
        }
        let mut guard = self.rx.lock();
        if guard.is_some() {
            return Err(KError::InvalidInput);
        }
        let (tx, rx) = new_dgram_queue();
        let poll = Arc::new(PollSet::new());
        *slot = Some(Bind {
            tx,
            poll: poll.clone(),
        });
        *guard = Some((rx, poll));
        self.local_addr.write().clone_from(local_addr);
        self.poll_state.wake();
        Ok(())
    }

    fn connect(&self, slot: &super::BindEntry, _local_addr: &UnixAddr) -> KResult {
        let mut guard = self.peer.write();
        if guard.is_some() {
            return Err(KError::AlreadyConnected);
        }
        *guard = Some(
            slot.dgram
                .lock()
                .as_ref()
                .ok_or(KError::NotConnected)?
                .connect(),
        );
        self.poll_state.wake();
        Ok(())
    }

    async fn accept(&self) -> KResult<(UnixTransport, UnixAddr)> {
        Err(KError::InvalidInput)
    }

    fn send(&self, mut src: impl Read + IoBuf, options: SendOptions) -> KResult<usize> {
        let remaining = src.remaining();
        if remaining > DGRAM_MAX_BYTES {
            return Err(LinuxError::EMSGSIZE.into());
        }
        let mut message = Vec::with_capacity(remaining);
        src.read_to_end(&mut message)?;
        let len = message.len();
        let packet = Datagram {
            data: message,
            ancillary: options.ancillary,
            sender: self.local_addr.read().clone(),
            cred: current_send_credentials(),
        };

        if let Some(addr) = options.to {
            let addr = addr.into_unix()?;
            lookup_bind_entry(&addr, |slot| {
                if let Some(bind) = slot.dgram.lock().as_ref() {
                    enqueue_dgram(&bind.tx, &bind.poll, packet)
                } else {
                    Err(KError::NotConnected)
                }
            })?;
            return Ok(len);
        }

        let mut pending = Some(packet);
        self.options
            .send_poller_with_nonblocking(self, options.flags.nonblocking(), || {
                let connected = self.peer.read();
                let Some(chan) = connected.as_ref() else {
                    return Err(KError::NotConnected);
                };
                let packet = pending.take().ok_or(KError::WouldBlock)?;
                match chan.tx.try_send(packet) {
                    Ok(()) => {
                        chan.poll.wake();
                        Ok(len)
                    }
                    Err(TrySendError::Full(packet)) => {
                        pending = Some(packet);
                        Err(KError::WouldBlock)
                    }
                    Err(TrySendError::Closed(_)) => Err(KError::BrokenPipe),
                }
            })
    }

    fn recv(&self, mut dst: impl Write, mut options: RecvOptions) -> KResult<usize> {
        self.options.recv_poller(self, move || {
            let mut guard = self.rx.lock();
            let Some((rx, poll)) = guard.as_mut() else {
                return Err(KError::NotConnected);
            };

            let Datagram {
                data,
                ancillary,
                sender,
                cred,
            } = match rx.try_recv() {
                Ok(packet) => packet,
                Err(TryRecvError::Empty) => {
                    return Err(KError::WouldBlock);
                }
                Err(TryRecvError::Closed) => {
                    return Ok(0);
                }
            };
            poll.wake();
            let count = dst.write(&data)?;
            if count < data.len() {
                warn!("UDP message truncated: {} -> {} bytes", data.len(), count);
            }

            if let Some(from) = options.from.as_mut() {
                **from = SocketAddrEx::Unix(sender);
            }
            if let Some(dst) = options.unix_cred.as_mut() {
                **dst = cred;
            }
            if let Some(dst) = options.ancillary.as_mut() {
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

impl Pollable for DgramTransport {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        if let Some((rx, _)) = self.rx.lock().as_ref() {
            events.set(IoEvents::IN, !rx.is_empty());
        }
        let can_send = match self.peer.read().as_ref() {
            Some(chan) => !chan.tx.is_full(),
            None => true,
        };
        events.set(IoEvents::OUT, can_send);
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN)
            && let Some((_, poll)) = self.rx.lock().as_ref()
        {
            poll.register(context.waker());
        }
        if events.contains(IoEvents::OUT) {
            if let Some(chan) = self.peer.read().as_ref() {
                chan.poll.register(context.waker());
            }
            self.poll_state.register(context.waker());
        }
    }
}

impl Drop for DgramTransport {
    fn drop(&mut self) {
        if let Some(chan) = self.peer.write().take() {
            chan.poll.wake();
        }
    }
}

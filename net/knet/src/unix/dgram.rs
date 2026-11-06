// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unix datagram socket transport.
use alloc::{boxed::Box, sync::Arc, vec::Vec};

use async_channel::TryRecvError;
use async_trait::async_trait;
use kerrno::{KError, KResult};
use kio::{IoBufMut, Read, Write};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::{Mutex, RwLock};

use crate::{
    AncillaryData, ConnectOptions, RecvFlags, RecvOptions, SendOptions, SocketAddrEx,
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, OptionHandled, PeerCredentials, SetSocketOption},
    unix::{UnixAddr, UnixTransport, UnixTransportOps},
};

pub(crate) struct Datagram {
    pub(crate) data: Vec<u8>,
    pub(crate) ancillary: Vec<AncillaryData>,
    pub(crate) sender: UnixAddr,
    pub(crate) sender_credentials: Option<crate::SenderCredentials>,
}

pub(crate) struct Channel {
    pub(crate) tx: async_channel::Sender<Datagram>,
    pub(crate) poll: Arc<PollSet>,
    /// Set only for socketpair peers. Datagram bind and connect do not set
    /// peer credentials.
    pub(crate) peer_credentials: PeerCredentials,
    peer_addr: UnixAddr,
}

pub(crate) struct Endpoint {
    tx: async_channel::Sender<Datagram>,
    poll: Arc<PollSet>,
}
impl Endpoint {
    fn connect(&self, addr: &UnixAddr) -> Channel {
        let tx = self.tx.clone();
        Channel {
            tx,
            poll: self.poll.clone(),
            peer_credentials: PeerCredentials::none(),
            peer_addr: addr.clone(),
        }
    }
}

pub struct DgramTransport {
    rx: Mutex<Option<(async_channel::Receiver<Datagram>, Arc<PollSet>)>>,
    peer: RwLock<Option<Channel>>,
    local_addr: RwLock<UnixAddr>,
    poll_state: Arc<PollSet>,
    options: GeneralOptions,
}
impl Default for DgramTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl DgramTransport {
    pub fn new() -> Self {
        DgramTransport {
            rx: Mutex::new(None),
            peer: RwLock::new(None),
            local_addr: RwLock::new(UnixAddr::Unbound),
            poll_state: Arc::default(),
            options: GeneralOptions::default(),
        }
    }

    pub(super) fn send_to(
        &self,
        src: impl Read,
        options: SendOptions,
        endpoint: &Endpoint,
    ) -> KResult<usize> {
        self.send_packet(src, options, &endpoint.tx, &endpoint.poll)
    }

    fn send_packet(
        &self,
        mut src: impl Read,
        options: SendOptions,
        tx: &async_channel::Sender<Datagram>,
        poll: &PollSet,
    ) -> KResult<usize> {
        let mut message = Vec::new();
        src.read_to_end(&mut message)?;
        let len = message.len();
        tx.try_send(Datagram {
            data: message,
            ancillary: options.ancillary,
            sender: self.local_addr.read().clone(),
            sender_credentials: options.sender_credentials,
        })
        .map_err(|_| KError::BrokenPipe)?;
        poll.wake();
        Ok(len)
    }

    fn new_connected(rx: (async_channel::Receiver<Datagram>, Arc<PollSet>), peer: Channel) -> Self {
        DgramTransport {
            rx: Mutex::new(Some(rx)),
            peer: RwLock::new(Some(peer)),
            local_addr: RwLock::new(UnixAddr::Unbound),
            poll_state: Arc::default(),
            options: GeneralOptions::default(),
        }
    }

    /// Creates a connected datagram pair. Both endpoints report the creator's
    /// credentials as their peer credentials.
    pub fn new_pair(credentials: PeerCredentials) -> (Self, Self) {
        let (tx1, rx1) = async_channel::unbounded();
        let (tx2, rx2) = async_channel::unbounded();
        let poll1 = Arc::new(PollSet::new());
        let poll2 = Arc::new(PollSet::new());
        let transport1 = DgramTransport::new_connected(
            (rx1, poll1.clone()),
            Channel {
                tx: tx2,
                poll: poll2.clone(),
                peer_credentials: credentials.clone(),
                peer_addr: UnixAddr::Unbound,
            },
        );
        let transport2 = DgramTransport::new_connected(
            (rx2, poll2.clone()),
            Channel {
                tx: tx1,
                poll: poll1.clone(),
                peer_credentials: credentials,
                peer_addr: UnixAddr::Unbound,
            },
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
impl UnixTransportOps for DgramTransport {
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
        let mut guard = self.rx.lock();
        if guard.is_some() {
            return Err(KError::InvalidInput);
        }
        let (tx, rx) = async_channel::unbounded();
        let poll = Arc::new(PollSet::new());
        let endpoint = Arc::new(Endpoint {
            tx,
            poll: poll.clone(),
        });
        *guard = Some((rx, poll));
        self.local_addr.write().clone_from(local_addr);
        self.poll_state.wake();
        Ok(super::transport::BoundEndpoint::Dgram(endpoint))
    }

    fn connect(
        &self,
        endpoint: &super::transport::BoundEndpoint,
        remote_addr: &UnixAddr,
        _options: ConnectOptions,
        _credentials: &PeerCredentials,
    ) -> KResult {
        let mut guard = self.peer.write();
        if guard.is_some() {
            return Err(KError::AlreadyConnected);
        }
        let super::transport::BoundEndpoint::Dgram(bind) = endpoint else {
            return Err(KError::NotConnected);
        };
        if bind.tx.is_closed() {
            return Err(KError::NotConnected);
        }
        *guard = Some(bind.connect(remote_addr));
        self.poll_state.wake();
        Ok(())
    }

    async fn accept(&self, _nonblocking: bool) -> KResult<UnixTransport> {
        Err(KError::InvalidInput)
    }

    fn send(&self, src: impl Read, options: SendOptions) -> KResult<usize> {
        let (tx, poll) = {
            let connected = self.peer.read();
            let peer = connected.as_ref().ok_or(KError::NotConnected)?;
            (peer.tx.clone(), peer.poll.clone())
        };
        self.send_packet(src, options, &tx, &poll)
    }

    fn recv(&self, mut dst: impl Write + IoBufMut, mut options: RecvOptions) -> KResult<usize> {
        self.options
            .recv_poller_with_nonblocking(self, options.flags.nonblocking(), move || {
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
                    Err(TryRecvError::Empty) => {
                        return Err(KError::WouldBlock);
                    }
                    Err(TryRecvError::Closed) => {
                        return Ok(0);
                    }
                };
                let count = dst.write(&data)?;
                if count < data.len() {
                    warn!("UDP message truncated: {} -> {} bytes", data.len(), count);
                }

                if let Some(from) = options.from.as_mut() {
                    **from = SocketAddrEx::Unix(sender);
                }
                match options.ancillary.as_mut() {
                    Some(dst) => {
                        // Deliver credentials first so passed files cannot fill
                        // a short control buffer before the credentials are written.
                        if self.options.pass_credentials()
                            && let Some(credentials) = sender_credentials
                        {
                            dst.push(Arc::new(crate::KernelAncillaryData::Credentials(
                                credentials,
                            )));
                        }
                        dst.extend(ancillary);
                    }
                    // Without a control buffer, reading the datagram discards its
                    // control data. Report the loss so the caller sets MSG_CTRUNC.
                    // This includes both passed files and SO_PASSCRED credentials.
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

impl Drop for DgramTransport {
    fn drop(&mut self) {
        if let Some(chan) = self.peer.write().take() {
            chan.poll.wake();
        }
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/unix/dgram/tests.rs");

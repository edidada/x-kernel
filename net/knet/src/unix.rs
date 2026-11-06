// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unix domain socket implementations.
pub(crate) mod dgram;
mod namespace;
pub(crate) mod seqpacket;
pub(crate) mod stream;
mod transport;

use alloc::sync::Arc;

use kalloc_types::KBox;
use kerrno::{KError, KResult};
use kio::{IoBuf, IoBufMut, Read, Write};
use kpoll::{IoEvents, PollContext, PollRegisterError, Pollable};
use kprocess::AsThread;
use ksync::Mutex;
use ktask::future::block_on_interruptible;
use kvfs::NodePermission;
use namespace::BindingGuard;
pub use transport::UnixTransport;
pub(crate) use transport::UnixTransportOps;

pub use self::{dgram::DgramTransport, seqpacket::SeqpacketTransport, stream::StreamTransport};
use crate::{
    AcceptOptions, ConnectOptions, RecvOptions, SendOptions, Shutdown, Socket, SocketAddrEx,
    SocketOps,
    options::{Configurable, GetSocketOption, OptionHandled, PeerCredentials, SetSocketOption},
};

/// Captures the current process identity and effective UID/GID as a peer
/// credential snapshot.
///
/// User-facing Unix listener and stream-connect entry points call this when
/// Linux establishes the peer relationship. The PID stays a process identity
/// until a reader resolves it through its own active PID namespace. Kernel
/// callers without a user thread call the inherent
/// [`UnixDomainSocket::listen_with_credentials`] or
/// [`UnixDomainSocket::connect_with_credentials`] on the concrete Unix socket
/// with an explicit snapshot (or [`PeerCredentials::none`]); the `SocketOps`
/// implementations below are the user-facing paths and require a user thread.
///
/// # Execution context
///
/// Must run on a user thread in process context. It reads the current task's
/// subjective credentials through a sleepable `RwLock`, so it must not run in
/// IRQ context, with preemption disabled, or while holding a spinlock; ordinary
/// sleepable locks may be held. It performs no I/O and does not wait for
/// another task.
///
/// # Panics
///
/// Panics when the current task has no user-thread runtime, including a kernel
/// task or an interrupt context.
pub fn current_peer_credentials() -> PeerCredentials {
    let thread = kprocess::current_user_thread();
    let cred = thread.cred();
    PeerCredentials::new(thread.process().pid_identity(), cred.euid(), cred.egid())
}

/// Captures the current sending process's identity and real UID/GID.
///
/// User-facing socket send entry points call this once per send. The pid stays
/// a process identity so each receiver resolves it in its own PID namespace
/// (Linux `pid_vnr` at delivery). Kernel callers without a user thread supply
/// an explicit [`SendOptions::sender_credentials`] or none.
///
/// # Execution context
///
/// Runs on a user thread in process context when one exists. It reads the
/// current task's subjective credentials through a sleepable `RwLock`, so it
/// must not run in IRQ context, with preemption disabled, or while holding a
/// spinlock; ordinary sleepable locks may be held. It performs no I/O and does
/// not wait for another task.
///
/// Returns `None` for a kernel task or interrupt context, which have no
/// user-visible sender identity.
pub fn current_sender_credentials() -> Option<crate::SenderCredentials> {
    let current = ktask::current();
    let thread = current.try_as_thread()?;
    let cred = kprocess::current_cred();
    Some(crate::SenderCredentials {
        pid: Some(thread.process().pid_identity()),
        uid: cred.ruid(),
        gid: cred.rgid(),
    })
}

#[derive(Default, Clone, Debug)]
pub enum UnixAddr {
    #[default]
    Unbound,
    Abstract(Arc<[u8]>),
    Path(Arc<str>),
}

/// The socket type requested at creation.
///
/// Linux keeps it in `sk_type` and reports it back from `SO_TYPE`, so it is
/// stored rather than derived from the transport: a socket may be created
/// before its transport kind is observable, and `unix_create` keeps
/// `SOCK_SEQPACKET` while rewriting only `SOCK_RAW` to `SOCK_DGRAM`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum UnixSocketType {
    Stream,
    Datagram,
    Seqpacket,
}

impl UnixSocketType {
    /// The `SOCK_*` value `SO_TYPE` reports for this type.
    pub const fn as_raw(self) -> i32 {
        match self {
            Self::Stream => linux_raw_sys::net::SOCK_STREAM as i32,
            Self::Datagram => linux_raw_sys::net::SOCK_DGRAM as i32,
            Self::Seqpacket => linux_raw_sys::net::SOCK_SEQPACKET as i32,
        }
    }
}

pub struct UnixDomainSocket {
    // Serialize bind and retain exclusive ownership of the address registration.
    binding: Mutex<Option<BindingGuard>>,
    transport: UnixTransport,
    socket_type: UnixSocketType,
}
impl Drop for UnixDomainSocket {
    fn drop(&mut self) {
        // Revoke the address before field destruction closes the transport.
        let binding = self.binding.lock().take();
        drop(binding);
    }
}

impl UnixDomainSocket {
    fn accept_with_storage(
        &self,
        options: AcceptOptions,
        prepare: impl FnOnce() -> KResult<KBox<core::mem::MaybeUninit<Self>>>,
    ) -> KResult<Socket> {
        let storage = prepare()?;
        let transport = block_on_interruptible(self.transport.accept(options.nonblocking))??;
        Ok(Socket::Unix(KBox::write(
            storage,
            Self {
                transport,
                // The accepted socket keeps the listener's type, as Linux keeps the
                // listening socket's `sk_type` for the connection it hands back.
                socket_type: self.socket_type,
                binding: Mutex::new(None),
            },
        )))
    }

    pub fn new(transport: impl Into<UnixTransport>, socket_type: UnixSocketType) -> Self {
        Self {
            transport: transport.into(),
            socket_type,
            binding: Mutex::new(None),
        }
    }

    /// Whether this socket uses the datagram transport.
    ///
    /// Seqpacket sockets now have their own connection-oriented transport, so
    /// only genuine `SOCK_DGRAM` sockets answer `true`.
    pub fn is_datagram(&self) -> bool {
        matches!(&self.transport, UnixTransport::Dgram(_))
    }

    /// Whether a send should capture the caller's credentials.
    ///
    /// Linux datagram sends always capture credentials so a receiver with
    /// `SO_PASSCRED` can observe the real sender. Stream sends capture when the
    /// sender or the connected receiver enabled `SO_PASSCRED`; Linux
    /// `unix_passcred_enabled` tests both sockets (`other` is the receiving
    /// peer). Ordinary stream traffic with neither option enabled adds no
    /// per-message credential barrier.
    pub fn should_attach_sender_credentials(&self) -> bool {
        match &self.transport {
            // Linux sends seqpacket messages through the datagram path, which
            // always captures the sender so a receiver with `SO_PASSCRED` can
            // observe it.
            UnixTransport::Dgram(_) | UnixTransport::Seqpacket(_) => true,
            UnixTransport::Stream(stream) => {
                stream.pass_credentials() || stream.peer_pass_credentials()
            }
        }
    }

    /// Binds this socket using an explicit credential for pathname access.
    ///
    /// Kernel callers use this entry because they do not have a current user
    /// task. User-facing bind enters through [`SocketOps::bind`], which takes
    /// one snapshot of the current task credential before calling this method.
    /// An unbound address is rejected; callers must supply an explicit name.
    ///
    /// # Execution context
    ///
    /// Requires sleepable task context with IRQs and preemption enabled and no
    /// spinlocks held. Pathname binding also requires a current filesystem
    /// context; abstract binding can run on a kernel task.
    pub fn bind_with_cred(
        &self,
        local_endpoint: SocketAddrEx,
        cred: &kcred::Cred,
        mode: NodePermission,
    ) -> KResult {
        let requested = local_endpoint.into_unix()?;
        let mut registration = self.binding.lock();
        if !matches!(self.transport.local_addr(), UnixAddr::Unbound) {
            return Err(KError::InvalidInput);
        }
        let guard = namespace::reserve(&requested, self.transport.kind(), mode, cred)?;
        let endpoint = self.transport.bind(&requested)?;
        guard.publish(endpoint);
        *registration = Some(guard);
        Ok(())
    }

    /// Starts listening with an explicit snapshot of this listener's identity.
    ///
    /// Kernel callers without a current user thread use this inherent entry on
    /// the concrete Unix socket before erasing it into a generic [`Socket`]; it
    /// neither reads nor requires a user-task credential. [`SocketOps::listen`]
    /// remains the user-facing path and snapshots the current user thread.
    /// Datagram transports ignore the snapshot because `listen` does not
    /// establish a peer there.
    pub fn listen_with_credentials(
        &self,
        backlog: usize,
        credentials: &PeerCredentials,
    ) -> KResult {
        self.transport.listen(backlog, credentials)
    }

    /// Connects using explicit credentials for pathname lookup and, for a
    /// stream, an explicit connector identity for the accepted endpoint.
    ///
    /// User-facing [`SocketOps::connect`] snapshots both values from the current
    /// task. Kernel callers without a user thread supply them here on the
    /// concrete Unix socket before erasing it into a generic [`Socket`].
    /// Datagram transports ignore `peer_credentials` because `connect`
    /// establishes no peer identity.
    pub fn connect_with_credentials(
        &self,
        remote_addr: SocketAddrEx,
        options: ConnectOptions,
        cred: &kcred::Cred,
        peer_credentials: &PeerCredentials,
    ) -> KResult {
        self.connect_unix_with_credentials(
            remote_addr.into_unix()?,
            options,
            cred,
            peer_credentials,
        )
    }

    fn connect_unix_with_credentials(
        &self,
        remote_addr: UnixAddr,
        options: ConnectOptions,
        cred: &kcred::Cred,
        peer_credentials: &PeerCredentials,
    ) -> KResult {
        let endpoint = namespace::lookup(&remote_addr, self.transport.kind(), cred)?;
        self.transport
            .connect(&endpoint, &remote_addr, options, peer_credentials)
    }
}
impl Configurable for UnixDomainSocket {
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<OptionHandled> {
        self.transport.get_option_inner(opt)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> KResult<OptionHandled> {
        self.transport.set_option_inner(opt)
    }
}
impl SocketOps for UnixDomainSocket {
    /// Returns the type the socket was created with, the way Linux reports
    /// `sk_type` back from `SO_TYPE`. Deriving it from the transport would
    /// report `SOCK_DGRAM` for `SOCK_SEQPACKET` pairs.
    fn socket_type(&self) -> i32 {
        self.socket_type.as_raw()
    }

    fn socket_family(&self) -> i32 {
        linux_raw_sys::net::AF_UNIX as i32
    }

    /// `unix_create` never records a protocol, so Linux leaves `sk_protocol` 0.
    fn socket_protocol(&self) -> i32 {
        0
    }

    fn bind(&self, local_endpoint: SocketAddrEx) -> KResult {
        let cred = kprocess::current_cred();
        let mode = NodePermission::from_bits_truncate(0o777 & !(kprocess::current_umask() as u16));
        self.bind_with_cred(local_endpoint, &cred, mode)
    }

    fn connect(&self, remote_addr: SocketAddrEx, options: ConnectOptions) -> KResult {
        let remote_addr = remote_addr.into_unix()?;
        let cred = kprocess::current_cred();
        // Linux initializes peer credentials only while establishing a
        // connection; datagram `connect` leaves `sk_peer_pid` unset.
        let peer_credentials = match &self.transport {
            UnixTransport::Stream(_) | UnixTransport::Seqpacket(_) => current_peer_credentials(),
            UnixTransport::Dgram(_) => PeerCredentials::none(),
        };
        self.connect_unix_with_credentials(remote_addr, options, &cred, &peer_credentials)
    }

    fn listen(&self, backlog: usize) -> KResult {
        // Only a connection-oriented listener records an identity for future
        // clients.
        let peer_credentials = match &self.transport {
            UnixTransport::Stream(_) | UnixTransport::Seqpacket(_) => current_peer_credentials(),
            UnixTransport::Dgram(_) => PeerCredentials::none(),
        };
        self.listen_with_credentials(backlog, &peer_credentials)
    }

    fn accept(&self, options: AcceptOptions) -> KResult<Socket> {
        self.accept_with_storage(options, KBox::<Self>::new_uninit)
    }

    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> KResult<usize> {
        if let (UnixTransport::Dgram(dgram), Some(addr)) = (&self.transport, options.to.as_ref()) {
            let addr = addr.clone().into_unix()?;
            let cred = kprocess::current_cred();
            let endpoint = namespace::lookup(&addr, UnixSocketType::Datagram, &cred)?;
            let transport::BoundEndpoint::Dgram(endpoint) = endpoint else {
                return Err(KError::NotConnected);
            };
            return dgram.send_to(src, options, &endpoint);
        }
        self.transport.send(src, options)
    }

    fn recv(&self, dst: impl Write + IoBufMut, options: RecvOptions<'_>) -> KResult<usize> {
        self.transport.recv(dst, options)
    }

    fn local_addr(&self) -> KResult<SocketAddrEx> {
        Ok(SocketAddrEx::Unix(self.transport.local_addr()))
    }

    fn peer_addr(&self) -> KResult<SocketAddrEx> {
        Ok(SocketAddrEx::Unix(self.transport.peer_addr()))
    }

    fn shutdown(&self, how: Shutdown) -> KResult {
        self.transport.shutdown(how)
    }
}

impl Pollable for UnixDomainSocket {
    fn poll(&self) -> IoEvents {
        self.transport.poll()
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        self.transport.register(context, events)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/unix/tests.rs");

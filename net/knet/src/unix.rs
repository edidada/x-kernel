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

/// Records the current process identity and effective UID/GID for a peer.
///
/// The PID is stored as a process handle. Readers get its numeric value in
/// their own PID namespace.
///
/// The `SocketOps` listen and connect methods use this helper to record the
/// caller's credentials. Kernel tasks must call
/// [`UnixDomainSocket::listen_with_credentials`] or
/// [`UnixDomainSocket::connect_with_credentials`] directly and pass credentials
/// or [`PeerCredentials::none`].
///
/// # Execution context
///
/// Requires a user thread in task context. Reading the task's credentials takes
/// a sleepable `RwLock` and may wait for the lock. IRQs and preemption must be
/// enabled, and the caller must hold no spinlock. Other sleepable locks may be
/// held. This helper performs no I/O.
///
/// # Panics
///
/// Panics if the current task has no user-thread runtime, as with a kernel task.
pub fn current_peer_credentials() -> PeerCredentials {
    let thread = kprocess::current_user_thread();
    let cred = thread.cred();
    PeerCredentials::new(thread.process().pid_identity(), cred.euid(), cred.egid())
}

/// Records the current sending process's identity and real UID/GID.
///
/// Socket send entry points call this once per send when credentials are needed.
/// The PID is stored as a process handle. Each receiver gets its numeric value
/// in its own PID namespace. Kernel callers can supply
/// [`SendOptions::sender_credentials`] directly or leave it unset.
///
/// Returns `None` if the current task has no user-thread runtime.
///
/// # Execution context
///
/// Requires task context. For a user thread, reading credentials takes a
/// sleepable `RwLock` and may wait for the lock. IRQs and preemption must be
/// enabled, and the caller must hold no spinlock. Other sleepable locks may be
/// held. This helper performs no I/O.
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
/// Stored on the socket so `SO_TYPE` reports the type selected at creation.
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
    // Allow one bind at a time and keep the socket's address registration alive.
    binding: Mutex<Option<BindingGuard>>,
    transport: UnixTransport,
    socket_type: UnixSocketType,
}
impl Drop for UnixDomainSocket {
    fn drop(&mut self) {
        // Remove the address registration before dropping the transport.
        let binding = self.binding.lock().take();
        drop(binding);
    }
}

impl UnixDomainSocket {
    pub fn new(transport: impl Into<UnixTransport>, socket_type: UnixSocketType) -> Self {
        Self {
            transport: transport.into(),
            socket_type,
            binding: Mutex::new(None),
        }
    }

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
                // Accepted sockets keep the listener's type.
                socket_type: self.socket_type,
                binding: Mutex::new(None),
            },
        )))
    }

    /// Whether this socket uses the datagram transport.
    ///
    /// Returns `true` only for `SOCK_DGRAM`, not `SOCK_SEQPACKET`.
    pub fn is_datagram(&self) -> bool {
        matches!(&self.transport, UnixTransport::Dgram(_))
    }

    /// Whether a send needs to record the caller's credentials.
    ///
    /// Datagram and seqpacket sends always record credentials for receivers
    /// using `SO_PASSCRED`. Stream sends record them when either peer enables
    /// `SO_PASSCRED`, or while the server connection is waiting for `accept`.
    /// Otherwise, stream reads have no per-message credential boundaries.
    pub fn should_attach_sender_credentials(&self) -> bool {
        match &self.transport {
            UnixTransport::Dgram(_) | UnixTransport::Seqpacket(_) => true,
            UnixTransport::Stream(stream) => {
                stream.pass_credentials() || stream.peer_pass_credentials()
            }
        }
    }

    /// Binds this socket using the supplied credentials for pathname access.
    ///
    /// Kernel tasks can call this method without a current user thread.
    /// [`SocketOps::bind`] reads the current task's credentials once and passes
    /// them here.
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

    /// Starts listening with the supplied listener credentials.
    ///
    /// Kernel tasks can call this method before wrapping the Unix socket in
    /// [`Socket`]. It does not read the current task's credentials.
    /// [`SocketOps::listen`] reads them from the current user thread.
    /// Datagram sockets do not support `listen` or use these credentials.
    pub fn listen_with_credentials(
        &self,
        backlog: usize,
        credentials: &PeerCredentials,
    ) -> KResult {
        self.transport.listen(backlog, credentials)
    }

    /// Connects using the supplied credentials for pathname lookup and peer identity.
    ///
    /// Stream and seqpacket sockets record `peer_credentials` for the accepted
    /// connection. Datagram sockets ignore them: `connect` does not set their
    /// peer credentials.
    ///
    /// [`SocketOps::connect`] reads the credentials from the current task.
    /// Kernel tasks can pass them directly before wrapping the Unix socket in
    /// [`Socket`].
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
    /// Returns the socket type selected at creation.
    fn socket_type(&self) -> i32 {
        self.socket_type.as_raw()
    }

    fn socket_family(&self) -> i32 {
        linux_raw_sys::net::AF_UNIX as i32
    }

    /// Unix sockets report protocol 0.
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
        // Datagram connect does not set peer credentials.
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

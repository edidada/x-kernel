// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;
use core::{bstr::ByteStr, future::poll_fn, task::Poll};

use kerrno::{KErrorKind, LinuxError, LinuxResult};
use knet::{
    RecvFlags, RecvOptions, SocketAddrEx, SocketOps,
    unix::{DgramTransport, UnixAddr, UnixDomainSocket, UnixSocketType},
};
use kpoll::{IoEvents, PollRegistrations, Pollable};
use kvfs::NodePermission;

/// Bind /dev/log as a Unix domain socket for syslog messages
pub fn bind_dev_log() -> LinuxResult<()> {
    let server = UnixDomainSocket::new(DgramTransport::new(), UnixSocketType::Datagram);
    let cred = kcred::initial_cred();
    if let Err(err) = server.bind_with_cred(
        SocketAddrEx::Unix(UnixAddr::Path("/dev/log".into())),
        &cred,
        NodePermission::from_bits_truncate(0o755),
    ) {
        let kind = KErrorKind::try_from(err);
        if matches!(
            kind,
            Ok(KErrorKind::Unsupported | KErrorKind::OperationNotSupported)
        ) {
            warn!("/dev/log not supported: {err}");
            return Ok(());
        }
        return Err(LinuxError::from(err));
    }
    ktask::spawn_with_name(
        move || {
            let mut buf = vec![0u8; 65536];
            loop {
                let mut registrations = PollRegistrations::new();
                let receive = poll_fn(|cx| {
                    let mut context = registrations.context(cx);
                    if server.register(&mut context, IoEvents::IN).is_err() {
                        return Poll::Ready(Err(kerrno::KError::BadState));
                    }
                    drop(context);
                    let options = RecvOptions {
                        flags: RecvFlags::DONT_WAIT,
                        ..RecvOptions::default()
                    };
                    match server.recv(&mut buf[..], options) {
                        Err(error) if error.canonicalize() == kerrno::KError::WouldBlock => {
                            Poll::Pending
                        }
                        result => Poll::Ready(result),
                    }
                });
                // SAFETY: nonblocking receive releases its socket/queue guards
                // on each poll. Pending retains only the stable message buffer
                // and readiness registrations, neither needed by PM recovery.
                let received = match unsafe { ktask::future::block_on_freezable(receive) } {
                    Ok(received) => received,
                    Err(error) => {
                        warn!("syslog freezer wait rejected: {error:?}");
                        break;
                    }
                };
                match received {
                    Ok(read) => {
                        let msg = ByteStr::new(buf[..read].trim_ascii_end());
                        info!("{msg}");
                    }
                    Err(err) => {
                        warn!("Failed to receive logs from client: {err:?}");
                        break;
                    }
                }
            }
        },
        "dev-log-server".into(),
    );
    Ok(())
}

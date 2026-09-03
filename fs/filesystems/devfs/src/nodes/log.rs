// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec};
use core::bstr::ByteStr;

use kerrno::{KErrorKind, LinuxError, LinuxResult};
use knet::{
    RecvOptions, SocketAddrEx, SocketOps,
    options::UnixCredentials,
    unix::{DgramTransport, UnixAddr, UnixDomainSocket},
};
use kvfs::NodeType;
use kvfs_simple::{DirMapping, SimpleFs};

/// Bind `/dev/log` as a Unix domain datagram socket for syslog messages.
///
/// The node stays world-writable so `syslog(3)` still works. User payloads
/// are forwarded to the kernel log only when the sender's `euid` is 0, the
/// current equivalent of Linux `CAP_SYSLOG`. Other datagrams are accepted
/// and dropped without logging the contents.
pub fn bind_dev_log() -> LinuxResult<()> {
    let server = UnixDomainSocket::new(DgramTransport::new(1));
    if let Err(err) = server.bind(SocketAddrEx::Unix(UnixAddr::Path("/dev/log".into()))) {
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
                let mut from = SocketAddrEx::Unix(UnixAddr::Unbound);
                // Fail closed: `Default` is uid 0. Overwrite from send-time creds.
                let mut cred = UnixCredentials {
                    pid: 0,
                    uid: u32::MAX,
                    gid: u32::MAX,
                };
                match server.recv(
                    &mut buf[..],
                    RecvOptions {
                        from: Some(&mut from),
                        unix_cred: Some(&mut cred),
                        ..RecvOptions::default()
                    },
                ) {
                    Ok(read) => {
                        // `euid == 0` stands in for Linux CAP_SYSLOG. Unprivileged
                        // payloads are not written to the kernel log at any level.
                        if cred.uid == 0 {
                            let msg = ByteStr::new(buf[..read].trim_ascii_end());
                            info!(
                                "/dev/log pid={} uid={} from={from:?}: {msg}",
                                cred.pid, cred.uid
                            );
                        }
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

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    // World-writable like Linux `/dev/log`; kernel-log injection is gated in
    // `bind_dev_log`, not by DAC.
    root.add(
        "log",
        kvfs_simple::SimpleFile::new(fs.clone(), NodeType::Socket, || Ok(b"")),
    );
}

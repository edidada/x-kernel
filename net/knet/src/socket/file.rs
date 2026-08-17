// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Open file operations for network sockets.

use alloc::{format, sync::Arc};

use kcred::Cred;
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollContext, PollRegisterError, Pollable};
use kvfs::{AnonInodeFs, FMode, FileOperations, OpenFlags, VfsFile, VfsResult};
use posix_types::{UserPtr, UserRead, UserWrite};

use crate::{
    RecvFlags, RecvOptions, SERVICE, SendFlags, SendOptions, Socket, SocketOps,
    device::LinkConfigUpdate,
};

const IFNAMSIZ: usize = 16;
const SIOCGIFFLAGS: u32 = 0x8913;
const SIOCSIFFLAGS: u32 = 0x8914;
const IFF_UP: u16 = 1;

/// Linux `struct ifreq` ABI carrier for commands whose union member is flags.
#[repr(C)]
#[derive(Clone, Copy, UserRead, UserWrite)]
struct IfReq {
    name: [u8; IFNAMSIZ],
    union_data: [u8; 24],
}

impl IfReq {
    fn interface_name(&self) -> KResult<&str> {
        let name_len = self
            .name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(IFNAMSIZ);
        core::str::from_utf8(&self.name[..name_len]).map_err(|_| KError::NoSuchDevice)
    }

    fn flags(&self) -> u16 {
        u16::from_ne_bytes([self.union_data[0], self.union_data[1]])
    }

    fn set_flags(&mut self, flags: u16) {
        self.union_data[..2].copy_from_slice(&flags.to_ne_bytes());
    }
}

/// Wraps a socket in a VFS file and captures `cred` as its open credential.
pub fn sock_alloc_file(socket: Socket, flags: u32, cred: Arc<Cred>) -> KResult<Arc<VfsFile>> {
    let flags = OpenFlags::from_bits(flags).ok_or(KError::InvalidInput)?;
    let socket = Arc::new(socket);
    let name = format!("socket:[{}]", socket.as_ref() as *const _ as usize);
    AnonInodeFs::global().get_file(
        &name,
        Arc::new(SocketFileOps),
        socket,
        FMode::READ | FMode::WRITE | FMode::STREAM,
        flags,
        cred,
    )
}

pub fn sock_from_file(file: &VfsFile) -> KResult<Arc<Socket>> {
    file.private_data_get::<Socket>().ok_or(KError::NotASocket)
}

struct SocketFileOps;

impl SocketFileOps {
    fn socket(file: &VfsFile) -> VfsResult<Arc<Socket>> {
        sock_from_file(file)
    }

    fn ioctl_ifreq(cmd: u32, arg: usize) -> VfsResult<usize> {
        let user_request = UserPtr::<IfReq>::from(arg);
        let mut request = user_request.read_vm()?;
        let interface_name = request.interface_name()?;
        let link = SERVICE
            .link_snapshots()
            .into_iter()
            .find(|link| link.name == interface_name)
            .ok_or(KError::NoSuchDevice)?;

        match cmd {
            SIOCGIFFLAGS => {
                request.set_flags(link.flags as u16);
                user_request.write_vm(request)?;
            }
            SIOCSIFFLAGS => {
                SERVICE
                    .update_device_link(
                        link.ifindex,
                        LinkConfigUpdate {
                            name: None,
                            mtu: None,
                            is_up: Some(request.flags() & IFF_UP != 0),
                        },
                    )
                    .map_err(KError::from)?;
            }
            _ => return Err(KError::NotATty),
        }
        Ok(0)
    }
}

impl FileOperations for SocketFileOps {
    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        let socket = Self::socket(file)?;
        let mut dst = buf;
        let mut options = RecvOptions::default();
        if file.is_nonblocking() {
            options.flags |= RecvFlags::DONT_WAIT;
        }
        socket.recv(&mut dst, options)
    }

    fn write(&self, file: &VfsFile, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        let socket = Self::socket(file)?;
        let src = buf;
        let mut options = SendOptions::default();
        if file.is_nonblocking() {
            options.flags |= SendFlags::DONT_WAIT;
        }
        match socket.as_ref() {
            Socket::Netlink(_) => {
                let cred = kprocess::current_cred();
                socket.send_with_cred(src, options, &cred)
            }
            _ => socket.send(src, options),
        }
    }

    fn ioctl(&self, _file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
        Self::ioctl_ifreq(cmd, arg)
    }

    fn poll(&self, file: &VfsFile) -> IoEvents {
        Self::socket(file)
            .map(|socket| socket.poll())
            .unwrap_or_else(|_| IoEvents::ERR)
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if let Ok(socket) = Self::socket(file) {
            socket.register(context, events)?;
        }
        Ok(())
    }
}

impl Pollable for Socket {
    fn poll(&self) -> IoEvents {
        match self {
            Socket::Tcp(tcp) => tcp.poll(),
            Socket::Udp(udp) => udp.poll(),
            Socket::Raw(raw) => raw.poll(),
            Socket::Unix(unix) => unix.poll(),
            Socket::Netlink(netlink) => netlink.poll(),
            Socket::Packet(packet) => packet.poll(),
            #[cfg(feature = "vsock")]
            Socket::Vsock(vsock) => vsock.poll(),
        }
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        match self {
            Socket::Tcp(tcp) => tcp.register(context, events),
            Socket::Udp(udp) => udp.register(context, events),
            Socket::Raw(raw) => raw.register(context, events),
            Socket::Unix(unix) => unix.register(context, events),
            Socket::Netlink(netlink) => netlink.register(context, events),
            Socket::Packet(packet) => packet.register(context, events),
            #[cfg(feature = "vsock")]
            Socket::Vsock(vsock) => vsock.register(context, events),
        }
    }
}

#[cfg(unittest)]
mod socket_tests {
    use core::mem::{offset_of, size_of};

    use linux_raw_sys::general::S_IFSOCK;
    use unittest::def_test;

    /// Test S_IFSOCK constant
    #[def_test]
    fn test_socket_mode_constant() {
        assert_eq!(S_IFSOCK, 0o140000);
    }

    #[def_test]
    fn ifreq_abi_layout_matches_linux_uapi() {
        assert_eq!(size_of::<super::IfReq>(), 40);
        assert_eq!(offset_of!(super::IfReq, union_data), 16);
    }
}

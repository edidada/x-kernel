// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Traditional network-device ioctl support for BusyBox `ifconfig`.
//!
//! This module implements the common SIOCGIF*/SIOCSIF* commands used by
//! BusyBox and net-tools against an AF_INET datagram socket. Control-plane
//! state is shared with rtnetlink through [`crate::netlink::ROUTE_STATE`].

use core::mem::MaybeUninit;

use kerrno::{KError, KResult, LinuxError};
use kvfs::VfsFile;
use linux_raw_sys::ioctl::{
    SIOCGIFADDR, SIOCGIFBRDADDR, SIOCGIFCONF, SIOCGIFFLAGS, SIOCGIFHWADDR, SIOCGIFINDEX,
    SIOCGIFMETRIC, SIOCGIFMTU, SIOCGIFNAME, SIOCGIFNETMASK, SIOCGIFTXQLEN, SIOCSIFADDR,
    SIOCSIFBRDADDR, SIOCSIFFLAGS, SIOCSIFMTU, SIOCSIFNETMASK,
};
use osvm::{read_vm_bytes, write_vm_bytes};

use crate::netlink::{
    ARPHRD_ETHER, ARPHRD_LOOPBACK, IFF_BROADCAST, IFF_MULTICAST, IFF_UP, del_primary_ipv4_addr,
    ipv4_mask_from_prefix, link_state_for_ifindex, link_state_for_name, primary_ipv4_addr,
    route_state, set_link_mtu, set_link_up, set_primary_ipv4_addr, set_primary_ipv4_broadcast,
    set_primary_ipv4_netmask,
};

/// `IFNAMSIZ` from Linux UAPI.
const IFNAMSIZ: usize = 16;
/// LP64 `sizeof(struct ifreq)`: 16-byte name + 24-byte union (`ifmap`).
const IFREQ_SIZE: usize = 40;
/// LP64 `sizeof(struct ifconf)`: `int` + pad + pointer.
const IFCONF_SIZE: usize = 16;
const AF_INET: u16 = 2;
const SOCKADDR_SIZE: usize = 16;

#[repr(C)]
#[derive(Clone, Copy)]
struct IfReqBytes {
    bytes: [u8; IFREQ_SIZE],
}

impl IfReqBytes {
    fn zeroed() -> Self {
        Self {
            bytes: [0; IFREQ_SIZE],
        }
    }

    fn name_bytes(&self) -> [u8; IFNAMSIZ] {
        let mut name = [0u8; IFNAMSIZ];
        name.copy_from_slice(&self.bytes[..IFNAMSIZ]);
        name
    }

    fn set_name(&mut self, name: &str) -> KResult<()> {
        if name.len() >= IFNAMSIZ {
            return Err(KError::from(LinuxError::ENODEV));
        }
        self.bytes[..IFNAMSIZ].fill(0);
        self.bytes[..name.len()].copy_from_slice(name.as_bytes());
        Ok(())
    }

    fn parse_name(&self) -> KResult<alloc::string::String> {
        let raw = self.name_bytes();
        let end = raw.iter().position(|&b| b == 0).unwrap_or(IFNAMSIZ);
        if end == 0 {
            return Err(KError::from(LinuxError::ENODEV));
        }
        core::str::from_utf8(&raw[..end])
            .map(alloc::string::String::from)
            .map_err(|_| KError::from(LinuxError::EINVAL))
    }

    fn set_flags(&mut self, flags: u16) {
        self.bytes[IFNAMSIZ..IFNAMSIZ + 2].copy_from_slice(&flags.to_ne_bytes());
    }

    fn flags(&self) -> u16 {
        u16::from_ne_bytes([self.bytes[IFNAMSIZ], self.bytes[IFNAMSIZ + 1]])
    }

    fn set_ivalue(&mut self, value: i32) {
        self.bytes[IFNAMSIZ..IFNAMSIZ + 4].copy_from_slice(&value.to_ne_bytes());
    }

    fn ivalue(&self) -> i32 {
        i32::from_ne_bytes([
            self.bytes[IFNAMSIZ],
            self.bytes[IFNAMSIZ + 1],
            self.bytes[IFNAMSIZ + 2],
            self.bytes[IFNAMSIZ + 3],
        ])
    }

    fn set_mtu(&mut self, mtu: i32) {
        self.set_ivalue(mtu);
    }

    fn mtu(&self) -> i32 {
        self.ivalue()
    }

    fn set_sockaddr_in(&mut self, addr: [u8; 4]) {
        let mut sa = [0u8; SOCKADDR_SIZE];
        sa[0..2].copy_from_slice(&AF_INET.to_ne_bytes());
        // sin_port stays zero.
        sa[4..8].copy_from_slice(&addr);
        self.bytes[IFNAMSIZ..IFNAMSIZ + SOCKADDR_SIZE].copy_from_slice(&sa);
    }

    fn sockaddr_in_addr(&self) -> KResult<[u8; 4]> {
        let family = u16::from_ne_bytes([self.bytes[IFNAMSIZ], self.bytes[IFNAMSIZ + 1]]);
        if family != AF_INET {
            return Err(KError::from(LinuxError::EINVAL));
        }
        Ok([
            self.bytes[IFNAMSIZ + 4],
            self.bytes[IFNAMSIZ + 5],
            self.bytes[IFNAMSIZ + 6],
            self.bytes[IFNAMSIZ + 7],
        ])
    }

    fn set_hwaddr(&mut self, link_type: u16, mac: [u8; 6]) {
        let mut sa = [0u8; SOCKADDR_SIZE];
        sa[0..2].copy_from_slice(&link_type.to_ne_bytes());
        sa[2..8].copy_from_slice(&mac);
        self.bytes[IFNAMSIZ..IFNAMSIZ + SOCKADDR_SIZE].copy_from_slice(&sa);
    }
}

fn read_ifreq(arg: usize) -> KResult<IfReqBytes> {
    let mut out = IfReqBytes::zeroed();
    let mut uninit = [MaybeUninit::<u8>::uninit(); IFREQ_SIZE];
    read_vm_bytes(arg as *const u8, &mut uninit)?;
    for (dst, src) in out.bytes.iter_mut().zip(uninit.iter()) {
        // SAFETY: `read_vm_bytes` fully initializes `uninit` on success.
        *dst = unsafe { src.assume_init() };
    }
    Ok(out)
}

fn write_ifreq(arg: usize, req: &IfReqBytes) -> KResult<()> {
    write_vm_bytes(arg as *mut u8, &req.bytes)?;
    Ok(())
}

fn read_ifconf_header(arg: usize) -> KResult<(i32, usize)> {
    let mut uninit = [MaybeUninit::<u8>::uninit(); IFCONF_SIZE];
    read_vm_bytes(arg as *const u8, &mut uninit)?;
    let mut bytes = [0u8; IFCONF_SIZE];
    for (dst, src) in bytes.iter_mut().zip(uninit.iter()) {
        // SAFETY: `read_vm_bytes` fully initializes `uninit` on success.
        *dst = unsafe { src.assume_init() };
    }
    let len = i32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let ptr = usize::from_ne_bytes(bytes[8..16].try_into().unwrap());
    Ok((len, ptr))
}

fn write_ifconf_len(arg: usize, len: i32) -> KResult<()> {
    write_vm_bytes(arg as *mut u8, &len.to_ne_bytes())?;
    Ok(())
}

fn require_net_admin() -> KResult<()> {
    if kprocess::current_cred().is_privileged() {
        Ok(())
    } else {
        Err(KError::from(LinuxError::EPERM))
    }
}

fn lookup_link_by_req(req: &IfReqBytes) -> KResult<crate::netlink::LinkState> {
    let name = req.parse_name()?;
    link_state_for_name(&name).ok_or_else(|| KError::from(LinuxError::ENODEV))
}

fn fill_ifreq_conf_entry(name: &str, addr: [u8; 4]) -> KResult<IfReqBytes> {
    let mut req = IfReqBytes::zeroed();
    req.set_name(name)?;
    req.set_sockaddr_in(addr);
    Ok(req)
}

fn handle_gifconf(arg: usize) -> KResult<usize> {
    let (capacity, buf_ptr) = read_ifconf_header(arg)?;
    if capacity < 0 {
        return Err(KError::from(LinuxError::EINVAL));
    }

    let state = route_state();
    let mut entries = alloc::vec::Vec::new();
    for link in &state.links {
        for addr in state
            .addrs
            .iter()
            .filter(|addr| addr.index == link.index as u32 && addr.family == AF_INET as u8)
        {
            let IpAddress::Ipv4(ipv4) = addr.address else {
                continue;
            };
            entries.push(fill_ifreq_conf_entry(&link.name, ipv4.octets())?);
        }
    }

    let needed = (entries.len() * IFREQ_SIZE) as i32;
    if buf_ptr == 0 {
        write_ifconf_len(arg, needed)?;
        return Ok(0);
    }

    let capacity = capacity as usize;
    let mut written = 0usize;
    for entry in &entries {
        if written + IFREQ_SIZE > capacity {
            break;
        }
        write_vm_bytes((buf_ptr + written) as *mut u8, &entry.bytes)?;
        written += IFREQ_SIZE;
    }
    write_ifconf_len(arg, written as i32)?;
    Ok(0)
}

fn handle_gifname(arg: usize) -> KResult<usize> {
    let mut req = read_ifreq(arg)?;
    let index = req.ivalue();
    let link = link_state_for_ifindex(index).ok_or_else(|| KError::from(LinuxError::ENODEV))?;
    req.set_name(&link.name)?;
    write_ifreq(arg, &req)?;
    Ok(0)
}

fn handle_gifflags(arg: usize) -> KResult<usize> {
    let mut req = read_ifreq(arg)?;
    let link = lookup_link_by_req(&req)?;
    req.set_flags(link.flags as u16);
    write_ifreq(arg, &req)?;
    Ok(0)
}

fn handle_gifaddr(arg: usize) -> KResult<usize> {
    let mut req = read_ifreq(arg)?;
    let link = lookup_link_by_req(&req)?;
    let addr = primary_ipv4_addr(link.index as u32)
        .ok_or_else(|| KError::from(LinuxError::EADDRNOTAVAIL))?;
    let IpAddress::Ipv4(ipv4) = addr.address else {
        return Err(KError::from(LinuxError::EADDRNOTAVAIL));
    };
    req.set_sockaddr_in(ipv4.octets());
    write_ifreq(arg, &req)?;
    Ok(0)
}

fn handle_gifnetmask(arg: usize) -> KResult<usize> {
    let mut req = read_ifreq(arg)?;
    let link = lookup_link_by_req(&req)?;
    let addr = primary_ipv4_addr(link.index as u32)
        .ok_or_else(|| KError::from(LinuxError::EADDRNOTAVAIL))?;
    req.set_sockaddr_in(ipv4_mask_from_prefix(addr.prefix_len));
    write_ifreq(arg, &req)?;
    Ok(0)
}

fn handle_gifbrdaddr(arg: usize) -> KResult<usize> {
    let mut req = read_ifreq(arg)?;
    let link = lookup_link_by_req(&req)?;
    let addr = primary_ipv4_addr(link.index as u32)
        .ok_or_else(|| KError::from(LinuxError::EADDRNOTAVAIL))?;
    // Linux copies `ifa_broadcast` even when zero (loopback /31 /32).
    let broadcast = addr.broadcast.unwrap_or([0, 0, 0, 0]);
    req.set_sockaddr_in(broadcast);
    write_ifreq(arg, &req)?;
    Ok(0)
}

fn handle_gifhwaddr(arg: usize) -> KResult<usize> {
    let mut req = read_ifreq(arg)?;
    let link = lookup_link_by_req(&req)?;
    let link_type = if link.link_type == ARPHRD_LOOPBACK {
        ARPHRD_LOOPBACK
    } else {
        ARPHRD_ETHER
    };
    req.set_hwaddr(link_type, link.mac);
    write_ifreq(arg, &req)?;
    Ok(0)
}

fn handle_gifmtu(arg: usize) -> KResult<usize> {
    let mut req = read_ifreq(arg)?;
    let link = lookup_link_by_req(&req)?;
    req.set_mtu(link.mtu as i32);
    write_ifreq(arg, &req)?;
    Ok(0)
}

fn handle_gifindex(arg: usize) -> KResult<usize> {
    let mut req = read_ifreq(arg)?;
    let link = lookup_link_by_req(&req)?;
    req.set_ivalue(link.index);
    write_ifreq(arg, &req)?;
    Ok(0)
}

fn handle_gifmetric(arg: usize) -> KResult<usize> {
    let mut req = read_ifreq(arg)?;
    let _link = lookup_link_by_req(&req)?;
    // Linux always reports metric 0 for this ioctl.
    req.set_ivalue(0);
    write_ifreq(arg, &req)?;
    Ok(0)
}

fn handle_giftxqlen(arg: usize) -> KResult<usize> {
    let mut req = read_ifreq(arg)?;
    let _link = lookup_link_by_req(&req)?;
    // Compatible placeholder used by BusyBox display paths.
    req.set_ivalue(1000);
    write_ifreq(arg, &req)?;
    Ok(0)
}

fn handle_sifflags(arg: usize) -> KResult<usize> {
    // Copy-in before privilege check so bad pointers return EFAULT like Linux.
    let req = read_ifreq(arg)?;
    require_net_admin()?;
    let link = lookup_link_by_req(&req)?;
    // Userspace-settable flags that BusyBox may toggle. Only IFF_UP is
    // implemented; other changes must not silently succeed.
    const IFF_DEBUG: u32 = 1 << 2;
    const IFF_NOTRAILERS: u32 = 1 << 5;
    const IFF_NOARP: u32 = 1 << 7;
    const IFF_PROMISC: u32 = 1 << 8;
    const IFF_ALLMULTI: u32 = 1 << 9;
    const IFF_DYNAMIC: u32 = 1 << 15;
    const USER_SETTABLE: u32 = IFF_UP
        | IFF_DEBUG
        | IFF_NOTRAILERS
        | IFF_NOARP
        | IFF_PROMISC
        | IFF_ALLMULTI
        | IFF_MULTICAST
        | IFF_DYNAMIC;
    const SUPPORTED_USER_SETTABLE: u32 = IFF_UP;
    let changed = (link.flags ^ u32::from(req.flags())) & USER_SETTABLE;
    if changed & !SUPPORTED_USER_SETTABLE != 0 {
        return Err(KError::from(LinuxError::EOPNOTSUPP));
    }
    let want_up = req.flags() as u32 & IFF_UP != 0;
    set_link_up(link.index, want_up).map_err(KError::from)?;
    Ok(0)
}

fn handle_sifaddr(arg: usize) -> KResult<usize> {
    let req = read_ifreq(arg)?;
    require_net_admin()?;
    let link = lookup_link_by_req(&req)?;
    let addr = req.sockaddr_in_addr()?;
    if addr == [0, 0, 0, 0] {
        del_primary_ipv4_addr(link.index as u32).map_err(KError::from)?;
    } else {
        let has_broadcast = link.flags & IFF_BROADCAST != 0;
        set_primary_ipv4_addr(link.index as u32, addr, has_broadcast).map_err(KError::from)?;
    }
    Ok(0)
}

fn handle_sifnetmask(arg: usize) -> KResult<usize> {
    let req = read_ifreq(arg)?;
    require_net_admin()?;
    let link = lookup_link_by_req(&req)?;
    let mask = req.sockaddr_in_addr()?;
    set_primary_ipv4_netmask(link.index as u32, mask).map_err(KError::from)?;
    Ok(0)
}

fn handle_sifbrdaddr(arg: usize) -> KResult<usize> {
    let req = read_ifreq(arg)?;
    require_net_admin()?;
    let link = lookup_link_by_req(&req)?;
    let broadcast = req.sockaddr_in_addr()?;
    set_primary_ipv4_broadcast(link.index as u32, broadcast).map_err(KError::from)?;
    Ok(0)
}

fn handle_sifmtu(arg: usize) -> KResult<usize> {
    let req = read_ifreq(arg)?;
    require_net_admin()?;
    let link = lookup_link_by_req(&req)?;
    let mtu = req.mtu();
    if mtu <= 0 {
        return Err(KError::from(LinuxError::EINVAL));
    }
    set_link_mtu(link.index, mtu as u32).map_err(KError::from)?;
    Ok(0)
}

fn is_inet_addr_ioctl(cmd: u32) -> bool {
    matches!(
        cmd,
        SIOCGIFADDR
            | SIOCGIFNETMASK
            | SIOCGIFBRDADDR
            | SIOCSIFADDR
            | SIOCSIFNETMASK
            | SIOCSIFBRDADDR
    )
}

fn socket_supports_inet_addr_ioctl(socket: &crate::Socket) -> bool {
    matches!(
        socket,
        crate::Socket::Tcp(_)
            | crate::Socket::Udp(_)
            | crate::Socket::Raw(_)
            | crate::Socket::Packet(_)
    )
}

/// Dispatches traditional network-device ioctl commands for socket files.
pub(crate) fn sock_dev_ioctl(
    socket: &crate::Socket,
    _file: &VfsFile,
    cmd: u32,
    arg: usize,
) -> KResult<usize> {
    if is_inet_addr_ioctl(cmd) && !socket_supports_inet_addr_ioctl(socket) {
        return Err(KError::NotATty);
    }
    match cmd {
        SIOCGIFCONF => handle_gifconf(arg),
        SIOCGIFNAME => handle_gifname(arg),
        SIOCGIFFLAGS => handle_gifflags(arg),
        SIOCGIFADDR => handle_gifaddr(arg),
        SIOCGIFNETMASK => handle_gifnetmask(arg),
        SIOCGIFBRDADDR => handle_gifbrdaddr(arg),
        SIOCGIFHWADDR => handle_gifhwaddr(arg),
        SIOCGIFMTU => handle_gifmtu(arg),
        SIOCGIFINDEX => handle_gifindex(arg),
        SIOCGIFMETRIC => handle_gifmetric(arg),
        SIOCGIFTXQLEN => handle_giftxqlen(arg),
        SIOCSIFFLAGS => handle_sifflags(arg),
        SIOCSIFADDR => handle_sifaddr(arg),
        SIOCSIFNETMASK => handle_sifnetmask(arg),
        SIOCSIFBRDADDR => handle_sifbrdaddr(arg),
        SIOCSIFMTU => handle_sifmtu(arg),
        _ => Err(KError::NotATty),
    }
}

use smoltcp::wire::IpAddress;

#[cfg(unittest)]
mod tests {
    use core::mem::size_of;

    use unittest::def_test;

    use super::*;
    use crate::netlink::{
        ROUTE_STATE, build_initial_state, inet_abc_len, init_route_state, ipv4_mask_from_prefix,
        ipv4_prefix_from_mask, primary_ipv4_addr, route_state, set_link_up, set_primary_ipv4_addr,
        set_primary_ipv4_broadcast, set_primary_ipv4_netmask,
    };

    fn init_state() {
        let state = build_initial_state(
            smoltcp::wire::IpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(
                smoltcp::wire::Ipv4Address::new(127, 0, 0, 1),
                8,
            )),
            Some(smoltcp::wire::IpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(
                smoltcp::wire::Ipv4Address::new(10, 0, 2, 15),
                24,
            ))),
            Some([0x02, 0, 0, 0, 0, 1]),
            Some(smoltcp::wire::IpAddress::Ipv4(
                smoltcp::wire::Ipv4Address::new(10, 0, 2, 2),
            )),
            1500,
        );
        if ROUTE_STATE.is_inited() {
            *ROUTE_STATE.write() = state;
        } else {
            init_route_state(state);
        }
    }

    #[def_test]
    fn abi_sizes_match_lp64_linux() {
        assert_eq!(IFREQ_SIZE, 40);
        assert_eq!(IFCONF_SIZE, 16);
        assert_eq!(size_of::<IfReqBytes>(), 40);
    }

    #[def_test]
    fn inet_abc_and_mask_helpers() {
        assert_eq!(inet_abc_len([10, 0, 2, 15]), Some(8));
        assert_eq!(inet_abc_len([127, 0, 0, 1]), Some(8));
        assert_eq!(inet_abc_len([172, 16, 0, 1]), Some(16));
        assert_eq!(inet_abc_len([192, 168, 1, 1]), Some(24));
        assert_eq!(inet_abc_len([240, 0, 0, 1]), Some(32));
        assert_eq!(inet_abc_len([224, 0, 0, 1]), None);
        assert_eq!(ipv4_mask_from_prefix(24), [255, 255, 255, 0]);
        assert_eq!(ipv4_prefix_from_mask([255, 255, 255, 128]), Some(25));
        assert!(ipv4_prefix_from_mask([255, 255, 0, 255]).is_none());
    }

    #[def_test(serial)]
    fn ifreq_name_and_sockaddr_roundtrip() {
        let mut req = IfReqBytes::zeroed();
        req.set_name("eth0").unwrap();
        assert_eq!(req.parse_name().unwrap(), "eth0");
        req.set_sockaddr_in([10, 0, 2, 15]);
        assert_eq!(req.sockaddr_in_addr().unwrap(), [10, 0, 2, 15]);
        req.set_flags(0x1043);
        assert_eq!(req.flags(), 0x1043);
    }

    #[def_test(serial)]
    fn primary_address_mutation_matches_linux_defaults() {
        init_state();
        set_primary_ipv4_addr(2, [192, 168, 1, 10], true).unwrap();
        let addr = primary_ipv4_addr(2).unwrap();
        assert_eq!(addr.prefix_len, 24);
        assert_eq!(addr.broadcast, Some([192, 168, 1, 255]));

        set_primary_ipv4_netmask(2, [255, 255, 255, 128]).unwrap();
        let addr = primary_ipv4_addr(2).unwrap();
        assert_eq!(addr.prefix_len, 25);
        assert_eq!(addr.broadcast, Some([192, 168, 1, 127]));

        set_primary_ipv4_broadcast(2, [192, 168, 1, 200]).unwrap();
        set_primary_ipv4_netmask(2, [255, 255, 255, 0]).unwrap();
        let addr = primary_ipv4_addr(2).unwrap();
        assert_eq!(addr.prefix_len, 24);
        assert_eq!(addr.broadcast, Some([192, 168, 1, 200]));

        set_link_up(2, false).unwrap();
        assert_eq!(
            route_state()
                .links
                .iter()
                .find(|link| link.index == 2)
                .unwrap()
                .flags
                & IFF_UP,
            0
        );
        assert!(primary_ipv4_addr(2).is_some());
    }
}

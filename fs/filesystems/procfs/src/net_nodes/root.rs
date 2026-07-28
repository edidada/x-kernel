// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::fmt::{Display, Write};

use kvfs::{DirMapping, SeqFileInode, SeqIterator, SimpleDir, SimpleFs};

fn format_ip(ip: impl Display) -> String {
    format!("{ip}")
}

fn format_ipv6_from_bytes(bytes: &[u8; 16]) -> String {
    let segments = [
        ((bytes[0] as u16) << 8) | (bytes[1] as u16),
        ((bytes[2] as u16) << 8) | (bytes[3] as u16),
        ((bytes[4] as u16) << 8) | (bytes[5] as u16),
        ((bytes[6] as u16) << 8) | (bytes[7] as u16),
        ((bytes[8] as u16) << 8) | (bytes[9] as u16),
        ((bytes[10] as u16) << 8) | (bytes[11] as u16),
        ((bytes[12] as u16) << 8) | (bytes[13] as u16),
        ((bytes[14] as u16) << 8) | (bytes[15] as u16),
    ];

    let mut best_start: isize = -1;
    let mut best_len: usize = 0;
    let mut cur_start: isize = -1;
    let mut cur_len: usize = 0;
    for (i, seg) in segments.iter().enumerate() {
        if *seg == 0 {
            if cur_start < 0 {
                cur_start = i as isize;
                cur_len = 1;
            } else {
                cur_len += 1;
            }
            if cur_len > best_len && cur_len >= 2 {
                best_start = cur_start;
                best_len = cur_len;
            }
        } else {
            cur_start = -1;
            cur_len = 0;
        }
    }

    let mut result = String::new();
    let mut i: usize = 0;
    while i < segments.len() {
        if best_start >= 0 && i == best_start as usize {
            if i == 0 {
                result.push_str("::");
            } else {
                result.push(':');
            }
            i += best_len;
            if i < segments.len() {
                result.push(':');
            }
            continue;
        }
        if i > 0 {
            result.push(':');
        }
        write!(result, "{:x}", segments[i]).unwrap();
        i += 1;
    }
    result
}

fn format_prefix_mask(family: u8, prefix_len: u8) -> String {
    match family {
        2 => {
            let mask = if prefix_len == 0 {
                0u32
            } else if prefix_len >= 32 {
                u32::MAX
            } else {
                u32::MAX << (32 - prefix_len)
            };
            format!(
                "{}.{}.{}.{}",
                (mask >> 24) & 0xff,
                (mask >> 16) & 0xff,
                (mask >> 8) & 0xff,
                mask & 0xff
            )
        }
        10 => {
            let mut mask_bytes = [0u8; 16];
            let full_bytes = (prefix_len as usize) / 8;
            let remaining_bits = (prefix_len as usize) % 8;
            for byte in mask_bytes.iter_mut().take(full_bytes.min(16)) {
                *byte = 0xff;
            }
            if remaining_bits > 0 && full_bytes < 16 {
                mask_bytes[full_bytes] = (!0u8) << (8 - remaining_bits);
            }
            format_ipv6_from_bytes(&mask_bytes)
        }
        _ => String::from("unknown"),
    }
}

fn format_mac(mac: [u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

struct NetDevIter {
    text: String,
    done: bool,
}

impl NetDevIter {
    fn new() -> Self {
        Self {
            text: String::new(),
            done: false,
        }
    }
}

impl SeqIterator for NetDevIter {
    type Item = String;

    fn rewind(&mut self) {
        self.text.clear();
        self.done = false;

        let state = knet::route_state();
        let s = &mut self.text;

        let _ = writeln!(
            s,
            "Inter-|   Receive                                                |  Transmit"
        );
        let _ = writeln!(
            s,
            " face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets \
             errs drop fifo colls carrier compressed"
        );

        for link in &state.links {
            let _ = writeln!(s, "{:6} 0         0      0    0    0     0          0         0        0         0      0    0    0     0       0          0", link.name);
        }

        if state.links.is_empty() {
            s.push_str("No network devices.\n");
        }
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.rewind();
        self.next()
    }

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        self.done = true;
        Some(core::mem::take(&mut self.text))
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> core::fmt::Result {
        buf.push_str(item);
        Ok(())
    }
}

struct RouteIter {
    lines: Vec<String>,
    next_index: usize,
}

impl RouteIter {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            next_index: 0,
        }
    }
}

impl SeqIterator for RouteIter {
    type Item = String;

    fn rewind(&mut self) {
        self.lines.clear();
        self.next_index = 0;

        let state = knet::route_state();

        for route in &state.routes {
            let dst = route
                .dst
                .map(format_ip)
                .unwrap_or_else(|| "0.0.0.0".to_string());
            let gateway = route
                .gateway
                .map(format_ip)
                .unwrap_or_else(|| "*".to_string());
            let prefsrc = route
                .prefsrc
                .map(format_ip)
                .unwrap_or_else(|| "0.0.0.0".to_string());
            let oif = route.oif;

            self.lines.push(format!(
                "{:<15} {:<15} {:<15} {:<6} {:<4} {:<4} {:<4} {:<4} {:<6} {:<6}\n",
                dst,
                gateway,
                prefsrc,
                oif,
                route.family,
                route.dst_len,
                route.table,
                route.protocol,
                route.scope,
                route.route_type,
            ));
        }

        if self.lines.is_empty() {
            self.lines.push(String::from("No routes.\n"));
        }
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.rewind();
        self.next()
    }

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.lines.get(self.next_index).cloned();
        if item.is_some() {
            self.next_index += 1;
        }
        item
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> core::fmt::Result {
        buf.push_str(item);
        Ok(())
    }
}

struct NetStatIter {
    text: String,
    done: bool,
}

impl NetStatIter {
    fn new() -> Self {
        Self {
            text: String::new(),
            done: false,
        }
    }
}

impl SeqIterator for NetStatIter {
    type Item = String;

    fn rewind(&mut self) {
        self.text.clear();
        self.done = false;

        let state = knet::route_state();
        let s = &mut self.text;

        let _ = writeln!(s, "Links: {}", state.links.len());
        let _ = writeln!(s, "Addresses: {}", state.addrs.len());
        let _ = writeln!(s, "Routes: {}", state.routes.len());
        let _ = writeln!(s, "Neighbors: {}", state.neighs.len());

        for link in &state.links {
            let _ = writeln!(s, "\nInterface: {}", link.name);
            let _ = writeln!(s, "  Index: {}", link.index);
            let _ = writeln!(s, "  MAC: {}", format_mac(link.mac));
            let _ = writeln!(s, "  MTU: {}", link.mtu);
            let _ = writeln!(s, "  Flags: {:x}", link.flags);
            let _ = writeln!(s, "  State: {}", link.operstate);
        }

        for addr in &state.addrs {
            let _ = writeln!(s, "\nAddress: {}", format_ip(addr.address));
            let _ = writeln!(s, "  Interface: {}", addr.index);
            let _ = writeln!(s, "  Prefix: {}", addr.prefix_len);
            let _ = writeln!(s, "  Family: {}", addr.family);
            let _ = writeln!(s, "  Scope: {}", addr.scope);
        }

        for route in &state.routes {
            let _ = writeln!(
                s,
                "\nRoute: {}",
                route.dst.map(format_ip).unwrap_or_else(|| "*".to_string())
            );
            let _ = writeln!(
                s,
                "  Gateway: {}",
                route
                    .gateway
                    .map(format_ip)
                    .unwrap_or_else(|| "*".to_string())
            );
            let _ = writeln!(s, "  Interface: {}", route.oif);
            let _ = writeln!(
                s,
                "  PrefSrc: {}",
                route
                    .prefsrc
                    .map(format_ip)
                    .unwrap_or_else(|| "*".to_string())
            );
        }
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.rewind();
        self.next()
    }

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        self.done = true;
        Some(core::mem::take(&mut self.text))
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> core::fmt::Result {
        buf.push_str(item);
        Ok(())
    }
}

struct DhcpIter {
    text: String,
    done: bool,
}

impl DhcpIter {
    fn new() -> Self {
        Self {
            text: String::new(),
            done: false,
        }
    }
}

impl SeqIterator for DhcpIter {
    type Item = String;

    fn rewind(&mut self) {
        self.text.clear();
        self.done = false;

        let state = knet::route_state();
        let s = &mut self.text;

        for link in &state.links {
            let _ = writeln!(s, "interface {}", link.name);

            let addr = state.addrs.iter().find(|a| a.index == link.index as u32);
            if let Some(a) = addr {
                let _ = writeln!(s, "ip_address {}", format_ip(a.address));
                let _ = writeln!(
                    s,
                    "subnet_mask {}",
                    format_prefix_mask(a.family, a.prefix_len)
                );
            } else {
                let _ = writeln!(s, "ip_address 0.0.0.0");
                let _ = writeln!(s, "subnet_mask 0.0.0.0");
            }

            let route = state
                .routes
                .iter()
                .find(|r| r.gateway.is_some() && r.oif == link.index as u32);
            if let Some(r) = route {
                let _ = writeln!(s, "router {}", format_ip(r.gateway.unwrap()));
            } else {
                let _ = writeln!(s, "router 0.0.0.0");
            }

            let _ = writeln!(s, "dhcp_server 0.0.0.0");
            let _ = writeln!(s, "dns_servers 0.0.0.0");
            let _ = writeln!(s, "lease_duration 0");
            let _ = writeln!(s, "lease_deadline 0");
            let _ = writeln!(s, "state static");
            let _ = writeln!(s);
        }

        if state.links.is_empty() {
            s.push_str("No interfaces configured.\n");
        }
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.rewind();
        self.next()
    }

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        self.done = true;
        Some(core::mem::take(&mut self.text))
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> core::fmt::Result {
        buf.push_str(item);
        Ok(())
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add("net", {
        let mut net = DirMapping::new();

        net.add(
            "dev",
            SeqFileInode::new_regular(fs.clone(), NetDevIter::new),
        );
        net.add(
            "route",
            SeqFileInode::new_regular(fs.clone(), RouteIter::new),
        );
        net.add(
            "stat",
            SeqFileInode::new_regular(fs.clone(), NetStatIter::new),
        );
        net.add("dhcp", SeqFileInode::new_regular(fs.clone(), DhcpIter::new));

        SimpleDir::new_maker(fs.clone(), Arc::new(net))
    });
}

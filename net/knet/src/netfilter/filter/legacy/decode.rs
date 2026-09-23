// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bounded decoder for the native legacy IPv4 `ipt_replace` entry array.

use alloc::{collections::BTreeMap, string::String, vec::Vec};

use super::{
    IPT_ENTRY_SIZE, IPT_ICMP_INV, IPT_INV_DSTIP, IPT_INV_PROTO, IPT_INV_SRCIP, IPT_INV_VIA_IN,
    IPT_INV_VIA_OUT, LegacyFilterReplace, LegacyReplaceError, NF_INET_FORWARD, NF_INET_LOCAL_IN,
    NF_INET_LOCAL_OUT, XT_ENTRY_HEADER_SIZE, XT_ERROR_TARGET_SIZE, XT_STANDARD_TARGET_SIZE,
    XT_TCP_INV_DSTPT, XT_TCP_INV_FLAGS, XT_TCP_INV_SRCPT, XT_UDP_INV_DSTPT, XT_UDP_INV_SRCPT,
};
use crate::{
    ip::{Ipv4Address, Ipv4Cidr},
    netfilter::filter::{
        matcher::{
            BaseMatch, ExtensionMatch, IcmpMatch, InterfaceMatch, Negated, PortRange,
            TcpFlagsMatch, TcpMatch, UdpMatch,
        },
        model::{
            Chain, ChainId, FilterHook, FilterTable, MAX_CHAINS, MAX_EXTENSION_MATCHES_PER_RULE,
            MAX_RULES, Rule, Target, Verdict,
        },
    },
};

pub(super) const MAX_ENTRIES_SIZE: usize = 4 * 1024 * 1024;
pub(super) const MAX_ENTRY_COUNT: usize = MAX_RULES + 2 * MAX_CHAINS + 1;

type DecodeResult<T> = Result<T, LegacyReplaceError>;

fn invalid<T>() -> DecodeResult<T> {
    Err(LegacyReplaceError::InvalidInput)
}

#[derive(Clone, Debug)]
enum DecodedTarget {
    Standard(i32),
    Error(String),
}

struct DecodedEntry {
    offset: usize,
    next_offset: usize,
    base: BaseMatch,
    extensions: Vec<ExtensionMatch>,
    target: DecodedTarget,
}

pub(super) fn decode_table(request: &LegacyFilterReplace<'_>) -> DecodeResult<FilterTable> {
    if request.valid_hooks != 0x0e
        || request.entries.len() > MAX_ENTRIES_SIZE
        || request.num_entries == 0
        || request.num_entries as usize > MAX_ENTRY_COUNT
    {
        return invalid();
    }

    let mut entries = Vec::new();
    let mut offset = 0;
    while offset < request.entries.len() {
        if entries.len() == MAX_ENTRY_COUNT {
            return invalid();
        }
        let entry = decode_entry(&request.entries[offset..], offset)?;
        offset += entry.next_offset;
        entries.push(entry);
    }
    if entries.len() != request.num_entries as usize
        || !matches!(entries.last().map(|entry| &entry.target), Some(DecodedTarget::Error(name)) if name == "ERROR")
    {
        return invalid();
    }

    let mut positions = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        positions.insert(entry.offset, index);
    }
    let terminal_index = entries.len() - 1;
    let mut starts = BTreeMap::new();
    for (hook_number, hook) in [
        (NF_INET_LOCAL_IN, FilterHook::Input),
        (NF_INET_FORWARD, FilterHook::Forward),
        (NF_INET_LOCAL_OUT, FilterHook::Output),
    ] {
        let start = *positions
            .get(&(request.hook_entry[hook_number] as usize))
            .ok_or(LegacyReplaceError::InvalidInput)?;
        let policy = *positions
            .get(&(request.underflow[hook_number] as usize))
            .ok_or(LegacyReplaceError::InvalidInput)?;
        if start >= terminal_index || starts.insert(start, (hook, policy)).is_some() {
            return invalid();
        }
    }

    let mut boundaries = starts.keys().copied().collect::<Vec<_>>();
    let mut user_destinations = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate().take(terminal_index) {
        if let DecodedTarget::Error(name) = &entry.target {
            if name == "ERROR" || starts.contains_key(&index) || index + 1 >= terminal_index {
                return invalid();
            }
            boundaries.push(index);
            user_destinations.insert(entry.offset + entry.next_offset, index);
        }
    }
    boundaries.sort_unstable();
    if boundaries.first() != Some(&0) || boundaries.len() > MAX_CHAINS {
        return invalid();
    }

    let mut chains = Vec::with_capacity(boundaries.len());
    for (chain_index, &start) in boundaries.iter().enumerate() {
        let end = boundaries
            .get(chain_index + 1)
            .copied()
            .unwrap_or(terminal_index);
        if start + 1 > end {
            return invalid();
        }
        let footer = &entries[end - 1];
        if !is_unconditional(footer) {
            return invalid();
        }
        let (kind_name, hook) = if let Some(&(hook, policy)) = starts.get(&start) {
            if policy != end - 1 || matches!(&entries[start].target, DecodedTarget::Error(_)) {
                return invalid();
            }
            (None, Some(hook))
        } else if let DecodedTarget::Error(name) = &entries[start].target {
            (Some(name.clone()), None)
        } else {
            return invalid();
        };
        let policy = match (&footer.target, hook) {
            (DecodedTarget::Standard(-2), Some(_)) => Some(Verdict::Accept),
            (DecodedTarget::Standard(-1), Some(_)) => Some(Verdict::Drop),
            (DecodedTarget::Standard(-5), None) => None,
            _ => return invalid(),
        };

        let rule_start = start + usize::from(kind_name.is_some());
        let mut rules = Vec::with_capacity(end - rule_start - 1);
        for entry in &entries[rule_start..end - 1] {
            if matches!(&entry.target, DecodedTarget::Error(_)) {
                return invalid();
            }
            let target = decode_target(entry, &user_destinations, &boundaries)?;
            rules.push(
                Rule::new(entry.base.clone(), entry.extensions.clone(), target)
                    .map_err(|_| LegacyReplaceError::InvalidInput)?,
            );
        }
        chains.push(match (hook, kind_name) {
            (Some(hook), None) => Chain::built_in(hook, policy.unwrap(), rules),
            (None, Some(name)) => Chain::user_defined(name, rules),
            _ => return invalid(),
        });
    }
    FilterTable::new(chains).map_err(|_| LegacyReplaceError::InvalidInput)
}

fn is_unconditional(entry: &DecodedEntry) -> bool {
    entry.base == BaseMatch::default() && entry.extensions.is_empty()
}

fn decode_target(
    entry: &DecodedEntry,
    user_destinations: &BTreeMap<usize, usize>,
    boundaries: &[usize],
) -> DecodeResult<Target> {
    let DecodedTarget::Standard(verdict) = &entry.target else {
        return invalid();
    };
    match *verdict {
        -2 => Ok(Target::Verdict(Verdict::Accept)),
        -1 => Ok(Target::Verdict(Verdict::Drop)),
        -5 => Ok(Target::Return),
        value if value >= 0 && value as usize == entry.offset + entry.next_offset => {
            Ok(Target::Continue)
        }
        value if value >= 0 => {
            let marker = user_destinations
                .get(&(value as usize))
                .ok_or(LegacyReplaceError::InvalidInput)?;
            let chain = boundaries
                .binary_search(marker)
                .map_err(|_| LegacyReplaceError::InvalidInput)?;
            Ok(Target::Jump(ChainId::from_index(chain)))
        }
        _ => invalid(),
    }
}

fn decode_entry(bytes: &[u8], offset: usize) -> DecodeResult<DecodedEntry> {
    if bytes.len() < IPT_ENTRY_SIZE + XT_ENTRY_HEADER_SIZE {
        return invalid();
    }
    let target_offset = read_u16(bytes, 88) as usize;
    let next_offset = read_u16(bytes, 90) as usize;
    if target_offset < IPT_ENTRY_SIZE
        || !target_offset.is_multiple_of(8)
        || !next_offset.is_multiple_of(8)
        || next_offset > bytes.len()
        || target_offset + XT_ENTRY_HEADER_SIZE > next_offset
    {
        return invalid();
    }
    let base = decode_base(&bytes[..IPT_ENTRY_SIZE])?;
    let mut extensions = Vec::new();
    let mut match_offset = IPT_ENTRY_SIZE;
    while match_offset < target_offset {
        if extensions.len() == MAX_EXTENSION_MATCHES_PER_RULE
            || match_offset + XT_ENTRY_HEADER_SIZE > target_offset
        {
            return invalid();
        }
        let size = read_u16(bytes, match_offset) as usize;
        if size < XT_ENTRY_HEADER_SIZE
            || !size.is_multiple_of(8)
            || match_offset + size > target_offset
        {
            return invalid();
        }
        extensions.push(decode_match(&bytes[match_offset..match_offset + size])?);
        match_offset += size;
    }
    let target = &bytes[target_offset..next_offset];
    let size = read_u16(target, 0) as usize;
    if size != target.len() || target[31] != 0 {
        return invalid();
    }
    let target = if target[2] == 0 {
        if size != XT_STANDARD_TARGET_SIZE {
            return invalid();
        }
        DecodedTarget::Standard(read_i32(target, XT_ENTRY_HEADER_SIZE))
    } else if &target[2..8] == b"ERROR\0" {
        if size != XT_ERROR_TARGET_SIZE
            || target_offset != IPT_ENTRY_SIZE
            || base != BaseMatch::default()
        {
            return invalid();
        }
        DecodedTarget::Error(read_name(
            &target[XT_ENTRY_HEADER_SIZE..XT_ENTRY_HEADER_SIZE + 30],
        )?)
    } else {
        return invalid();
    };
    Ok(DecodedEntry {
        offset,
        next_offset,
        base,
        extensions,
        target,
    })
}

fn decode_base(ip: &[u8]) -> DecodeResult<BaseMatch> {
    let inverse = ip[83];
    if ip[82] != 0
        || inverse
            & !(IPT_INV_VIA_IN | IPT_INV_VIA_OUT | IPT_INV_SRCIP | IPT_INV_DSTIP | IPT_INV_PROTO)
            != 0
    {
        return invalid();
    }
    let proto = read_u16(ip, 80);
    if proto > u8::MAX as u16 {
        return invalid();
    }
    Ok(BaseMatch {
        source: decode_cidr(&ip[0..4], &ip[8..12], inverse & IPT_INV_SRCIP != 0)?,
        destination: decode_cidr(&ip[4..8], &ip[12..16], inverse & IPT_INV_DSTIP != 0)?,
        input_interface: decode_interface(&ip[16..32], &ip[48..64], inverse & IPT_INV_VIA_IN != 0)?,
        output_interface: decode_interface(
            &ip[32..48],
            &ip[64..80],
            inverse & IPT_INV_VIA_OUT != 0,
        )?,
        protocol: (proto != 0).then_some(Negated::new(proto as u8, inverse & IPT_INV_PROTO != 0)),
    })
}

fn decode_cidr(
    address: &[u8],
    mask: &[u8],
    inverse: bool,
) -> DecodeResult<Option<Negated<Ipv4Cidr>>> {
    let bits = u32::from_be_bytes(mask.try_into().unwrap());
    let prefix = bits.leading_ones() as u8;
    if bits != u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0) {
        return invalid();
    }
    let addr = u32::from_be_bytes(address.try_into().unwrap());
    if addr & !bits != 0 {
        return invalid();
    }
    if prefix == 0 && !inverse {
        return Ok(None);
    }
    let octets: [u8; 4] = address.try_into().unwrap();
    let cidr = Ipv4Cidr::new(
        Ipv4Address::new(octets[0], octets[1], octets[2], octets[3]),
        prefix,
    )
    .ok_or(LegacyReplaceError::InvalidInput)?;
    Ok(Some(Negated::new(cidr, inverse)))
}

fn decode_interface(
    name: &[u8],
    mask: &[u8],
    inverse: bool,
) -> DecodeResult<Option<Negated<InterfaceMatch>>> {
    let prefix = mask.iter().take_while(|&&byte| byte == 0xff).count();
    if mask[prefix..].iter().any(|&byte| byte != 0) {
        return invalid();
    }
    if prefix == 0 {
        return if inverse { invalid() } else { Ok(None) };
    }
    let text = read_name(name)?;
    let bytes = text.as_bytes();
    let interface = if prefix == bytes.len() + 1 {
        InterfaceMatch::exact(text)
    } else if prefix + 1 == bytes.len() && text.ends_with('+') {
        InterfaceMatch::prefix(String::from(&text[..text.len() - 1]))
    } else if prefix == bytes.len() {
        InterfaceMatch::prefix(text)
    } else {
        return invalid();
    };
    Ok(Some(Negated::new(
        interface.ok_or(LegacyReplaceError::InvalidInput)?,
        inverse,
    )))
}

fn decode_match(extension: &[u8]) -> DecodeResult<ExtensionMatch> {
    if extension[31] != 0 {
        return invalid();
    }
    let name = read_name(&extension[2..31])?;
    let data = &extension[XT_ENTRY_HEADER_SIZE..];
    match name.as_str() {
        "tcp" if extension.len() == 48 => {
            if data[8] != 0
                || data[11] & !(XT_TCP_INV_SRCPT | XT_TCP_INV_DSTPT | XT_TCP_INV_FLAGS) != 0
            {
                return invalid();
            }
            let mut tcp = TcpMatch::new();
            tcp = tcp.with_source_port(decode_ports(data, 0, data[11] & XT_TCP_INV_SRCPT != 0)?);
            tcp =
                tcp.with_destination_port(decode_ports(data, 4, data[11] & XT_TCP_INV_DSTPT != 0)?);
            let flags =
                TcpFlagsMatch::new(data[9], data[10]).ok_or(LegacyReplaceError::InvalidInput)?;
            if flags.mask() != 0 || data[11] & XT_TCP_INV_FLAGS != 0 {
                tcp = tcp.with_flags(Negated::new(flags, data[11] & XT_TCP_INV_FLAGS != 0));
            }
            Ok(ExtensionMatch::Tcp(tcp))
        }
        "udp" if extension.len() == 48 => {
            if data[8] & !(XT_UDP_INV_SRCPT | XT_UDP_INV_DSTPT) != 0 {
                return invalid();
            }
            let udp = UdpMatch::new()
                .with_source_port(decode_ports(data, 0, data[8] & XT_UDP_INV_SRCPT != 0)?)
                .with_destination_port(decode_ports(data, 4, data[8] & XT_UDP_INV_DSTPT != 0)?);
            Ok(ExtensionMatch::Udp(udp))
        }
        "icmp" if extension.len() == 40 => {
            if data[3] & !IPT_ICMP_INV != 0 {
                return invalid();
            }
            let icmp_type = (data[0] != u8::MAX).then_some(data[0]);
            let icmp = IcmpMatch::new(icmp_type, data[1], data[2])
                .ok_or(LegacyReplaceError::InvalidInput)?
                .with_inversion(data[3] & IPT_ICMP_INV != 0);
            Ok(ExtensionMatch::Icmp(icmp))
        }
        _ => invalid(),
    }
}

fn decode_ports(data: &[u8], offset: usize, inverse: bool) -> DecodeResult<Negated<PortRange>> {
    let range = PortRange::new(read_u16(data, offset), read_u16(data, offset + 2))
        .ok_or(LegacyReplaceError::InvalidInput)?;
    Ok(Negated::new(range, inverse))
}

fn read_name(bytes: &[u8]) -> DecodeResult<String> {
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .ok_or(LegacyReplaceError::InvalidInput)?;
    if bytes[end + 1..].iter().any(|&byte| byte != 0) {
        return invalid();
    }
    core::str::from_utf8(&bytes[..end])
        .map(String::from)
        .map_err(|_| LegacyReplaceError::InvalidInput)
}

fn read_u16(input: &[u8], offset: usize) -> u16 {
    u16::from_ne_bytes(input[offset..offset + 2].try_into().unwrap())
}

fn read_i32(input: &[u8], offset: usize) -> i32 {
    i32::from_ne_bytes(input[offset..offset + 4].try_into().unwrap())
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn interface_prefix_accepts_legacy_plus_syntax() {
        let mut name = [0; 16];
        name[..4].copy_from_slice(b"eth+");
        let mut mask = [0; 16];
        mask[..3].fill(0xff);
        let condition = decode_interface(&name, &mask, false).unwrap().unwrap();
        assert_eq!(condition.value().name(), "eth");
        assert_eq!(condition.value().is_prefix(), true);
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Legacy IPv4 iptables serialization.

use alloc::{vec, vec::Vec};
use core::mem::{align_of, size_of};

use klazy::Lazy;

use super::{
    matcher::{ExtensionMatch, InterfaceMatch, Negated, PortRange},
    model::{ChainKind, FilterHook, FilterTable, Rule, Target, Verdict},
};

const NF_INET_NUMHOOKS: usize = 5;
const NF_INET_LOCAL_IN: usize = 1;
const NF_INET_FORWARD: usize = 2;
const NF_INET_LOCAL_OUT: usize = 3;

const IPT_ENTRY_SIZE: usize = 112;
const IPT_ENTRY_TARGET_OFFSET_OFFSET: usize = 88;
const IPT_ENTRY_NEXT_OFFSET_OFFSET: usize = 90;

const XT_ENTRY_HEADER_SIZE: usize = 32;
const XT_EXTENSION_NAME_OFFSET: usize = 2;
const XT_ERROR_NAME_SIZE: usize = 30;
const XT_STANDARD_VERDICT_OFFSET: usize = XT_ENTRY_HEADER_SIZE;

const XT_TCP_SIZE: usize = 12;
const XT_UDP_SIZE: usize = 10;
const IPT_ICMP_SIZE: usize = 4;

const IPT_INV_VIA_IN: u8 = 0x01;
const IPT_INV_VIA_OUT: u8 = 0x02;
const IPT_INV_SRCIP: u8 = 0x08;
const IPT_INV_DSTIP: u8 = 0x10;
const IPT_INV_PROTO: u8 = 0x40;

const XT_TCP_INV_SRCPT: u8 = 0x01;
const XT_TCP_INV_DSTPT: u8 = 0x02;
const XT_TCP_INV_FLAGS: u8 = 0x04;
const XT_UDP_INV_SRCPT: u8 = 0x01;
const XT_UDP_INV_DSTPT: u8 = 0x02;
const IPT_ICMP_INV: u8 = 0x01;

const NF_DROP: i32 = 0;
const NF_ACCEPT: i32 = 1;
const NF_REPEAT: i32 = 4;
const XT_RETURN: i32 = -NF_REPEAT - 1;

const XT_ALIGN: usize = align_of::<u64>();
const XT_STANDARD_TARGET_SIZE: usize = align_up(XT_ENTRY_HEADER_SIZE + size_of::<i32>());
const XT_ERROR_TARGET_SIZE: usize = align_up(XT_ENTRY_HEADER_SIZE + XT_ERROR_NAME_SIZE);
const STANDARD_ENTRY_SIZE: usize = IPT_ENTRY_SIZE + XT_STANDARD_TARGET_SIZE;
const ERROR_ENTRY_SIZE: usize = IPT_ENTRY_SIZE + XT_ERROR_TARGET_SIZE;

static FILTER_TABLE: Lazy<FilterTable> = Lazy::new(FilterTable::accept_all);

/// Owned legacy representation of one IPv4 `filter` table snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyFilterTable {
    valid_hooks: u32,
    hook_entry: [u32; NF_INET_NUMHOOKS],
    underflow: [u32; NF_INET_NUMHOOKS],
    num_entries: u32,
    entries: Vec<u8>,
}

impl LegacyFilterTable {
    /// Returns the netfilter hook mask supported by the table.
    pub const fn valid_hooks(&self) -> u32 {
        self.valid_hooks
    }

    /// Returns each hook's byte offset into the legacy entry array.
    pub const fn hook_entry(&self) -> &[u32; NF_INET_NUMHOOKS] {
        &self.hook_entry
    }

    /// Returns each built-in chain policy entry's byte offset.
    pub const fn underflow(&self) -> &[u32; NF_INET_NUMHOOKS] {
        &self.underflow
    }

    /// Returns the number of legacy entries, including chain markers.
    pub const fn num_entries(&self) -> u32 {
        self.num_entries
    }

    /// Returns the serialized `ipt_entry` array.
    pub fn entries(&self) -> &[u8] {
        &self.entries
    }
}

pub(crate) fn snapshot() -> LegacyFilterTable {
    encode_table(&FILTER_TABLE)
}

#[derive(Clone, Debug)]
struct ChainLayout {
    head_offset: usize,
    foot_offset: usize,
    rule_offsets: Vec<usize>,
}

fn encode_table(table: &FilterTable) -> LegacyFilterTable {
    let (layouts, size, num_entries) = layout_table(table);
    let mut entries = vec![0; size];
    let mut hook_entry = [0; NF_INET_NUMHOOKS];
    let mut underflow = [0; NF_INET_NUMHOOKS];

    for (chain_index, chain) in table.chains().iter().enumerate() {
        let layout = &layouts[chain_index];
        match chain.kind() {
            ChainKind::BuiltIn { hook, .. } => {
                let hook = linux_hook(*hook);
                hook_entry[hook] = to_u32(layout.head_offset);
                underflow[hook] = to_u32(layout.foot_offset);
            }
            ChainKind::UserDefined { name } => {
                encode_error_entry(
                    &mut entries[layout.head_offset..layout.head_offset + ERROR_ENTRY_SIZE],
                    name.as_bytes(),
                );
            }
        }

        for (rule, offset) in chain.rules().iter().zip(&layout.rule_offsets) {
            let size = rule_size(rule);
            encode_rule(
                &mut entries[*offset..*offset + size],
                *offset,
                rule,
                &layouts,
            );
        }

        let policy = match chain.kind() {
            ChainKind::BuiltIn { policy, .. } => verdict_code(*policy),
            ChainKind::UserDefined { .. } => XT_RETURN,
        };
        encode_standard_entry(
            &mut entries[layout.foot_offset..layout.foot_offset + STANDARD_ENTRY_SIZE],
            policy,
        );
    }

    encode_error_entry(&mut entries[size - ERROR_ENTRY_SIZE..], b"ERROR");

    LegacyFilterTable {
        valid_hooks: (1 << NF_INET_LOCAL_IN) | (1 << NF_INET_FORWARD) | (1 << NF_INET_LOCAL_OUT),
        hook_entry,
        underflow,
        num_entries: to_u32(num_entries),
        entries,
    }
}

fn layout_table(table: &FilterTable) -> (Vec<ChainLayout>, usize, usize) {
    let mut layouts = Vec::with_capacity(table.chains().len());
    let mut offset = 0usize;
    let mut num_entries = 0usize;

    for chain in table.chains() {
        let head_offset = offset;
        if matches!(chain.kind(), ChainKind::UserDefined { .. }) {
            offset += ERROR_ENTRY_SIZE;
            num_entries += 1;
        }

        let mut rule_offsets = Vec::with_capacity(chain.rules().len());
        for rule in chain.rules() {
            rule_offsets.push(offset);
            offset += rule_size(rule);
            num_entries += 1;
        }

        let foot_offset = offset;
        offset += STANDARD_ENTRY_SIZE;
        num_entries += 1;
        layouts.push(ChainLayout {
            head_offset,
            foot_offset,
            rule_offsets,
        });
    }

    (layouts, offset + ERROR_ENTRY_SIZE, num_entries + 1)
}

fn rule_size(rule: &Rule) -> usize {
    IPT_ENTRY_SIZE
        + rule.extensions().iter().map(extension_size).sum::<usize>()
        + XT_STANDARD_TARGET_SIZE
}

fn extension_size(extension: &ExtensionMatch) -> usize {
    match extension {
        ExtensionMatch::Tcp(_) => align_up(XT_ENTRY_HEADER_SIZE + XT_TCP_SIZE),
        ExtensionMatch::Udp(_) => align_up(XT_ENTRY_HEADER_SIZE + XT_UDP_SIZE),
        ExtensionMatch::Icmp(_) => align_up(XT_ENTRY_HEADER_SIZE + IPT_ICMP_SIZE),
    }
}

fn encode_rule(entry: &mut [u8], entry_offset: usize, rule: &Rule, layouts: &[ChainLayout]) {
    encode_base_match(&mut entry[..84], rule);

    let mut offset = IPT_ENTRY_SIZE;
    for extension in rule.extensions() {
        let size = extension_size(extension);
        encode_extension(&mut entry[offset..offset + size], extension);
        offset += size;
    }

    write_u16(entry, IPT_ENTRY_TARGET_OFFSET_OFFSET, to_u16(offset));
    write_u16(entry, IPT_ENTRY_NEXT_OFFSET_OFFSET, to_u16(entry.len()));
    let verdict = match rule.target() {
        Target::Verdict(verdict) => verdict_code(verdict),
        Target::Continue => to_i32(entry_offset + entry.len()),
        Target::Return => XT_RETURN,
        Target::Jump(chain) => to_i32(layouts[chain.index()].head_offset + ERROR_ENTRY_SIZE),
    };
    encode_standard_target(&mut entry[offset..], verdict);
}

fn encode_base_match(ip: &mut [u8], rule: &Rule) {
    let base = rule.base();
    let mut inverse = 0u8;

    if let Some(source) = &base.source {
        let cidr = source.value().network();
        ip[0..4].copy_from_slice(&cidr.address().octets());
        ip[8..12].copy_from_slice(&prefix_mask(cidr.prefix_len()));
        inverse |= inversion(source, IPT_INV_SRCIP);
    }
    if let Some(destination) = &base.destination {
        let cidr = destination.value().network();
        ip[4..8].copy_from_slice(&cidr.address().octets());
        ip[12..16].copy_from_slice(&prefix_mask(cidr.prefix_len()));
        inverse |= inversion(destination, IPT_INV_DSTIP);
    }
    if let Some(interface) = &base.input_interface {
        encode_interface(ip, 16, 48, interface.value());
        inverse |= inversion(interface, IPT_INV_VIA_IN);
    }
    if let Some(interface) = &base.output_interface {
        encode_interface(ip, 32, 64, interface.value());
        inverse |= inversion(interface, IPT_INV_VIA_OUT);
    }
    if let Some(protocol) = &base.protocol {
        write_u16(ip, 80, u16::from(*protocol.value()));
        inverse |= inversion(protocol, IPT_INV_PROTO);
    }
    ip[83] = inverse;
}

fn encode_interface(
    ip: &mut [u8],
    name_offset: usize,
    mask_offset: usize,
    interface: &InterfaceMatch,
) {
    let bytes = interface.name().as_bytes();
    ip[name_offset..name_offset + bytes.len()].copy_from_slice(bytes);
    let mask_len = if interface.is_prefix() {
        bytes.len()
    } else {
        bytes.len() + 1
    };
    ip[mask_offset..mask_offset + mask_len].fill(u8::MAX);
}

fn encode_extension(output: &mut [u8], extension: &ExtensionMatch) {
    match extension {
        ExtensionMatch::Tcp(tcp) => {
            encode_extension_header(output, b"tcp");
            let data = &mut output[XT_ENTRY_HEADER_SIZE..];
            let mut inverse = 0;
            encode_ports(
                data,
                0,
                tcp.source_port_range(),
                XT_TCP_INV_SRCPT,
                &mut inverse,
            );
            encode_ports(
                data,
                4,
                tcp.destination_port_range(),
                XT_TCP_INV_DSTPT,
                &mut inverse,
            );
            if let Some(flags) = tcp.flags_match() {
                data[9] = flags.value().mask();
                data[10] = flags.value().expected();
                inverse |= inversion(&flags, XT_TCP_INV_FLAGS);
            }
            data[11] = inverse;
        }
        ExtensionMatch::Udp(udp) => {
            encode_extension_header(output, b"udp");
            let data = &mut output[XT_ENTRY_HEADER_SIZE..];
            let mut inverse = 0;
            encode_ports(
                data,
                0,
                udp.source_port_range(),
                XT_UDP_INV_SRCPT,
                &mut inverse,
            );
            encode_ports(
                data,
                4,
                udp.destination_port_range(),
                XT_UDP_INV_DSTPT,
                &mut inverse,
            );
            data[8] = inverse;
        }
        ExtensionMatch::Icmp(icmp) => {
            encode_extension_header(output, b"icmp");
            let data = &mut output[XT_ENTRY_HEADER_SIZE..];
            data[0] = icmp.icmp_type().unwrap_or(u8::MAX);
            data[1] = icmp.code_start();
            data[2] = icmp.code_end();
            data[3] = u8::from(icmp.is_inverted()) * IPT_ICMP_INV;
        }
    }
}

fn encode_ports(
    data: &mut [u8],
    offset: usize,
    range: Option<Negated<PortRange>>,
    inverse_bit: u8,
    inverse: &mut u8,
) {
    let (start, end) = range
        .map(|range| {
            *inverse |= inversion(&range, inverse_bit);
            (range.value().start(), range.value().end())
        })
        .unwrap_or((0, u16::MAX));
    write_u16(data, offset, start);
    write_u16(data, offset + 2, end);
}

fn encode_standard_entry(entry: &mut [u8], verdict: i32) {
    write_u16(
        entry,
        IPT_ENTRY_TARGET_OFFSET_OFFSET,
        to_u16(IPT_ENTRY_SIZE),
    );
    write_u16(entry, IPT_ENTRY_NEXT_OFFSET_OFFSET, to_u16(entry.len()));
    encode_standard_target(&mut entry[IPT_ENTRY_SIZE..], verdict);
}

fn encode_standard_target(target: &mut [u8], verdict: i32) {
    write_u16(target, 0, to_u16(XT_STANDARD_TARGET_SIZE));
    write_i32(target, XT_STANDARD_VERDICT_OFFSET, verdict);
}

fn encode_error_entry(entry: &mut [u8], error_name: &[u8]) {
    write_u16(
        entry,
        IPT_ENTRY_TARGET_OFFSET_OFFSET,
        to_u16(IPT_ENTRY_SIZE),
    );
    write_u16(entry, IPT_ENTRY_NEXT_OFFSET_OFFSET, to_u16(entry.len()));
    let target = &mut entry[IPT_ENTRY_SIZE..];
    write_u16(target, 0, to_u16(XT_ERROR_TARGET_SIZE));
    target[XT_EXTENSION_NAME_OFFSET..XT_EXTENSION_NAME_OFFSET + 5].copy_from_slice(b"ERROR");
    target[XT_ENTRY_HEADER_SIZE..XT_ENTRY_HEADER_SIZE + error_name.len()]
        .copy_from_slice(error_name);
}

fn encode_extension_header(extension: &mut [u8], name: &[u8]) {
    write_u16(extension, 0, to_u16(extension.len()));
    extension[XT_EXTENSION_NAME_OFFSET..XT_EXTENSION_NAME_OFFSET + name.len()]
        .copy_from_slice(name);
}

const fn linux_hook(hook: FilterHook) -> usize {
    match hook {
        FilterHook::Input => NF_INET_LOCAL_IN,
        FilterHook::Forward => NF_INET_FORWARD,
        FilterHook::Output => NF_INET_LOCAL_OUT,
    }
}

const fn verdict_code(verdict: Verdict) -> i32 {
    match verdict {
        Verdict::Accept => -NF_ACCEPT - 1,
        Verdict::Drop => -NF_DROP - 1,
    }
}

fn inversion<T>(condition: &Negated<T>, bit: u8) -> u8 {
    u8::from(condition.is_inverted()) * bit
}

fn prefix_mask(prefix_len: u8) -> [u8; 4] {
    match prefix_len {
        0 => [0; 4],
        1..=32 => (u32::MAX << (32 - prefix_len)).to_be_bytes(),
        _ => unreachable!("validated IPv4 prefix length must not exceed 32"),
    }
}

const fn align_up(size: usize) -> usize {
    (size + XT_ALIGN - 1) & !(XT_ALIGN - 1)
}

fn write_u16(output: &mut [u8], offset: usize, value: u16) {
    output[offset..offset + 2].copy_from_slice(&value.to_ne_bytes());
}

fn write_i32(output: &mut [u8], offset: usize, value: i32) {
    output[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

fn to_u16(value: usize) -> u16 {
    u16::try_from(value).expect("validated legacy entry size must fit u16")
}

fn to_u32(value: usize) -> u32 {
    u32::try_from(value).expect("validated legacy table size must fit u32")
}

fn to_i32(value: usize) -> i32 {
    i32::try_from(value).expect("validated legacy jump offset must fit i32")
}

#[cfg(unittest)]
mod tests {
    use alloc::{string::ToString, vec};

    use unittest::{assert_eq, def_test};

    use super::*;
    use crate::{
        ipv4::PROTOCOL_TCP,
        netfilter::filter::{
            matcher::{BaseMatch, TcpMatch},
            model::{Chain, ChainId},
        },
    };

    fn read_u16(input: &[u8], offset: usize) -> u16 {
        u16::from_ne_bytes(input[offset..offset + 2].try_into().unwrap())
    }

    fn read_i32(input: &[u8], offset: usize) -> i32 {
        i32::from_ne_bytes(input[offset..offset + 4].try_into().unwrap())
    }

    #[def_test]
    fn default_filter_table_matches_linux_initial_layout() {
        let snapshot = encode_table(&FilterTable::accept_all());

        assert_eq!(snapshot.valid_hooks(), 0x0e);
        assert_eq!(snapshot.hook_entry(), &[0, 0, 152, 304, 0]);
        assert_eq!(snapshot.underflow(), &[0, 0, 152, 304, 0]);
        assert_eq!(snapshot.num_entries(), 4);
        assert_eq!(snapshot.entries().len(), 632);

        for offset in [0, 152, 304] {
            assert_eq!(read_u16(snapshot.entries(), offset + 88), 112);
            assert_eq!(read_u16(snapshot.entries(), offset + 90), 152);
            assert_eq!(read_i32(snapshot.entries(), offset + 144), -2);
        }
        assert_eq!(&snapshot.entries()[570..575], b"ERROR");
        assert_eq!(&snapshot.entries()[600..605], b"ERROR");
    }

    #[def_test]
    fn custom_chain_jump_uses_first_rule_after_chain_marker() {
        let jump = Rule::new(
            BaseMatch::default(),
            Vec::new(),
            Target::Jump(ChainId::from_index(3)),
        )
        .unwrap();
        let tcp = Rule::new(
            BaseMatch {
                protocol: Some(Negated::new(PROTOCOL_TCP, false)),
                ..Default::default()
            },
            vec![ExtensionMatch::Tcp(TcpMatch::new().with_destination_port(
                Negated::new(PortRange::new(80, 80).unwrap(), false),
            ))],
            Target::Verdict(Verdict::Drop),
        )
        .unwrap();
        let table = FilterTable::new(vec![
            Chain::built_in(FilterHook::Input, Verdict::Accept, vec![jump]),
            Chain::built_in(FilterHook::Forward, Verdict::Accept, Vec::new()),
            Chain::built_in(FilterHook::Output, Verdict::Accept, Vec::new()),
            Chain::user_defined("WEB".to_string(), vec![tcp]),
        ])
        .unwrap();

        let snapshot = encode_table(&table);
        let custom_head = 608usize;
        assert_eq!(read_i32(snapshot.entries(), 144), to_i32(custom_head + 176));
        assert_eq!(
            &snapshot.entries()[custom_head + 114..custom_head + 119],
            b"ERROR"
        );
        assert_eq!(
            &snapshot.entries()[custom_head + 144..custom_head + 147],
            b"WEB"
        );
        let tcp_rule = custom_head + 176;
        assert_eq!(&snapshot.entries()[tcp_rule + 114..tcp_rule + 117], b"tcp");
        assert_eq!(read_u16(snapshot.entries(), tcp_rule + 148), 80);
    }
}

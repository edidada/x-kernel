// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Native legacy IPv4 iptables state and serialization.

use alloc::{sync::Arc, vec, vec::Vec};
use core::{
    mem::{align_of, size_of},
    sync::atomic::{AtomicU64, Ordering},
};

use klazy::Lazy;
use ksync::RwLock;

use super::{
    matcher::{ExtensionMatch, FilterPacket, InterfaceMatch, Negated, PortRange},
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

mod decode;

/// Largest native entry array accepted by the legacy decoder.
pub const MAX_LEGACY_FILTER_ENTRIES_SIZE: usize = decode::MAX_ENTRIES_SIZE;
/// Largest number of native entries accepted by the legacy decoder.
pub const MAX_LEGACY_FILTER_ENTRY_COUNT: usize = decode::MAX_ENTRY_COUNT;

static FILTER_TABLE: Lazy<RwLock<Arc<FilterState>>> =
    Lazy::new(|| RwLock::new(Arc::new(FilterState::new(FilterTable::accept_all()))));

struct FilterState {
    table: FilterTable,
    exported: LegacyFilterTable,
    counters: Vec<EntryCounter>,
    counter_offsets: Vec<usize>,
    chain_counters: Vec<ChainCounterIndices>,
}

struct EntryCounter {
    packets: AtomicU64,
    bytes: AtomicU64,
}

struct ChainCounterIndices {
    rules: Vec<usize>,
    footer: usize,
}

/// Packet and byte totals for one native legacy table entry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LegacyCounter {
    /// Number of packets that matched the entry.
    pub packets: u64,
    /// Sum of the matching packets' IPv4 lengths.
    pub bytes: u64,
}

impl EntryCounter {
    fn new() -> Self {
        Self {
            packets: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> LegacyCounter {
        LegacyCounter {
            packets: self.packets.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }

    fn add(&self, counter: LegacyCounter) {
        self.packets.fetch_add(counter.packets, Ordering::Relaxed);
        self.bytes.fetch_add(counter.bytes, Ordering::Relaxed);
    }
}

impl FilterState {
    fn new(table: FilterTable) -> Self {
        let exported = encode_table(&table);
        let (layouts, ..) = layout_table(&table);
        let mut counter_offsets = Vec::with_capacity(exported.num_entries as usize);
        let mut chain_counters = Vec::with_capacity(layouts.len());
        for (chain, layout) in table.chains().iter().zip(&layouts) {
            if matches!(chain.kind(), ChainKind::UserDefined { .. }) {
                counter_offsets.push(layout.head_offset);
            }
            let mut rules = Vec::with_capacity(layout.rule_offsets.len());
            for offset in &layout.rule_offsets {
                rules.push(counter_offsets.len());
                counter_offsets.push(*offset);
            }
            let footer = counter_offsets.len();
            counter_offsets.push(layout.foot_offset);
            chain_counters.push(ChainCounterIndices { rules, footer });
        }
        counter_offsets.push(exported.entries.len() - ERROR_ENTRY_SIZE);
        debug_assert_eq!(counter_offsets.len(), exported.num_entries as usize);
        let counters = (0..counter_offsets.len())
            .map(|_| EntryCounter::new())
            .collect();
        Self {
            table,
            exported,
            counters,
            counter_offsets,
            chain_counters,
        }
    }

    fn counter_snapshot(&self) -> Vec<LegacyCounter> {
        self.counters.iter().map(EntryCounter::snapshot).collect()
    }

    fn export_snapshot(&self) -> LegacyFilterTable {
        let mut exported = self.exported.clone();
        for (offset, counter) in self.counter_offsets.iter().zip(&self.counters) {
            let value = counter.snapshot();
            exported.entries[offset + 96..offset + 104]
                .copy_from_slice(&value.packets.to_ne_bytes());
            exported.entries[offset + 104..offset + 112]
                .copy_from_slice(&value.bytes.to_ne_bytes());
        }
        exported
    }

    fn evaluate(&self, hook: FilterHook, packet: &FilterPacket<'_>) -> Verdict {
        self.table
            .evaluate_with_counter(hook, packet, |chain_id, rule_index| {
                let chain = &self.chain_counters[chain_id.index()];
                let index = rule_index.map_or(chain.footer, |rule| chain.rules[rule]);
                self.counters[index].add(LegacyCounter {
                    packets: 1,
                    bytes: packet.packet_len() as u64,
                });
            })
    }
}

/// Metadata copied from the native `ipt_replace` ABI after syscall validation.
pub struct LegacyFilterReplace<'a> {
    /// Hook mask; only INPUT, FORWARD, and OUTPUT are supported.
    pub valid_hooks: u32,
    /// Entry offsets for the five IPv4 hooks.
    pub hook_entry: [u32; NF_INET_NUMHOOKS],
    /// Policy-entry offsets for the five IPv4 hooks.
    pub underflow: [u32; NF_INET_NUMHOOKS],
    /// Number of `ipt_entry` records in `entries`.
    pub num_entries: u32,
    /// Expected count of the old table's counter records.
    pub num_counters: u32,
    /// Copied native `ipt_entry` byte array.
    pub entries: &'a [u8],
}

/// Failure to decode or publish a legacy filter replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyReplaceError {
    /// The replacement has invalid layout or unsupported rule semantics.
    InvalidInput,
    /// The table changed after the caller read its counter count.
    CounterMismatch,
}

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
    // Writers of entry counters hold a read guard; an exclusive guard keeps
    // packet and byte totals from different updates out of the same export.
    let current = FILTER_TABLE.write();
    current.export_snapshot()
}

/// Evaluates a packet using one complete, published table generation.
///
/// A replacement waits for in-flight evaluations before reading old counters.
pub(crate) fn evaluate_ipv4(hook: FilterHook, packet: &[u8], indev: &str, outdev: &str) -> Verdict {
    let parsed = if hook == FilterHook::Output {
        FilterPacket::parse_output(packet, outdev)
    } else {
        FilterPacket::parse(packet, indev, outdev)
    };
    match parsed {
        Ok(parsed) => FILTER_TABLE.read().evaluate(hook, &parsed),
        Err(_) => Verdict::Drop,
    }
}

/// Adds native entry counters to the current filter table.
///
/// The record count is checked while holding the publication lock.
pub fn add_counters(counters: &[LegacyCounter]) -> Result<(), LegacyReplaceError> {
    let current = FILTER_TABLE.read();
    if counters.len() != current.counters.len() {
        return Err(LegacyReplaceError::CounterMismatch);
    }
    for (entry, value) in current.counters.iter().zip(counters) {
        entry.add(*value);
    }
    Ok(())
}

/// Validates and publishes a complete IPv4 `filter` table replacement.
///
/// Parsing and export construction finish before publication. Readers hold an
/// owned snapshot, so each GET operation observes a single table generation.
///
/// # Errors
///
/// Returns `InvalidInput` for malformed or unsupported entries and
/// `CounterMismatch` when `num_counters` does not match the current table.
pub fn replace(request: LegacyFilterReplace<'_>) -> Result<Vec<LegacyCounter>, LegacyReplaceError> {
    replace_in(&FILTER_TABLE, request)
}

fn replace_in(
    store: &RwLock<Arc<FilterState>>,
    request: LegacyFilterReplace<'_>,
) -> Result<Vec<LegacyCounter>, LegacyReplaceError> {
    let table = decode::decode_table(&request)?;
    let replacement = FilterState::new(table);
    let mut current = store.write();
    if request.num_counters != current.exported.num_entries {
        return Err(LegacyReplaceError::CounterMismatch);
    }
    let old = core::mem::replace(&mut *current, Arc::new(replacement));
    let old_counters = old.counter_snapshot();
    drop(current);
    Ok(old_counters)
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
        ip[name_offset + bytes.len()] = b'+';
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

    fn replacement<'a>(
        snapshot: &'a LegacyFilterTable,
        num_counters: u32,
    ) -> LegacyFilterReplace<'a> {
        LegacyFilterReplace {
            valid_hooks: snapshot.valid_hooks,
            hook_entry: snapshot.hook_entry,
            underflow: snapshot.underflow,
            num_entries: snapshot.num_entries,
            num_counters,
            entries: &snapshot.entries,
        }
    }

    #[def_test]
    fn legacy_decoder_round_trips_initial_table() {
        let table = FilterTable::accept_all();
        let snapshot = encode_table(&table);
        assert_eq!(decode::decode_table(&replacement(&snapshot, 4)), Ok(table));
    }

    #[def_test]
    fn legacy_interface_prefix_preserves_plus_syntax() {
        let mut ip = [0; 84];
        let interface = InterfaceMatch::prefix("eth".to_string()).unwrap();
        encode_interface(&mut ip, 16, 48, &interface);
        assert_eq!(&ip[16..21], b"eth+\0");
        assert_eq!(&ip[48..52], &[0xff, 0xff, 0xff, 0]);
        let table = FilterTable::new(vec![
            Chain::built_in(
                FilterHook::Input,
                Verdict::Accept,
                vec![
                    Rule::new(
                        BaseMatch {
                            input_interface: Some(Negated::new(interface, false)),
                            ..Default::default()
                        },
                        Vec::new(),
                        Target::Verdict(Verdict::Drop),
                    )
                    .unwrap(),
                ],
            ),
            Chain::built_in(FilterHook::Forward, Verdict::Accept, Vec::new()),
            Chain::built_in(FilterHook::Output, Verdict::Accept, Vec::new()),
        ])
        .unwrap();
        let snapshot = encode_table(&table);
        assert_eq!(decode::decode_table(&replacement(&snapshot, 4)), Ok(table));
    }

    #[def_test]
    fn replacement_publishes_only_valid_complete_snapshots() {
        let initial = FilterTable::accept_all();
        let store = RwLock::new(Arc::new(FilterState::new(initial.clone())));
        let replacement_table = FilterTable::new(vec![
            Chain::built_in(FilterHook::Input, Verdict::Drop, Vec::new()),
            Chain::built_in(FilterHook::Forward, Verdict::Accept, Vec::new()),
            Chain::built_in(FilterHook::Output, Verdict::Accept, Vec::new()),
        ])
        .unwrap();
        let snapshot = encode_table(&replacement_table);
        assert_eq!(
            replace_in(&store, replacement(&snapshot, 3)),
            Err(LegacyReplaceError::CounterMismatch)
        );
        assert_eq!(store.read().table, initial);
        let mut malformed = snapshot.clone();
        malformed.underflow[1] = 7;
        assert_eq!(
            replace_in(&store, replacement(&malformed, 4)),
            Err(LegacyReplaceError::InvalidInput)
        );
        assert_eq!(store.read().table, initial);
        assert_eq!(
            replace_in(&store, replacement(&snapshot, 4)),
            Ok(vec![LegacyCounter::default(); 4])
        );
        assert_eq!(store.read().table, replacement_table);
        assert_eq!(store.read().exported, snapshot);
    }

    #[def_test]
    fn matched_rule_and_policy_counters_follow_packet_evaluation() {
        let drop_icmp = Rule::new(
            BaseMatch {
                protocol: Some(Negated::new(crate::ipv4::PROTOCOL_ICMP, false)),
                ..Default::default()
            },
            Vec::new(),
            Target::Verdict(Verdict::Drop),
        )
        .unwrap();
        let table = FilterTable::new(vec![
            Chain::built_in(FilterHook::Input, Verdict::Accept, vec![drop_icmp]),
            Chain::built_in(FilterHook::Forward, Verdict::Accept, Vec::new()),
            Chain::built_in(FilterHook::Output, Verdict::Accept, Vec::new()),
        ])
        .unwrap();
        let state = FilterState::new(table);
        let packet = crate::ipv4::build_ipv4_packet(
            crate::ip::Ipv4Address::new(192, 0, 2, 1),
            crate::ip::Ipv4Address::new(192, 0, 2, 2),
            crate::ipv4::PROTOCOL_ICMP,
            64,
            &[8, 0, 0, 0, 0, 0, 0, 0],
        )
        .unwrap();
        let parsed = FilterPacket::parse(&packet, "lo", "").unwrap();
        assert_eq!(state.evaluate(FilterHook::Input, &parsed), Verdict::Drop);
        let snapshot = state.export_snapshot();
        assert_eq!(
            u64::from_ne_bytes(snapshot.entries[96..104].try_into().unwrap()),
            1
        );
        assert_eq!(
            u64::from_ne_bytes(snapshot.entries[104..112].try_into().unwrap()),
            packet.len() as u64
        );
        assert_eq!(state.counter_snapshot()[1], LegacyCounter::default());

        let tcp = crate::ipv4::build_ipv4_packet(
            crate::ip::Ipv4Address::new(192, 0, 2, 1),
            crate::ip::Ipv4Address::new(192, 0, 2, 2),
            PROTOCOL_TCP,
            64,
            &[0; 20],
        )
        .unwrap();
        let parsed = FilterPacket::parse(&tcp, "lo", "").unwrap();
        assert_eq!(state.evaluate(FilterHook::Input, &parsed), Verdict::Accept);
        assert_eq!(
            state.counter_snapshot()[1],
            LegacyCounter {
                packets: 1,
                bytes: tcp.len() as u64
            }
        );
    }

    #[def_test]
    fn legacy_decoder_rejects_bad_entry_layout_and_policy() {
        let snapshot = encode_table(&FilterTable::accept_all());
        let mut bad = snapshot.clone();
        bad.entries[90..92].copy_from_slice(&8u16.to_ne_bytes());
        assert_eq!(
            decode::decode_table(&replacement(&bad, 4)),
            Err(LegacyReplaceError::InvalidInput)
        );

        let mut bad = snapshot.clone();
        bad.entries[144..148].copy_from_slice(&(-5i32).to_ne_bytes());
        assert_eq!(
            decode::decode_table(&replacement(&bad, 4)),
            Err(LegacyReplaceError::InvalidInput)
        );

        let mut bad = snapshot.clone();
        bad.hook_entry[1] = 7;
        assert_eq!(
            decode::decode_table(&replacement(&bad, 4)),
            Err(LegacyReplaceError::InvalidInput)
        );
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
        let decoded = decode::decode_table(&replacement(&snapshot, 4)).unwrap();
        assert_eq!(encode_table(&decoded), snapshot);
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

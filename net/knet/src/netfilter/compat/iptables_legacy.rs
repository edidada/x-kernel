// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Native iptables-legacy ABI adapter for semantic IPv4 filter configuration.

use alloc::{vec, vec::Vec};

use klazy::Lazy;
use ksync::Mutex;

use crate::netfilter::filter::{
    ChainKind, ExtensionMatch, FilterHook, FilterTable, InterfaceMatch, Negated, PortRange, Rule,
    Target, Verdict,
    config::{
        self, ChainCounters, FilterConfig, FilterConfigError, FilterCounter, FilterGeneration,
        FilterSnapshot,
    },
};

mod abi;
mod decode;

use abi::{
    ERROR_ENTRY_SIZE, FILTER_VALID_HOOKS, IFNAMSIZ, IPT_ENTRY_COUNTERS_OFFSET, IPT_ENTRY_SIZE,
    IPT_ICMP_INV, IPT_ICMP_MATCH_SIZE, IPT_INV_DSTIP, IPT_INV_PROTO, IPT_INV_SRCIP, IPT_INV_VIA_IN,
    IPT_INV_VIA_OUT, IPV4_ADDRESS_SIZE, IptEntry, IptIcmp, IptIp, NF_INET_FORWARD,
    NF_INET_LOCAL_IN, NF_INET_LOCAL_OUT, NF_INET_NUMHOOKS, STANDARD_ENTRY_SIZE,
    XT_ENTRY_HEADER_SIZE, XT_ERROR_TARGET, XT_ERROR_TARGET_SIZE, XT_RETURN,
    XT_STANDARD_TARGET_SIZE, XT_TCP_INV_DSTPT, XT_TCP_INV_FLAGS, XT_TCP_INV_SRCPT,
    XT_TCP_MATCH_SIZE, XT_UDP_INV_DSTPT, XT_UDP_INV_SRCPT, XT_UDP_MATCH_SIZE, XT_VERDICT_ACCEPT,
    XT_VERDICT_DROP, XtCounters, XtEntryHeader, XtErrorTarget, XtStandardTarget, XtTcp, XtUdp,
};

/// Largest native entry array accepted by the legacy decoder.
pub const MAX_LEGACY_FILTER_ENTRIES_SIZE: usize = decode::MAX_ENTRIES_SIZE;
/// Largest number of native entries accepted by the legacy decoder.
pub const MAX_LEGACY_FILTER_ENTRY_COUNT: usize = decode::MAX_ENTRY_COUNT;

// The adapter lock precedes the configuration lock. Core configuration and
// packet evaluation never acquire this lock.
static LEGACY_PROJECTION: Lazy<Mutex<LegacyProjection>> =
    Lazy::new(|| Mutex::new(LegacyProjection::default()));

/// Packet and byte totals for one native legacy table entry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LegacyCounter {
    /// Number of packets that matched the entry.
    pub packets: u64,
    /// Sum of the matching packets' IPv4 lengths.
    pub bytes: u64,
}

impl LegacyCounter {
    fn from_semantic(value: FilterCounter) -> Self {
        Self {
            packets: value.packets,
            bytes: value.bytes,
        }
    }

    fn into_semantic(self) -> FilterCounter {
        FilterCounter {
            packets: self.packets,
            bytes: self.bytes,
        }
    }

    fn add(&mut self, value: Self) {
        self.packets = self.packets.wrapping_add(value.packets);
        self.bytes = self.bytes.wrapping_add(value.bytes);
    }
}

// ERROR chain markers and the terminal entry have no semantic execution role,
// but ADD_COUNTERS and GET_ENTRIES must preserve their supplied counter values.
#[derive(Default)]
struct LegacyProjection {
    generation: Option<FilterGeneration>,
    markers: Vec<LegacyCounter>,
    terminal: LegacyCounter,
}

impl LegacyProjection {
    fn synchronize(&mut self, generation: &FilterGeneration, marker_count: usize) {
        if self.generation.as_ref() == Some(generation) {
            return;
        }
        self.generation = Some(generation.clone());
        self.markers = vec![LegacyCounter::default(); marker_count];
        self.terminal = LegacyCounter::default();
    }
}

/// Metadata copied from the native 64-bit `ipt_replace` ABI after syscall validation.
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
    /// Copied native 64-bit `ipt_entry` byte array.
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
    snapshot_in(config::global(), &LEGACY_PROJECTION)
}

fn snapshot_in(store: &FilterConfig, projection: &Mutex<LegacyProjection>) -> LegacyFilterTable {
    let mut projection = projection.lock();
    let snapshot = store.snapshot();
    projection.synchronize(&snapshot.generation, marker_count(&snapshot.table));
    let counters = project_counters(&snapshot, &projection);
    let mut exported = encode_table(&snapshot.table);
    let mut offset = 0;
    for counter in counters {
        let entry: IptEntry = abi::read(&exported.entries[offset..])
            .expect("serialized legacy entry must contain its header");
        abi::write(
            &mut exported.entries[offset + IPT_ENTRY_COUNTERS_OFFSET..],
            &XtCounters {
                packets: counter.packets,
                bytes: counter.bytes,
            },
        );
        offset += entry.next_offset as usize;
    }
    exported
}

/// Converts native counter records into semantic updates for the shared store.
pub(crate) fn add_counters(counters: &[LegacyCounter]) -> Result<(), LegacyReplaceError> {
    add_counters_in(config::global(), &LEGACY_PROJECTION, counters)
}

fn add_counters_in(
    store: &FilterConfig,
    projection: &Mutex<LegacyProjection>,
    counters: &[LegacyCounter],
) -> Result<(), LegacyReplaceError> {
    let mut projection = projection.lock();
    let mut markers = Vec::new();
    let generation = store
        .add_counters(|table| {
            if counters.len() != entry_count(table) {
                return Err(FilterConfigError::Conflict);
            }
            let mut entries = counters.iter();
            let mut updates = Vec::with_capacity(table.chains().len());
            for chain in table.chains() {
                if matches!(chain.kind(), ChainKind::UserDefined { .. }) {
                    markers.push(*entries.next().unwrap());
                }
                let rules = chain
                    .rules()
                    .iter()
                    .map(|_| entries.next().unwrap().into_semantic())
                    .collect();
                let end = entries.next().unwrap().into_semantic();
                updates.push(ChainCounters { rules, end });
            }
            Ok(updates)
        })
        .map_err(config_error)?;
    projection.synchronize(&generation, markers.len());
    for (current, value) in projection.markers.iter_mut().zip(markers) {
        current.add(value);
    }
    projection.terminal.add(*counters.last().unwrap());
    Ok(())
}

/// Decodes a legacy request and conditionally publishes its semantic table.
///
/// # Errors
///
/// Returns `InvalidInput` for malformed or unsupported entries and
/// `CounterMismatch` when `num_counters` does not match the current table's
/// legacy projection. The count check runs under the shared publication lock.
pub(crate) fn replace(
    request: LegacyFilterReplace<'_>,
) -> Result<Vec<LegacyCounter>, LegacyReplaceError> {
    replace_in(config::global(), &LEGACY_PROJECTION, request)
}

fn replace_in(
    store: &FilterConfig,
    projection: &Mutex<LegacyProjection>,
    request: LegacyFilterReplace<'_>,
) -> Result<Vec<LegacyCounter>, LegacyReplaceError> {
    let table = decode::decode_table(&request)?;
    let new_marker_count = marker_count(&table);
    let mut projection = projection.lock();
    let (previous, generation) = store
        .replace(table, |current, _| {
            if request.num_counters as usize != entry_count(current) {
                return Err(FilterConfigError::Conflict);
            }
            Ok(())
        })
        .map_err(config_error)?;
    projection.synchronize(&previous.generation, marker_count(&previous.table));
    let old_counters = project_counters(&previous, &projection);
    projection.synchronize(&generation, new_marker_count);
    Ok(old_counters)
}

fn config_error(error: FilterConfigError) -> LegacyReplaceError {
    match error {
        FilterConfigError::Conflict | FilterConfigError::InvalidCounterShape => {
            LegacyReplaceError::CounterMismatch
        }
    }
}

fn marker_count(table: &FilterTable) -> usize {
    table
        .chains()
        .iter()
        .filter(|chain| matches!(chain.kind(), ChainKind::UserDefined { .. }))
        .count()
}

fn entry_count(table: &FilterTable) -> usize {
    table
        .chains()
        .iter()
        .map(|chain| chain.rules().len() + 1)
        .sum::<usize>()
        + marker_count(table)
        + 1
}

fn project_counters(
    snapshot: &FilterSnapshot,
    projection: &LegacyProjection,
) -> Vec<LegacyCounter> {
    let mut result = Vec::with_capacity(entry_count(&snapshot.table));
    let mut markers = projection.markers.iter();
    for (chain, counters) in snapshot.table.chains().iter().zip(&snapshot.counters) {
        if matches!(chain.kind(), ChainKind::UserDefined { .. }) {
            result.push(*markers.next().unwrap());
        }
        result.extend(
            counters
                .rules
                .iter()
                .copied()
                .map(LegacyCounter::from_semantic),
        );
        result.push(LegacyCounter::from_semantic(counters.end));
    }
    result.push(projection.terminal);
    result
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
        valid_hooks: FILTER_VALID_HOOKS,
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
        ExtensionMatch::Tcp(_) => XT_TCP_MATCH_SIZE,
        ExtensionMatch::Udp(_) => XT_UDP_MATCH_SIZE,
        ExtensionMatch::Icmp(_) => IPT_ICMP_MATCH_SIZE,
    }
}

fn encode_rule(entry: &mut [u8], entry_offset: usize, rule: &Rule, layouts: &[ChainLayout]) {
    let mut offset = IPT_ENTRY_SIZE;
    for extension in rule.extensions() {
        let size = extension_size(extension);
        encode_extension(&mut entry[offset..offset + size], extension);
        offset += size;
    }

    encode_entry_header(entry, encode_base_match(rule), offset);
    let verdict = match rule.target() {
        Target::Verdict(verdict) => verdict_code(verdict),
        Target::Continue => to_i32(entry_offset + entry.len()),
        Target::Return => XT_RETURN,
        Target::Jump(chain) => to_i32(layouts[chain.index()].head_offset + ERROR_ENTRY_SIZE),
    };
    encode_standard_target(&mut entry[offset..], verdict);
}

fn encode_base_match(rule: &Rule) -> IptIp {
    let mut ip = IptIp::default();
    let base = rule.base();
    let mut inverse = 0u8;

    if let Some(source) = &base.source {
        let cidr = source.value().network();
        ip.source = cidr.address().octets();
        ip.source_mask = prefix_mask(cidr.prefix_len());
        inverse |= inversion(source, IPT_INV_SRCIP);
    }
    if let Some(destination) = &base.destination {
        let cidr = destination.value().network();
        ip.destination = cidr.address().octets();
        ip.destination_mask = prefix_mask(cidr.prefix_len());
        inverse |= inversion(destination, IPT_INV_DSTIP);
    }
    if let Some(interface) = &base.input_interface {
        encode_interface(
            &mut ip.input_interface,
            &mut ip.input_interface_mask,
            interface.value(),
        );
        inverse |= inversion(interface, IPT_INV_VIA_IN);
    }
    if let Some(interface) = &base.output_interface {
        encode_interface(
            &mut ip.output_interface,
            &mut ip.output_interface_mask,
            interface.value(),
        );
        inverse |= inversion(interface, IPT_INV_VIA_OUT);
    }
    if let Some(protocol) = &base.protocol {
        ip.protocol = u16::from(*protocol.value());
        inverse |= inversion(protocol, IPT_INV_PROTO);
    }
    ip.inverse_flags = inverse;
    ip
}

fn encode_interface(
    name: &mut [u8; IFNAMSIZ],
    mask: &mut [u8; IFNAMSIZ],
    interface: &InterfaceMatch,
) {
    let bytes = interface.name().as_bytes();
    name[..bytes.len()].copy_from_slice(bytes);
    let mask_len = if interface.is_prefix() {
        name[bytes.len()] = b'+';
        bytes.len()
    } else {
        bytes.len() + 1
    };
    mask[..mask_len].fill(u8::MAX);
}

fn encode_extension(output: &mut [u8], extension: &ExtensionMatch) {
    match extension {
        ExtensionMatch::Tcp(tcp) => {
            encode_extension_header(output, b"tcp");
            let data = &mut output[XT_ENTRY_HEADER_SIZE..];
            let mut inverse = 0;
            let source_ports =
                encode_ports(tcp.source_port_range(), XT_TCP_INV_SRCPT, &mut inverse);
            let destination_ports =
                encode_ports(tcp.destination_port_range(), XT_TCP_INV_DSTPT, &mut inverse);
            let mut header = XtTcp {
                source_ports,
                destination_ports,
                ..Default::default()
            };
            if let Some(flags) = tcp.flags_match() {
                header.flags_mask = flags.value().mask();
                header.flags_compare = flags.value().expected();
                inverse |= inversion(&flags, XT_TCP_INV_FLAGS);
            }
            header.inverse_flags = inverse;
            abi::write(data, &header);
        }
        ExtensionMatch::Udp(udp) => {
            encode_extension_header(output, b"udp");
            let data = &mut output[XT_ENTRY_HEADER_SIZE..];
            let mut inverse = 0;
            let source_ports =
                encode_ports(udp.source_port_range(), XT_UDP_INV_SRCPT, &mut inverse);
            let destination_ports =
                encode_ports(udp.destination_port_range(), XT_UDP_INV_DSTPT, &mut inverse);
            abi::write(
                data,
                &XtUdp {
                    source_ports,
                    destination_ports,
                    inverse_flags: inverse,
                    ..Default::default()
                },
            );
        }
        ExtensionMatch::Icmp(icmp) => {
            encode_extension_header(output, b"icmp");
            let data = &mut output[XT_ENTRY_HEADER_SIZE..];
            abi::write(
                data,
                &IptIcmp {
                    icmp_type: icmp.icmp_type().unwrap_or(u8::MAX),
                    code: [icmp.code_start(), icmp.code_end()],
                    inverse_flags: u8::from(icmp.is_inverted()) * IPT_ICMP_INV,
                },
            );
        }
    }
}

fn encode_ports(range: Option<Negated<PortRange>>, inverse_bit: u8, inverse: &mut u8) -> [u16; 2] {
    let (start, end) = range
        .map(|range| {
            *inverse |= inversion(&range, inverse_bit);
            (range.value().start(), range.value().end())
        })
        .unwrap_or((0, u16::MAX));
    [start, end]
}

fn encode_entry_header(entry: &mut [u8], ip: IptIp, target_offset: usize) {
    abi::write(
        entry,
        &IptEntry {
            ip,
            target_offset: to_u16(target_offset),
            next_offset: to_u16(entry.len()),
            ..Default::default()
        },
    );
}

fn encode_standard_entry(entry: &mut [u8], verdict: i32) {
    encode_entry_header(entry, IptIp::default(), IPT_ENTRY_SIZE);
    encode_standard_target(&mut entry[IPT_ENTRY_SIZE..], verdict);
}

fn encode_standard_target(target: &mut [u8], verdict: i32) {
    abi::write(
        target,
        &XtStandardTarget {
            header: XtEntryHeader {
                size: to_u16(XT_STANDARD_TARGET_SIZE),
                ..Default::default()
            },
            verdict,
            ..Default::default()
        },
    );
}

fn encode_error_entry(entry: &mut [u8], error_name: &[u8]) {
    encode_entry_header(entry, IptIp::default(), IPT_ENTRY_SIZE);
    let mut target = XtErrorTarget {
        header: XtEntryHeader {
            size: to_u16(XT_ERROR_TARGET_SIZE),
            ..Default::default()
        },
        ..Default::default()
    };
    target.header.name[..XT_ERROR_TARGET.len()].copy_from_slice(XT_ERROR_TARGET);
    target.name[..error_name.len()].copy_from_slice(error_name);
    abi::write(&mut entry[IPT_ENTRY_SIZE..], &target);
}

fn encode_extension_header(extension: &mut [u8], name: &[u8]) {
    let mut header = XtEntryHeader {
        size: to_u16(extension.len()),
        ..Default::default()
    };
    header.name[..name.len()].copy_from_slice(name);
    abi::write(extension, &header);
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
        Verdict::Accept => XT_VERDICT_ACCEPT,
        Verdict::Drop => XT_VERDICT_DROP,
    }
}

fn inversion<T>(condition: &Negated<T>, bit: u8) -> u8 {
    u8::from(condition.is_inverted()) * bit
}

fn prefix_mask(prefix_len: u8) -> [u8; IPV4_ADDRESS_SIZE] {
    let shift = u32::BITS
        .checked_sub(u32::from(prefix_len))
        .expect("validated IPv4 prefix length must fit u32");
    u32::MAX.checked_shl(shift).unwrap_or(0).to_be_bytes()
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
        ip::Ipv4Cidr,
        ipv4::PROTOCOL_TCP,
        netfilter::filter::{
            BaseMatch, Chain, ChainId, IcmpMatch, TcpFlagsMatch, TcpMatch, UdpMatch,
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

    fn exported_counters(snapshot: &LegacyFilterTable) -> Vec<LegacyCounter> {
        let mut result = Vec::new();
        let mut offset = 0;
        while offset < snapshot.entries.len() {
            let entry: IptEntry = abi::read(&snapshot.entries[offset..]).unwrap();
            result.push(LegacyCounter {
                packets: entry.counters.packets,
                bytes: entry.counters.bytes,
            });
            offset += entry.next_offset as usize;
        }
        result
    }

    #[def_test]
    fn legacy_counters_follow_shared_configuration_generations() {
        let table = FilterTable::new(vec![
            Chain::built_in(FilterHook::Input, Verdict::Accept, Vec::new()),
            Chain::built_in(FilterHook::Forward, Verdict::Accept, Vec::new()),
            Chain::built_in(FilterHook::Output, Verdict::Accept, Vec::new()),
            Chain::user_defined(
                "custom".to_string(),
                vec![Rule::new(BaseMatch::default(), Vec::new(), Target::Return).unwrap()],
            ),
        ])
        .unwrap();
        let store = FilterConfig::new(table.clone());
        let projection = Mutex::new(LegacyProjection::default());
        let counters = (1..=7)
            .map(|packets| LegacyCounter {
                packets,
                bytes: packets * 10,
            })
            .collect::<Vec<_>>();
        add_counters_in(&store, &projection, &counters).unwrap();
        assert_eq!(
            exported_counters(&snapshot_in(&store, &projection)),
            counters
        );
        let semantic = store.snapshot();
        assert_eq!(semantic.counters[0].end, counters[0].into_semantic());
        assert_eq!(semantic.counters[3].rules[0], counters[4].into_semantic());
        assert_eq!(semantic.counters[3].end, counters[5].into_semantic());
        assert_eq!(
            add_counters_in(&store, &projection, &counters[..6]),
            Err(LegacyReplaceError::CounterMismatch)
        );
        assert_eq!(
            exported_counters(&snapshot_in(&store, &projection)),
            counters
        );

        // A different management adapter can publish through the same API.
        // Legacy-only marker values belong to the previous generation.
        store.replace(table, |_, _| Ok(())).unwrap();
        assert_eq!(
            exported_counters(&snapshot_in(&store, &projection)),
            vec![LegacyCounter::default(); 7]
        );
        add_counters_in(&store, &projection, &counters).unwrap();
        let replacement_table = encode_table(&FilterTable::accept_all());
        assert_eq!(
            replace_in(&store, &projection, replacement(&replacement_table, 7)),
            Ok(counters)
        );
        assert_eq!(snapshot_in(&store, &projection), replacement_table);
    }

    #[def_test]
    fn legacy_decoder_round_trips_initial_table() {
        let table = FilterTable::accept_all();
        let snapshot = encode_table(&table);
        assert_eq!(decode::decode_table(&replacement(&snapshot, 4)), Ok(table));

        let mut unaligned = vec![0];
        unaligned.extend_from_slice(snapshot.entries());
        let mut request = replacement(&snapshot, 4);
        request.entries = &unaligned[1..];
        assert_eq!(
            decode::decode_table(&request),
            Ok(FilterTable::accept_all())
        );
    }

    #[def_test]
    fn legacy_decoder_round_trips_supported_matches() {
        let cases = [
            (
                PROTOCOL_TCP,
                ExtensionMatch::Tcp(
                    TcpMatch::new()
                        .with_source_port(Negated::new(PortRange::new(1234, 5678).unwrap(), true))
                        .with_destination_port(Negated::new(
                            PortRange::new(80, 443).unwrap(),
                            false,
                        ))
                        .with_flags(Negated::new(TcpFlagsMatch::new(0x12, 0x02).unwrap(), true)),
                ),
            ),
            (
                crate::ipv4::PROTOCOL_UDP,
                ExtensionMatch::Udp(
                    UdpMatch::new()
                        .with_source_port(Negated::new(PortRange::new(1234, 5678).unwrap(), true))
                        .with_destination_port(Negated::new(
                            PortRange::new(53, 53).unwrap(),
                            false,
                        )),
                ),
            ),
            (
                crate::ipv4::PROTOCOL_ICMP,
                ExtensionMatch::Icmp(IcmpMatch::new(Some(3), 1, 4).unwrap().with_inversion(true)),
            ),
        ];
        for (protocol, extension) in cases {
            let rule = Rule::new(
                BaseMatch {
                    source: Some(Negated::new(
                        Ipv4Cidr::new(crate::ip::Ipv4Address::new(192, 0, 2, 0), 24).unwrap(),
                        true,
                    )),
                    destination: Some(Negated::new(
                        Ipv4Cidr::new(crate::ip::Ipv4Address::new(198, 51, 100, 0), 24).unwrap(),
                        false,
                    )),
                    input_interface: Some(Negated::new(
                        InterfaceMatch::exact("eth0".to_string()).unwrap(),
                        true,
                    )),
                    output_interface: Some(Negated::new(
                        InterfaceMatch::prefix("eth".to_string()).unwrap(),
                        false,
                    )),
                    protocol: Some(Negated::new(protocol, false)),
                },
                vec![extension],
                Target::Verdict(Verdict::Drop),
            )
            .unwrap();
            let table = FilterTable::new(vec![
                Chain::built_in(FilterHook::Input, Verdict::Accept, vec![rule]),
                Chain::built_in(FilterHook::Forward, Verdict::Accept, Vec::new()),
                Chain::built_in(FilterHook::Output, Verdict::Accept, Vec::new()),
            ])
            .unwrap();
            let snapshot = encode_table(&table);
            assert_eq!(decode::decode_table(&replacement(&snapshot, 4)), Ok(table));
        }
    }

    #[def_test]
    fn legacy_interface_prefix_preserves_plus_syntax() {
        let mut ip = IptIp::default();
        let interface = InterfaceMatch::prefix("eth".to_string()).unwrap();
        encode_interface(
            &mut ip.input_interface,
            &mut ip.input_interface_mask,
            &interface,
        );
        assert_eq!(&ip.input_interface[..5], b"eth+\0");
        assert_eq!(&ip.input_interface_mask[..4], &[0xff, 0xff, 0xff, 0]);
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
        let store = FilterConfig::new(initial.clone());
        let projection = Mutex::new(LegacyProjection::default());
        let replacement_table = FilterTable::new(vec![
            Chain::built_in(FilterHook::Input, Verdict::Drop, Vec::new()),
            Chain::built_in(FilterHook::Forward, Verdict::Accept, Vec::new()),
            Chain::built_in(FilterHook::Output, Verdict::Accept, Vec::new()),
        ])
        .unwrap();
        let snapshot = encode_table(&replacement_table);
        assert_eq!(
            replace_in(&store, &projection, replacement(&snapshot, 3)),
            Err(LegacyReplaceError::CounterMismatch)
        );
        assert_eq!(*store.snapshot().table, initial);
        let mut malformed = snapshot.clone();
        malformed.underflow[1] = 7;
        assert_eq!(
            replace_in(&store, &projection, replacement(&malformed, 4)),
            Err(LegacyReplaceError::InvalidInput)
        );
        assert_eq!(*store.snapshot().table, initial);
        assert_eq!(
            replace_in(&store, &projection, replacement(&snapshot, 4)),
            Ok(vec![LegacyCounter::default(); 4])
        );
        assert_eq!(*store.snapshot().table, replacement_table);
        assert_eq!(snapshot_in(&store, &projection), snapshot);
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
        let store = FilterConfig::new(table);
        let projection = Mutex::new(LegacyProjection::default());
        let packet = crate::ipv4::build_ipv4_packet(
            crate::ip::Ipv4Address::new(192, 0, 2, 1),
            crate::ip::Ipv4Address::new(192, 0, 2, 2),
            crate::ipv4::PROTOCOL_ICMP,
            64,
            &[8, 0, 0, 0, 0, 0, 0, 0],
        )
        .unwrap();
        assert_eq!(
            store.evaluate_ipv4(FilterHook::Input, &packet, "lo", ""),
            Verdict::Drop
        );
        let snapshot = snapshot_in(&store, &projection);
        assert_eq!(
            u64::from_ne_bytes(snapshot.entries[96..104].try_into().unwrap()),
            1
        );
        assert_eq!(
            u64::from_ne_bytes(snapshot.entries[104..112].try_into().unwrap()),
            packet.len() as u64
        );
        assert_eq!(store.snapshot().counters[0].end, FilterCounter::default());

        let tcp = crate::ipv4::build_ipv4_packet(
            crate::ip::Ipv4Address::new(192, 0, 2, 1),
            crate::ip::Ipv4Address::new(192, 0, 2, 2),
            PROTOCOL_TCP,
            64,
            &[0; 20],
        )
        .unwrap();
        assert_eq!(
            store.evaluate_ipv4(FilterHook::Input, &tcp, "lo", ""),
            Verdict::Accept
        );
        assert_eq!(
            store.snapshot().counters[0].end,
            FilterCounter {
                packets: 1,
                bytes: tcp.len() as u64
            }
        );
    }

    #[def_test]
    fn legacy_decoder_rejects_bad_entry_layout_and_policy() {
        let snapshot = encode_table(&FilterTable::accept_all());
        for length in [0, 1, 83, 88, 90, 111, 112, 143] {
            let mut bad = snapshot.clone();
            bad.entries.truncate(length);
            assert_eq!(
                decode::decode_table(&replacement(&bad, 4)),
                Err(LegacyReplaceError::InvalidInput)
            );
        }
        for (offset, value) in [
            (88, 0u16),
            (88, 111),
            (88, 113),
            (88, 120),
            (88, u16::MAX),
            (90, 8),
            (90, 151),
            (90, u16::MAX),
        ] {
            let mut bad = snapshot.clone();
            bad.entries[offset..offset + 2].copy_from_slice(&value.to_ne_bytes());
            assert_eq!(
                decode::decode_table(&replacement(&bad, 4)),
                Err(LegacyReplaceError::InvalidInput)
            );
        }

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

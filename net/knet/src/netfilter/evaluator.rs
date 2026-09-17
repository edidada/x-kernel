// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 `filter` table packet evaluator.
//!
//! Evaluates a validated [`FilterTable`] against one IPv4 packet using the
//! semantics of Linux 6.8 `ipt_do_table`, `ip_packet_match`, and the built-in
//! `tcp` / `udp` / `icmp` matches.

use super::model::{
    ChainId, ChainKind, FilterHook, FilterTable, IcmpMatch, InterfaceMatch, Match, MatchKind,
    PortRange, Rule, Target, TcpFlagsMatch, Verdict,
};
#[cfg(unittest)]
use crate::stack::ipv4::IPV4_MIN_HEADER_LEN;
use crate::{
    ip::{IpAddress, Ipv4Address},
    stack::ipv4::{Ipv4Error, Ipv4Header, PROTOCOL_ICMP, PROTOCOL_TCP, PROTOCOL_UDP},
};

/// Maximum user-chain nesting tracked on the preallocated jump stack.
///
/// Deeper recursion returns [`Verdict::Drop`], matching Linux stack overflow
/// handling in `ipt_do_table`.
pub(crate) const MAX_JUMP_STACK_DEPTH: usize = 8;

const TCP_HEADER_LEN: usize = 20;
const UDP_HEADER_LEN: usize = 8;
const ICMP_HEADER_LEN: usize = 8;
const TCP_FLAGS_BYTE_OFFSET: usize = 13;
/// Linux treats `frag_off == 1` (8-byte payload offset) as an evil TCP fragment.
const TCP_EVIL_FRAGMENT_OFFSET_BYTES: usize = 8;

/// Parsed IPv4 packet metadata supplied to the evaluator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FilterPacket<'a> {
    packet: &'a [u8],
    header: Ipv4Header,
    indev: &'a str,
    outdev: &'a str,
}

impl<'a> FilterPacket<'a> {
    pub(crate) fn parse(
        packet: &'a [u8],
        indev: &'a str,
        outdev: &'a str,
    ) -> Result<Self, Ipv4Error> {
        let header = Ipv4Header::parse_input(packet)?;
        Ok(Self {
            packet,
            header,
            indev,
            outdev,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MatchOutcome {
    Matched,
    NotMatched,
    HotDrop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct JumpFrame {
    chain_id: ChainId,
    next_rule: usize,
}

impl FilterTable {
    /// Evaluates the built-in chain for `hook` against `packet`.
    pub(crate) fn evaluate(&self, hook: FilterHook, packet: &FilterPacket<'_>) -> Verdict {
        evaluate_from(self, self.built_in_chain_id(hook), 0, packet)
    }
}

fn evaluate_from(
    table: &FilterTable,
    mut chain_id: ChainId,
    mut rule_index: usize,
    packet: &FilterPacket<'_>,
) -> Verdict {
    let mut jump_stack: [Option<JumpFrame>; MAX_JUMP_STACK_DEPTH] = [None; MAX_JUMP_STACK_DEPTH];
    let mut stack_top = 0usize;

    loop {
        let Some(chain) = table.chain(chain_id) else {
            return Verdict::Drop;
        };

        if rule_index >= chain.rules().len() {
            if stack_top == 0 {
                return builtin_policy(table, chain_id);
            }
            stack_top -= 1;
            let frame = jump_stack[stack_top]
                .take()
                .expect("jump stack entry must exist for active depth");
            chain_id = frame.chain_id;
            rule_index = frame.next_rule;
            continue;
        }

        let rule = &chain.rules()[rule_index];
        match evaluate_rule(table, rule, packet) {
            RuleOutcome::NextRule => rule_index += 1,
            RuleOutcome::Continue => rule_index += 1,
            RuleOutcome::Verdict(verdict) => return verdict,
            RuleOutcome::Jump(target) => {
                if stack_top >= MAX_JUMP_STACK_DEPTH {
                    return Verdict::Drop;
                }
                jump_stack[stack_top] = Some(JumpFrame {
                    chain_id,
                    next_rule: rule_index + 1,
                });
                stack_top += 1;
                chain_id = target;
                rule_index = 0;
            }
            RuleOutcome::Return => {
                if stack_top == 0 {
                    return builtin_policy(table, chain_id);
                }
                stack_top -= 1;
                let frame = jump_stack[stack_top]
                    .take()
                    .expect("jump stack entry must exist for active depth");
                chain_id = frame.chain_id;
                rule_index = frame.next_rule;
            }
        }
    }
}

enum RuleOutcome {
    NextRule,
    Continue,
    Verdict(Verdict),
    Jump(ChainId),
    Return,
}

fn evaluate_rule(table: &FilterTable, rule: &Rule, packet: &FilterPacket<'_>) -> RuleOutcome {
    match evaluate_matches(rule.matches(), packet) {
        MatchOutcome::HotDrop => RuleOutcome::Verdict(Verdict::Drop),
        MatchOutcome::NotMatched => RuleOutcome::NextRule,
        MatchOutcome::Matched => match rule.target() {
            Target::Verdict(verdict) => RuleOutcome::Verdict(verdict),
            Target::Continue => RuleOutcome::Continue,
            Target::Return => RuleOutcome::Return,
            Target::Jump(chain_id) => {
                if table
                    .chain(chain_id)
                    .is_some_and(|chain| matches!(chain.kind(), ChainKind::UserDefined))
                {
                    RuleOutcome::Jump(chain_id)
                } else {
                    RuleOutcome::Verdict(Verdict::Drop)
                }
            }
        },
    }
}

fn builtin_policy(table: &FilterTable, chain_id: ChainId) -> Verdict {
    let Some(chain) = table.chain(chain_id) else {
        return Verdict::Drop;
    };
    match chain.kind() {
        ChainKind::BuiltIn { policy, .. } => policy,
        ChainKind::UserDefined => Verdict::Drop,
    }
}

fn evaluate_matches(matches: &[Match], packet: &FilterPacket<'_>) -> MatchOutcome {
    for condition in matches {
        match evaluate_match(condition, packet) {
            MatchOutcome::HotDrop => return MatchOutcome::HotDrop,
            MatchOutcome::NotMatched => return MatchOutcome::NotMatched,
            MatchOutcome::Matched => {}
        }
    }
    MatchOutcome::Matched
}

fn evaluate_match(condition: &Match, packet: &FilterPacket<'_>) -> MatchOutcome {
    let matched = match condition.kind() {
        MatchKind::SourceAddress(cidr) => {
            address_matches(cidr.address(), cidr.prefix_len(), packet.header.src_addr())
        }
        MatchKind::DestinationAddress(cidr) => {
            address_matches(cidr.address(), cidr.prefix_len(), packet.header.dst_addr())
        }
        MatchKind::Protocol(protocol) => packet.header.protocol() == *protocol,
        MatchKind::InputInterface(pattern) => interface_matches(pattern, packet.indev),
        MatchKind::OutputInterface(pattern) => interface_matches(pattern, packet.outdev),
        MatchKind::TcpSourcePort(range) => match tcp_field(packet, TcpField::SourcePort) {
            FieldOutcome::Matched(value) => port_in_range(*range, value),
            FieldOutcome::NotMatched => return MatchOutcome::NotMatched,
            FieldOutcome::HotDrop => return MatchOutcome::HotDrop,
        },
        MatchKind::TcpDestinationPort(range) => {
            match tcp_field(packet, TcpField::DestinationPort) {
                FieldOutcome::Matched(value) => port_in_range(*range, value),
                FieldOutcome::NotMatched => return MatchOutcome::NotMatched,
                FieldOutcome::HotDrop => return MatchOutcome::HotDrop,
            }
        }
        MatchKind::TcpFlags(flags) => match tcp_flags_byte(packet) {
            FieldOutcome::Matched(value) => tcp_flags_match(*flags, value),
            FieldOutcome::NotMatched => return MatchOutcome::NotMatched,
            FieldOutcome::HotDrop => return MatchOutcome::HotDrop,
        },
        MatchKind::UdpSourcePort(range) => match udp_field(packet, UdpField::SourcePort) {
            FieldOutcome::Matched(value) => port_in_range(*range, value),
            FieldOutcome::NotMatched => return MatchOutcome::NotMatched,
            FieldOutcome::HotDrop => return MatchOutcome::HotDrop,
        },
        MatchKind::UdpDestinationPort(range) => {
            match udp_field(packet, UdpField::DestinationPort) {
                FieldOutcome::Matched(value) => port_in_range(*range, value),
                FieldOutcome::NotMatched => return MatchOutcome::NotMatched,
                FieldOutcome::HotDrop => return MatchOutcome::HotDrop,
            }
        }
        MatchKind::Icmp(icmp) => match icmp_field(packet) {
            FieldOutcome::Matched((icmp_type, code)) => icmp_matches(*icmp, icmp_type, code),
            FieldOutcome::NotMatched => return MatchOutcome::NotMatched,
            FieldOutcome::HotDrop => return MatchOutcome::HotDrop,
        },
    };

    if matched ^ condition.is_inverted() {
        MatchOutcome::Matched
    } else {
        MatchOutcome::NotMatched
    }
}

fn address_matches(network: Ipv4Address, prefix_len: u8, addr: Ipv4Address) -> bool {
    crate::ip::Ipv4Cidr::new(network, prefix_len).contains_addr(&IpAddress::Ipv4(addr))
}

fn interface_matches(pattern: &InterfaceMatch, device_name: &str) -> bool {
    match pattern {
        InterfaceMatch::Exact(expected) => device_name == expected,
        InterfaceMatch::Prefix(prefix) => device_name.starts_with(prefix.as_str()),
    }
}

fn port_in_range(range: PortRange, port: u16) -> bool {
    port >= range.start() && port <= range.end()
}

fn tcp_flags_match(expected: TcpFlagsMatch, flags_byte: u8) -> bool {
    (flags_byte & expected.mask()) == expected.expected()
}

fn icmp_matches(spec: IcmpMatch, icmp_type: u8, code: u8) -> bool {
    let type_ok = spec
        .icmp_type()
        .is_none_or(|expected| expected == icmp_type);
    type_ok && code >= spec.code_start() && code <= spec.code_end()
}

enum FieldOutcome<T> {
    Matched(T),
    NotMatched,
    HotDrop,
}

enum TcpField {
    SourcePort,
    DestinationPort,
}

enum UdpField {
    SourcePort,
    DestinationPort,
}

fn l4_unavailable(header: Ipv4Header, l4_protocol: u8) -> bool {
    header.protocol() != l4_protocol || header.fragment_offset() != 0
}

fn transport_slice<'a>(
    packet: &'a FilterPacket<'_>,
    required_len: usize,
) -> Result<&'a [u8], FieldOutcome<()>> {
    // Link-layer padding cannot supply bytes missing from the IPv4 payload.
    crate::stack::ipv4::payload(packet.packet, &packet.header)
        .and_then(|payload| payload.get(..required_len))
        .ok_or(FieldOutcome::HotDrop)
}

fn tcp_field(packet: &FilterPacket<'_>, field: TcpField) -> FieldOutcome<u16> {
    if l4_unavailable(packet.header, PROTOCOL_TCP) {
        if packet.header.protocol() == PROTOCOL_TCP
            && packet.header.fragment_offset() == TCP_EVIL_FRAGMENT_OFFSET_BYTES
        {
            return FieldOutcome::HotDrop;
        }
        return FieldOutcome::NotMatched;
    }

    let transport = match transport_slice(packet, TCP_HEADER_LEN) {
        Ok(slice) => slice,
        Err(FieldOutcome::HotDrop) => return FieldOutcome::HotDrop,
        Err(FieldOutcome::NotMatched) => return FieldOutcome::NotMatched,
        Err(FieldOutcome::Matched(_)) => unreachable!(),
    };

    match field {
        TcpField::SourcePort => {
            FieldOutcome::Matched(u16::from_be_bytes([transport[0], transport[1]]))
        }
        TcpField::DestinationPort => {
            FieldOutcome::Matched(u16::from_be_bytes([transport[2], transport[3]]))
        }
    }
}

fn tcp_flags_byte(packet: &FilterPacket<'_>) -> FieldOutcome<u8> {
    if l4_unavailable(packet.header, PROTOCOL_TCP) {
        if packet.header.protocol() == PROTOCOL_TCP
            && packet.header.fragment_offset() == TCP_EVIL_FRAGMENT_OFFSET_BYTES
        {
            return FieldOutcome::HotDrop;
        }
        return FieldOutcome::NotMatched;
    }

    let transport = match transport_slice(packet, TCP_HEADER_LEN) {
        Ok(slice) => slice,
        Err(FieldOutcome::HotDrop) => return FieldOutcome::HotDrop,
        Err(FieldOutcome::NotMatched) => return FieldOutcome::NotMatched,
        Err(FieldOutcome::Matched(_)) => unreachable!(),
    };

    if transport.len() <= TCP_FLAGS_BYTE_OFFSET {
        FieldOutcome::HotDrop
    } else {
        FieldOutcome::Matched(transport[TCP_FLAGS_BYTE_OFFSET])
    }
}

fn udp_field(packet: &FilterPacket<'_>, field: UdpField) -> FieldOutcome<u16> {
    if l4_unavailable(packet.header, PROTOCOL_UDP) {
        return FieldOutcome::NotMatched;
    }

    let transport = match transport_slice(packet, UDP_HEADER_LEN) {
        Ok(slice) => slice,
        Err(FieldOutcome::HotDrop) => return FieldOutcome::HotDrop,
        Err(FieldOutcome::NotMatched) => return FieldOutcome::NotMatched,
        Err(FieldOutcome::Matched(_)) => unreachable!(),
    };

    match field {
        UdpField::SourcePort => {
            FieldOutcome::Matched(u16::from_be_bytes([transport[0], transport[1]]))
        }
        UdpField::DestinationPort => {
            FieldOutcome::Matched(u16::from_be_bytes([transport[2], transport[3]]))
        }
    }
}

fn icmp_field(packet: &FilterPacket<'_>) -> FieldOutcome<(u8, u8)> {
    if l4_unavailable(packet.header, PROTOCOL_ICMP) {
        return FieldOutcome::NotMatched;
    }

    let transport = match transport_slice(packet, ICMP_HEADER_LEN) {
        Ok(slice) => slice,
        Err(FieldOutcome::HotDrop) => return FieldOutcome::HotDrop,
        Err(FieldOutcome::NotMatched) => return FieldOutcome::NotMatched,
        Err(FieldOutcome::Matched(_)) => unreachable!(),
    };

    FieldOutcome::Matched((transport[0], transport[1]))
}

#[cfg(unittest)]
mod tests {
    use alloc::{string::ToString, vec, vec::Vec};

    use etherparse::{IpFragOffset, IpNumber, Ipv4Header as EtherIpv4Header};
    use unittest::def_test;

    use super::*;
    use crate::ip::Ipv4Cidr;

    fn built_in_chains() -> Vec<super::super::model::Chain> {
        use super::super::model::Chain;

        vec![
            Chain::built_in(
                "INPUT".to_string(),
                FilterHook::Input,
                Verdict::Accept,
                Vec::new(),
            ),
            Chain::built_in(
                "FORWARD".to_string(),
                FilterHook::Forward,
                Verdict::Drop,
                Vec::new(),
            ),
            Chain::built_in(
                "OUTPUT".to_string(),
                FilterHook::Output,
                Verdict::Accept,
                Vec::new(),
            ),
        ]
    }

    fn with_input_rules(
        mut chains: Vec<super::super::model::Chain>,
        policy: Verdict,
        rules: Vec<Rule>,
    ) -> Vec<super::super::model::Chain> {
        use super::super::model::Chain;

        chains[0] = Chain::built_in("INPUT".to_string(), FilterHook::Input, policy, rules);
        chains
    }

    fn ipv4_packet(protocol: u8, payload: &[u8]) -> Vec<u8> {
        let mut header = EtherIpv4Header::new(
            payload.len() as u16,
            64,
            IpNumber(protocol),
            Ipv4Address::new(192, 0, 2, 1).octets(),
            Ipv4Address::new(192, 0, 2, 2).octets(),
        )
        .unwrap();
        header.header_checksum = header.calc_header_checksum();

        let mut packet = header.to_bytes().to_vec();
        packet.extend_from_slice(payload);
        packet
    }

    fn tcp_payload(src_port: u16, dst_port: u16, flags: u8) -> Vec<u8> {
        let mut payload = vec![0u8; TCP_HEADER_LEN];
        payload[0..2].copy_from_slice(&src_port.to_be_bytes());
        payload[2..4].copy_from_slice(&dst_port.to_be_bytes());
        payload[TCP_FLAGS_BYTE_OFFSET] = flags;
        payload
    }

    fn udp_payload(src_port: u16, dst_port: u16) -> Vec<u8> {
        let mut payload = vec![0u8; UDP_HEADER_LEN];
        payload[0..2].copy_from_slice(&src_port.to_be_bytes());
        payload[2..4].copy_from_slice(&dst_port.to_be_bytes());
        payload
    }

    fn icmp_payload(icmp_type: u8, code: u8) -> Vec<u8> {
        vec![icmp_type, code, 0, 0, 0, 0, 0, 0]
    }

    fn evaluate_input(table: &FilterTable, packet: &[u8], indev: &str) -> Verdict {
        let filter_packet = FilterPacket::parse(packet, indev, "eth1").unwrap();
        table.evaluate(FilterHook::Input, &filter_packet)
    }

    #[def_test]
    fn empty_builtin_chain_applies_accept_policy() {
        let table = FilterTable::new(built_in_chains()).unwrap();
        let packet = ipv4_packet(PROTOCOL_UDP, &udp_payload(53, 1024));

        assert_eq!(evaluate_input(&table, &packet, "lo"), Verdict::Accept);
    }

    #[def_test]
    fn rule_matches_require_all_conditions() {
        let chains = with_input_rules(
            built_in_chains(),
            Verdict::Accept,
            vec![
                Rule::new(
                    vec![
                        Match::new(MatchKind::Protocol(PROTOCOL_TCP), false),
                        Match::new(
                            MatchKind::TcpDestinationPort(PortRange::new(443, 443).unwrap()),
                            false,
                        ),
                    ],
                    Target::Verdict(Verdict::Drop),
                ),
                Rule::new(Vec::new(), Target::Verdict(Verdict::Accept)),
            ],
        );

        let table = FilterTable::new(chains).unwrap();
        let tcp_https = ipv4_packet(PROTOCOL_TCP, &tcp_payload(1234, 443, 0x02));
        let tcp_other = ipv4_packet(PROTOCOL_TCP, &tcp_payload(1234, 80, 0x02));
        let udp_packet = ipv4_packet(PROTOCOL_UDP, &udp_payload(1234, 443));

        assert_eq!(evaluate_input(&table, &tcp_https, "eth0"), Verdict::Drop);
        assert_eq!(evaluate_input(&table, &tcp_other, "eth0"), Verdict::Accept);
        assert_eq!(evaluate_input(&table, &udp_packet, "eth0"), Verdict::Accept);
    }

    #[def_test]
    fn inverted_match_negates_one_condition() {
        let chains = with_input_rules(
            built_in_chains(),
            Verdict::Accept,
            vec![Rule::new(
                vec![Match::new(MatchKind::Protocol(PROTOCOL_TCP), true)],
                Target::Verdict(Verdict::Drop),
            )],
        );

        let table = FilterTable::new(chains).unwrap();
        let tcp_packet = ipv4_packet(PROTOCOL_TCP, &tcp_payload(1, 2, 0));
        let udp_packet = ipv4_packet(PROTOCOL_UDP, &udp_payload(1, 2));

        assert_eq!(evaluate_input(&table, &tcp_packet, "eth0"), Verdict::Accept);
        assert_eq!(evaluate_input(&table, &udp_packet, "eth0"), Verdict::Drop);
    }

    #[def_test]
    fn address_interface_and_l4_matches_apply() {
        let chains = with_input_rules(
            built_in_chains(),
            Verdict::Accept,
            vec![Rule::new(
                vec![
                    Match::new(
                        MatchKind::SourceAddress(Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 0), 24)),
                        false,
                    ),
                    Match::new(
                        MatchKind::DestinationAddress(Ipv4Cidr::new(
                            Ipv4Address::new(192, 0, 2, 0),
                            24,
                        )),
                        false,
                    ),
                    Match::new(
                        MatchKind::InputInterface(InterfaceMatch::Prefix("eth".to_string())),
                        false,
                    ),
                    Match::new(
                        MatchKind::OutputInterface(InterfaceMatch::Exact("eth1".to_string())),
                        false,
                    ),
                    Match::new(MatchKind::Protocol(PROTOCOL_TCP), false),
                    Match::new(
                        MatchKind::TcpSourcePort(PortRange::new(1000, 2000).unwrap()),
                        false,
                    ),
                    Match::new(
                        MatchKind::TcpDestinationPort(PortRange::new(80, 80).unwrap()),
                        false,
                    ),
                    Match::new(
                        MatchKind::TcpFlags(TcpFlagsMatch::new(0x12, 0x02).unwrap()),
                        false,
                    ),
                ],
                Target::Verdict(Verdict::Drop),
            )],
        );

        let table = FilterTable::new(chains).unwrap();
        let matching = ipv4_packet(PROTOCOL_TCP, &tcp_payload(1024, 80, 0x02));

        assert_eq!(evaluate_input(&table, &matching, "eth0"), Verdict::Drop);
        assert_eq!(evaluate_input(&table, &matching, "lo"), Verdict::Accept);
    }

    #[def_test]
    fn icmp_match_honors_type_and_code_range() {
        let chains = with_input_rules(
            built_in_chains(),
            Verdict::Accept,
            vec![Rule::new(
                vec![Match::new(
                    MatchKind::Icmp(IcmpMatch::new(Some(3), 0, 1).unwrap()),
                    false,
                )],
                Target::Verdict(Verdict::Drop),
            )],
        );

        let table = FilterTable::new(chains).unwrap();
        let matching = ipv4_packet(PROTOCOL_ICMP, &icmp_payload(3, 1));
        let wrong_code = ipv4_packet(PROTOCOL_ICMP, &icmp_payload(3, 2));

        assert_eq!(evaluate_input(&table, &matching, "eth0"), Verdict::Drop);
        assert_eq!(evaluate_input(&table, &wrong_code, "eth0"), Verdict::Accept);
    }

    #[def_test]
    fn continue_target_falls_through_to_next_rule() {
        let chains = with_input_rules(
            built_in_chains(),
            Verdict::Accept,
            vec![
                Rule::new(
                    vec![Match::new(MatchKind::Protocol(PROTOCOL_UDP), false)],
                    Target::Continue,
                ),
                Rule::new(Vec::new(), Target::Verdict(Verdict::Drop)),
            ],
        );

        let table = FilterTable::new(chains).unwrap();
        let packet = ipv4_packet(PROTOCOL_UDP, &udp_payload(1, 2));

        assert_eq!(evaluate_input(&table, &packet, "eth0"), Verdict::Drop);
    }

    #[def_test]
    fn jump_and_return_resume_caller_rule() {
        let mut chains = with_input_rules(
            built_in_chains(),
            Verdict::Accept,
            vec![
                Rule::new(
                    vec![Match::new(MatchKind::Protocol(PROTOCOL_TCP), false)],
                    Target::Jump(ChainId::from_index(3)),
                ),
                Rule::new(Vec::new(), Target::Verdict(Verdict::Accept)),
            ],
        );
        chains.push(super::super::model::Chain::user_defined(
            "TCP-CHECK".to_string(),
            vec![
                Rule::new(
                    vec![Match::new(
                        MatchKind::TcpDestinationPort(PortRange::new(22, 22).unwrap()),
                        false,
                    )],
                    Target::Verdict(Verdict::Drop),
                ),
                Rule::new(Vec::new(), Target::Return),
            ],
        ));

        let table = FilterTable::new(chains).unwrap();
        let ssh = ipv4_packet(PROTOCOL_TCP, &tcp_payload(1024, 22, 0));
        let other = ipv4_packet(PROTOCOL_TCP, &tcp_payload(1024, 80, 0));

        assert_eq!(evaluate_input(&table, &ssh, "eth0"), Verdict::Drop);
        assert_eq!(evaluate_input(&table, &other, "eth0"), Verdict::Accept);
    }

    #[def_test]
    fn builtin_return_and_chain_end_apply_policy() {
        let chains = with_input_rules(
            built_in_chains(),
            Verdict::Drop,
            vec![Rule::new(Vec::new(), Target::Return)],
        );

        let table = FilterTable::new(chains).unwrap();
        let packet = ipv4_packet(PROTOCOL_UDP, &udp_payload(1, 2));

        assert_eq!(evaluate_input(&table, &packet, "eth0"), Verdict::Drop);
    }

    #[def_test]
    fn non_first_fragment_skips_l4_match_without_hotdrop() {
        let chains = with_input_rules(
            built_in_chains(),
            Verdict::Accept,
            vec![Rule::new(
                vec![Match::new(
                    MatchKind::TcpDestinationPort(PortRange::new(80, 80).unwrap()),
                    false,
                )],
                Target::Verdict(Verdict::Drop),
            )],
        );

        let table = FilterTable::new(chains).unwrap();
        let mut packet = ipv4_packet(PROTOCOL_TCP, &tcp_payload(1, 80, 0));
        let mut header = EtherIpv4Header::from_slice(&packet[..IPV4_MIN_HEADER_LEN])
            .unwrap()
            .0;
        header.fragment_offset = IpFragOffset::try_new(2).unwrap();
        header.more_fragments = true;
        header.header_checksum = header.calc_header_checksum();
        packet[..IPV4_MIN_HEADER_LEN].copy_from_slice(&header.to_bytes());

        assert_eq!(evaluate_input(&table, &packet, "eth0"), Verdict::Accept);
    }

    #[def_test]
    fn truncated_l4_header_hotdrops_when_tcp_match_present() {
        let chains = with_input_rules(
            built_in_chains(),
            Verdict::Accept,
            vec![Rule::new(
                vec![Match::new(
                    MatchKind::TcpDestinationPort(PortRange::new(80, 80).unwrap()),
                    false,
                )],
                Target::Verdict(Verdict::Accept),
            )],
        );

        let table = FilterTable::new(chains).unwrap();
        let mut packet = ipv4_packet(PROTOCOL_TCP, &[]);
        packet.truncate(IPV4_MIN_HEADER_LEN + 4);

        assert_eq!(evaluate_input(&table, &packet, "eth0"), Verdict::Drop);
    }

    #[def_test]
    fn jump_stack_overflow_drops_packet() {
        let mut chains = with_input_rules(
            built_in_chains(),
            Verdict::Accept,
            vec![Rule::new(Vec::new(), Target::Jump(ChainId::from_index(3)))],
        );

        for index in 0..=MAX_JUMP_STACK_DEPTH {
            let name = alloc::format!("CHAIN-{index}");
            let target = if index == MAX_JUMP_STACK_DEPTH {
                Target::Verdict(Verdict::Accept)
            } else {
                Target::Jump(ChainId::from_index(index + 4))
            };
            chains.push(super::super::model::Chain::user_defined(
                name,
                vec![Rule::new(Vec::new(), target)],
            ));
        }

        let table = FilterTable::new(chains).unwrap();
        let packet = ipv4_packet(PROTOCOL_UDP, &udp_payload(1, 2));

        assert_eq!(evaluate_input(&table, &packet, "eth0"), Verdict::Drop);
    }

    #[def_test]
    fn inverted_port_skips_later_fragment() {
        let table = FilterTable::new(with_input_rules(
            built_in_chains(),
            Verdict::Drop,
            vec![Rule::new(
                vec![
                    Match::new(MatchKind::Protocol(PROTOCOL_TCP), false),
                    Match::new(
                        MatchKind::TcpDestinationPort(PortRange::new(80, 80).unwrap()),
                        true,
                    ),
                ],
                Target::Verdict(Verdict::Accept),
            )],
        ))
        .unwrap();
        let mut packet = ipv4_packet(PROTOCOL_TCP, &[0; 24]);
        let mut header = EtherIpv4Header::from_slice(&packet).unwrap().0;
        header.fragment_offset = IpFragOffset::try_new(2).unwrap();
        header.more_fragments = true;
        header.header_checksum = header.calc_header_checksum();
        packet[..IPV4_MIN_HEADER_LEN].copy_from_slice(&header.to_bytes());

        assert_eq!(evaluate_input(&table, &packet, "eth0"), Verdict::Drop);
    }

    #[def_test]
    fn tcp_header_must_fit_declared_ipv4_length() {
        let table = FilterTable::new(with_input_rules(
            built_in_chains(),
            Verdict::Drop,
            vec![Rule::new(
                vec![Match::new(
                    MatchKind::TcpDestinationPort(PortRange::new(80, 80).unwrap()),
                    false,
                )],
                Target::Verdict(Verdict::Accept),
            )],
        ))
        .unwrap();
        let mut packet = ipv4_packet(PROTOCOL_TCP, &[]);
        packet.extend_from_slice(&tcp_payload(1024, 80, 0x02));

        assert_eq!(evaluate_input(&table, &packet, "eth0"), Verdict::Drop);
    }

    #[def_test]
    fn icmp_short_header_hotdrops() {
        let table = FilterTable::new(with_input_rules(
            built_in_chains(),
            Verdict::Accept,
            vec![Rule::new(
                vec![Match::new(
                    MatchKind::Icmp(IcmpMatch::new(Some(8), 0, 0).unwrap()),
                    false,
                )],
                Target::Verdict(Verdict::Accept),
            )],
        ))
        .unwrap();
        let packet = ipv4_packet(PROTOCOL_ICMP, &[8, 0]);

        assert_eq!(evaluate_input(&table, &packet, "eth0"), Verdict::Drop);
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 `filter` table chain evaluator.
//!
//! Evaluates a validated [`FilterTable`] against one IPv4 packet using the
//! semantics of Linux 6.8 `ipt_do_table`.

use super::{
    matcher::{FilterPacket, MatchOutcome, evaluate_matches},
    model::{ChainId, ChainKind, FilterHook, FilterTable, Rule, Target, Verdict},
};

/// Maximum user-chain nesting tracked on the preallocated jump stack.
///
/// Deeper recursion returns [`Verdict::Drop`], matching Linux stack overflow
/// handling in `ipt_do_table`.
pub(crate) const MAX_JUMP_STACK_DEPTH: usize = 8;

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
                    .is_some_and(|chain| matches!(chain.kind(), ChainKind::UserDefined { .. }))
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
        ChainKind::BuiltIn { policy, .. } => *policy,
        ChainKind::UserDefined { .. } => Verdict::Drop,
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::{string::ToString, vec, vec::Vec};

    use etherparse::{IpFragOffset, IpNumber, Ipv4Header as EtherIpv4Header, TcpHeader, UdpHeader};
    use unittest::def_test;

    use super::{
        super::{
            matcher::{
                IcmpMatch, InterfaceMatch, Match, MatchKind, PortRange, TcpFlagsMatch, TcpMatch,
                UdpMatch,
            },
            model::{Chain, Rule},
        },
        *,
    };
    use crate::{
        ip::{Ipv4Address, Ipv4Cidr},
        stack::ipv4::{IPV4_MIN_HEADER_LEN, PROTOCOL_ICMP, PROTOCOL_TCP, PROTOCOL_UDP},
    };

    fn built_in_chains() -> Vec<Chain> {
        vec![
            Chain::built_in(FilterHook::Input, Verdict::Accept, Vec::new()),
            Chain::built_in(FilterHook::Forward, Verdict::Drop, Vec::new()),
            Chain::built_in(FilterHook::Output, Verdict::Accept, Vec::new()),
        ]
    }

    fn with_input_rules(mut chains: Vec<Chain>, policy: Verdict, rules: Vec<Rule>) -> Vec<Chain> {
        chains[0] = Chain::built_in(FilterHook::Input, policy, rules);
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
        let mut header = TcpHeader::new(src_port, dst_port, 0, 0);
        header.syn = flags & 0x02 != 0;
        header.ack = flags & 0x10 != 0;
        header.to_bytes().to_vec()
    }

    fn udp_payload(src_port: u16, dst_port: u16) -> Vec<u8> {
        UdpHeader::without_ipv4_checksum(src_port, dst_port, 0)
            .unwrap()
            .to_bytes()
            .to_vec()
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
                            MatchKind::Tcp(TcpMatch::destination_port(
                                PortRange::new(443, 443).unwrap(),
                            )),
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

        // Linux port matching only requires the fixed TCP header, even when
        // the data offset describes an invalid or incomplete options area.
        for data_offset in [0, 4, 6, 15] {
            let mut payload = tcp_payload(1234, 80, 0x02);
            payload[12] = data_offset << 4;
            let packet = ipv4_packet(PROTOCOL_TCP, &payload);
            assert_eq!(
                evaluate_input(&table, &packet, "eth0"),
                Verdict::Accept,
                "TCP data offset {data_offset} must not hot-drop a port mismatch"
            );
        }
    }

    #[def_test]
    fn udp_matches_honor_port_ranges_and_independent_inversion() {
        for (invert_source, invert_destination) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let table = FilterTable::new(with_input_rules(
                built_in_chains(),
                Verdict::Drop,
                vec![Rule::new(
                    vec![
                        Match::new(
                            MatchKind::Udp(UdpMatch::source_port(
                                PortRange::new(1000, 2000).unwrap(),
                            )),
                            invert_source,
                        ),
                        Match::new(
                            MatchKind::Udp(UdpMatch::destination_port(
                                PortRange::new(53, 54).unwrap(),
                            )),
                            invert_destination,
                        ),
                    ],
                    Target::Verdict(Verdict::Accept),
                )],
            ))
            .unwrap();

            for (source, destination, source_matches, destination_matches) in [
                (1000, 53, true, true),
                (2000, 54, true, true),
                (999, 53, false, true),
                (2001, 54, false, true),
                (1000, 52, true, false),
                (2000, 55, true, false),
                (999, 55, false, false),
            ] {
                let expected = if (source_matches ^ invert_source)
                    && (destination_matches ^ invert_destination)
                {
                    Verdict::Accept
                } else {
                    Verdict::Drop
                };
                let packet = ipv4_packet(PROTOCOL_UDP, &udp_payload(source, destination));
                assert_eq!(
                    evaluate_input(&table, &packet, "eth0"),
                    expected,
                    "UDP {source}:{destination}, inversion {invert_source}:{invert_destination}"
                );
            }
        }
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
                        MatchKind::SourceAddress(
                            Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 0), 24).unwrap(),
                        ),
                        false,
                    ),
                    Match::new(
                        MatchKind::DestinationAddress(
                            Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 0), 24).unwrap(),
                        ),
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
                        MatchKind::Tcp(TcpMatch::source_port(PortRange::new(1000, 2000).unwrap())),
                        false,
                    ),
                    Match::new(
                        MatchKind::Tcp(TcpMatch::destination_port(PortRange::new(80, 80).unwrap())),
                        false,
                    ),
                    Match::new(
                        MatchKind::Tcp(TcpMatch::flags(TcpFlagsMatch::new(0x12, 0x02).unwrap())),
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
        for (expected_type, packet_type, code, payload_len, expected) in [
            (3, 3, 1, 8, Verdict::Drop),
            (3, 3, 2, 8, Verdict::Accept),
            (3, 8, 0, 8, Verdict::Accept),
            (3, 13, 0, 8, Verdict::Accept),
            (3, 13, 0, 20, Verdict::Accept),
            (3, 13, 0, 24, Verdict::Accept),
            (3, 14, 0, 8, Verdict::Accept),
            (3, 14, 0, 20, Verdict::Accept),
            (3, 14, 0, 24, Verdict::Accept),
            (13, 13, 0, 8, Verdict::Drop),
            (13, 13, 0, 20, Verdict::Drop),
            (13, 13, 0, 24, Verdict::Drop),
            (14, 14, 0, 8, Verdict::Drop),
            (14, 14, 0, 20, Verdict::Drop),
            (14, 14, 0, 24, Verdict::Drop),
        ] {
            let table = FilterTable::new(with_input_rules(
                built_in_chains(),
                Verdict::Accept,
                vec![Rule::new(
                    vec![Match::new(
                        MatchKind::Icmp(IcmpMatch::new(Some(expected_type), 0, 1).unwrap()),
                        false,
                    )],
                    Target::Verdict(Verdict::Drop),
                )],
            ))
            .unwrap();
            let mut payload = icmp_payload(packet_type, code);
            payload.resize(payload_len, 0);
            let packet = ipv4_packet(PROTOCOL_ICMP, &payload);
            assert_eq!(
                evaluate_input(&table, &packet, "eth0"),
                expected,
                "ICMP rule type {expected_type}, packet {packet_type}/{code}, length {payload_len}"
            );
        }
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
        chains.push(Chain::user_defined(
            "TCP-CHECK".to_string(),
            vec![
                Rule::new(
                    vec![Match::new(
                        MatchKind::Tcp(TcpMatch::destination_port(PortRange::new(22, 22).unwrap())),
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
                    MatchKind::Tcp(TcpMatch::destination_port(PortRange::new(80, 80).unwrap())),
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
    fn l4_matches_require_base_headers_within_ipv4_length() {
        let range = PortRange::new(80, 80).unwrap();
        for (protocol, kind, payload) in [
            (
                PROTOCOL_TCP,
                MatchKind::Tcp(TcpMatch::destination_port(range)),
                tcp_payload(1, 80, 0),
            ),
            (
                PROTOCOL_UDP,
                MatchKind::Udp(UdpMatch::destination_port(range)),
                udp_payload(1, 80),
            ),
            (
                PROTOCOL_ICMP,
                MatchKind::Icmp(IcmpMatch::new(Some(8), 0, 0).unwrap()),
                icmp_payload(8, 0),
            ),
        ] {
            let table = FilterTable::new(with_input_rules(
                built_in_chains(),
                Verdict::Accept,
                vec![Rule::new(
                    vec![Match::new(kind, false)],
                    Target::Verdict(Verdict::Accept),
                )],
            ))
            .unwrap();

            for payload_len in [0, 2, 4, payload.len() - 1, payload.len()] {
                let expected = if payload_len == payload.len() {
                    Verdict::Accept
                } else {
                    Verdict::Drop
                };
                let mut packet = ipv4_packet(protocol, &payload[..payload_len]);
                // Trailing link-layer padding must not complete a short header.
                packet.extend_from_slice(&payload[payload_len..]);
                assert_eq!(
                    evaluate_input(&table, &packet, "eth0"),
                    expected,
                    "protocol {protocol}, IPv4 payload length {payload_len}"
                );
            }
        }
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
            chains.push(Chain::user_defined(
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
                        MatchKind::Tcp(TcpMatch::destination_port(PortRange::new(80, 80).unwrap())),
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
}

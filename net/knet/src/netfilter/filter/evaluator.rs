// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 `filter` table chain evaluator.
//!
//! Evaluates a validated [`FilterTable`] against one IPv4 packet using the
//! semantics of Linux 6.8 `ipt_do_table`.

use super::{
    matcher::{FilterPacket, MatchOutcome, evaluate_rule},
    model::{ChainId, ChainKind, FilterHook, FilterTable, MAX_JUMP_DEPTH, Target, Verdict},
};

/// Maximum number of rules evaluated for one packet.
///
/// This also bounds repeated evaluation of shared user-defined chains.
pub(crate) const MAX_RULE_STEPS: usize = 4096;

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
    let mut jump_stack: [Option<JumpFrame>; MAX_JUMP_DEPTH] = [None; MAX_JUMP_DEPTH];
    let mut stack_top = 0usize;
    let mut rule_steps = 0usize;

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
        if rule_steps >= MAX_RULE_STEPS {
            return Verdict::Drop;
        }
        rule_steps += 1;

        match evaluate_rule(rule, packet) {
            MatchOutcome::HotDrop => return Verdict::Drop,
            MatchOutcome::NotMatched => {
                rule_index += 1;
                continue;
            }
            MatchOutcome::Matched => {}
        }

        match rule.target() {
            Target::Continue => rule_index += 1,
            Target::Verdict(verdict) => return verdict,
            Target::Jump(target) => {
                if stack_top >= MAX_JUMP_DEPTH {
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
            Target::Return => {
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
                BaseMatch, ExtensionMatch, IcmpMatch, InterfaceMatch, Negated, PortRange,
                TcpFlagsMatch, TcpMatch, UdpMatch,
            },
            model::{Chain, Rule},
        },
        *,
    };
    use crate::{
        ip::{Ipv4Address, Ipv4Cidr},
        ipv4::{IPV4_MIN_HEADER_LEN, PROTOCOL_ICMP, PROTOCOL_TCP, PROTOCOL_UDP},
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

    fn rule(base: BaseMatch, extensions: Vec<ExtensionMatch>, target: Target) -> Rule {
        Rule::new(base, extensions, target).expect("test rule must be valid")
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
                rule(
                    BaseMatch {
                        protocol: Some(Negated::new(PROTOCOL_TCP, false)),
                        ..BaseMatch::default()
                    },
                    vec![ExtensionMatch::Tcp(TcpMatch::new().with_destination_port(
                        Negated::new(PortRange::new(443, 443).unwrap(), false),
                    ))],
                    Target::Verdict(Verdict::Drop),
                ),
                rule(
                    BaseMatch::default(),
                    Vec::new(),
                    Target::Verdict(Verdict::Accept),
                ),
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
    fn base_conditions_apply_inversion() {
        let bytes = ipv4_packet(PROTOCOL_TCP, &[]);
        let packet = FilterPacket::parse(&bytes, "eth0", "eth1").unwrap();

        for is_inverted in [false, true] {
            for value_matches in [false, true] {
                let source = if value_matches { 1 } else { 3 };
                let destination = if value_matches { 2 } else { 3 };
                let input_interface = if value_matches { "eth" } else { "lo" };
                let output_interface = if value_matches { "eth1" } else { "eth0" };
                let protocol = if value_matches {
                    PROTOCOL_TCP
                } else {
                    PROTOCOL_UDP
                };

                for base in [
                    BaseMatch {
                        source: Some(Negated::new(
                            Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, source), 32).unwrap(),
                            is_inverted,
                        )),
                        ..BaseMatch::default()
                    },
                    BaseMatch {
                        destination: Some(Negated::new(
                            Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, destination), 32).unwrap(),
                            is_inverted,
                        )),
                        ..BaseMatch::default()
                    },
                    BaseMatch {
                        input_interface: Some(Negated::new(
                            InterfaceMatch::prefix(input_interface.to_string()).unwrap(),
                            is_inverted,
                        )),
                        ..BaseMatch::default()
                    },
                    BaseMatch {
                        output_interface: Some(Negated::new(
                            InterfaceMatch::exact(output_interface.to_string()).unwrap(),
                            is_inverted,
                        )),
                        ..BaseMatch::default()
                    },
                    BaseMatch {
                        protocol: Some(Negated::new(protocol, is_inverted)),
                        ..BaseMatch::default()
                    },
                ] {
                    let rule = rule(base, Vec::new(), Target::Continue);
                    let expected = if value_matches ^ is_inverted {
                        MatchOutcome::Matched
                    } else {
                        MatchOutcome::NotMatched
                    };
                    assert_eq!(evaluate_rule(&rule, &packet), expected, "rule {rule:?}");
                }
            }
        }
    }

    #[def_test]
    fn base_mismatch_skips_extension_payload_read() {
        let bytes = ipv4_packet(PROTOCOL_TCP, &[]);
        let packet = FilterPacket::parse(&bytes, "eth0", "eth1").unwrap();
        let rule = rule(
            BaseMatch {
                source: Some(Negated::new(
                    Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 3), 32).unwrap(),
                    false,
                )),
                protocol: Some(Negated::new(PROTOCOL_TCP, false)),
                ..BaseMatch::default()
            },
            vec![ExtensionMatch::Tcp(TcpMatch::new())],
            Target::Continue,
        );

        assert_eq!(evaluate_rule(&rule, &packet), MatchOutcome::NotMatched);
    }

    #[def_test]
    fn udp_matches_honor_port_ranges_and_independent_inversion() {
        for (invert_source, invert_destination) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let table = FilterTable::new(with_input_rules(
                built_in_chains(),
                Verdict::Drop,
                vec![rule(
                    BaseMatch {
                        protocol: Some(Negated::new(PROTOCOL_UDP, false)),
                        ..BaseMatch::default()
                    },
                    vec![ExtensionMatch::Udp(
                        UdpMatch::new()
                            .with_source_port(Negated::new(
                                PortRange::new(1000, 2000).unwrap(),
                                invert_source,
                            ))
                            .with_destination_port(Negated::new(
                                PortRange::new(53, 54).unwrap(),
                                invert_destination,
                            )),
                    )],
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
            vec![rule(
                BaseMatch {
                    protocol: Some(Negated::new(PROTOCOL_TCP, true)),
                    ..BaseMatch::default()
                },
                Vec::new(),
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
        for inversion in 0u8..8 {
            let chains = with_input_rules(
                built_in_chains(),
                Verdict::Accept,
                vec![rule(
                    BaseMatch {
                        source: Some(Negated::new(
                            Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 0), 24).unwrap(),
                            false,
                        )),
                        destination: Some(Negated::new(
                            Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 0), 24).unwrap(),
                            false,
                        )),
                        input_interface: Some(Negated::new(
                            InterfaceMatch::prefix("eth".to_string()).unwrap(),
                            false,
                        )),
                        output_interface: Some(Negated::new(
                            InterfaceMatch::exact("eth1".to_string()).unwrap(),
                            false,
                        )),
                        protocol: Some(Negated::new(PROTOCOL_TCP, false)),
                    },
                    vec![ExtensionMatch::Tcp(
                        TcpMatch::new()
                            .with_source_port(Negated::new(
                                PortRange::new(1000, 2000).unwrap(),
                                inversion & 1 != 0,
                            ))
                            .with_destination_port(Negated::new(
                                PortRange::new(80, 80).unwrap(),
                                inversion & 2 != 0,
                            ))
                            .with_flags(Negated::new(
                                TcpFlagsMatch::new(0x12, 0x02).unwrap(),
                                inversion & 4 != 0,
                            )),
                    )],
                    Target::Verdict(Verdict::Drop),
                )],
            );

            let table = FilterTable::new(chains).unwrap();
            for mismatch in 0u8..8 {
                let source = if mismatch & 1 == 0 { 1024 } else { 999 };
                let destination = if mismatch & 2 == 0 { 80 } else { 81 };
                let flags = if mismatch & 4 == 0 { 0x02 } else { 0x10 };
                let packet = ipv4_packet(PROTOCOL_TCP, &tcp_payload(source, destination, flags));
                let expected = if mismatch == inversion {
                    Verdict::Drop
                } else {
                    Verdict::Accept
                };
                assert_eq!(
                    evaluate_input(&table, &packet, "eth0"),
                    expected,
                    "TCP mismatch {mismatch}, inversion {inversion}"
                );
                assert_eq!(evaluate_input(&table, &packet, "lo"), Verdict::Accept);
            }
        }
    }

    #[def_test]
    fn icmp_match_honors_type_and_code_range() {
        for is_inverted in [false, true] {
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
                    vec![rule(
                        BaseMatch {
                            protocol: Some(Negated::new(PROTOCOL_ICMP, false)),
                            ..BaseMatch::default()
                        },
                        vec![ExtensionMatch::Icmp(
                            IcmpMatch::new(Some(expected_type), 0, 1)
                                .unwrap()
                                .with_inversion(is_inverted),
                        )],
                        Target::Verdict(Verdict::Drop),
                    )],
                ))
                .unwrap();
                let mut payload = icmp_payload(packet_type, code);
                payload.resize(payload_len, 0);
                let packet = ipv4_packet(PROTOCOL_ICMP, &payload);
                let expected = if is_inverted {
                    if expected == Verdict::Drop {
                        Verdict::Accept
                    } else {
                        Verdict::Drop
                    }
                } else {
                    expected
                };
                assert_eq!(
                    evaluate_input(&table, &packet, "eth0"),
                    expected,
                    "ICMP rule type {expected_type}, packet {packet_type}/{code}, length \
                     {payload_len}"
                );
            }
        }
    }

    #[def_test]
    fn continue_target_falls_through_to_next_rule() {
        let chains = with_input_rules(
            built_in_chains(),
            Verdict::Accept,
            vec![
                rule(
                    BaseMatch {
                        protocol: Some(Negated::new(PROTOCOL_UDP, false)),
                        ..BaseMatch::default()
                    },
                    Vec::new(),
                    Target::Continue,
                ),
                rule(
                    BaseMatch::default(),
                    Vec::new(),
                    Target::Verdict(Verdict::Drop),
                ),
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
                rule(
                    BaseMatch {
                        protocol: Some(Negated::new(PROTOCOL_TCP, false)),
                        ..BaseMatch::default()
                    },
                    Vec::new(),
                    Target::Jump(ChainId::from_index(3)),
                ),
                rule(
                    BaseMatch::default(),
                    Vec::new(),
                    Target::Verdict(Verdict::Accept),
                ),
            ],
        );
        chains.push(Chain::user_defined(
            "TCP-CHECK".to_string(),
            vec![
                rule(
                    BaseMatch {
                        protocol: Some(Negated::new(PROTOCOL_TCP, false)),
                        ..BaseMatch::default()
                    },
                    vec![ExtensionMatch::Tcp(TcpMatch::new().with_destination_port(
                        Negated::new(PortRange::new(22, 22).unwrap(), false),
                    ))],
                    Target::Verdict(Verdict::Drop),
                ),
                rule(BaseMatch::default(), Vec::new(), Target::Return),
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
            vec![rule(BaseMatch::default(), Vec::new(), Target::Return)],
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
            vec![rule(
                BaseMatch {
                    protocol: Some(Negated::new(PROTOCOL_TCP, false)),
                    ..BaseMatch::default()
                },
                vec![ExtensionMatch::Tcp(TcpMatch::new().with_destination_port(
                    Negated::new(PortRange::new(80, 80).unwrap(), false),
                ))],
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
        for is_inverted in [false, true] {
            let range = Negated::new(PortRange::new(80, 80).unwrap(), is_inverted);
            for (protocol, kind, payload) in [
                (
                    PROTOCOL_TCP,
                    ExtensionMatch::Tcp(TcpMatch::new().with_destination_port(range)),
                    tcp_payload(1, 80, 0),
                ),
                (
                    PROTOCOL_UDP,
                    ExtensionMatch::Udp(UdpMatch::new().with_destination_port(range)),
                    udp_payload(1, 80),
                ),
                (
                    PROTOCOL_ICMP,
                    ExtensionMatch::Icmp(
                        IcmpMatch::new(Some(8), 0, 0)
                            .unwrap()
                            .with_inversion(is_inverted),
                    ),
                    icmp_payload(8, 0),
                ),
            ] {
                let table = FilterTable::new(with_input_rules(
                    built_in_chains(),
                    Verdict::Accept,
                    vec![rule(
                        BaseMatch {
                            protocol: Some(Negated::new(protocol, false)),
                            ..BaseMatch::default()
                        },
                        vec![kind],
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
    }

    #[def_test]
    fn rule_step_limit_drops_packet() {
        let mut chains = with_input_rules(
            built_in_chains(),
            Verdict::Accept,
            vec![
                rule(
                    BaseMatch::default(),
                    Vec::new(),
                    Target::Jump(ChainId::from_index(3))
                );
                MAX_RULE_STEPS / 2 + 1
            ],
        );
        chains.push(Chain::user_defined(
            "SHARED".to_string(),
            vec![rule(BaseMatch::default(), Vec::new(), Target::Return)],
        ));

        let table = FilterTable::new(chains).unwrap();
        let packet = ipv4_packet(PROTOCOL_UDP, &udp_payload(1, 2));

        assert_eq!(evaluate_input(&table, &packet, "eth0"), Verdict::Drop);
    }

    #[def_test]
    fn inverted_port_skips_later_fragment() {
        let table = FilterTable::new(with_input_rules(
            built_in_chains(),
            Verdict::Drop,
            vec![rule(
                BaseMatch {
                    protocol: Some(Negated::new(PROTOCOL_TCP, false)),
                    ..BaseMatch::default()
                },
                vec![ExtensionMatch::Tcp(TcpMatch::new().with_destination_port(
                    Negated::new(PortRange::new(80, 80).unwrap(), true),
                ))],
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

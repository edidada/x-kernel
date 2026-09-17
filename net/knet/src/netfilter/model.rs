// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Immutable rule graph for the IPv4 `filter` table.

use alloc::{string::String, vec, vec::Vec};

use crate::ip::Ipv4Cidr;

const BUILTIN_CHAIN_COUNT: usize = 3;

/// IPv4 hooks supported by the initial `filter` table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FilterHook {
    Input,
    Forward,
    Output,
}

impl FilterHook {
    const ALL: [Self; BUILTIN_CHAIN_COUNT] = [Self::Input, Self::Forward, Self::Output];

    const fn index(self) -> usize {
        match self {
            Self::Input => 0,
            Self::Forward => 1,
            Self::Output => 2,
        }
    }

    const fn chain_name(self) -> &'static str {
        match self {
            Self::Input => "INPUT",
            Self::Forward => "FORWARD",
            Self::Output => "OUTPUT",
        }
    }
}

/// A terminal packet-filtering decision.
///
/// Chain-local control such as `RETURN` remains a [`Target`] so callers of a
/// built-in chain always receive a terminal result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Verdict {
    Accept,
    Drop,
}

/// Index of a chain within one [`FilterTable`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ChainId(usize);

impl ChainId {
    pub(crate) const fn from_index(index: usize) -> Self {
        Self(index)
    }

    pub(crate) const fn index(self) -> usize {
        self.0
    }
}

/// Action executed after every match in a rule succeeds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Target {
    Verdict(Verdict),
    Continue,
    Return,
    Jump(ChainId),
}

/// A closed inclusive transport-port range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PortRange {
    start: u16,
    end: u16,
}

impl PortRange {
    pub(crate) const fn new(start: u16, end: u16) -> Option<Self> {
        if start <= end {
            Some(Self { start, end })
        } else {
            None
        }
    }

    pub(crate) const fn start(self) -> u16 {
        self.start
    }

    pub(crate) const fn end(self) -> u16 {
        self.end
    }
}

/// Interface-name condition accepted from legacy iptables syntax.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InterfaceMatch {
    Exact(String),
    Prefix(String),
}

impl InterfaceMatch {
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Exact(name) | Self::Prefix(name) => name,
        }
    }

    pub(crate) const fn is_prefix(&self) -> bool {
        matches!(self, Self::Prefix(_))
    }
}

/// TCP flag mask and expected value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TcpFlagsMatch {
    mask: u8,
    expected: u8,
}

impl TcpFlagsMatch {
    pub(crate) const fn new(mask: u8, expected: u8) -> Option<Self> {
        if expected & !mask == 0 {
            Some(Self { mask, expected })
        } else {
            None
        }
    }

    pub(crate) const fn mask(self) -> u8 {
        self.mask
    }

    pub(crate) const fn expected(self) -> u8 {
        self.expected
    }
}

/// ICMP type and inclusive code range selected by the initial L4 matcher.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IcmpMatch {
    icmp_type: Option<u8>,
    code_start: u8,
    code_end: u8,
}

impl IcmpMatch {
    pub(crate) const fn new(icmp_type: Option<u8>, code_start: u8, code_end: u8) -> Option<Self> {
        if code_start <= code_end {
            Some(Self {
                icmp_type,
                code_start,
                code_end,
            })
        } else {
            None
        }
    }

    pub(crate) const fn icmp_type(self) -> Option<u8> {
        self.icmp_type
    }

    pub(crate) const fn code_start(self) -> u8 {
        self.code_start
    }

    pub(crate) const fn code_end(self) -> u8 {
        self.code_end
    }
}

/// Semantic match operations supported by the initial IPv4 filter model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MatchKind {
    SourceAddress(Ipv4Cidr),
    DestinationAddress(Ipv4Cidr),
    Protocol(u8),
    InputInterface(InterfaceMatch),
    OutputInterface(InterfaceMatch),
    TcpSourcePort(PortRange),
    TcpDestinationPort(PortRange),
    TcpFlags(TcpFlagsMatch),
    UdpSourcePort(PortRange),
    UdpDestinationPort(PortRange),
    Icmp(IcmpMatch),
}

/// One independently invertible rule condition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Match {
    kind: MatchKind,
    is_inverted: bool,
}

impl Match {
    pub(crate) fn new(kind: MatchKind, is_inverted: bool) -> Self {
        Self { kind, is_inverted }
    }

    pub(crate) const fn kind(&self) -> &MatchKind {
        &self.kind
    }

    pub(crate) const fn is_inverted(&self) -> bool {
        self.is_inverted
    }
}

/// Ordered conjunction of match conditions and its selected action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Rule {
    matches: Vec<Match>,
    target: Target,
}

impl Rule {
    pub(crate) fn new(matches: Vec<Match>, target: Target) -> Self {
        Self { matches, target }
    }

    pub(crate) fn matches(&self) -> &[Match] {
        &self.matches
    }

    pub(crate) const fn target(&self) -> Target {
        self.target
    }
}

/// Built-in or user-defined chain role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChainKind {
    BuiltIn { hook: FilterHook, policy: Verdict },
    UserDefined,
}

/// Ordered rules belonging to one named chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Chain {
    name: String,
    kind: ChainKind,
    rules: Vec<Rule>,
}

impl Chain {
    pub(crate) fn built_in(
        name: String,
        hook: FilterHook,
        policy: Verdict,
        rules: Vec<Rule>,
    ) -> Self {
        Self {
            name,
            kind: ChainKind::BuiltIn { hook, policy },
            rules,
        }
    }

    pub(crate) fn user_defined(name: String, rules: Vec<Rule>) -> Self {
        Self {
            name,
            kind: ChainKind::UserDefined,
            rules,
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) const fn kind(&self) -> ChainKind {
        self.kind
    }

    pub(crate) fn rules(&self) -> &[Rule] {
        &self.rules
    }
}

/// Structural validation failure for an immutable filter table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FilterTableError {
    EmptyChainName,
    DuplicateChainName,
    InvalidBuiltInChainName(FilterHook),
    MissingBuiltInChain(FilterHook),
    DuplicateBuiltInChain(FilterHook),
    InvalidIpv4Prefix(u8),
    InvalidJumpTarget(ChainId),
    JumpToBuiltInChain(ChainId),
    RecursiveChainGraph,
}

/// Immutable, validated representation of the IPv4 `filter` table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FilterTable {
    chains: Vec<Chain>,
    built_in_chains: [ChainId; BUILTIN_CHAIN_COUNT],
}

impl FilterTable {
    /// Builds a table and validates its chain graph.
    pub(crate) fn new(chains: Vec<Chain>) -> Result<Self, FilterTableError> {
        let mut built_in_chains = [None; BUILTIN_CHAIN_COUNT];

        for (index, chain) in chains.iter().enumerate() {
            if chain.name.is_empty() {
                return Err(FilterTableError::EmptyChainName);
            }

            if let ChainKind::BuiltIn { hook, .. } = chain.kind {
                if chain.name != hook.chain_name() {
                    return Err(FilterTableError::InvalidBuiltInChainName(hook));
                }
                let slot = &mut built_in_chains[hook.index()];
                if slot.replace(ChainId::from_index(index)).is_some() {
                    return Err(FilterTableError::DuplicateBuiltInChain(hook));
                }
            }

            if chains[..index]
                .iter()
                .any(|existing| existing.name == chain.name)
            {
                return Err(FilterTableError::DuplicateChainName);
            }

            validate_rule_matches(chain)?;
        }

        let input = built_in_chains[FilterHook::Input.index()]
            .ok_or(FilterTableError::MissingBuiltInChain(FilterHook::Input))?;
        let forward = built_in_chains[FilterHook::Forward.index()]
            .ok_or(FilterTableError::MissingBuiltInChain(FilterHook::Forward))?;
        let output = built_in_chains[FilterHook::Output.index()]
            .ok_or(FilterTableError::MissingBuiltInChain(FilterHook::Output))?;

        validate_jump_graph(&chains)?;

        Ok(Self {
            chains,
            built_in_chains: [input, forward, output],
        })
    }

    /// Returns an empty table whose built-in policies all accept packets.
    pub(crate) fn accept_all() -> Self {
        let chains: Vec<_> = FilterHook::ALL
            .map(|hook| {
                Chain::built_in(
                    String::from(hook.chain_name()),
                    hook,
                    Verdict::Accept,
                    Vec::new(),
                )
            })
            .into();

        Self {
            chains,
            built_in_chains: [
                ChainId::from_index(0),
                ChainId::from_index(1),
                ChainId::from_index(2),
            ],
        }
    }

    pub(crate) fn chains(&self) -> &[Chain] {
        &self.chains
    }

    pub(crate) fn chain(&self, id: ChainId) -> Option<&Chain> {
        self.chains.get(id.index())
    }

    pub(crate) fn built_in_chain(&self, hook: FilterHook) -> &Chain {
        &self.chains[self.built_in_chains[hook.index()].index()]
    }
}

fn validate_rule_matches(chain: &Chain) -> Result<(), FilterTableError> {
    for rule in &chain.rules {
        for condition in &rule.matches {
            let prefix_len = match &condition.kind {
                MatchKind::SourceAddress(cidr) | MatchKind::DestinationAddress(cidr) => {
                    Some(cidr.prefix_len())
                }
                _ => None,
            };
            if let Some(prefix_len) = prefix_len
                && prefix_len > 32
            {
                return Err(FilterTableError::InvalidIpv4Prefix(prefix_len));
            }
        }
    }
    Ok(())
}

fn validate_jump_graph(chains: &[Chain]) -> Result<(), FilterTableError> {
    let mut incoming_edges = vec![0usize; chains.len()];

    for chain in chains {
        for rule in &chain.rules {
            let Target::Jump(target) = rule.target else {
                continue;
            };
            let Some(target_chain) = chains.get(target.index()) else {
                return Err(FilterTableError::InvalidJumpTarget(target));
            };
            if matches!(target_chain.kind, ChainKind::BuiltIn { .. }) {
                return Err(FilterTableError::JumpToBuiltInChain(target));
            }
            incoming_edges[target.index()] += 1;
        }
    }

    let mut ready: Vec<_> = incoming_edges
        .iter()
        .enumerate()
        .filter_map(|(index, incoming)| (*incoming == 0).then_some(index))
        .collect();
    let mut visited = 0usize;

    while let Some(index) = ready.pop() {
        visited += 1;
        for rule in &chains[index].rules {
            let Target::Jump(target) = rule.target else {
                continue;
            };
            incoming_edges[target.index()] -= 1;
            if incoming_edges[target.index()] == 0 {
                ready.push(target.index());
            }
        }
    }

    if visited == chains.len() {
        Ok(())
    } else {
        Err(FilterTableError::RecursiveChainGraph)
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::{string::ToString, vec};

    use unittest::def_test;

    use super::*;
    use crate::ip::Ipv4Address;

    fn built_in_chains() -> Vec<Chain> {
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

    #[def_test]
    fn filter_table_validates_builtin_hooks() {
        let table = FilterTable::new(built_in_chains()).unwrap();

        assert_eq!(table.built_in_chain(FilterHook::Input).name(), "INPUT");
        assert_eq!(
            table.built_in_chain(FilterHook::Forward).kind(),
            ChainKind::BuiltIn {
                hook: FilterHook::Forward,
                policy: Verdict::Drop,
            }
        );
        assert_eq!(table.built_in_chain(FilterHook::Output).name(), "OUTPUT");

        let mut missing_output = built_in_chains();
        missing_output.pop();
        assert_eq!(
            FilterTable::new(missing_output),
            Err(FilterTableError::MissingBuiltInChain(FilterHook::Output))
        );

        let mut duplicate_input = built_in_chains();
        duplicate_input.push(Chain::built_in(
            "INPUT".to_string(),
            FilterHook::Input,
            Verdict::Accept,
            Vec::new(),
        ));
        assert_eq!(
            FilterTable::new(duplicate_input),
            Err(FilterTableError::DuplicateBuiltInChain(FilterHook::Input))
        );

        let mut misnamed_input = built_in_chains();
        misnamed_input[0] = Chain::built_in(
            "BAD-INPUT".to_string(),
            FilterHook::Input,
            Verdict::Accept,
            Vec::new(),
        );
        assert_eq!(
            FilterTable::new(misnamed_input),
            Err(FilterTableError::InvalidBuiltInChainName(FilterHook::Input))
        );

        let mut empty_name = built_in_chains();
        empty_name.push(Chain::user_defined(String::new(), Vec::new()));
        assert_eq!(
            FilterTable::new(empty_name),
            Err(FilterTableError::EmptyChainName)
        );

        let mut duplicate_name = built_in_chains();
        duplicate_name.push(Chain::user_defined("CUSTOM".to_string(), Vec::new()));
        duplicate_name.push(Chain::user_defined("CUSTOM".to_string(), Vec::new()));
        assert_eq!(
            FilterTable::new(duplicate_name),
            Err(FilterTableError::DuplicateChainName)
        );
    }

    #[def_test]
    fn match_value_constructors_validate_ranges_and_masks() {
        let port_range = PortRange::new(80, 443).unwrap();
        assert_eq!(port_range.start(), 80);
        assert_eq!(port_range.end(), 443);
        assert_eq!(PortRange::new(443, 80), None);

        let flags = TcpFlagsMatch::new(0x12, 0x02).unwrap();
        assert_eq!(flags.mask(), 0x12);
        assert_eq!(flags.expected(), 0x02);
        assert_eq!(TcpFlagsMatch::new(0x02, 0x12), None);

        let icmp = IcmpMatch::new(Some(3), 0, 15).unwrap();
        assert_eq!(icmp.icmp_type(), Some(3));
        assert_eq!(icmp.code_start(), 0);
        assert_eq!(icmp.code_end(), 15);
        assert_eq!(IcmpMatch::new(Some(3), 15, 0), None);
    }

    #[def_test]
    fn filter_table_rejects_invalid_ipv4_prefix() {
        let mut chains = built_in_chains();
        chains[0].rules.push(Rule::new(
            vec![Match::new(
                MatchKind::SourceAddress(Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 33)),
                false,
            )],
            Target::Verdict(Verdict::Drop),
        ));

        assert_eq!(
            FilterTable::new(chains),
            Err(FilterTableError::InvalidIpv4Prefix(33))
        );
    }

    #[def_test]
    fn filter_table_rejects_invalid_jump_targets() {
        let mut chains = built_in_chains();
        chains[0]
            .rules
            .push(Rule::new(Vec::new(), Target::Jump(ChainId::from_index(3))));

        assert_eq!(
            FilterTable::new(chains),
            Err(FilterTableError::InvalidJumpTarget(ChainId::from_index(3)))
        );

        let mut chains = built_in_chains();
        chains[0]
            .rules
            .push(Rule::new(Vec::new(), Target::Jump(ChainId::from_index(1))));

        assert_eq!(
            FilterTable::new(chains),
            Err(FilterTableError::JumpToBuiltInChain(ChainId::from_index(1)))
        );
    }

    #[def_test]
    fn filter_table_rejects_recursive_user_chains() {
        let mut chains = built_in_chains();
        chains.push(Chain::user_defined(
            "A".to_string(),
            vec![Rule::new(Vec::new(), Target::Jump(ChainId::from_index(4)))],
        ));
        chains.push(Chain::user_defined(
            "B".to_string(),
            vec![Rule::new(Vec::new(), Target::Jump(ChainId::from_index(3)))],
        ));

        assert_eq!(
            FilterTable::new(chains),
            Err(FilterTableError::RecursiveChainGraph)
        );
    }

    #[def_test]
    fn filter_table_accepts_acyclic_user_chain() {
        let mut chains = built_in_chains();
        chains[0].rules.push(Rule::new(
            vec![Match::new(MatchKind::Protocol(6), false)],
            Target::Jump(ChainId::from_index(3)),
        ));
        chains.push(Chain::user_defined(
            "TCP-IN".to_string(),
            vec![Rule::new(Vec::new(), Target::Verdict(Verdict::Accept))],
        ));

        let table = FilterTable::new(chains).unwrap();
        assert_eq!(table.chains().len(), 4);
        assert_eq!(
            table.chain(ChainId::from_index(3)).unwrap().name(),
            "TCP-IN"
        );
    }
}

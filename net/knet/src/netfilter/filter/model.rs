// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Immutable rule graph for the IPv4 `filter` table.

use alloc::{collections::BTreeSet, string::String, vec, vec::Vec};

use super::matcher::{BaseMatch, ExtensionMatch};

/// Maximum number of built-in and user-defined chains in one table.
pub(crate) const MAX_CHAINS: usize = 256;
/// Maximum total number of rules across all chains in one table.
pub(crate) const MAX_RULES: usize = 4096;
/// Maximum number of extension matches attached to one rule.
pub(crate) const MAX_EXTENSION_MATCHES_PER_RULE: usize = 16;
/// Maximum user-defined chain-name length in bytes.
pub(crate) const MAX_CHAIN_NAME_LEN: usize = 29;
/// Maximum number of nested user-chain calls.
pub(crate) const MAX_JUMP_DEPTH: usize = 8;

/// Netfilter hook positions supported by the IPv4 `filter` table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FilterHook {
    LocalIn,
    Forward,
    LocalOut,
}

const BUILTIN_CHAIN_COUNT: usize = FilterTable::BUILTIN_HOOKS.len();
/// A terminal packet-filtering decision.
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

/// Validation failure while constructing a filter rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuleError {
    TooManyExtensionMatches,
    MissingProtocol {
        required_protocol: u8,
    },
    InvertedProtocol {
        required_protocol: u8,
        actual_protocol: u8,
    },
    ProtocolMismatch {
        required_protocol: u8,
        actual_protocol: u8,
    },
}

/// Fixed IPv4 conditions, ordered extension matches, and their selected action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Rule {
    base: BaseMatch,
    extensions: Vec<ExtensionMatch>,
    target: Target,
}

impl Rule {
    /// Builds a rule whose extensions match its non-inverted base protocol.
    ///
    /// # Errors
    ///
    /// Returns `TooManyExtensionMatches` above the per-rule extension limit,
    /// or a protocol error for a missing, inverted, or incompatible protocol.
    pub(crate) fn new(
        base: BaseMatch,
        extensions: Vec<ExtensionMatch>,
        target: Target,
    ) -> Result<Self, RuleError> {
        if extensions.len() > MAX_EXTENSION_MATCHES_PER_RULE {
            return Err(RuleError::TooManyExtensionMatches);
        }
        validate_extensions(&base, &extensions)?;

        Ok(Self {
            base,
            extensions,
            target,
        })
    }

    pub(crate) const fn base(&self) -> &BaseMatch {
        &self.base
    }

    pub(crate) fn extensions(&self) -> &[ExtensionMatch] {
        &self.extensions
    }

    pub(crate) const fn target(&self) -> Target {
        self.target
    }
}

fn validate_extensions(base: &BaseMatch, extensions: &[ExtensionMatch]) -> Result<(), RuleError> {
    for extension in extensions {
        let required_protocol = extension.required_protocol();
        let Some(protocol) = base.protocol.as_ref() else {
            return Err(RuleError::MissingProtocol { required_protocol });
        };
        let actual_protocol = *protocol.value();
        if protocol.is_inverted() {
            return Err(RuleError::InvertedProtocol {
                required_protocol,
                actual_protocol,
            });
        }
        if actual_protocol != required_protocol {
            return Err(RuleError::ProtocolMismatch {
                required_protocol,
                actual_protocol,
            });
        }
    }

    Ok(())
}

/// Built-in or user-defined chain role.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ChainKind {
    BuiltIn { hook: FilterHook, policy: Verdict },
    UserDefined { name: String },
}

/// Validation failure while constructing a chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChainError {
    EmptyName,
    NameTooLong,
}

/// Ordered rules belonging to one built-in or user-defined chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Chain {
    kind: ChainKind,
    rules: Vec<Rule>,
}

impl Chain {
    /// Builds a built-in chain named for its hook.
    pub(crate) fn built_in(hook: FilterHook, policy: Verdict, rules: Vec<Rule>) -> Self {
        Self {
            kind: ChainKind::BuiltIn { hook, policy },
            rules,
        }
    }

    /// Builds a user chain after validating its name.
    ///
    /// # Errors
    ///
    /// Returns `EmptyName` or `NameTooLong` if the name is empty or exceeds
    /// [`MAX_CHAIN_NAME_LEN`] bytes. [`FilterTable::new`] checks uniqueness.
    pub(crate) fn user_defined(name: String, rules: Vec<Rule>) -> Result<Self, ChainError> {
        if name.is_empty() {
            return Err(ChainError::EmptyName);
        }
        if name.len() > MAX_CHAIN_NAME_LEN {
            return Err(ChainError::NameTooLong);
        }

        Ok(Self {
            kind: ChainKind::UserDefined { name },
            rules,
        })
    }

    pub(crate) fn name(&self) -> &str {
        match &self.kind {
            ChainKind::BuiltIn { hook, .. } => match hook {
                FilterHook::LocalIn => "INPUT",
                FilterHook::Forward => "FORWARD",
                FilterHook::LocalOut => "OUTPUT",
            },
            ChainKind::UserDefined { name } => name,
        }
    }

    pub(crate) const fn kind(&self) -> &ChainKind {
        &self.kind
    }

    pub(crate) fn rules(&self) -> &[Rule] {
        &self.rules
    }
}

/// Structural validation failure for an immutable filter table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FilterTableError {
    TooManyChains,
    TooManyRules,
    DuplicateChainName,
    MissingHook(FilterHook),
    DuplicateHook(FilterHook),
    InvalidJumpTarget(ChainId),
    JumpToBuiltInChain(ChainId),
    ChainCycle,
    JumpDepthExceeded,
}

/// Immutable, validated representation of the IPv4 `filter` table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FilterTable {
    chains: Vec<Chain>,
    built_in_chains: [ChainId; BUILTIN_CHAIN_COUNT],
}

impl FilterTable {
    const BUILTIN_HOOKS: [FilterHook; 3] = [
        FilterHook::LocalIn,
        FilterHook::Forward,
        FilterHook::LocalOut,
    ];

    /// Builds a table from validated chains.
    ///
    /// Requires exactly one chain per supported hook and unique chain names.
    /// Jumps must target user chains and form an acyclic graph within
    /// [`MAX_JUMP_DEPTH`], including chains unreachable from built-in hooks.
    pub(crate) fn new(chains: Vec<Chain>) -> Result<Self, FilterTableError> {
        if chains.len() > MAX_CHAINS {
            return Err(FilterTableError::TooManyChains);
        }

        let mut built_in_chains = [None; BUILTIN_CHAIN_COUNT];
        let mut chain_names = BTreeSet::new();
        let mut total_rules = 0usize;

        for (index, chain) in chains.iter().enumerate() {
            if chain.rules.len() > MAX_RULES - total_rules {
                return Err(FilterTableError::TooManyRules);
            }
            total_rules += chain.rules.len();
            if let ChainKind::BuiltIn { hook, .. } = &chain.kind {
                let slot = &mut built_in_chains[Self::built_in_slot(*hook)];
                if slot.replace(ChainId::from_index(index)).is_some() {
                    return Err(FilterTableError::DuplicateHook(*hook));
                }
            }

            if !chain_names.insert(chain.name()) {
                return Err(FilterTableError::DuplicateChainName);
            }
        }

        let mut validated_chains = [ChainId::from_index(0); BUILTIN_CHAIN_COUNT];
        for (slot, hook) in Self::BUILTIN_HOOKS.into_iter().enumerate() {
            validated_chains[slot] =
                built_in_chains[slot].ok_or(FilterTableError::MissingHook(hook))?;
        }

        validate_jump_graph(&chains)?;

        Ok(Self {
            chains,
            built_in_chains: validated_chains,
        })
    }

    /// Returns an empty table whose built-in policies all accept packets.
    pub(crate) fn accept_all() -> Self {
        let chains: Vec<_> = Self::BUILTIN_HOOKS
            .map(|hook| Chain::built_in(hook, Verdict::Accept, Vec::new()))
            .into();

        Self::new(chains).expect("generated filter table must be valid")
    }

    pub(crate) fn chains(&self) -> &[Chain] {
        &self.chains
    }

    pub(crate) fn chain(&self, id: ChainId) -> Option<&Chain> {
        self.chains.get(id.index())
    }

    pub(crate) fn built_in_chain_id(&self, hook: FilterHook) -> ChainId {
        self.built_in_chains[Self::built_in_slot(hook)]
    }

    fn built_in_slot(hook: FilterHook) -> usize {
        Self::BUILTIN_HOOKS
            .iter()
            .position(|candidate| *candidate == hook)
            .expect("every FilterHook has an entry in BUILTIN_HOOKS")
    }
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
            if matches!(&target_chain.kind, ChainKind::BuiltIn { .. }) {
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
    let mut jump_depths = vec![0usize; chains.len()];
    let mut visited = 0usize;

    while let Some(index) = ready.pop() {
        visited += 1;
        for rule in &chains[index].rules {
            let Target::Jump(target) = rule.target else {
                continue;
            };
            jump_depths[target.index()] = jump_depths[target.index()].max(jump_depths[index] + 1);
            incoming_edges[target.index()] -= 1;
            if incoming_edges[target.index()] == 0 {
                ready.push(target.index());
            }
        }
    }

    if visited != chains.len() {
        return Err(FilterTableError::ChainCycle);
    }
    if jump_depths.into_iter().any(|depth| depth > MAX_JUMP_DEPTH) {
        return Err(FilterTableError::JumpDepthExceeded);
    }

    Ok(())
}

#[unittest::mod_test]
mod tests {
    use alloc::{string::ToString, vec};

    use unittest::def_test;

    use super::*;
    use crate::{
        ipv4::{PROTOCOL_ICMP, PROTOCOL_TCP, PROTOCOL_UDP},
        netfilter::filter::matcher::{IcmpMatch, Negated, TcpMatch, UdpMatch},
    };

    fn built_in_chains() -> Vec<Chain> {
        vec![
            Chain::built_in(FilterHook::LocalIn, Verdict::Accept, Vec::new()),
            Chain::built_in(FilterHook::Forward, Verdict::Drop, Vec::new()),
            Chain::built_in(FilterHook::LocalOut, Verdict::Accept, Vec::new()),
        ]
    }

    fn rule(base: BaseMatch, extensions: Vec<ExtensionMatch>, target: Target) -> Rule {
        Rule::new(base, extensions, target).expect("test rule must be valid")
    }

    fn user_chain(name: String, rules: Vec<Rule>) -> Chain {
        Chain::user_defined(name, rules).expect("test chain must be valid")
    }

    #[def_test]
    fn chain_validates_user_defined_name() {
        assert_eq!(
            Chain::user_defined(String::new(), Vec::new()),
            Err(ChainError::EmptyName)
        );
        assert!(Chain::user_defined("X".repeat(MAX_CHAIN_NAME_LEN), Vec::new()).is_ok());
        assert_eq!(
            Chain::user_defined("X".repeat(MAX_CHAIN_NAME_LEN + 1), Vec::new()),
            Err(ChainError::NameTooLong)
        );
    }

    #[def_test]
    fn filter_table_validates_builtin_hooks() {
        let default_table = FilterTable::accept_all();
        assert!(default_table.chains().iter().all(|chain| matches!(
            chain.kind(),
            ChainKind::BuiltIn {
                policy: Verdict::Accept,
                ..
            }
        )));
        let mut chains = built_in_chains();
        chains.rotate_left(1);
        let table = FilterTable::new(chains).unwrap();
        for (hook, name) in [
            (FilterHook::LocalIn, "INPUT"),
            (FilterHook::Forward, "FORWARD"),
            (FilterHook::LocalOut, "OUTPUT"),
        ] {
            let chain = table.chain(table.built_in_chain_id(hook)).unwrap();
            assert_eq!(chain.name(), name);
        }

        for missing_hook in [
            FilterHook::LocalIn,
            FilterHook::Forward,
            FilterHook::LocalOut,
        ] {
            let mut chains = built_in_chains();
            chains.retain(|chain| {
                !matches!(chain.kind(), ChainKind::BuiltIn { hook, .. } if *hook == missing_hook)
            });
            assert_eq!(
                FilterTable::new(chains),
                Err(FilterTableError::MissingHook(missing_hook))
            );
        }

        let mut duplicate_input = built_in_chains();
        duplicate_input.push(Chain::built_in(
            FilterHook::LocalIn,
            Verdict::Accept,
            Vec::new(),
        ));
        assert_eq!(
            FilterTable::new(duplicate_input),
            Err(FilterTableError::DuplicateHook(FilterHook::LocalIn))
        );

        let mut duplicate_name = built_in_chains();
        duplicate_name.push(user_chain("CUSTOM".to_string(), Vec::new()));
        duplicate_name.push(user_chain("CUSTOM".to_string(), Vec::new()));
        assert_eq!(
            FilterTable::new(duplicate_name),
            Err(FilterTableError::DuplicateChainName)
        );
        let mut reserved_name = built_in_chains();
        reserved_name.push(user_chain("INPUT".to_string(), Vec::new()));
        assert_eq!(
            FilterTable::new(reserved_name),
            Err(FilterTableError::DuplicateChainName)
        );
    }

    #[def_test]
    fn rule_validates_extensions() {
        let tcp = ExtensionMatch::Tcp(TcpMatch::new());
        for (protocol, error) in [
            (
                None,
                RuleError::MissingProtocol {
                    required_protocol: PROTOCOL_TCP,
                },
            ),
            (
                Some(Negated::new(PROTOCOL_TCP, true)),
                RuleError::InvertedProtocol {
                    required_protocol: PROTOCOL_TCP,
                    actual_protocol: PROTOCOL_TCP,
                },
            ),
            (
                Some(Negated::new(PROTOCOL_UDP, false)),
                RuleError::ProtocolMismatch {
                    required_protocol: PROTOCOL_TCP,
                    actual_protocol: PROTOCOL_UDP,
                },
            ),
        ] {
            assert_eq!(
                Rule::new(
                    BaseMatch {
                        protocol,
                        ..BaseMatch::default()
                    },
                    vec![tcp.clone()],
                    Target::Continue
                ),
                Err(error)
            );
        }
        for (protocol, extension) in [
            (PROTOCOL_TCP, tcp.clone()),
            (PROTOCOL_UDP, ExtensionMatch::Udp(UdpMatch::new())),
            (
                PROTOCOL_ICMP,
                ExtensionMatch::Icmp(IcmpMatch::new(None, 0, u8::MAX).unwrap()),
            ),
        ] {
            assert!(
                Rule::new(
                    BaseMatch {
                        protocol: Some(Negated::new(protocol, false)),
                        ..BaseMatch::default()
                    },
                    vec![extension],
                    Target::Continue,
                )
                .is_ok()
            );
        }
        let base = BaseMatch {
            protocol: Some(Negated::new(PROTOCOL_TCP, false)),
            ..BaseMatch::default()
        };
        let mut extensions = vec![tcp.clone(); MAX_EXTENSION_MATCHES_PER_RULE];
        assert!(Rule::new(base.clone(), extensions.clone(), Target::Continue).is_ok());
        extensions.push(tcp);
        assert_eq!(
            Rule::new(base, extensions, Target::Continue),
            Err(RuleError::TooManyExtensionMatches)
        );
    }

    #[def_test]
    fn filter_table_enforces_structural_limits() {
        let mut chains = built_in_chains();
        for index in BUILTIN_CHAIN_COUNT..MAX_CHAINS {
            chains.push(user_chain(alloc::format!("CHAIN-{index}"), Vec::new()));
        }
        assert!(FilterTable::new(chains.clone()).is_ok());
        chains.push(user_chain("EXTRA".to_string(), Vec::new()));
        assert_eq!(
            FilterTable::new(chains),
            Err(FilterTableError::TooManyChains)
        );

        let mut chains = built_in_chains();
        chains[0].rules =
            vec![rule(BaseMatch::default(), Vec::new(), Target::Continue); MAX_RULES - 1];
        chains[1]
            .rules
            .push(rule(BaseMatch::default(), Vec::new(), Target::Return));
        assert!(FilterTable::new(chains.clone()).is_ok());
        chains[2]
            .rules
            .push(rule(BaseMatch::default(), Vec::new(), Target::Continue));
        assert_eq!(
            FilterTable::new(chains),
            Err(FilterTableError::TooManyRules)
        );
    }

    #[def_test]
    fn filter_table_rejects_invalid_jump_targets() {
        for (target, error) in [
            (
                ChainId::from_index(3),
                FilterTableError::InvalidJumpTarget(ChainId::from_index(3)),
            ),
            (
                ChainId::from_index(1),
                FilterTableError::JumpToBuiltInChain(ChainId::from_index(1)),
            ),
        ] {
            let mut chains = built_in_chains();
            chains[0]
                .rules
                .push(rule(BaseMatch::default(), Vec::new(), Target::Jump(target)));
            assert_eq!(FilterTable::new(chains), Err(error));
        }
    }

    #[def_test]
    fn filter_table_rejects_recursive_user_chains() {
        let mut chains = built_in_chains();
        chains.push(user_chain(
            "A".to_string(),
            vec![rule(
                BaseMatch::default(),
                Vec::new(),
                Target::Jump(ChainId::from_index(4)),
            )],
        ));
        chains.push(user_chain(
            "B".to_string(),
            vec![rule(
                BaseMatch::default(),
                Vec::new(),
                Target::Jump(ChainId::from_index(3)),
            )],
        ));

        assert_eq!(FilterTable::new(chains), Err(FilterTableError::ChainCycle));
    }

    #[def_test]
    fn filter_table_rejects_excessive_jump_depth() {
        let mut chains = built_in_chains();
        chains[0].rules.push(rule(
            BaseMatch::default(),
            Vec::new(),
            Target::Jump(ChainId::from_index(BUILTIN_CHAIN_COUNT)),
        ));

        for depth in 0..MAX_JUMP_DEPTH {
            let target = if depth + 1 == MAX_JUMP_DEPTH {
                Target::Jump(ChainId::from_index(BUILTIN_CHAIN_COUNT + MAX_JUMP_DEPTH))
            } else {
                Target::Jump(ChainId::from_index(BUILTIN_CHAIN_COUNT + depth + 1))
            };
            chains.push(user_chain(
                alloc::format!("CHAIN-{depth}"),
                vec![rule(BaseMatch::default(), Vec::new(), target)],
            ));
        }
        chains.push(user_chain("DEEPEST".to_string(), Vec::new()));

        assert_eq!(
            FilterTable::new(chains.clone()),
            Err(FilterTableError::JumpDepthExceeded)
        );
        chains.pop();
        chains.last_mut().unwrap().rules.clear();
        assert!(FilterTable::new(chains).is_ok());
    }
}

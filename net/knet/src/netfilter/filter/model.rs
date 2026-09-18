// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Immutable rule graph for the IPv4 `filter` table.

use alloc::{collections::BTreeSet, string::String, vec, vec::Vec};

use super::matcher::Match;

/// Maximum number of built-in and user-defined chains in one table.
pub(crate) const MAX_CHAINS: usize = 256;
/// Maximum total number of rules across all chains in one table.
pub(crate) const MAX_RULES: usize = 4096;
/// Maximum number of match conditions attached to one rule.
pub(crate) const MAX_MATCHES_PER_RULE: usize = 16;
/// Maximum user-defined chain-name length in bytes.
pub(crate) const MAX_CHAIN_NAME_LEN: usize = 29;
/// Maximum number of nested user-chain calls.
pub(crate) const MAX_JUMP_DEPTH: usize = 8;

/// IPv4 hooks supported by the initial `filter` table.
#[repr(usize)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FilterHook {
    Input   = 0,
    Forward = 1,
    Output  = 2,
}

impl FilterHook {
    const ALL: [Self; 3] = [Self::Input, Self::Forward, Self::Output];

    const fn index(self) -> usize {
        self as usize
    }

    const fn chain_name(self) -> &'static str {
        match self {
            Self::Input => "INPUT",
            Self::Forward => "FORWARD",
            Self::Output => "OUTPUT",
        }
    }
}

const BUILTIN_CHAIN_COUNT: usize = FilterHook::ALL.len();
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ChainKind {
    BuiltIn { hook: FilterHook, policy: Verdict },
    UserDefined { name: String },
}

/// Ordered rules belonging to one built-in or user-defined chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Chain {
    kind: ChainKind,
    rules: Vec<Rule>,
}

impl Chain {
    pub(crate) fn built_in(hook: FilterHook, policy: Verdict, rules: Vec<Rule>) -> Self {
        Self {
            kind: ChainKind::BuiltIn { hook, policy },
            rules,
        }
    }

    pub(crate) fn user_defined(name: String, rules: Vec<Rule>) -> Self {
        Self {
            kind: ChainKind::UserDefined { name },
            rules,
        }
    }

    pub(crate) fn name(&self) -> &str {
        match &self.kind {
            ChainKind::BuiltIn { hook, .. } => hook.chain_name(),
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
    TooManyMatches,
    EmptyChainName,
    ChainNameTooLong,
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
    /// Builds a table and validates its chain graph.
    pub(crate) fn new(chains: Vec<Chain>) -> Result<Self, FilterTableError> {
        if chains.len() > MAX_CHAINS {
            return Err(FilterTableError::TooManyChains);
        }

        let mut built_in_chains = [None; BUILTIN_CHAIN_COUNT];
        let mut chain_names = BTreeSet::new();
        let mut total_rules = 0usize;

        for (index, chain) in chains.iter().enumerate() {
            if chain.name().is_empty() {
                return Err(FilterTableError::EmptyChainName);
            }
            if chain.name().len() > MAX_CHAIN_NAME_LEN {
                return Err(FilterTableError::ChainNameTooLong);
            }

            if chain.rules.len() > MAX_RULES - total_rules {
                return Err(FilterTableError::TooManyRules);
            }
            total_rules += chain.rules.len();
            if chain
                .rules
                .iter()
                .any(|rule| rule.matches.len() > MAX_MATCHES_PER_RULE)
            {
                return Err(FilterTableError::TooManyMatches);
            }

            if let ChainKind::BuiltIn { hook, .. } = &chain.kind {
                let slot = &mut built_in_chains[hook.index()];
                if slot.replace(ChainId::from_index(index)).is_some() {
                    return Err(FilterTableError::DuplicateHook(*hook));
                }
            }

            if !chain_names.insert(chain.name()) {
                return Err(FilterTableError::DuplicateChainName);
            }
        }

        let built_in_chains = [
            require_hook(&built_in_chains, FilterHook::Input)?,
            require_hook(&built_in_chains, FilterHook::Forward)?,
            require_hook(&built_in_chains, FilterHook::Output)?,
        ];

        validate_jump_graph(&chains)?;

        Ok(Self {
            chains,
            built_in_chains,
        })
    }

    /// Returns an empty table whose built-in policies all accept packets.
    pub(crate) fn accept_all() -> Self {
        let chains: Vec<_> = FilterHook::ALL
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

    pub(crate) fn built_in_chain(&self, hook: FilterHook) -> &Chain {
        &self.chains[self.built_in_chains[hook.index()].index()]
    }

    pub(crate) fn built_in_chain_id(&self, hook: FilterHook) -> ChainId {
        self.built_in_chains[hook.index()]
    }
}

fn require_hook(
    built_in_chains: &[Option<ChainId>; BUILTIN_CHAIN_COUNT],
    hook: FilterHook,
) -> Result<ChainId, FilterTableError> {
    built_in_chains[hook.index()].ok_or(FilterTableError::MissingHook(hook))
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

#[cfg(unittest)]
mod tests {
    use alloc::{string::ToString, vec};

    use unittest::def_test;

    use super::*;
    use crate::netfilter::filter::matcher::MatchKind;

    fn built_in_chains() -> Vec<Chain> {
        vec![
            Chain::built_in(FilterHook::Input, Verdict::Accept, Vec::new()),
            Chain::built_in(FilterHook::Forward, Verdict::Drop, Vec::new()),
            Chain::built_in(FilterHook::Output, Verdict::Accept, Vec::new()),
        ]
    }

    #[def_test]
    fn filter_table_validates_builtin_hooks() {
        let table = FilterTable::new(built_in_chains()).unwrap();

        assert_eq!(table.built_in_chain(FilterHook::Input).name(), "INPUT");
        assert_eq!(
            table.built_in_chain(FilterHook::Forward).kind(),
            &ChainKind::BuiltIn {
                hook: FilterHook::Forward,
                policy: Verdict::Drop,
            }
        );
        assert_eq!(table.built_in_chain(FilterHook::Output).name(), "OUTPUT");

        let mut missing_output = built_in_chains();
        missing_output.pop();
        assert_eq!(
            FilterTable::new(missing_output),
            Err(FilterTableError::MissingHook(FilterHook::Output))
        );

        let mut duplicate_input = built_in_chains();
        duplicate_input.push(Chain::built_in(
            FilterHook::Input,
            Verdict::Accept,
            Vec::new(),
        ));
        assert_eq!(
            FilterTable::new(duplicate_input),
            Err(FilterTableError::DuplicateHook(FilterHook::Input))
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
    fn filter_table_enforces_structural_limits() {
        let mut too_many_chains = built_in_chains();
        for index in BUILTIN_CHAIN_COUNT..=MAX_CHAINS {
            too_many_chains.push(Chain::user_defined(
                alloc::format!("CHAIN-{index}"),
                Vec::new(),
            ));
        }
        assert_eq!(
            FilterTable::new(too_many_chains),
            Err(FilterTableError::TooManyChains)
        );

        let mut too_many_rules = built_in_chains();
        too_many_rules[0].rules = vec![Rule::new(Vec::new(), Target::Continue); MAX_RULES + 1];
        assert_eq!(
            FilterTable::new(too_many_rules),
            Err(FilterTableError::TooManyRules)
        );

        let mut too_many_matches = built_in_chains();
        too_many_matches[0].rules.push(Rule::new(
            vec![Match::new(MatchKind::Protocol(6), false); MAX_MATCHES_PER_RULE + 1],
            Target::Continue,
        ));
        assert_eq!(
            FilterTable::new(too_many_matches),
            Err(FilterTableError::TooManyMatches)
        );

        let mut long_chain_name = built_in_chains();
        long_chain_name.push(Chain::user_defined(
            "X".repeat(MAX_CHAIN_NAME_LEN + 1),
            Vec::new(),
        ));
        assert_eq!(
            FilterTable::new(long_chain_name),
            Err(FilterTableError::ChainNameTooLong)
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

        assert_eq!(FilterTable::new(chains), Err(FilterTableError::ChainCycle));
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

    #[def_test]
    fn filter_table_rejects_excessive_jump_depth() {
        let mut chains = built_in_chains();
        chains[0].rules.push(Rule::new(
            Vec::new(),
            Target::Jump(ChainId::from_index(BUILTIN_CHAIN_COUNT)),
        ));

        for depth in 0..MAX_JUMP_DEPTH {
            let target = if depth + 1 == MAX_JUMP_DEPTH {
                Target::Jump(ChainId::from_index(BUILTIN_CHAIN_COUNT + MAX_JUMP_DEPTH))
            } else {
                Target::Jump(ChainId::from_index(BUILTIN_CHAIN_COUNT + depth + 1))
            };
            chains.push(Chain::user_defined(
                alloc::format!("CHAIN-{depth}"),
                vec![Rule::new(Vec::new(), target)],
            ));
        }
        chains.push(Chain::user_defined(
            "DEEPEST".to_string(),
            vec![Rule::new(Vec::new(), Target::Verdict(Verdict::Accept))],
        ));

        assert_eq!(
            FilterTable::new(chains),
            Err(FilterTableError::JumpDepthExceeded)
        );
    }
}

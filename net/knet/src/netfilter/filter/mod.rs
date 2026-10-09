// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 `filter` table model, configuration, matcher, and evaluator.

pub(super) mod config;
mod evaluator;
mod matcher;
mod model;

pub(crate) use config::evaluate_ipv4;
pub(super) use matcher::{
    BaseMatch, ExtensionMatch, IcmpMatch, InterfaceMatch, Negated, PortRange, TcpFlagsMatch,
    TcpMatch, UdpMatch,
};
pub(super) use model::{
    Chain, ChainId, ChainKind, FilterTable, MAX_CHAINS, MAX_EXTENSION_MATCHES_PER_RULE, MAX_RULES,
    Rule, Target,
};
pub(crate) use model::{FilterHook, Verdict};

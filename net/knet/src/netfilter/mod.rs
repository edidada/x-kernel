// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Internal IPv4 netfilter representation.
//!
//! Legacy iptables ABI data must be decoded and validated before it enters
//! this module. The model stores owned semantic values and does not retain
//! offsets or pointers into the userspace replacement blob.

pub(crate) mod filter;

pub use filter::LegacyFilterTable;

/// Captures the current IPv4 `filter` table in the legacy iptables layout.
///
/// The returned snapshot owns its entry bytes and remains stable if a later
/// table replacement publishes new state.
pub fn legacy_ipv4_filter_table() -> LegacyFilterTable {
    filter::snapshot()
}

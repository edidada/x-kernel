// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 netfilter configuration, packet evaluation, and management adapters.
//!
//! Management ABIs decode into validated semantic tables and publish them
//! through `filter::config`. Packet evaluation depends only on that semantic
//! configuration; `compat` owns wire formats and protocol-specific metadata.

mod compat;
pub(crate) mod filter;
pub use compat::iptables_legacy::{
    LegacyCounter, LegacyFilterReplace, LegacyFilterTable, LegacyReplaceError,
    MAX_LEGACY_FILTER_ENTRIES_SIZE, MAX_LEGACY_FILTER_ENTRY_COUNT,
};
pub(crate) use filter::{FilterHook, Verdict, evaluate_ipv4};

/// Captures the current IPv4 `filter` table in the legacy iptables layout.
///
/// The returned snapshot owns its entry bytes and remains stable if a later
/// table replacement publishes new state.
pub fn legacy_ipv4_filter_table() -> LegacyFilterTable {
    compat::iptables_legacy::snapshot()
}

/// Validates and publishes a complete legacy IPv4 `filter` replacement.
///
/// # Errors
///
/// Returns [`LegacyReplaceError::InvalidInput`] for unsupported or malformed
/// rules, or [`LegacyReplaceError::CounterMismatch`] if the old table changed.
pub fn replace_legacy_ipv4_filter_table(
    request: LegacyFilterReplace<'_>,
) -> Result<alloc::vec::Vec<LegacyCounter>, LegacyReplaceError> {
    compat::iptables_legacy::replace(request)
}

/// Adds packet and byte counters to the current IPv4 `filter` table.
///
/// # Errors
///
/// Returns [`LegacyReplaceError::CounterMismatch`] when the supplied entry
/// count differs from the current table generation.
pub fn add_legacy_ipv4_filter_counters(
    counters: &[LegacyCounter],
) -> Result<(), LegacyReplaceError> {
    compat::iptables_legacy::add_counters(counters)
}

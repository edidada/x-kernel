// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Internal IPv4 netfilter representation.
//!
//! Legacy iptables ABI data must be decoded and validated before it enters
//! this module. The model stores owned semantic values and does not retain
//! offsets or pointers into the userspace replacement blob.

pub(crate) mod filter;
pub(crate) use filter::{FilterHook, Verdict, evaluate_ipv4};
pub use filter::{
    LegacyCounter, LegacyFilterReplace, LegacyFilterTable, LegacyReplaceError,
    MAX_LEGACY_FILTER_ENTRIES_SIZE, MAX_LEGACY_FILTER_ENTRY_COUNT,
};

/// Captures the current IPv4 `filter` table in the legacy iptables layout.
///
/// The returned snapshot owns its entry bytes and remains stable if a later
/// table replacement publishes new state.
pub fn legacy_ipv4_filter_table() -> LegacyFilterTable {
    filter::snapshot()
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
    filter::replace(request)
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
    filter::add_counters(counters)
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 `filter` table model, matcher, and evaluator.

mod evaluator;
mod legacy;
mod matcher;
mod model;

pub use legacy::{
    LegacyCounter, LegacyFilterReplace, LegacyFilterTable, LegacyReplaceError,
    MAX_LEGACY_FILTER_ENTRIES_SIZE, MAX_LEGACY_FILTER_ENTRY_COUNT, add_counters, replace,
};
pub(crate) use legacy::{evaluate_ipv4, snapshot};
pub(crate) use model::{FilterHook, Verdict};

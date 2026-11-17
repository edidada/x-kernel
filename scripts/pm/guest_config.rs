// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Checked configuration shared by the guest probe and host regressions.

/// Parses a decimal cycle count in 1..=100, rejecting malformed/overflowing input.
pub fn parse_rounds(input: &str) -> Option<u64> {
    if input.is_empty() || !input.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    match input.parse::<u64>() {
        Ok(value) if (1..=100).contains(&value) => Some(value),
        _ => None,
    }
}

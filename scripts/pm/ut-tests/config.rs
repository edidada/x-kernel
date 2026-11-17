// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[path = "../guest_config.rs"]
mod config;

#[test]
fn accepted_round_limits_are_exact() {
    for n in [1, 3, 12, 99, 100] {
        assert_eq!(config::parse_rounds(&n.to_string()), Some(n));
    }
}

#[test]
fn invalid_configuration_returns_none_without_panicking() {
    for text in [
        "",
        "0",
        "101",
        "-1",
        "+1",
        " 1",
        "1 ",
        "abc",
        "12x",
        "18446744073709551616",
        "99999999999999999999999999999999",
    ] {
        assert_eq!(config::parse_rounds(text), None, "{text:?}");
    }
}

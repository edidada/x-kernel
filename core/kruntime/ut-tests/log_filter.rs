// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Boot log-filter resolution: the build level must be in force before any
//! source spec is applied, so a rules-only spec keeps it as its default.

use alloc::format;

use klogger::{
    current_log_level, filter_spec, level_name, parse_log_level, set_filter_spec,
    set_log_level_filter, MAX_FILTER_SPEC,
};
use unittest::{assert, assert_eq, def_test};

/// Canonical text of the active filter, as `/proc/sys/kernel/printk` reports it.
fn active_spec() -> alloc::string::String {
    let mut buf = [0u8; MAX_FILTER_SPEC];
    let len = filter_spec(&mut buf).expect("the active spec fits");
    alloc::string::String::from_utf8_lossy(&buf[..len]).into_owned()
}

/// Restores the filter that was active when the test started, rules included.
///
/// Returns whether the saved spec still applies; the caller asserts it, because
/// the test macros in this framework expand to `return TestResult::Failed` and
/// therefore only work inside a `#[def_test]` body.
fn restore(spec: &str) -> bool {
    set_filter_spec(spec).is_ok()
}

/// A rules-only source must not reset the default level: it narrows one target
/// and leaves every other module at the level the build asked for.
#[def_test(serial)]
fn rules_only_source_keeps_the_build_level() {
    let saved = active_spec();
    let build_level = super::apply_build_log_level();
    let expected = parse_log_level(build_level).expect("the build level is valid");
    // Pretend something else was in force before the source is consulted.
    set_log_level_filter(parse_log_level("off").expect("off is valid"));

    let resolved = super::resolve_log_filter(None, None, "ksyscall=trace");
    assert_eq!(resolved, "ksyscall=trace");
    assert_eq!(
        current_log_level(),
        expected,
        "a rules-only spec replaced the build level {build_level}"
    );
    assert_eq!(
        active_spec(),
        format!("{} ksyscall=trace", level_name(expected)),
        "the boot spec did not keep the build level as its default"
    );

    assert!(restore(&saved), "the saved spec failed to re-apply");
}

/// Priority: an explicit command-line request beats the board's device-tree
/// default, which beats the build configuration — an operator must be able to
/// override a value the firmware shipped.
#[def_test(serial)]
fn command_line_overrides_the_device_tree() {
    let saved = active_spec();

    // `loglevel=` on the command line wins over the DT property.
    let _ = super::resolve_log_filter(Some("info"), Some("loglevel=debug"), "warn drv_uart=1");
    assert_eq!(
        current_log_level(),
        parse_log_level("debug").expect("debug is valid")
    );

    // The `quiet`/`debug` shorthands count as explicit requests too.
    let _ = super::resolve_log_filter(Some("info"), Some("quiet"), "");
    assert_eq!(
        current_log_level(),
        parse_log_level("warn").expect("warn is valid")
    );
    let _ = super::resolve_log_filter(Some("warn"), Some("debug"), "");
    assert_eq!(
        current_log_level(),
        parse_log_level("debug").expect("debug is valid")
    );

    // With no command-line request the DT property is next.
    let _ = super::resolve_log_filter(Some("info"), None, "warn");
    assert_eq!(
        current_log_level(),
        parse_log_level("info").expect("info is valid")
    );
    assert_eq!(active_spec(), "info");

    // Then the build configuration, then the build level.
    let _ = super::resolve_log_filter(None, None, "error drv_uart=debug");
    assert_eq!(active_spec(), "error drv_uart=debug");

    assert!(restore(&saved), "the saved spec failed to re-apply");
}

/// An invalid source is skipped and the fallback is the build level, not a
/// leftover from a previous source or the logger's hard-coded default.
#[def_test(serial)]
fn invalid_sources_fall_back_to_the_build_level() {
    let saved = active_spec();
    let build_level = super::apply_build_log_level();
    let expected = parse_log_level(build_level).expect("the build level is valid");

    // An invalid build-time spec is skipped (`LOG_FILTER` is compiled in, so the
    // rejection is reported on the console).
    let resolved = super::resolve_log_filter(None, None, "warn bogus=nonsense");
    assert_eq!(resolved, build_level);
    assert_eq!(current_log_level(), expected);

    // An invalid `loglevel=` token is skipped, and a later valid shorthand in the
    // same command line still applies.
    let resolved = super::resolve_log_filter(None, Some("loglevel=verbos debug"), "info");
    assert_eq!(resolved, "4");
    assert_eq!(
        current_log_level(),
        parse_log_level("debug").expect("debug is valid")
    );

    assert!(restore(&saved), "the saved spec failed to re-apply");
}
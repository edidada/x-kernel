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
use unittest::{assert_eq, def_test};

/// `ksyscall=trace` carries no bare level: it must narrow that target and leave
/// every other module at the level the build asked for, not at the logger's
/// hard-coded default.
#[def_test(serial)]
fn rules_only_boot_spec_keeps_the_build_level() {
    let build_level = super::apply_build_log_level();
    let expected = parse_log_level(build_level).expect("the build level is valid");
    assert_eq!(current_log_level(), expected);

    // Pretend something else was in force before the boot source is consulted.
    set_log_level_filter(parse_log_level("off").expect("off is valid"));
    assert_eq!(current_log_level(), parse_log_level("off").unwrap());

    let build_level = super::apply_build_log_level();
    assert_eq!(current_log_level(), expected);

    assert_eq!(set_filter_spec("ksyscall=trace"), Ok(()));
    assert_eq!(
        current_log_level(),
        expected,
        "a rules-only spec replaced the build level {build_level}"
    );

    // The canonical text reports the inherited default, not a hard-coded level.
    let mut buf = [0u8; MAX_FILTER_SPEC];
    let len = filter_spec(&mut buf).expect("spec fits");
    let text = core::str::from_utf8(&buf[..len]).expect("spec text is utf-8");
    assert_eq!(
        text,
        format!("{} ksyscall=trace", level_name(expected)),
        "the boot spec did not keep the build level as its default"
    );

    // Leave the boot state behind for the tests that follow.
    let _ = super::apply_build_log_level();
}
// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#![no_main]
#![doc = "XHyper EL2 runtime entry."]

#[macro_use]
extern crate klogger;

// Keep the feature aggregation crate linked into the final image.
extern crate kfeat;
extern crate kruntime;

mod image_metadata;
#[cfg(not(feature = "unittest"))]
mod shell;

#[kiface::provide]
impl kruntime::HypervisorRuntimeEntry {
    fn enter() {
        image_metadata::print();
        #[cfg(feature = "unittest")]
        run_unittests();

        #[cfg(not(feature = "unittest"))]
        shell::run();

        ktask::future::block_on(core::future::pending::<()>());
    }
}

#[cfg(feature = "unittest")]
fn run_unittests() {
    fn print(args: core::fmt::Arguments<'_>) {
        kprintln!("{}", args);
    }

    let crate_filter = option_env!("UNITTEST_CRATE").and_then(|raw| {
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    });

    unittest::set_printer(print);
    let is_ok = unittest::test_run_ok_filtered(crate_filter);
    kprintln!(
        "=== UNITTEST_STATUS: {} ===",
        if is_ok {
            "TESTS_PASSED"
        } else {
            "TESTS_FAILED"
        }
    );
    khal::power::shutdown();
}

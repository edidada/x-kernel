// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x-kernel Dylint library.
//!
//! Every repository lint is a module of this crate and is registered in
//! [`register_lints`]; each module carries its own UI test. See
//! `tools/lints/README.md`.
#![feature(rustc_private)]

extern crate rustc_lint;
extern crate rustc_session;

dylint_linting::dylint_library!();

#[cfg(test)]
#[allow(dead_code, reason = "shared harness for the lint modules' UI tests")]
mod testing;

/// Registers every lint and pass in this library with `rustc`.
#[expect(clippy::no_mangle_with_rust_abi)]
#[unsafe(no_mangle)]
pub fn register_lints(sess: &rustc_session::Session, lint_store: &mut rustc_lint::LintStore) {
    dylint_linting::init_config(sess);
    lint_store.register_lints(&[]);
}

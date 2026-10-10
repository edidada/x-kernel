// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x-kernel Dylint library.
//!
//! Every repository lint is a module of this crate and is registered in
//! [`register_lints`]; each module carries its own UI test. See
//! `tools/lints/README.md`.
#![feature(rustc_private)]

extern crate rustc_abi;
extern crate rustc_ast;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_lint;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;

dylint_linting::dylint_library!();

mod facts_export;
mod std_box_in_kernel_tests;
mod static_lock_helpers;
mod static_mut_option;
mod unregistered_static_lock;
mod unregistered_static_spinlock;
mod untracked_runtime_lock;
mod vendored;

#[cfg(test)]
mod testing;

/// Registers every lint and pass in this library with `rustc`.
#[expect(clippy::no_mangle_with_rust_abi)]
#[unsafe(no_mangle)]
pub fn register_lints(sess: &rustc_session::Session, lint_store: &mut rustc_lint::LintStore) {
    dylint_linting::init_config(sess);
    facts_export::track_env(sess);
    lint_store.register_lints(&[
        unregistered_static_spinlock::UNREGISTERED_STATIC_SPINLOCK,
        facts_export::FACTS_EXPORT,
        std_box_in_kernel_tests::STD_BOX_IN_KERNEL_TESTS,
        unregistered_static_lock::UNREGISTERED_STATIC_LOCK,
        untracked_runtime_lock::UNTRACKED_RUNTIME_LOCK,
        static_mut_option::STATIC_MUT_OPTION,
    ]);
    lint_store.register_late_pass(|_| Box::new(facts_export::FactsExport));
    // CG rules do not apply to vendored third-party code. Its facts are still
    // exported; no checking pass runs on it, so no scan or `--fix` touches it.
    if vendored::is_vendored_crate() {
        return;
    }
    std_box_in_kernel_tests::register(lint_store);
    lint_store
        .register_late_pass(|_| Box::new(unregistered_static_spinlock::UnregisteredStaticSpinlock));
    lint_store.register_late_pass(|_| Box::new(unregistered_static_lock::UnregisteredStaticLock));
    lint_store.register_late_pass(|_| Box::new(untracked_runtime_lock::UntrackedRuntimeLock));
    lint_store.register_late_pass(|_| Box::new(static_mut_option::StaticMutOption));
}

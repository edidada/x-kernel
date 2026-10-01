// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x-kernel Dylint library.
//!
//! Every repository lint is a module of this crate and is registered in
//! [`register_lints`]; each module carries its own UI test. See
//! `tools/lints/README.md`.
#![feature(rustc_private)]

extern crate rustc_ast;
extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_lint;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;

dylint_linting::dylint_library!();

mod exported_foreign_item;
mod static_lock_helpers;
mod static_mut_option;
mod undocumented_unsafe_attribute;
mod unitless_time_integer;
mod unregistered_static_lock;
mod unregistered_static_spinlock;
mod untracked_runtime_lock;
mod untyped_error_result;
mod vec_shift_in_loop;

#[cfg(test)]
mod testing;

// SAFETY: Dylint loads this uniquely named export with Rust's register_lints
// signature; the function pointer contract is set by dylint_linting.
/// Registers every lint and pass in this library with `rustc`.
#[expect(clippy::no_mangle_with_rust_abi)]
#[unsafe(no_mangle)]
pub fn register_lints(sess: &rustc_session::Session, lint_store: &mut rustc_lint::LintStore) {
    dylint_linting::init_config(sess);
    lint_store.register_lints(&[
        unregistered_static_spinlock::UNREGISTERED_STATIC_SPINLOCK,
        unregistered_static_lock::UNREGISTERED_STATIC_LOCK,
        untracked_runtime_lock::UNTRACKED_RUNTIME_LOCK,
        static_mut_option::STATIC_MUT_OPTION,
        vec_shift_in_loop::VEC_SHIFT_IN_LOOP,
        untyped_error_result::UNTYPED_ERROR_RESULT,
        unitless_time_integer::UNITLESS_TIME_INTEGER,
        exported_foreign_item::EXPORTED_FOREIGN_ITEM,
        undocumented_unsafe_attribute::UNDOCUMENTED_UNSAFE_ATTRIBUTE,
    ]);
    lint_store
        .register_late_pass(|_| Box::new(unregistered_static_spinlock::UnregisteredStaticSpinlock));
    lint_store.register_late_pass(|_| Box::new(unregistered_static_lock::UnregisteredStaticLock));
    lint_store.register_late_pass(|_| Box::new(untracked_runtime_lock::UntrackedRuntimeLock));
    lint_store.register_late_pass(|_| Box::new(static_mut_option::StaticMutOption));
    lint_store.register_late_pass(|_| Box::new(vec_shift_in_loop::VecShiftInLoop));
    lint_store.register_late_pass(|_| Box::new(untyped_error_result::UntypedErrorResult));
    lint_store.register_late_pass(|_| Box::new(unitless_time_integer::UnitlessTimeInteger));
    lint_store.register_late_pass(|_| Box::new(exported_foreign_item::ExportedForeignItem));
    lint_store.register_pre_expansion_pass(|| {
        Box::new(undocumented_unsafe_attribute::UndocumentedUnsafeAttribute)
    });
}

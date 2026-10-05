// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `unregistered_static_lock`: static `ksync::Mutex` and `ksync::RwLock` items
//! initialized with `new` outside the real `static_lock!` macro.
//!
//! Rule: `docs/code-guidelines/CG-002-static-mutex-registration.md`.

use rustc_hir::{ExprKind, Item, ItemKind};
use rustc_lint::{LateContext, LateLintPass};

use crate::static_lock_helpers::{
    defined_by_static_lock, direct_call_to_type, is_type, peel_blocks,
};

rustc_session::declare_lint! {
    /// ### What it does
    ///
    /// Reports static `ksync::Mutex` and `ksync::RwLock` items whose initializer
    /// directly calls `new` or default-raw `const_new` outside `static_lock!`.
    ///
    /// ### Why is this bad?
    ///
    /// With lock statistics enabled, static locks must be registered through
    /// `static_lock!` so they receive a per-lock statistics class.
    ///
    /// ### Known problems
    ///
    /// Only direct `new` calls and `const_new` calls with a direct default raw
    /// constructor are inspected after peeling statement-free blocks. Custom
    /// raw locks built with `with_config` are outside this lint's scope.
    ///
    /// ### Example
    ///
    /// ```rust,ignore
    /// static DATA: ksync::Mutex<u32> = ksync::Mutex::new(0);
    /// ```
    ///
    /// Rewrite the item in the macro's accepted form: canonical lock type and a
    /// direct `::new` initializer, with no `mut`; see the rule file.
    pub UNREGISTERED_STATIC_LOCK,
    Deny,
    "static Mutex or RwLock is not registered with static_lock!",
    report_in_external_macro
}

rustc_session::declare_lint_pass!(UnregisteredStaticLock => [UNREGISTERED_STATIC_LOCK]);

impl<'tcx> LateLintPass<'tcx> for UnregisteredStaticLock {
    fn check_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx Item<'tcx>) {
        let ItemKind::Static(_, _, _, body_id) = item.kind else {
            return;
        };
        let tcx = cx.tcx;
        let owner = item.owner_id;
        let declared = tcx.type_of(owner).instantiate_identity();
        let item_ty = tcx
            .try_normalize_erasing_regions(cx.typing_env(), declared)
            .unwrap_or(declared);
        let is_mutex = is_type(tcx, item_ty, "ksync", "Mutex");
        let is_rwlock = is_type(tcx, item_ty, "ksync", "RwLock");
        if !is_mutex && !is_rwlock {
            return;
        }
        if defined_by_static_lock(tcx, owner.to_def_id()) {
            return;
        }
        let types = tcx.typeck(owner.def_id);
        let body = tcx.hir_body(body_id);
        let lock_name = if is_mutex { "Mutex" } else { "RwLock" };
        let initializer = peel_blocks(body.value);
        let call =
            direct_call_to_type(tcx, types, initializer, "ksync", lock_name, "new").or_else(|| {
                let call =
                    direct_call_to_type(tcx, types, initializer, "ksync", lock_name, "const_new")?;
                let ExprKind::Call(_, arguments) = initializer.kind else {
                    return None;
                };
                let raw_name = if is_mutex { "RawMutex" } else { "RawRwLock" };
                let raw_constructor = arguments.first().map(|argument| peel_blocks(argument))?;
                direct_call_to_type(tcx, types, raw_constructor, "ksync", raw_name, "new")?;
                Some(call)
            });
        let Some(call) = call else {
            return;
        };
        tcx.node_span_lint(
            UNREGISTERED_STATIC_LOCK,
            item.hir_id(),
            item.span,
            |diagnostic| {
                diagnostic.primary_message(format!(
                    "static {lock_name} is not registered with static_lock!"
                ));
                diagnostic.span_label(
                    call,
                    "register this lock so it receives a per-lock statistics class",
                );
                diagnostic.help(
                    "rewrite as `static_lock! { static NAME: Mutex<T> = Mutex::new(arg); }` (use \
                     `RwLock` as needed; remove `mut`); see \
                     docs/code-guidelines/CG-002-static-mutex-registration.md",
                );
            },
        );
    }
}

#[cfg(test)]
mod tests {
    const SOURCE: &str = r#"#![allow(dead_code, unused)]

mod flagged {
    use ksync::{Mutex, RawMutex, RwLock, RawRwLock};

    static A: Mutex<u32> = Mutex::new(0);
    static B: RwLock<u32> = RwLock::new(0);

    mod aliases {
        use ksync::Mutex as M;
        static C: M<u32> = M::new(0);
    }

    static D: ksync::Mutex<u32> = unsafe { ksync::Mutex::new(1) };
    static mut MUTABLE: Mutex<u32> = Mutex::new(2);
    static WITH_CONST_NEW: Mutex<u32> = Mutex::const_new(RawMutex::new(), 4);

    fn local() {
        static E: ksync::RwLock<u32> = { ksync::RwLock::new(2) };
    }

    mod fake {
        use ksync::Mutex;
        macro_rules! static_lock { ($item:item) => { $item }; }
        static_lock! { static F: Mutex<u32> = Mutex::new(3); }
    }

    ksync::declare_unregistered!(G);

    trait Types { type Lock; }
    impl Types for () { type Lock = ksync::Mutex<u32>; }
    static H: <() as Types>::Lock = ksync::Mutex::new(4);
}

mod registered {
    use ksync::{Mutex, RwLock, static_lock};
    static_lock! { static A: Mutex<u32> = Mutex::new(0); }
    mod renamed {
        use ksync::{RwLock, static_lock as register};
        register! { static B: RwLock<u32> = RwLock::new(1); }
    }
}

mod out_of_scope {
    use std::sync::LazyLock;
    use ksync::{Mutex, RawMutex, RawRwLock, SpinConfig};

    const fn make() -> Mutex<u32> { Mutex::new(0) }
    static A: Mutex<u32> = make();
    static CUSTOM_RAW: Mutex<u32> =
        Mutex::const_new(RawMutex::with_config(SpinConfig), 0);
    static CUSTOM_RW_RAW: ksync::RwLock<u32> =
        ksync::RwLock::const_new(RawRwLock::with_config(SpinConfig), 0);
    static C: LazyLock<Mutex<u32>> = LazyLock::new(|| Mutex::new(0));
    struct Wrapper(Mutex<u32>);
    static D: Wrapper = Wrapper(Mutex::new(0));
}

mod allow {
    use ksync::Mutex;
    #[expect(clippy::allow_attributes, reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy")]
    #[allow(
        unknown_lints,
        unregistered_static_lock,
        reason = "CG-002-static-mutex-registration: test-only lock"
    )]
    static A: Mutex<u32> = Mutex::new(0);
}

fn main() {}
"#;

    const STDERR: &str = r#"error: static Mutex is not registered with static_lock!
  --> $DIR/unregistered_static_lock.rs:6:5
   |
LL |     static A: Mutex<u32> = Mutex::new(0);
   |     ^^^^^^^^^^^^^^^^^^^^^^^----------^^^^
   |                            |
   |                            register this lock so it receives a per-lock statistics class
   |
   = help: rewrite as `static_lock! { static NAME: Mutex<T> = Mutex::new(arg); }` (use `RwLock` as needed; remove `mut`); see docs/code-guidelines/CG-002-static-mutex-registration.md
   = note: `#[deny(unregistered_static_lock)]` on by default

error: static RwLock is not registered with static_lock!
  --> $DIR/unregistered_static_lock.rs:7:5
   |
LL |     static B: RwLock<u32> = RwLock::new(0);
   |     ^^^^^^^^^^^^^^^^^^^^^^^^-----------^^^^
   |                             |
   |                             register this lock so it receives a per-lock statistics class
   |
   = help: rewrite as `static_lock! { static NAME: Mutex<T> = Mutex::new(arg); }` (use `RwLock` as needed; remove `mut`); see docs/code-guidelines/CG-002-static-mutex-registration.md

error: static Mutex is not registered with static_lock!
  --> $DIR/unregistered_static_lock.rs:11:9
   |
LL |         static C: M<u32> = M::new(0);
   |         ^^^^^^^^^^^^^^^^^^^------^^^^
   |                            |
   |                            register this lock so it receives a per-lock statistics class
   |
   = help: rewrite as `static_lock! { static NAME: Mutex<T> = Mutex::new(arg); }` (use `RwLock` as needed; remove `mut`); see docs/code-guidelines/CG-002-static-mutex-registration.md

error: static Mutex is not registered with static_lock!
  --> $DIR/unregistered_static_lock.rs:14:5
   |
LL |     static D: ksync::Mutex<u32> = unsafe { ksync::Mutex::new(1) };
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^-----------------^^^^^^
   |                                            |
   |                                            register this lock so it receives a per-lock statistics class
   |
   = help: rewrite as `static_lock! { static NAME: Mutex<T> = Mutex::new(arg); }` (use `RwLock` as needed; remove `mut`); see docs/code-guidelines/CG-002-static-mutex-registration.md

error: static Mutex is not registered with static_lock!
  --> $DIR/unregistered_static_lock.rs:15:5
   |
LL |     static mut MUTABLE: Mutex<u32> = Mutex::new(2);
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^----------^^^^
   |                                      |
   |                                      register this lock so it receives a per-lock statistics class
   |
   = help: rewrite as `static_lock! { static NAME: Mutex<T> = Mutex::new(arg); }` (use `RwLock` as needed; remove `mut`); see docs/code-guidelines/CG-002-static-mutex-registration.md

error: static Mutex is not registered with static_lock!
  --> $DIR/unregistered_static_lock.rs:16:5
   |
LL |     static WITH_CONST_NEW: Mutex<u32> = Mutex::const_new(RawMutex::new(), 4);
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^----------------^^^^^^^^^^^^^^^^^^^^^
   |                                         |
   |                                         register this lock so it receives a per-lock statistics class
   |
   = help: rewrite as `static_lock! { static NAME: Mutex<T> = Mutex::new(arg); }` (use `RwLock` as needed; remove `mut`); see docs/code-guidelines/CG-002-static-mutex-registration.md

error: static RwLock is not registered with static_lock!
  --> $DIR/unregistered_static_lock.rs:19:9
   |
LL |         static E: ksync::RwLock<u32> = { ksync::RwLock::new(2) };
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^------------------^^^^^^
   |                                          |
   |                                          register this lock so it receives a per-lock statistics class
   |
   = help: rewrite as `static_lock! { static NAME: Mutex<T> = Mutex::new(arg); }` (use `RwLock` as needed; remove `mut`); see docs/code-guidelines/CG-002-static-mutex-registration.md

error: static Mutex is not registered with static_lock!
  --> $DIR/unregistered_static_lock.rs:25:24
   |
LL |         static_lock! { static F: Mutex<u32> = Mutex::new(3); }
   |                        ^^^^^^^^^^^^^^^^^^^^^^^----------^^^^
   |                                               |
   |                                               register this lock so it receives a per-lock statistics class
   |
   = help: rewrite as `static_lock! { static NAME: Mutex<T> = Mutex::new(arg); }` (use `RwLock` as needed; remove `mut`); see docs/code-guidelines/CG-002-static-mutex-registration.md

error: static Mutex is not registered with static_lock!
  --> $DIR/unregistered_static_lock.rs:28:5
   |
LL |     ksync::declare_unregistered!(G);
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ register this lock so it receives a per-lock statistics class
   |
   = help: rewrite as `static_lock! { static NAME: Mutex<T> = Mutex::new(arg); }` (use `RwLock` as needed; remove `mut`); see docs/code-guidelines/CG-002-static-mutex-registration.md
   = note: this error originates in the macro `ksync::declare_unregistered` (in Nightly builds, run with -Z macro-backtrace for more info)

error: static Mutex is not registered with static_lock!
  --> $DIR/unregistered_static_lock.rs:32:5
   |
LL |     static H: <() as Types>::Lock = ksync::Mutex::new(4);
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^-----------------^^^^
   |                                     |
   |                                     register this lock so it receives a per-lock statistics class
   |
   = help: rewrite as `static_lock! { static NAME: Mutex<T> = Mutex::new(arg); }` (use `RwLock` as needed; remove `mut`); see docs/code-guidelines/CG-002-static-mutex-registration.md

error: aborting due to 10 previous errors

"#;

    #[test]
    fn ui() {
        crate::testing::run_ui(
            "unregistered_static_lock",
            SOURCE,
            STDERR,
            &[crate::testing::MOCK_KSYNC],
        );
    }
}

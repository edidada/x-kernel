// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `unregistered_static_spinlock`: static `kspin::SpinLock` items that bypass
//! `static_lock!` registration.
//!
//! Rule: `docs/code-guidelines/CG-001-static-spinlock-registration.md`.

use rustc_hir::{Item, ItemKind};
use rustc_lint::{LateContext, LateLintPass};

use crate::static_lock_helpers::{
    defined_by_static_lock, direct_call_to_type, is_type, peel_blocks,
};

rustc_session::declare_lint! {
    /// ### What it does
    ///
    /// Reports a `static` or `static mut` item whose type is
    /// `kspin::SpinLock<G, T>` (directly or through `SpinRaw`,
    /// `SpinNoPreempt`, `SpinNoIrq`, a renamed import or an associated-type
    /// projection), whose initializer is a direct call to `SpinLock::new`,
    /// and which was not declared inside the real `static_lock!` macro from
    /// `kspin`, `ksync` or `klockstat_macros`. Items produced by other macros,
    /// including macros from other crates, are reported.
    ///
    /// ### Why is this bad?
    ///
    /// With the `stats` feature enabled, `SpinLock::new` binds the lock to
    /// the shared `klockstat::NOOP_CLASS`, so the lock never appears in
    /// `/proc/lock_stat`. `static_lock!` rewrites the initializer to
    /// `new_with_stats` with a per-lock class.
    ///
    /// ### Known problems
    ///
    /// Only the outermost initializer expression is inspected, after peeling
    /// statement-free `{ .. }` and `unsafe { .. }` blocks. The lint does not
    /// report statics whose type is a wrapper containing a lock, locks built
    /// by `const fn` helpers, lazily initialised locks (closures, `LazyLock`,
    /// `LazyInit`), `SpinRwLock`, explicit `new_with_stats` calls, or
    /// `ksync::Mutex` / `ksync::RwLock`.
    ///
    /// ### Example
    ///
    /// ```rust,ignore
    /// static LOCK: SpinNoIrq<u32> = SpinNoIrq::new(0);
    /// ```
    ///
    /// Use instead:
    ///
    /// ```rust,ignore
    /// use kspin::static_lock;
    ///
    /// static_lock! {
    ///     static LOCK: SpinNoIrq<u32> = SpinNoIrq::new(0);
    /// }
    /// ```
    pub UNREGISTERED_STATIC_SPINLOCK,
    Deny,
    "static kspin::SpinLock built with `new` outside static_lock!",
    report_in_external_macro
}

rustc_session::declare_lint_pass!(UnregisteredStaticSpinlock => [UNREGISTERED_STATIC_SPINLOCK]);

/// Returns whether `ty` is `kspin::SpinLock<G, T>`, however it was spelled.
impl<'tcx> LateLintPass<'tcx> for UnregisteredStaticSpinlock {
    fn check_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx Item<'tcx>) {
        let ItemKind::Static(_, _, _, body_id) = item.kind else {
            return;
        };
        let tcx = cx.tcx;
        let owner = item.owner_id;
        // Normalize so associated-type projections resolve to the lock ADT.
        let declared = tcx.type_of(owner).instantiate_identity();
        let item_ty = tcx
            .try_normalize_erasing_regions(cx.typing_env(), declared)
            .unwrap_or(declared);
        if !is_type(tcx, item_ty, "kspin", "SpinLock") {
            return;
        }
        if defined_by_static_lock(tcx, owner.to_def_id()) {
            return;
        }
        let types = tcx.typeck(owner.def_id);
        let body = tcx.hir_body(body_id);
        let Some(call) = direct_call_to_type(
            tcx,
            types,
            peel_blocks(body.value),
            "kspin",
            "SpinLock",
            "new",
        ) else {
            return;
        };
        tcx.node_span_lint(
            UNREGISTERED_STATIC_SPINLOCK,
            item.hir_id(),
            item.span,
            |diagnostic| {
                diagnostic.primary_message("static spinlock is not registered with static_lock!");
                diagnostic.span_label(
                    call,
                    "`SpinLock::new` binds `klockstat::NOOP_CLASS` under the `stats` feature; \
                     this lock never appears in /proc/lock_stat",
                );
                diagnostic.help(
                    "wrap the item: `static_lock! { static NAME: SpinNoIrq<T> = \
                     SpinNoIrq::new(..); }` (import `kspin::static_lock`); see \
                     docs/code-guidelines/CG-001-static-spinlock-registration.md",
                );
            },
        );
    }
}

#[cfg(test)]
mod tests {
    /// One crate covering flagged, registered, out-of-scope and allowed items.
    const SOURCE: &str = r#"#![allow(dead_code, unused)]

// Flagged: every static in this module must be reported.
mod flagged {
    use kspin::{SpinNoIrq, SpinRaw};

    // A: plain.
    static A: SpinNoIrq<u32> = SpinNoIrq::new(0);

    // B: alias through a renamed import.
    mod nested {
        use kspin::SpinRaw as L;

        static B: L<u32> = L::new(0);
    }

    // C: fully qualified paths.
    static C: kspin::SpinNoIrq<u32> = kspin::SpinNoIrq::new(0);

    // D: `static mut`.
    static mut D: SpinRaw<u32> = SpinRaw::new(0);

    // E: `unsafe { .. }` block.
    static E: SpinNoIrq<u32> = unsafe { SpinNoIrq::new(0) };

    // F: function-local static.
    fn local() {
        static F: kspin::SpinNoPreempt<u32> = kspin::SpinNoPreempt::new(0);
    }

    // G: attributed `pub(crate)` item with a `{ .. }` block.
    #[allow(dead_code)]
    pub(crate) static G: SpinNoIrq<u32> = { SpinNoIrq::new(0) };

    // H: a local macro named `static_lock` is not the registration macro.
    mod fake {
        use kspin::SpinNoIrq;

        macro_rules! static_lock {
            ($i:item) => {
                $i
            };
        }

        static_lock! {
            static H: SpinNoIrq<u32> = SpinNoIrq::new(0);
        }
    }

    // I: generated by a macro defined in another crate.
    kspin::declare_unregistered!(I);

    // J: type spelled as an associated-type projection.
    trait Types {
        type L;
    }
    impl Types for () {
        type L = kspin::SpinRaw<u32>;
    }
    static J: <() as Types>::L = kspin::SpinRaw::new(0);
}

// Registered through the real `kspin::static_lock!`: no diagnostics.
mod registered {
    use kspin::{SpinNoIrq, SpinRaw, static_lock};

    static_lock! {
        static A: SpinNoIrq<u32> = SpinNoIrq::new(0);
    }

    mod renamed {
        use kspin::{SpinRaw, static_lock as reg};

        reg! {
            static B: SpinRaw<u32> = SpinRaw::new(0);
        }
    }

    #[allow(dead_code)]
    static_lock! {
        static C: SpinRaw<u32> = SpinRaw::new(0);
    }
}

// Outside the rule's scope: no diagnostics.
mod out_of_scope {
    use std::sync::LazyLock;

    use kspin::{SpinNoIrq, SpinRaw, SpinRwNoIrq, static_lock};

    // Wrapper types are not `kspin::SpinLock` themselves.
    struct W(SpinRaw<u32>);
    static A: W = W(SpinRaw::new(0));

    struct Named {
        lock: SpinRaw<u32>,
    }
    static B: Named = Named {
        lock: SpinRaw::new(0),
    };

    // Lazily created locks are built at run time.
    static C: LazyLock<SpinRaw<u32>> = LazyLock::new(|| SpinRaw::new(0));

    // Locks built by a const helper are not a direct `new` call.
    const fn mk() -> SpinNoIrq<u32> {
        SpinNoIrq::new(0)
    }
    static D: SpinNoIrq<u32> = mk();

    // Explicit statistics class.
    static E: SpinNoIrq<u32> = SpinNoIrq::new_with_stats(0, &kspin::NOOP_CLASS);

    // Reader-writer locks are not `SpinLock`.
    static F: SpinRwNoIrq<u32> = SpinRwNoIrq::new(0);

    // An unrelated type that merely shares the name.
    mod other {
        pub struct SpinLock<T>(T);

        impl<T> SpinLock<T> {
            pub const fn new(t: T) -> Self {
                Self(t)
            }
        }

        static UNRELATED: SpinLock<u32> = SpinLock::new(0);
    }

    // A wrapper holding a reference to an already registered lock.
    static_lock! {
        static REGISTERED: SpinRaw<u32> = SpinRaw::new(0);
    }
    struct R(&'static SpinRaw<u32>);
    static G: R = R(&REGISTERED);
}

// `#[allow]` on the item is honoured: no diagnostics.
mod allow {
    use kspin::SpinNoIrq;

    #[expect(clippy::allow_attributes, reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy")]
    #[allow(unknown_lints, unregistered_static_spinlock, reason = "CG-001-static-spinlock-registration: scratch test-only lock")]
    static A: SpinNoIrq<u32> = SpinNoIrq::new(0);

    #[allow(unregistered_static_spinlock)]
    static B: SpinNoIrq<u32> = SpinNoIrq::new(0);
}

fn main() {}
"#;

    /// Expected diagnostics for [`SOURCE`], as rendered by compiletest.
    const STDERR: &str = r#"error: static spinlock is not registered with static_lock!
  --> $DIR/unregistered_static_spinlock.rs:8:5
   |
LL |     static A: SpinNoIrq<u32> = SpinNoIrq::new(0);
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^--------------^^^^
   |                                |
   |                                `SpinLock::new` binds `klockstat::NOOP_CLASS` under the `stats` feature; this lock never appears in /proc/lock_stat
   |
   = help: wrap the item: `static_lock! { static NAME: SpinNoIrq<T> = SpinNoIrq::new(..); }` (import `kspin::static_lock`); see docs/code-guidelines/CG-001-static-spinlock-registration.md
   = note: `#[deny(unregistered_static_spinlock)]` on by default

error: static spinlock is not registered with static_lock!
  --> $DIR/unregistered_static_spinlock.rs:14:9
   |
LL |         static B: L<u32> = L::new(0);
   |         ^^^^^^^^^^^^^^^^^^^------^^^^
   |                            |
   |                            `SpinLock::new` binds `klockstat::NOOP_CLASS` under the `stats` feature; this lock never appears in /proc/lock_stat
   |
   = help: wrap the item: `static_lock! { static NAME: SpinNoIrq<T> = SpinNoIrq::new(..); }` (import `kspin::static_lock`); see docs/code-guidelines/CG-001-static-spinlock-registration.md

error: static spinlock is not registered with static_lock!
  --> $DIR/unregistered_static_spinlock.rs:18:5
   |
LL |     static C: kspin::SpinNoIrq<u32> = kspin::SpinNoIrq::new(0);
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^---------------------^^^^
   |                                       |
   |                                       `SpinLock::new` binds `klockstat::NOOP_CLASS` under the `stats` feature; this lock never appears in /proc/lock_stat
   |
   = help: wrap the item: `static_lock! { static NAME: SpinNoIrq<T> = SpinNoIrq::new(..); }` (import `kspin::static_lock`); see docs/code-guidelines/CG-001-static-spinlock-registration.md

error: static spinlock is not registered with static_lock!
  --> $DIR/unregistered_static_spinlock.rs:21:5
   |
LL |     static mut D: SpinRaw<u32> = SpinRaw::new(0);
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^------------^^^^
   |                                  |
   |                                  `SpinLock::new` binds `klockstat::NOOP_CLASS` under the `stats` feature; this lock never appears in /proc/lock_stat
   |
   = help: wrap the item: `static_lock! { static NAME: SpinNoIrq<T> = SpinNoIrq::new(..); }` (import `kspin::static_lock`); see docs/code-guidelines/CG-001-static-spinlock-registration.md

error: static spinlock is not registered with static_lock!
  --> $DIR/unregistered_static_spinlock.rs:24:5
   |
LL |     static E: SpinNoIrq<u32> = unsafe { SpinNoIrq::new(0) };
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^--------------^^^^^^
   |                                         |
   |                                         `SpinLock::new` binds `klockstat::NOOP_CLASS` under the `stats` feature; this lock never appears in /proc/lock_stat
   |
   = help: wrap the item: `static_lock! { static NAME: SpinNoIrq<T> = SpinNoIrq::new(..); }` (import `kspin::static_lock`); see docs/code-guidelines/CG-001-static-spinlock-registration.md

error: static spinlock is not registered with static_lock!
  --> $DIR/unregistered_static_spinlock.rs:28:9
   |
LL |         static F: kspin::SpinNoPreempt<u32> = kspin::SpinNoPreempt::new(0);
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^-------------------------^^^^
   |                                               |
   |                                               `SpinLock::new` binds `klockstat::NOOP_CLASS` under the `stats` feature; this lock never appears in /proc/lock_stat
   |
   = help: wrap the item: `static_lock! { static NAME: SpinNoIrq<T> = SpinNoIrq::new(..); }` (import `kspin::static_lock`); see docs/code-guidelines/CG-001-static-spinlock-registration.md

error: static spinlock is not registered with static_lock!
  --> $DIR/unregistered_static_spinlock.rs:33:5
   |
LL |     pub(crate) static G: SpinNoIrq<u32> = { SpinNoIrq::new(0) };
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^--------------^^^^^^
   |                                             |
   |                                             `SpinLock::new` binds `klockstat::NOOP_CLASS` under the `stats` feature; this lock never appears in /proc/lock_stat
   |
   = help: wrap the item: `static_lock! { static NAME: SpinNoIrq<T> = SpinNoIrq::new(..); }` (import `kspin::static_lock`); see docs/code-guidelines/CG-001-static-spinlock-registration.md

error: static spinlock is not registered with static_lock!
  --> $DIR/unregistered_static_spinlock.rs:46:13
   |
LL |             static H: SpinNoIrq<u32> = SpinNoIrq::new(0);
   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^--------------^^^^
   |                                        |
   |                                        `SpinLock::new` binds `klockstat::NOOP_CLASS` under the `stats` feature; this lock never appears in /proc/lock_stat
   |
   = help: wrap the item: `static_lock! { static NAME: SpinNoIrq<T> = SpinNoIrq::new(..); }` (import `kspin::static_lock`); see docs/code-guidelines/CG-001-static-spinlock-registration.md

error: static spinlock is not registered with static_lock!
  --> $DIR/unregistered_static_spinlock.rs:51:5
   |
LL |     kspin::declare_unregistered!(I);
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ `SpinLock::new` binds `klockstat::NOOP_CLASS` under the `stats` feature; this lock never appears in /proc/lock_stat
   |
   = help: wrap the item: `static_lock! { static NAME: SpinNoIrq<T> = SpinNoIrq::new(..); }` (import `kspin::static_lock`); see docs/code-guidelines/CG-001-static-spinlock-registration.md
   = note: this error originates in the macro `kspin::declare_unregistered` (in Nightly builds, run with -Z macro-backtrace for more info)

error: static spinlock is not registered with static_lock!
  --> $DIR/unregistered_static_spinlock.rs:60:5
   |
LL |     static J: <() as Types>::L = kspin::SpinRaw::new(0);
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^-------------------^^^^
   |                                  |
   |                                  `SpinLock::new` binds `klockstat::NOOP_CLASS` under the `stats` feature; this lock never appears in /proc/lock_stat
   |
   = help: wrap the item: `static_lock! { static NAME: SpinNoIrq<T> = SpinNoIrq::new(..); }` (import `kspin::static_lock`); see docs/code-guidelines/CG-001-static-spinlock-registration.md

error: aborting due to 10 previous errors

"#;

    #[test]
    fn ui() {
        crate::testing::run_ui(
            "unregistered_static_spinlock",
            SOURCE,
            STDERR,
            &[crate::testing::MOCK_KSPIN],
        );
    }
}

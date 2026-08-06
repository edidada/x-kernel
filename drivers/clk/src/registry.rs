// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Clock-provider registry, keyed by **OS-neutral provider name**.
//!
//! Unlike `device-res`'s single MMIO/DMA/IRQ provider, a clock tree has many
//! producers, so this is a small lock-guarded table rather than a `Once`. A
//! linear scan is fine: there are only a handful of clock-provider nodes per
//! board and lookups happen at probe time, never on a data hot path.
//!
//! # Why a name key (not a DT phandle)
//!
//! The registry is keyed by `&'static str` — the provider's OS-neutral name
//! (for the DT path, the producer node name with its `@unit` suffix stripped).
//! This keeps the registry firmware-description-agnostic: the `clk` crate knows
//! nothing about phandles or the device tree, so an ACPI/PCI backend can drive
//! the same registry with its own naming. The DT→name translation lives once,
//! outside this crate — in the firmware layer that parses `clocks`, which the
//! bus layer then turns into `ClkDesc`.
//!
//! # Injectable, with a process-global default
//!
//! [`ClkRegistry`] is a plain struct owning its entries plus a core-only
//! spinlock — no `k*` dependency, so this crate stays OS-agnostic and unit
//! tests construct their own registry without touching globals. A
//! process-global default ([`GLOBAL`]) backs the free functions
//! ([`register_provider`] / [`try_provider`]) so call sites that don't need
//! injection stay unchanged.

use alloc::{sync::Arc, vec::Vec};
use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::ClkProvider;

/// Minimal core-only spinlock: serialises access with an `AtomicBool` flag.
/// Fairness is irrelevant here (a handful of providers, probe-time only, no
/// deep nesting), so a simple test-and-set test-and-test loop suffices and
/// keeps this crate free of any host lock dependency.
struct Spin<T> {
    locked: AtomicBool,
    inner: UnsafeCell<T>,
}

// SAFETY: the only way to reach `inner` is through `Spin::with`, which holds
// the `locked` flag for the duration. `T: Send` is the data's own
// thread-safety bound; we provide the synchronisation, so `Sync` is sound.
unsafe impl<T: Send> Sync for Spin<T> {}
// SAFETY: `Spin<T>` is moveable between threads exactly when `T` is; the lock
// itself is a plain `AtomicBool` (already `Send`). There is no interior data
// that pins it to a thread.
unsafe impl<T: Send> Send for Spin<T> {}

impl<T> Spin<T> {
    const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            inner: UnsafeCell::new(value),
        }
    }

    /// Run `f` with exclusive access to the inner value.
    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        // Acquire: spin until we win the CAS, test-and-test to spare the bus.
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        // SAFETY: we hold `locked` (set above with Acquire), so no other thread
        // can reach `inner` concurrently; the borrow lives only for `f`'s
        // duration and is released before we drop the lock.
        let r = f(unsafe { &mut *self.inner.get() });
        self.locked.store(false, Ordering::Release);
        r
    }
}

/// A clock-provider registry: name → provider. Owns its entries behind a
/// core-only spinlock, so it is `Send + Sync` and `const`-constructible.
pub struct ClkRegistry {
    entries: Spin<Vec<(&'static str, Arc<dyn ClkProvider>)>>,
}

impl ClkRegistry {
    /// Construct an empty registry. `const`, so it can back a `static`.
    pub const fn new() -> Self {
        Self {
            entries: Spin::new(Vec::new()),
        }
    }

    /// Register `provider` under `name`. If `name` is already registered, the
    /// new provider shadows the old one (lookups scan front-to-back), mirroring
    /// Linux's last-writer behaviour for `of_clk_add_hw_provider`.
    pub fn register(&self, name: &'static str, provider: Arc<dyn ClkProvider>) {
        self.entries.with(|entries| {
            if let Some(pos) = entries.iter().position(|(n, _)| *n == name) {
                entries.remove(pos);
            }
            entries.insert(0, (name, provider));
        });
    }

    /// Look up the provider registered under `name`, if any.
    pub fn try_provider(&self, name: &str) -> Option<Arc<dyn ClkProvider>> {
        self.entries.with(|entries| {
            entries
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, p)| p.clone())
        })
    }
}

impl Default for ClkRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-global default registry backing the free-function API.
///
/// x-kernel is single-instance today, so a global is the natural default; the
/// struct-based API ([`ClkRegistry::new`]) exists for test isolation and for
/// future multi-instance hosts (e.g. a hypervisor managing guest clock trees),
/// which construct their own registry instead of touching this one.
static GLOBAL: ClkRegistry = ClkRegistry::new();

/// Register a clock provider under its OS-neutral name.
///
/// Called by a provider's probe (SoC CRU driver, SCMI client, `fixed-clock`
/// registration, ...) once it can answer rate queries.
pub fn register_provider(name: &'static str, provider: Arc<dyn ClkProvider>) {
    GLOBAL.register(name, provider);
}

/// Look up a provider in the process-global registry by name.
pub(crate) fn try_provider(name: &str) -> Option<Arc<dyn ClkProvider>> {
    GLOBAL.try_provider(name)
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    /// A provider that always returns the same rate regardless of specifier.
    struct FixedRate(u64);
    impl ClkProvider for FixedRate {
        fn get_rate(&self, _specifier: &[u32]) -> Option<u64> {
            Some(self.0)
        }
    }

    #[def_test]
    fn register_routes_and_shadows() {
        let reg = ClkRegistry::new();
        reg.register("a", Arc::new(FixedRate(1)));
        reg.register("b", Arc::new(FixedRate(2)));
        assert_eq!(reg.try_provider("a").unwrap().get_rate(&[]), Some(1));
        assert_eq!(reg.try_provider("b").unwrap().get_rate(&[]), Some(2));
        assert!(reg.try_provider("missing").is_none());

        // Re-registration shadows.
        reg.register("a", Arc::new(FixedRate(9)));
        assert_eq!(reg.try_provider("a").unwrap().get_rate(&[]), Some(9));
    }

    #[def_test]
    fn injectable_registry_is_isolated() {
        // Two registries are independent — no global state shared.
        let r1 = ClkRegistry::new();
        let r2 = ClkRegistry::new();
        r1.register("only-in-r1", Arc::new(FixedRate(7)));
        assert!(r1.try_provider("only-in-r1").is_some());
        assert!(r2.try_provider("only-in-r1").is_none());
    }
}

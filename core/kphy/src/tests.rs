// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for the phy core: refcount edges (hw ops run exactly on
//! 0→1 / 1→0), failure rollback, unbalance rejection, mode binding via
//! acquire, and registry resolution.

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use kspin::SpinNoIrq;
use unittest::{assert, assert_eq, def_test};

use crate::{
    PhyError, PhyHwOps, PhyInstance, PhyLookup, PhyMode, PhySpecifier, phy_get, provider_installed,
    register_provider, unregister_provider,
};

/// Recording fake: counts every op call, `power_on` fails on demand.
#[derive(Default)]
struct FakePhy {
    inits: SpinNoIrq<u32>,
    exits: SpinNoIrq<u32>,
    power_ons: SpinNoIrq<u32>,
    power_offs: SpinNoIrq<u32>,
    modes: SpinNoIrq<Vec<PhyMode>>,
    fail_power_on: SpinNoIrq<bool>,
}

impl PhyHwOps for FakePhy {
    fn hw_init(&self) -> Result<(), PhyError> {
        *self.inits.lock() += 1;
        Ok(())
    }

    fn hw_exit(&self) -> Result<(), PhyError> {
        *self.exits.lock() += 1;
        Ok(())
    }

    fn hw_power_on(&self) -> Result<(), PhyError> {
        if *self.fail_power_on.lock() {
            return Err(PhyError::Io);
        }
        *self.power_ons.lock() += 1;
        Ok(())
    }

    fn hw_power_off(&self) {
        *self.power_offs.lock() += 1;
    }

    fn hw_set_mode(&self, mode: PhyMode) -> Result<(), PhyError> {
        self.modes.lock().push(mode);
        Ok(())
    }
}

struct TestProvider {
    id: u32,
    /// Mode already bound by the first acquirer (None = unbound).
    bound: SpinNoIrq<Option<PhyMode>>,
    phy: Arc<FakePhy>,
    instance: SpinNoIrq<Option<Arc<PhyInstance>>>,
}

impl TestProvider {
    fn new(id: u32) -> Arc<Self> {
        Arc::new(Self {
            id,
            bound: SpinNoIrq::new(None),
            phy: Arc::new(FakePhy::default()),
            instance: SpinNoIrq::new(None),
        })
    }
}

impl PhyLookup for TestProvider {
    fn provider_id(&self) -> u32 {
        self.id
    }

    fn acquire(&self, spec: PhySpecifier) -> Result<Arc<PhyInstance>, PhyError> {
        // The combphy contract: the specifier cell binds the mode; a
        // conflicting second acquire is rejected.
        let mode = spec.first();
        let mut bound = self.bound.lock();
        match *bound {
            None => *bound = Some(mode),
            Some(bound) if bound != mode => return Err(PhyError::ModeConflict),
            _ => {}
        }
        drop(bound);
        let mut slot = self.instance.lock();
        Ok(slot
            .get_or_insert_with(|| PhyInstance::new(self.phy.clone()))
            .clone())
    }
}

fn spec(mode: u32) -> PhySpecifier {
    PhySpecifier {
        cells: [mode, 0],
        ncells: 1,
    }
}

#[def_test(serial)]
fn hw_ops_run_exactly_on_refcount_edges() {
    let provider: &'static TestProvider = Box::leak(Box::new(TestProvider::new(0x11)));
    register_provider(provider);

    let a = phy_get(spec(2), 0x11).expect("acquire");
    let b = phy_get(spec(2), 0x11).expect("shared acquire");

    a.init().unwrap();
    assert_eq!(*provider.phy.inits.lock(), 1, "hw init on the 0→1 edge");
    b.init().unwrap();
    assert_eq!(*provider.phy.inits.lock(), 1, "second consumer: no hw call");

    a.power_on().unwrap();
    b.power_on().unwrap();
    assert_eq!(*provider.phy.power_ons.lock(), 1);

    a.power_off().unwrap();
    assert_eq!(*provider.phy.power_offs.lock(), 0, "still one consumer on");
    b.power_off().unwrap();
    assert_eq!(*provider.phy.power_offs.lock(), 1, "hw off on the 1→0 edge");

    a.exit().unwrap();
    assert_eq!(*provider.phy.exits.lock(), 0);
    b.exit().unwrap();
    assert_eq!(*provider.phy.exits.lock(), 1);

    unregister_provider(0x11);
}

#[def_test(serial)]
fn failed_power_on_rolls_the_count_back() {
    let provider: &'static TestProvider = Box::leak(Box::new(TestProvider::new(0x12)));
    register_provider(provider);
    *provider.phy.fail_power_on.lock() = true;

    let handle = phy_get(spec(2), 0x12).unwrap();
    handle.init().unwrap();
    assert!(matches!(handle.power_on(), Err(PhyError::Io)));

    // Recovery path: the failed attempt left no count; a clean power_on
    // still runs the hardware op.
    *provider.phy.fail_power_on.lock() = false;
    handle.power_on().unwrap();
    assert_eq!(*provider.phy.power_ons.lock(), 1);

    unregister_provider(0x12);
}

#[def_test(serial)]
fn unbalanced_exit_and_power_off_are_rejected() {
    let provider: &'static TestProvider = Box::leak(Box::new(TestProvider::new(0x13)));
    register_provider(provider);

    let handle = phy_get(spec(2), 0x13).unwrap();
    assert!(matches!(handle.exit(), Err(PhyError::Unbalanced)));
    assert!(matches!(handle.power_off(), Err(PhyError::Unbalanced)));

    unregister_provider(0x13);
}

#[def_test(serial)]
fn acquire_binds_mode_and_rejects_conflicts() {
    let provider: &'static TestProvider = Box::leak(Box::new(TestProvider::new(0x14)));
    register_provider(provider);

    let first = phy_get(spec(2), 0x14).unwrap(); // PHY_TYPE_PCIE
    assert!(matches!(
        phy_get(spec(1), 0x14),
        Err(PhyError::ModeConflict)
    ));
    let again = phy_get(spec(2), 0x14).unwrap(); // same mode: shared
    first.init().unwrap();
    again.init().unwrap();
    assert_eq!(*provider.phy.inits.lock(), 1);

    unregister_provider(0x14);
}

#[def_test(serial)]
fn set_mode_reaches_the_provider_and_missing_provider_defers() {
    let provider: &'static TestProvider = Box::leak(Box::new(TestProvider::new(0x15)));
    register_provider(provider);

    let handle = phy_get(spec(0), 0x15).unwrap();
    handle.set_mode(1).unwrap();
    assert_eq!(*provider.phy.modes.lock(), vec![1]);

    unregister_provider(0x15);
    assert!(!provider_installed());
    assert!(matches!(phy_get(spec(0), 0x15), Err(PhyError::NoProvider)));
}

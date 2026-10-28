// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for the reset-line core: hiword write semantics, the
//! bit<16 construction guard, and provider registry resolution.

use alloc::{boxed::Box, collections::BTreeMap, sync::Arc, vec::Vec};

use kspin::SpinNoIrq;
use unittest::{assert, assert_eq, def_test};

use crate::{
    ProviderId, ResetError, ResetLine, ResetLookup, ResetRegAccess, provider_installed,
    register_provider, reset_get, unregister_provider,
};

/// Array-backed register window recording every write (for hiword shape
/// assertions).
struct MockReg {
    mem: SpinNoIrq<BTreeMap<usize, u32>>,
    /// (offset, value) pairs in write order.
    writes: SpinNoIrq<Vec<(usize, u32)>>,
}

impl MockReg {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            mem: SpinNoIrq::new(BTreeMap::new()),
            writes: SpinNoIrq::new(Vec::new()),
        })
    }
}

impl ResetRegAccess for MockReg {
    fn reg_read(&self, offset: usize) -> u32 {
        self.mem.lock().get(&offset).copied().unwrap_or(0)
    }

    fn reg_write(&self, offset: usize, value: u32) {
        // Model the hiword convention like the hardware: the written value's
        // upper half is the write-enable mask; only the masked low-half bits
        // change, the rest of the word is preserved.
        let mut mem = self.mem.lock();
        let old = mem.get(&offset).copied().unwrap_or(0);
        let write_mask = value >> 16;
        let new = (old & !write_mask) | (value & write_mask);
        mem.insert(offset, new);
        drop(mem);
        self.writes.lock().push((offset, value));
    }
}

struct TestProvider {
    id: ProviderId,
    lines: [(u32, Arc<ResetLine>); 1],
}

impl ResetLookup for TestProvider {
    fn provider_id(&self) -> ProviderId {
        self.id
    }

    fn get_by_index(&self, index: u32) -> Option<Arc<ResetLine>> {
        self.lines
            .iter()
            .find(|(i, _)| *i == index)
            .map(|(_, l)| l.clone())
    }
}

#[def_test(serial)]
fn hiword_write_carries_value_and_mask() {
    let reg = MockReg::new();
    let line = ResetLine::new(reg.clone(), 0xa84, 15).expect("bit < 16");

    line.assert();
    let writes = reg.writes.lock();
    let last = *writes.last().expect("one write");
    assert_eq!(last, (0xa84, (1 << 15) | (1 << 31) | (1 << 15)));
    drop(writes);
    assert!(line.status(), "assert reads set");

    line.deassert();
    assert!(!line.status(), "deassert reads clear");
    // The deassert store is still a hiword write (mask in the high half).
    let last = *reg.writes.lock().last().expect("second write");
    assert_eq!(last.1 & (1 << 15), 0, "value bit clear");
    assert_eq!(last.1 >> 16, 1 << 15, "write-enable mask present");
}

#[def_test(serial)]
fn bit_must_stay_below_the_hiword_half() {
    let reg = MockReg::new();
    assert!(
        ResetLine::new(reg, 0, 16).is_none(),
        "bit 16 collides with the mask half"
    );
}

#[def_test(serial)]
fn registry_resolves_by_provider_and_index() {
    let reg = MockReg::new();
    let line = ResetLine::new(reg.clone(), 0xa84, 0).expect("bit < 16");
    let provider: &'static TestProvider = Box::leak(Box::new(TestProvider {
        id: 0x02,
        lines: [(0x210, alloc::sync::Arc::new(line))],
    }));
    register_provider(provider);

    let handle = reset_get(0x02, 0x210).expect("registered line");
    handle.deassert();
    assert!(!handle.status());
    handle.assert();
    assert!(handle.status());

    assert!(matches!(reset_get(0x02, 0x999), Err(ResetError::NotFound)));
    assert!(matches!(
        reset_get(0x99, 0x210),
        Err(ResetError::NoProvider)
    ));
    assert!(provider_installed());

    unregister_provider(0x02);
    assert!(!provider_installed());
}

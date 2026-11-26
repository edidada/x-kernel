// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `IoMapClkReg` over a fixture-backed mapping.

use alloc::{sync::Arc, vec};

use driver_os_api::{IoMap, MmioMapping, MmioOp, MmioRegion, ResError, ResResult};
use kclk::{
    ClkBranch, ClkError, ClkFlags, ClkNodeDesc, ClkRegAccess, ClkTable, GateFlags, register_table,
    reset_providers,
};
use unittest::{assert_eq, def_test};

use super::IoMapClkReg;

const WINDOW_BYTES: usize = 16;

/// Fixture mapping provider: every mapping aliases one leaked window and the
/// unmapped path is a no-op, so no memspace state is involved.
#[derive(Clone, Copy)]
struct FixtureProvider {
    vaddr: usize,
}

// SAFETY: the leaked window outlives the test, every mapping returned aliases
// that same window, and `unmap_mmio` only forgets the alias.
unsafe impl MmioOp for FixtureProvider {
    fn map_mmio(&self, region: MmioRegion, _name: &'static str) -> ResResult<MmioMapping> {
        if region.size == 0 || region.size > WINDOW_BYTES {
            return Err(ResError::InvalidResource);
        }
        Ok(MmioMapping {
            vaddr: self.vaddr,
            region,
        })
    }

    unsafe fn unmap_mmio(&self, _mapping: MmioMapping) {}
}

/// A `ClkRegAccess` over a `WINDOW_BYTES`-sized fixture window.
fn fixture_reg() -> IoMapClkReg {
    let words: &'static mut [u32] = vec![0u32; WINDOW_BYTES / 4].leak();
    let providers: &'static mut [FixtureProvider] = vec![FixtureProvider {
        vaddr: words.as_mut_ptr() as usize,
    }]
    .leak();
    let map = IoMap::map_with(
        &providers[0],
        MmioRegion::new(0x4000_0000, WINDOW_BYTES),
        "clk-reg-test",
    )
    .expect("fixture mapping");
    IoMapClkReg::new(map)
}

#[def_test(serial)]
fn accesses_go_through_the_mapped_window_and_report_its_size() {
    let reg = fixture_reg();
    assert_eq!(reg.window_bytes(), Some(WINDOW_BYTES));
    assert_eq!(reg.region().size, WINDOW_BYTES);

    reg.reg_write(4, 0xdead_beef);
    assert_eq!(reg.reg_read(4), 0xdead_beef);
    // A read-modify-write keeps the untouched fields of the same word.
    reg.reg_modify(4, 0x0000_ff00, 0x0000_1200);
    assert_eq!(reg.reg_read(4), 0xdead_12ef);
}

#[def_test(serial)]
fn a_descriptor_outside_the_mapping_is_rejected_by_the_clock_tree() {
    static TABLE: ClkTable = ClkTable::new(0x73);

    // The adapter reports the mapping size as its register window, so the clock
    // framework rejects a table entry that would reach past the mapping instead
    // of touching memory the mapping does not cover.
    TABLE.clear();
    reset_providers();
    let reg: Arc<dyn ClkRegAccess> = Arc::new(fixture_reg());
    let past_end = [ClkBranch::new(
        0,
        "past_end",
        &[],
        ClkFlags::empty(),
        ClkNodeDesc::Gate {
            offset_bytes: WINDOW_BYTES,
            bit: 0,
            flags: GateFlags::empty(),
        },
    )];
    assert_eq!(
        register_table(&TABLE, reg, &past_end),
        Err(ClkError::InvalidValue)
    );
    reset_providers();

    // A descriptor inside the window registers without error.
    let reg: Arc<dyn ClkRegAccess> = Arc::new(fixture_reg());
    let inside = [ClkBranch::new(
        0,
        "inside",
        &[],
        ClkFlags::empty(),
        ClkNodeDesc::Gate {
            offset_bytes: WINDOW_BYTES - 4,
            bit: 0,
            flags: GateFlags::empty(),
        },
    )];
    register_table(&TABLE, reg, &inside).expect("in-window descriptor");
    reset_providers();
}
// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Global default affinity tests.

use kcpu_id_map::{KCpuMask, KCpuMaskExt, LogicalCpuId};
use unittest::{TestResult, assert, assert_eq, def_test};

use crate::{
    DefaultSmpAffinityError, IrqDesc, IrqDescError,
    runtime::{
        default_affinity::{default_smp_affinity, reset_for_tests, set_default_smp_affinity},
        state::IrqStateDesc,
    },
};

struct ResetDefault;

impl Drop for ResetDefault {
    fn drop(&mut self) {
        reset_for_tests(None);
    }
}

fn reset_default() -> ResetDefault {
    reset_for_tests(None);
    ResetDefault
}

fn cpu_mask(cpu: usize) -> KCpuMask {
    let mut mask = KCpuMask::new();
    mask.set_logical(LogicalCpuId::new(cpu), true);
    mask
}

fn ordinary_entry(virq: usize) -> IrqStateDesc {
    IrqStateDesc::test_new(IrqDesc::from_virq(virq))
}

#[def_test(serial)]
fn initial_default_is_present_and_feeds_any_policy() -> TestResult {
    let _reset = reset_default();
    let Some(present) = kcpu_id_map::present_mask() else {
        return TestResult::Ignored;
    };
    assert_eq!(default_smp_affinity(), Some(present));
    assert_eq!(
        ordinary_entry(0x7100).prepare_initial_affinity_policy(),
        Ok(Some(present))
    );
    assert_eq!(
        ordinary_entry(0x7101).prepare_initial_affinity_policy_with(None),
        Err(IrqDescError::NotReady)
    );
    TestResult::Ok
}

#[def_test(serial)]
fn written_default_feeds_future_any_policies_only() -> TestResult {
    let _reset = reset_default();
    let Some(present) = kcpu_id_map::present_mask() else {
        return TestResult::Ignored;
    };
    let replacement = if present.get_logical(LogicalCpuId::new(0)) {
        cpu_mask(0)
    } else {
        return TestResult::Ignored;
    };
    let mut old = ordinary_entry(0x7102);
    old.affinity.requested = Some(present);

    assert_eq!(set_default_smp_affinity(&replacement), Ok(()));
    assert_eq!(default_smp_affinity(), Some(replacement));
    assert_eq!(
        ordinary_entry(0x7103).prepare_initial_affinity_policy(),
        Ok(Some(replacement))
    );
    assert_eq!(old.prepare_initial_affinity_policy(), Ok(None));
    assert_eq!(old.affinity.requested, Some(present));
    assert_eq!(old.affinity.effective, None);
    TestResult::Ok
}

#[def_test(serial)]
fn invalid_default_is_rejected_without_replacing_old_value() -> TestResult {
    let _reset = reset_default();
    let Some(present) = kcpu_id_map::present_mask() else {
        return TestResult::Ignored;
    };
    let replacement = if present.get_logical(LogicalCpuId::new(0)) {
        cpu_mask(0)
    } else {
        return TestResult::Ignored;
    };
    set_default_smp_affinity(&replacement).unwrap();

    assert_eq!(
        set_default_smp_affinity(&KCpuMask::new()),
        Err(DefaultSmpAffinityError::Empty)
    );
    assert_eq!(default_smp_affinity(), Some(replacement));

    let full = KCpuMask::full();
    if full != present {
        let error = set_default_smp_affinity(&full);
        assert!(matches!(
            error,
            Err(DefaultSmpAffinityError::NotPresent { .. })
        ));
        assert_eq!(default_smp_affinity(), Some(replacement));
    }
    TestResult::Ok
}

#[def_test(serial)]
fn explicit_requested_policy_is_not_overwritten_by_default() -> TestResult {
    let _reset = reset_default();
    let Some(present) = kcpu_id_map::present_mask() else {
        return TestResult::Ignored;
    };
    let mut entry = ordinary_entry(0x7104);
    entry.affinity.requested = Some(present);
    assert_eq!(entry.prepare_initial_affinity_policy(), Ok(None));
    assert_eq!(entry.affinity.requested, Some(present));
    TestResult::Ok
}

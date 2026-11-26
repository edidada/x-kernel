// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Global default affinity for future ordinary IRQ Lines.
//!
//! The default is consumed only while preparing an initial ordinary
//! `IrqAffinity::Any` policy. It is never read by dispatch and never migrates
//! an existing Line.

use kcpu_id_map::{KCpuMask, KCpuMaskExt};
use kiface_task_api::TaskBlockingContextIf;
use kspin::{SpinNoPreempt, static_lock};

use crate::DefaultSmpAffinityError;

static_lock! {
    static DEFAULT_SMP_AFFINITY: SpinNoPreempt<Option<KCpuMask>> =
        SpinNoPreempt::new(None);
}

/// Returns the current global default, initializing it from the present CPU
/// mask on first use. `None` means the runtime CPU map is not ready yet.
pub fn default_smp_affinity() -> Option<KCpuMask> {
    let mut default = DEFAULT_SMP_AFFINITY.lock();
    if default.is_none() {
        *default = kcpu_id_map::present_mask();
    }
    *default
}

/// Replaces the default used by future ordinary `IrqAffinity::Any` policies.
///
/// Requires sleepable task context. Existing Lines are not inspected or
/// migrated; failure leaves the previous default unchanged.
pub fn set_default_smp_affinity(mask: &KCpuMask) -> Result<(), DefaultSmpAffinityError> {
    if crate::context::is_in_interrupt_context() || !TaskBlockingContextIf::is_sleepable_context() {
        return Err(DefaultSmpAffinityError::InvalidContext {
            operation: "set default affinity",
        });
    }
    if mask.is_empty() {
        return Err(DefaultSmpAffinityError::Empty);
    }
    let present = kcpu_id_map::present_mask().ok_or(DefaultSmpAffinityError::NotReady)?;
    for cpu in mask.iter_logical() {
        if !present.get_logical(cpu) {
            return Err(DefaultSmpAffinityError::NotPresent {
                cpu: cpu.as_usize(),
            });
        }
    }
    *DEFAULT_SMP_AFFINITY.lock() = Some(*mask);
    Ok(())
}

#[cfg(unittest)]
pub(crate) fn reset_for_tests(default: Option<KCpuMask>) {
    *DEFAULT_SMP_AFFINITY.lock() = default;
}

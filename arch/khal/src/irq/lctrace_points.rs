// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LCTrace probes on the IRQ trap entry path.
//!
//! Each probe has a real body when the `lctrace` feature is on and an empty
//! one otherwise, so the trap adapter stays free of `cfg` attributes.
//!
//! These run in interrupt context with local IRQs masked. `lctrace::record`
//! reserves its slot with a single atomic increment and never blocks, so it is
//! valid here and in pseudo-NMI context.

/// Records entry into the normal IRQ trap adapter.
#[cfg(feature = "lctrace")]
#[inline]
pub(crate) fn irq_entry(vector: usize) {
    lctrace::event!(IrqEntry, vector);
}

/// Records that the IRQ adapter finished dispatching `vector`.
#[cfg(feature = "lctrace")]
#[inline]
pub(crate) fn irq_exit(vector: usize, handled: bool) {
    lctrace::event!(IrqExit, vector, handled);
}

#[cfg(not(feature = "lctrace"))]
#[inline]
pub(crate) fn irq_entry(_vector: usize) {}

#[cfg(not(feature = "lctrace"))]
#[inline]
pub(crate) fn irq_exit(_vector: usize, _handled: bool) {}

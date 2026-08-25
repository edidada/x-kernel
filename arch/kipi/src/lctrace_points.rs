// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LCTrace probes on the inter-processor interrupt paths.
//!
//! Each probe has a real body when the `lctrace` feature is on and an empty
//! one otherwise, so the IPI paths stay free of `cfg` attributes.
//!
//! Only genuine cross-CPU sends are recorded. `run_on_cpu` targeting the
//! calling CPU runs the callback inline without raising an interrupt, and
//! recording that as an IPI would be misleading in a dump.

/// Records a callback queued on `dest_cpu` and the interrupt raised for it.
#[cfg(feature = "lctrace")]
#[inline]
pub(crate) fn ipi_send(dest_cpu: usize, src_cpu: usize) {
    lctrace::event!(IpiSend, dest_cpu, src_cpu);
}

/// Records a queued callback about to run on the receiving CPU.
#[cfg(feature = "lctrace")]
#[inline]
pub(crate) fn ipi_handle(src_cpu: usize) {
    lctrace::event!(IpiHandle, src_cpu);
}

#[cfg(not(feature = "lctrace"))]
#[inline]
pub(crate) fn ipi_send(_dest_cpu: usize, _src_cpu: usize) {}

#[cfg(not(feature = "lctrace"))]
#[inline]
pub(crate) fn ipi_handle(_src_cpu: usize) {}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform system control interface.

use kcpu_id_map::LogicalCpuId;
use kerrno::KResult;
use kplat_macros::device_interface;

#[device_interface]
pub trait SysCtrl {
    #[cfg(feature = "smp")]
    /// Boots an application processor selected by logical CPU ID.
    ///
    /// Platform implementations must translate the logical CPU ID to any
    /// firmware- or hardware-specific raw CPU identifier before issuing the
    /// actual boot command. This is a kernel-internal bring-up interface, not a
    /// user-callable authorization boundary; callers must supply an ID from the
    /// configured logical CPU map and a valid, aligned kernel stack top.
    fn boot_ap(logical_cpu_id: LogicalCpuId, stack_top: usize) -> KResult;

    /// Shuts down the system from an already-authorized kernel control path.
    ///
    /// Syscall or device-control frontends must perform their privilege check
    /// before dispatching to this non-returning platform operation.
    fn shutdown() -> !;
}

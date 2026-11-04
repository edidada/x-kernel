// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! MSI reservations shared by both IRQ contract entry points during migration.

use driver_os_api::{MsiResource, ResResult};

pub(super) fn allocate() -> ResResult<MsiResource> {
    #[cfg(target_arch = "x86_64")]
    {
        let allocation =
            kirq::alloc_msix(kirq::IrqAffinity::Any).ok_or(driver_os_api::ResError::NoMemory)?;
        let message = allocation.message();
        Ok(MsiResource::new(
            driver_os_api::IrqResource::plain(
                driver_os_api::Virq::new(allocation.virq()),
                driver_os_api::IrqTrigger::EdgeRising,
            ),
            driver_os_api::MsiMessage::new(message.address(), message.data()),
        ))
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        Err(driver_os_api::ResError::Unsupported)
    }
}

pub(super) fn release(virq: usize) {
    #[cfg(target_arch = "x86_64")]
    if !kirq::free_msix(virq) {
        log::warn!("failed to free MSI-X IRQ {virq} (not allocated?)");
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = virq;
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `hwrng` class: one const-constructed static plus the admission-gated
//! consumer data plane (the Linux `struct class hwrng` analogue).

use alloc::sync::Arc;

use driver_base::{ClassDevice, class::class_api::Class};
use kerrno::KResult;

use super::HwrngDevice;

pub type HwrngDeviceImpl = Arc<dyn HwrngDevice + Send + Sync>;

pub static HWRNG: Class<HwrngDeviceImpl> = Class::new(());

impl HwrngDevice for ClassDevice<Arc<dyn HwrngDevice + Send + Sync>> {
    fn read(&self, buf: &mut [u8]) -> KResult<usize> {
        self.with_available(|device| device.read(buf))
    }

    fn is_failed(&self) -> bool {
        // Query on cached state; a post-removal clone answers its last
        // known terminal state.
        self.with(|device| device.is_failed())
    }
}

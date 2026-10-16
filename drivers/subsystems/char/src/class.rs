// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `char` class: one const-constructed static plus the admission-gated
//! consumer data plane (the Linux `struct class char` analogue).

use alloc::boxed::Box;

use driver_base::{ClassDevice, class::class_api::Class};
use kerrno::KResult;

use super::CharDevice;

pub type CharDeviceImpl = Box<dyn CharDevice + Send + Sync>;

pub static CHAR: Class<CharDeviceImpl> = Class::new(());

impl CharDevice for ClassDevice<Box<dyn CharDevice + Send + Sync>> {
    fn read(&self, buf: &mut [u8]) -> KResult<usize> {
        self.with_available(|device| device.read(buf))
    }

    fn write(&self, buf: &[u8]) -> KResult<usize> {
        self.with_available(|device| device.write(buf))
    }

    fn flush(&self) -> KResult {
        self.with_available(|device| device.flush())
    }
}

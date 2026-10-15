// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The consumer-facing half of this subsystem: its device trait
//! implemented for the class device (`ClassDevice<Box<dyn Trait>>`) —
//! admission-gated data-plane forwarding over the shared runtime.
//! Lives HERE, with the trait it forwards to (the orphan rule agrees
//! with the layering: a subsystem knows both its trait and the model's
//! class device, like drivers/block knows struct device).

extern crate alloc;

use alloc::boxed::Box;

use driver_base::ClassDevice;
use kerrno::KResult;

use super::CharDevice;

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

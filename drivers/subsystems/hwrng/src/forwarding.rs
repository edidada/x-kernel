// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The consumer-facing half of this subsystem: its device trait
//! implemented for the class device — admission-gated data-plane
//! forwarding over the shared runtime. Lives HERE, with the trait it
//! forwards to.

extern crate alloc;

use alloc::sync::Arc;

use driver_base::ClassDevice;
use kerrno::KResult;

use super::HwrngDevice;

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

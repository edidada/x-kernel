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

use super::{Event, EventType, InputDevice, InputDeviceId};

impl InputDevice for ClassDevice<Box<dyn InputDevice + Send + Sync>> {
    fn name(&self) -> &str {
        self.name()
    }

    fn device_id(&self) -> InputDeviceId {
        self.with(|device| device.device_id())
    }

    fn physical_location(&self) -> &str {
        self.input_physical_location()
    }

    fn unique_id(&self) -> &str {
        self.input_unique_id()
    }

    fn get_event_bits(&self, ty: EventType, out: &mut [u8]) -> KResult<bool> {
        self.with_available(|device| device.get_event_bits(ty, out))
    }

    fn read_event(&self) -> KResult<Event> {
        self.with_available(|device| device.read_event())
    }
}

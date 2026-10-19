// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `rtc` class: one const-constructed static plus the admission-gated
//! consumer data plane (the Linux `struct class rtc_class` analogue).

use alloc::sync::Arc;

use driver_base::{Publication, class::class_api::Class};
use kerrno::KResult;
use ktime_types::SystemTime;

use super::RtcDevice;

/// The published device payload: a shared, driver-owned RTC instance.
///
/// `Arc` (not `Box`, unlike char) because an RTC driver keeps its register
/// handle alive in its own `Data` and hands the same instance to the class;
/// a future alarm-IRQ driver can then share it with its handler.
pub type RtcDeviceImpl = Arc<dyn RtcDevice + Send + Sync>;

/// The RTC class registry. One static, no lazy init.
pub static RTC: Class<RtcDeviceImpl> = Class::new(());

impl RtcDevice for Publication<Arc<dyn RtcDevice + Send + Sync>> {
    fn read_time(&self) -> KResult<SystemTime> {
        self.with_available(|device| device.read_time())
    }
}

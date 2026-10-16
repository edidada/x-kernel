// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `virtio-9p` class: one const-constructed static over the family's
//! own trait — the 9p "subsystem" lives with the family (no standalone
//! subsystem crate), so its class does too.

use alloc::boxed::Box;

use driver_base::{ClassDevice, class::class_api::Class};

use crate::Virtio9pDevice;

/// The 9p class payload: one boxed device.
pub type Virtio9pDeviceImpl = Box<dyn Virtio9pDevice + Send + Sync>;

/// A published 9p device handle.
pub type Virtio9pClassDevice = ClassDevice<Virtio9pDeviceImpl>;

/// The virtio-9p device class.
pub static P9: Class<Virtio9pDeviceImpl> = Class::new(());

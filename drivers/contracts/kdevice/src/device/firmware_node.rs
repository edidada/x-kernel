// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! An opaque firmware-node handle on a device.
//!
//! The Linux counterpart is `dev.of_node`: a probe can read any property
//! from its own firmware node on demand instead of carrying pre-extracted
//! copies of every attribute the device model might someday need. Provider
//! drivers (clock, reset, gpio controllers) are the primary users — their
//! registration parameters stay in the firmware description and are read
//! at probe time, exactly like Linux's `of_*` property accessors.
//!
//! The trait lives in the OS-neutral contract layer; each firmware source
//! (device tree, ACPI) provides its own implementation. Absence (`None`)
//! means the device has no firmware node — platform-static and PCI
//! fallback devices.

use alloc::sync::Arc;

/// Read-only property access to a device's firmware node.
pub trait FirmwareNode: Send + Sync {
    /// Read a `u32` property, absent or unparsable yields `None`.
    fn read_u32(&self, name: &str) -> Option<u32>;

    /// Read a string property's first NUL-separated entry, absent yields
    /// `None`. (Firmware string-list properties such as
    /// `clock-output-names` commonly carry several entries; the first is
    /// the meaningful one for single-output providers.)
    fn read_str(&self, name: &str) -> Option<&'static str>;
}

/// Shorthand for the shared node handle stored on a device.
pub type SharedFirmwareNode = Option<Arc<dyn FirmwareNode>>;

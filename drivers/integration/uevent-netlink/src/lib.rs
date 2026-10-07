// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Netlink transport for kobject uevents.
//!
//! The Linux `lib/kobject_uevent.c::kobject_uevent()` netlink half: this
//! crate subscribes to devnode lifecycle events and broadcasts the payloads
//! (serialized by `devnode::uevent`, the format layer in the node model)
//! on the KOBJECT_UEVENT netlink group. It is the one drivers-tree
//! consumer of `knet` — the framework-to-kernel-facility direction this
//! directory is for.
//!
//! The uevent payload format is a udev-visible compatibility surface; the
//! format rules live in `devnode::uevent`, shared with sysfs attributes.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;

pub use devnode::uevent::UeventAction;
use devnode::{DevNode, DevNodeEvent, install_lifecycle_publisher, uevent::uevent_payload};
use klazy::Once;

const UEVENT_GROUP: u32 = 1;

static INITIALIZATION: Once<()> = Once::new();

/// Initializes automatic device-node lifecycle publication.
///
/// This must run before device discovery starts. The device model rejects a
/// late installation so boot cannot silently omit earlier add events.
pub fn init() {
    INITIALIZATION.call_once(|| {
        assert!(
            install_lifecycle_publisher(Arc::new(handle_lifecycle_event)),
            "device-node uevent publisher must initialize before device discovery"
        );
    });
}

/// Broadcasts one action for a device object (the sysfs `uevent` file write
/// path goes through here too, so file-triggered and automatic messages
/// can never disagree).
pub fn publish(action: UeventAction, node: &DevNode) {
    let payload = uevent_payload(action, node);
    knet::netlink::publish_kobject_uevent(UEVENT_GROUP, payload.as_slice());
}

fn handle_lifecycle_event(event: DevNodeEvent) {
    match event {
        DevNodeEvent::Added(node) => publish(UeventAction::Add, &node),
        DevNodeEvent::Removed(node) => publish(UeventAction::Remove, &node),
        DevNodeEvent::Removing(_) => {}
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, def_test};

    use super::*;

    #[def_test]
    fn actions_accept_only_the_linux_kobject_set() {
        assert!(UeventAction::try_from("change").is_ok());
        assert!(UeventAction::try_from("change\0remove").is_err());
    }
}

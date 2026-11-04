// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[cfg(feature = "device-model")]
#[def_test]
fn model_rows_append_without_reinterpreting_legacy_ids() {
    let mut text = String::from("platform       7 firmware:legacy [active] driver=legacy\n");
    append_model_rows(
        &mut text,
        [
            ModelDeviceRow {
                path: "devices/platform/controller",
                parent: None,
                bus: "platform",
                identity: Some("controller"),
                driver: Some("controller-driver"),
                state: driver_model::DeviceState::Bound.as_str(),
            },
            ModelDeviceRow {
                path: "devices/platform/controller/rng",
                parent: Some("devices/platform/controller"),
                bus: "platform",
                identity: Some("virtio-mmio:4"),
                driver: Some("virtio-rng-mmio"),
                state: driver_model::DeviceState::Bound.as_str(),
            },
        ],
    );
    unittest::assert_eq!(
        text,
        "platform       7 firmware:legacy [active] driver=legacy\nModel devices (model:path \
         namespace):\n  model:devices/platform/controller bus=platform controller [bound] \
         driver=controller-driver parent=<root>\n  model:devices/platform/controller/rng \
         bus=platform virtio-mmio:4 [bound] driver=virtio-rng-mmio \
         parent=model:devices/platform/controller\n"
    );
}

#[cfg(feature = "device-model")]
#[def_test]
fn model_rows_refresh_without_retaining_removed_devices() {
    let mut text = String::new();
    append_model_rows(
        &mut text,
        [ModelDeviceRow {
            path: "devices/platform/rng",
            parent: None,
            bus: "platform",
            identity: None,
            driver: None,
            state: "unclaimed",
        }],
    );
    unittest::assert_eq!(
        text,
        "Model devices (model:path namespace):\n  model:devices/platform/rng bus=platform \
         <unknown> [unclaimed] driver=<none> parent=<root>\n"
    );
    text.clear();
    append_model_rows(&mut text, []);
    unittest::assert!(text.is_empty());
    text.push_str("legacy row\n");
    append_model_rows(&mut text, []);
    unittest::assert_eq!(text, "legacy row\n");
}

#[def_test]
fn named_cdev_output_follows_successful_registration() {
    struct Backend;
    impl kvfs::DeviceFileOps for Backend {}
    let device = kvfs::DeviceId::new(246, 900002);
    kvfs::cdev_add_named(
        device,
        String::from("proc_projection_test"),
        String::from("test"),
        Arc::new(Backend),
    )
    .unwrap();
    let mut iter = DevicesIter::new();
    iter.rewind();
    assert!(iter.text.contains("proc_projection_test [active]"));
    assert!(
        iter.text
            .contains("Registered character devices (major:minor):\n")
    );
    assert!(
        iter.text
            .contains("246:900002 proc_projection_test [active]")
    );
    kvfs::cdev_del(device, 1).unwrap();
    iter.rewind();
    assert!(!iter.text.contains("proc_projection_test"));
}

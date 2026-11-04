// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! One-shot real model RNG projection acceptance in a dedicated guest.
//!
//! The exact module filter excludes tests that need the terminally removed
//! device. Procfs is read through VFS, not its formatter helpers. The entropy
//! worker and other lifecycle fixtures must be disabled. Terminal removal is
//! intentionally permanent for this boot; no registration is resurrected.
//! An outer guest watchdog bounds hung hardware teardown, not a release timeout.

use alloc::{format, string::String, vec::Vec};

use driver_model::{DeviceRef, DeviceState, RuntimeError, global_domain};
use kvfs::{Filename, Mount, NodePermission, Path, SuperBlockFlags};
use unittest::{assert, assert_eq, def_test};

const DRIVER_NAME: &str = "virtio-rng-mmio-model";
const MODULE_FILTER: &str = "procfs::model_mmio_tests";
const MAX_TEXT_BYTES: usize = 64 * 1024;

fn read_text(root: &Path, name: &str) -> Result<String, &'static str> {
    let file = Filename::new(name)
        .open_with_flags_at(
            root,
            root,
            0,
            NodePermission::default(),
            NodePermission::default(),
            kcred::initial_cred(),
        )
        .map_err(|_| "procfs VFS open failed")?;
    let mut bytes = Vec::new();
    let mut position = 0;
    loop {
        let mut buffer = [0u8; 512];
        let requested = buffer.len().min(MAX_TEXT_BYTES - bytes.len() + 1);
        let count = file
            .read_from(&mut buffer[..requested], &mut position)
            .map_err(|_| "procfs VFS read failed")?;
        if count == 0 {
            return String::from_utf8(bytes).map_err(|_| "procfs text is not UTF-8");
        }
        if count > requested || count > MAX_TEXT_BYTES - bytes.len() {
            return Err("procfs text exceeded fixture bound before EOF");
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
}

fn check_projection(
    root: &Path,
    device: &DeviceRef,
    expected: Option<DeviceState>,
) -> Result<(), &'static str> {
    let key = format!("model:{}", device.path());
    for name in ["/devices", "/bus/devices", "/bus/topology"] {
        let text = read_text(root, name)?;
        let mut rows = text
            .lines()
            .filter(|line| line.split_whitespace().next() == Some(key.as_str()));
        let row = rows.next();
        if rows.next().is_some() {
            return Err("duplicate target model row");
        }
        match (expected, row) {
            (None, None) => {}
            (Some(state), Some(row)) => {
                let state_field = format!("[{}]", state.as_str());
                let driver_field = if state == DeviceState::Active {
                    format!("driver={DRIVER_NAME}")
                } else {
                    String::from("driver=<none>")
                };
                if !row.split_whitespace().any(|field| field == state_field)
                    || !row.split_whitespace().any(|field| field == driver_field)
                {
                    return Err("model row has incorrect state or driver");
                }
            }
            _ => return Err("target model row presence is incorrect"),
        }
    }
    Ok(())
}

fn same_registration(device: &DeviceRef, state: DeviceState) -> bool {
    global_domain().snapshot(device).is_some_and(|snapshot| {
        snapshot.device.same_device(device)
            && snapshot.state == state
            && snapshot.driver
                == if state == DeviceState::Active {
                    Some(DRIVER_NAME)
                } else {
                    None
                }
            && device.is_online()
    })
}

struct RestoreBinding {
    device: DeviceRef,
    is_armed: bool,
}

impl RestoreBinding {
    fn restore(&mut self) -> Result<(), RuntimeError> {
        global_domain().bind_device(&self.device)?;
        if !same_registration(&self.device, DeviceState::Active) {
            return Err(RuntimeError::BadState);
        }
        self.is_armed = false;
        Ok(())
    }
}

impl Drop for RestoreBinding {
    fn drop(&mut self) {
        if self.is_armed
            && let Err(error) = self.restore()
        {
            log::error!("procfs MMIO fixture restoration failed: {error:?}; registration retained");
        }
    }
}

#[def_test(serial)]
fn real_model_mmio_rng_procfs_tracks_unbind_rebind_and_remove() {
    if option_env!("XK_TEST_MODEL_MMIO_PROCFS") != Some("1") {
        return unittest::TestResult::Ignored;
    }
    assert_eq!(option_env!("UNITTEST_CRATE"), Some(MODULE_FILTER));
    assert!(cfg!(target_arch = "aarch64"));
    assert!(kbuild_config::KFEAT_VIRTIO_BUS_MMIO);
    assert!(!kbuild_config::KFEAT_ENTROPY_VIRTIO_RNG);
    assert!(option_env!("XK_TEST_MODEL_MMIO_LIFECYCLE").is_none());
    assert!(option_env!("XK_TEST_MODEL_MMIO_PENDING").is_none());
    assert!(option_env!("XK_TEST_MODEL_MMIO_ISOLATION").is_none());
    assert!(option_env!("XK_TEST_MODEL_MMIO_UNREGISTER").is_none());
    assert!(ktask::is_sleepable_context());
    let domain = global_domain();
    let mut devices: Vec<_> = domain
        .devices()
        .into_iter()
        .filter(|snapshot| snapshot.driver == Some(DRIVER_NAME))
        .collect();
    assert_eq!(devices.len(), 1);
    let device = devices.pop().unwrap().device;
    assert!(same_registration(&device, DeviceState::Active));
    let fs = crate::new_procfs(SuperBlockFlags::empty()).unwrap();
    let root = Mount::new_root(&fs).root_path();
    assert_eq!(
        check_projection(&root, &device, Some(DeviceState::Active)),
        Ok(())
    );

    let mut restore = RestoreBinding {
        device: device.clone(),
        is_armed: true,
    };
    let unbind = domain.unbind_device(&device);
    let retained = same_registration(&device, DeviceState::Unclaimed);
    let unclaimed_text = check_projection(&root, &device, Some(DeviceState::Unclaimed));
    let rebound = restore.restore();
    drop(restore);
    let active_again = same_registration(&device, DeviceState::Active);
    let rebound_text = check_projection(&root, &device, Some(DeviceState::Active));
    assert_eq!(unbind, Ok(()));
    assert!(retained);
    assert_eq!(unclaimed_text, Ok(()));
    assert_eq!(rebound, Ok(()));
    assert!(active_again);
    assert_eq!(rebound_text, Ok(()));

    let removal = domain.remove_device(&device);
    let removed_text = check_projection(&root, &device, None);
    assert_eq!(removal, Ok(()));
    assert!(!device.is_online());
    assert!(domain.snapshot(&device).is_none());
    assert_eq!(removed_text, Ok(()));
}

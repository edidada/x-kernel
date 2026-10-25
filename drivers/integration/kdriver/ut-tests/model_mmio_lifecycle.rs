// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Opt-in real-device lifecycle regression, not ordinary isolated unit coverage.
//!
//! Enable with XK_TEST_MODEL_MMIO_LIFECYCLE=1 only on the single-model-RNG
//! QEMU fixture, without competing device-management operations. Serial UT
//! execution does not stop kernel entropy workers: unbind may drain their reads.
//! After restoring the binding, the test performs a real nonempty entropy read.
//! The outer QEMU timeout bounds hangs; no production read timeout is introduced.
//! This makes no idle, pending-request cancellation, physical IRQ synchronization
//! or bounded reset-latency acceptance claim, nor any entropy-quality claim.

use alloc::vec::Vec;

use driver_model::{DeviceRef, DeviceState, RuntimeError, global_domain};
use subsys_hwrng::{HWRNG, HwrngHandle};
use unittest::{assert, assert_eq, def_test};

const DRIVER_NAME: &str = "virtio-rng-mmio-model";
const SAMPLE_LEN: usize = 16;

struct ReadObservation {
    result: kerrno::KResult<usize>,
    guards_unchanged: bool,
    buffer_unchanged: bool,
    suffix_unchanged: bool,
}

fn sample(handle: &HwrngHandle) -> ReadObservation {
    let mut buffer = [0xa5u8; SAMPLE_LEN + 8];
    let result = handle.read(&mut buffer[4..4 + SAMPLE_LEN]);
    let suffix_unchanged = match result {
        Ok(count) if count <= SAMPLE_LEN => buffer[4 + count..4 + SAMPLE_LEN]
            .iter()
            .all(|byte| *byte == 0xa5),
        _ => false,
    };
    ReadObservation {
        result,
        guards_unchanged: buffer[..4] == [0xa5; 4] && buffer[4 + SAMPLE_LEN..] == [0xa5; 4],
        buffer_unchanged: buffer == [0xa5; SAMPLE_LEN + 8],
        suffix_unchanged,
    }
}

fn binding_state(device: &DeviceRef) -> (Option<DeviceState>, Option<&'static str>, bool) {
    let snapshot = global_domain().snapshot(device);
    (
        snapshot.as_ref().map(|snapshot| snapshot.state),
        snapshot.as_ref().and_then(|snapshot| snapshot.driver),
        snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.device.same_device(device)),
    )
}

fn publications() -> Vec<HwrngHandle> {
    HWRNG
        .devices()
        .into_iter()
        .filter(|handle| handle.driver_name() == Some(DRIVER_NAME))
        .collect()
}

struct RestoreBinding {
    device: DeviceRef,
    is_armed: bool,
}

impl RestoreBinding {
    fn restore(&mut self) -> Result<(), RuntimeError> {
        if !self.is_armed {
            return Ok(());
        }
        let domain = global_domain();
        domain.bind_device(&self.device)?;
        if !domain.snapshot(&self.device).is_some_and(|snapshot| {
            snapshot.state == DeviceState::Active && snapshot.driver == Some(DRIVER_NAME)
        }) {
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
            log::error!(
                "model MMIO lifecycle fixture could not restore {}: {error:?}; device \
                 registration retained",
                self.device.path()
            );
        }
    }
}

#[def_test(serial)]
fn real_model_mmio_rng_unbind_rebind_closes_retained_publication() {
    if option_env!("XK_TEST_MODEL_MMIO_LIFECYCLE") != Some("1") {
        return unittest::TestResult::Ignored;
    }
    assert!(cfg!(target_arch = "aarch64"));
    assert!(kbuild_config::KFEAT_VIRTIO_BUS_MMIO);
    assert!(ktask::is_sleepable_context());

    let domain = global_domain();
    let mut devices: Vec<_> = domain
        .devices()
        .into_iter()
        .filter(|snapshot| snapshot.driver == Some(DRIVER_NAME))
        .collect();
    let mut initial = publications();
    assert_eq!(devices.len(), 1);
    assert_eq!(initial.len(), 1);
    let snapshot = devices.pop().unwrap();
    assert_eq!(snapshot.state, DeviceState::Active);
    let device = snapshot.device;
    let old = initial.pop().unwrap();
    assert!(old.is_available());
    assert_eq!(old.read(&mut []), Ok(0));

    let mut restore = RestoreBinding {
        device: device.clone(),
        is_armed: true,
    };
    let unbind = domain.unbind_device(&device);
    let (unbound_state, unbound_driver, same_registered_device) = binding_state(&device);
    let remained_online = device.is_online();
    let publications_while_unbound = publications().len();
    let old_available_unbound = old.is_available();
    let old_read_unbound = sample(&old);

    let rebound = restore.restore();
    drop(restore);

    let (restored_state, restored_driver, restored_same_device) = binding_state(&device);
    let current = publications();
    let current_count = current.len();
    let new_handle = match current.as_slice() {
        [handle] => Some(handle),
        _ => None,
    };
    let new_available = new_handle.map(HwrngHandle::is_available);
    let new_read = new_handle.map(sample);
    let reused_publication = new_handle.map(|handle| handle.same_publication(&old));
    let old_available_rebound = old.is_available();
    let old_read_rebound = sample(&old);
    drop(current);
    drop(old);
    drop(device);

    log::info!(
        "model MMIO lifecycle: unbind={unbind:?}, rebind={rebound:?}, unbound={unbound_state:?}, \
         restored={restored_state:?}, publications={publications_while_unbound}->{current_count}, \
         reused_publication={reused_publication:?}"
    );
    assert_eq!(unbind, Ok(()));
    assert_eq!(unbound_state, Some(DeviceState::Unclaimed));
    assert_eq!(unbound_driver, None);
    assert!(same_registered_device);
    assert!(remained_online);
    assert_eq!(publications_while_unbound, 0);
    assert_eq!(old_available_unbound, false);
    assert_eq!(old_read_unbound.result, Err(kerrno::KError::BadState));
    assert!(old_read_unbound.buffer_unchanged);
    assert_eq!(rebound, Ok(()));
    assert_eq!(restored_state, Some(DeviceState::Active));
    assert_eq!(restored_driver, Some(DRIVER_NAME));
    assert!(restored_same_device);
    assert_eq!(current_count, 1);
    assert_eq!(new_available, Some(true));
    let new_read = new_read.unwrap();
    log::info!("model MMIO lifecycle rebound sample: {:?}", new_read.result);
    assert!(matches!(new_read.result, Ok(count) if count > 0 && count <= SAMPLE_LEN));
    assert!(new_read.guards_unchanged);
    assert!(new_read.suffix_unchanged);
    assert_eq!(reused_publication, Some(false));
    assert_eq!(old_available_rebound, false);
    assert_eq!(old_read_rebound.result, Err(kerrno::KError::BadState));
    assert!(old_read_rebound.buffer_unchanged);
}

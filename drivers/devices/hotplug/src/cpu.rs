// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU online/offline policy.

use alloc::{sync::Arc, vec::Vec};

use kdevice::{
    SystemClass, SystemDeviceControl, SystemDeviceDescriptor, SystemDeviceId, SystemDeviceState,
    install_system_device, present_system_devices, system_device_state,
};
use kerrno::{KErrorKind, KResult};
use kspin::SpinNoPreempt;

use crate::HotplugController;

/// CPU hotplug policy.
pub struct CpuHotplugController {
    /// Serializes concurrent policy checks.
    ///
    /// `kdevice` has its own per-device transitioning flag, but it cannot cover
    /// the online-CPU-count check: two concurrent offline requests for the last
    /// two CPUs would each observe two online CPUs and both pass. CPU requests
    /// never enter the generic transition because no real transition is
    /// supported.
    transition_lock: SpinNoPreempt<()>,
}

impl CpuHotplugController {
    /// Creates a controller. Registration happens in [`crate::init`].
    pub const fn new() -> Self {
        Self {
            transition_lock: SpinNoPreempt::new(()),
        }
    }
}

impl Default for CpuHotplugController {
    fn default() -> Self {
        Self::new()
    }
}

impl HotplugController for CpuHotplugController {
    fn class(&self) -> SystemClass {
        SystemClass::Cpu
    }

    fn present(&self) -> Vec<SystemDeviceId> {
        present_system_devices(SystemClass::Cpu)
    }

    fn set_online(&self, id: SystemDeviceId, is_online: bool) -> KResult<()> {
        if id.class() != SystemClass::Cpu {
            return Err(KErrorKind::InvalidInput.into());
        }
        let _transition = self.transition_lock.lock();
        let state = system_device_state(id).ok_or(KErrorKind::NoSuchDevice)?;
        let is_boot_cpu = is_boot_cpu(id.index());
        validate_cpu_request(
            id.index(),
            is_boot_cpu,
            state.is_online,
            is_online,
            kcpu_id_map::online_cpu_count(),
        )?;
        if state.is_online == is_online {
            return Ok(());
        }

        // There is no runtime architecture operation that can safely stop and
        // restart a CPU. Report the gap instead of publishing a fake transition
        // (and in particular never publish `GoingOffline` for an action that
        // cannot happen).
        Err(KErrorKind::OperationNotSupported.into())
    }

    fn state(&self, id: SystemDeviceId) -> Option<SystemDeviceState> {
        if id.class() != SystemClass::Cpu {
            return None;
        }
        system_device_state(id)
    }
}

/// Pure validation shared by the controller, the per-device control callback,
/// and unit tests.
///
/// Returns `Ok(())` for an idempotent request and `OperationNotSupported` for a
/// real transition. The latter is not a policy rejection: the request is legal,
/// the platform cannot perform it.
pub(crate) fn validate_cpu_request(
    index: u32,
    is_boot_cpu: bool,
    is_currently_online: bool,
    is_online_requested: bool,
    online_cpu_count: usize,
) -> KResult<()> {
    if is_currently_online == is_online_requested {
        return Ok(());
    }
    if !is_online_requested {
        if is_boot_cpu || index == 0 {
            return Err(KErrorKind::OperationNotPermitted.into());
        }
        if online_cpu_count <= 1 {
            return Err(KErrorKind::OperationNotPermitted.into());
        }
    }
    Err(KErrorKind::OperationNotSupported.into())
}

fn is_boot_cpu(index: u32) -> bool {
    index == 0 || khal::percpu::id().as_usize() == index as usize
}

/// Per-device control installed in `kdevice`.
///
/// It repeats the policy check because `set_system_device_online` is public and
/// can be called without going through [`CpuHotplugController`]. It cannot make
/// a transition real either.
pub(crate) struct CpuControl {
    index: u32,
}

impl CpuControl {
    pub(crate) fn new(index: u32) -> Self {
        Self { index }
    }
}

impl SystemDeviceControl for CpuControl {
    fn set_online(&self, is_online: bool) -> KResult<()> {
        let id = SystemDeviceId::new(SystemClass::Cpu, self.index);
        let state = system_device_state(id).ok_or(KErrorKind::NoSuchDevice)?;
        validate_cpu_request(
            self.index,
            is_boot_cpu(self.index),
            state.is_online,
            is_online,
            kcpu_id_map::online_cpu_count(),
        )
    }
}

/// Installs every CPU discovered at boot as present and online.
pub(crate) fn install_boot_cpus() {
    kcpu_id_map::for_each_present_logical_cpu(|_, logical_cpu_id, _| {
        let index = logical_cpu_id.as_usize() as u32;
        let id = SystemDeviceId::new(SystemClass::Cpu, index);
        if system_device_state(id).is_some() {
            return;
        }
        let _ = install_system_device(SystemDeviceDescriptor::new(
            id,
            0,
            0,
            kcpu_id_map::is_cpu_online(logical_cpu_id),
            Arc::new(CpuControl::new(index)),
        ));
    });
}

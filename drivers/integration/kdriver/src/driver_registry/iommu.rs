// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IOMMU driver registration and the late device-attach pass.
//!
//! Hardware IOMMU drivers register like any platform driver so their
//! firmware match specs are visible to enumeration. After discovery and
//! probing complete, [`attach_consumers`] walks every bound device whose
//! firmware node declares `iommus` and attaches it to the instance its
//! phandle names: the IOMMU driver programs its matcher/context for that
//! master, and the device translates through the instance's domain from
//! then on.

use alloc::vec::Vec;

use kiommu::{DeviceKey, InstanceId, IommuBinding};

/// Registers the enabled IOMMU hardware driver descriptors.
pub(crate) fn register_all(registrar: &mut crate::driver_registry::DriverRegistrar) {
    #[cfg(feature = "rk-iommu")]
    registrar.register(rk_iommu::driver_descriptor());
    #[cfg(feature = "arm-smmu")]
    registrar.register(arm_smmu::driver_descriptor());
}

/// Attaches every enumerated consumer device to the IOMMU instance its
/// `iommus` property names.
///
/// Runs once after discovery: IOMMU instances probe in the same pass as
/// their consumers, so attachment must wait until all probes have run. A
/// device whose instance did not probe (IOMMU driver disabled, or the node
/// is not available) is logged and skipped; the device keeps running
/// unmanaged, which matches the platform's identity-mapping status quo.
pub(crate) fn attach_consumers() {
    let records = kdevice::device_records_snapshot();
    let mut attached = 0usize;
    let mut missing = 0usize;
    for record in &records {
        let Some(device) = kdevice::find_device(record.id) else {
            continue;
        };
        let Some(node) = device.firmware_node() else {
            continue;
        };
        let mut bindings: Vec<IommuBinding> = Vec::new();
        for index in 0.. {
            match node.iommu_spec(index) {
                Ok(Some(spec)) => bindings.push(IommuBinding {
                    instance: InstanceId(u64::from(spec.controller.id().raw())),
                    args: spec.params.as_slice().to_vec(),
                }),
                Ok(None) => break,
                Err(error) => {
                    // ACPI or unsupported backends report Unsupported; DT
                    // nodes without the property never reach here.
                    log::debug!("iommu: no bindings for device {:?}: {error:?}", record.id);
                    break;
                }
            }
        }
        for binding in bindings {
            match kiommu::attach_device(binding.instance, DeviceKey(record.id.raw()), &binding.args)
            {
                Ok(()) => {
                    attached += 1;
                    log::info!(
                        "iommu: attached device {:?} to instance {:?} via {args:?}",
                        record.id,
                        binding.instance,
                        args = binding.args
                    );
                }
                Err(kiommu::IommuError::NotFound) => {
                    missing += 1;
                    log::warn!(
                        "iommu: device {:?} declares instance {:?} which did not probe; leaving \
                         device unmanaged",
                        record.id,
                        binding.instance
                    );
                }
                Err(error) => {
                    log::error!(
                        "iommu: attach failed for device {:?} on instance {:?}: {error}",
                        record.id,
                        binding.instance
                    );
                }
            }
        }
    }
    if attached != 0 || missing != 0 {
        log::info!("iommu: attach pass complete: {attached} attached, {missing} unmanaged");
    }
}

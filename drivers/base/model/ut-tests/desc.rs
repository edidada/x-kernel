// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;

use unittest::{assert_eq, def_test};

use crate::{
    Bus, BusInstanceId, DeviceDescriptor, DeviceDomain, DmaInfo, FirmwareNodeInfo, PlatformBus,
    PlatformBusData,
};

#[def_test]
fn descriptor_builder_defaults_leave_optional_capabilities_absent() {
    let instance = BusInstanceId::from_raw(1);
    let descriptor = DeviceDescriptor::builder()
        .bus_instance(instance)
        .bus_data(Box::new(PlatformBusData::firmware_node(
            1,
            &["test,device"],
        )))
        .build();

    assert_eq!(descriptor.bus_instance, instance);
    assert!(descriptor.parent.is_none());
    assert!(descriptor.fw.is_none());
    assert!(descriptor.dma.is_none());
    assert!(descriptor.resources.is_empty());
    assert!(descriptor.bus_data.as_any().is::<PlatformBusData>());
}

#[def_test]
fn descriptor_builder_accepts_optional_enumerator_inputs() {
    let domain = DeviceDomain::new();
    let instance = domain
        .add_bus_instance(PlatformBus::ID, None, "builder-test")
        .unwrap();
    let parent = domain
        .add_device(
            DeviceDescriptor::builder()
                .bus_instance(instance)
                .bus_data(Box::new(PlatformBusData::firmware_node(
                    7,
                    &["test,parent"],
                )))
                .build(),
        )
        .unwrap();
    let descriptor = DeviceDescriptor::builder()
        .bus_instance(BusInstanceId::from_raw(1))
        .bus_data(Box::new(PlatformBusData::firmware_node(
            1,
            &["test,device"],
        )))
        .parent_opt(Some(parent.clone()))
        .fw_opt(Some(FirmwareNodeInfo {
            phandle: Some(9),
            ..FirmwareNodeInfo::EMPTY
        }))
        .dma_opt(Some(DmaInfo::DEFAULT))
        .build();

    assert!(descriptor.parent.as_ref().unwrap().same_device(&parent));
    let cloned = descriptor.clone();
    assert!(cloned.parent.as_ref().unwrap().same_device(&parent));
    domain.remove_device(&parent).unwrap();
    assert_eq!(descriptor.fw.as_ref().unwrap().phandle, Some(9));
    assert_eq!(descriptor.dma, Some(DmaInfo::DEFAULT));

    let descriptor = DeviceDescriptor::builder()
        .bus_instance(BusInstanceId::from_raw(1))
        .bus_data(Box::new(PlatformBusData::firmware_node(
            1,
            &["test,device"],
        )))
        .parent_opt(None)
        .fw_opt(None)
        .dma_opt(None)
        .build();

    assert!(descriptor.parent.is_none());
    assert!(descriptor.fw.is_none());
    assert!(descriptor.dma.is_none());
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device-table tests (no controller, no class involved).
//!
//! What is under test is the ownership rule: a binding is created once, its
//! owners are dropped exactly once, and the drop happens at a point the caller
//! chose -- not inside `detach`.

use alloc::{boxed::Box, string::ToString, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use kerrno::KError;
use unittest::{assert, assert_eq, def_test};
use crate::bus::{ClassBinding, RejectedBinding, UsbBus, drop_bindings};
use crate::{
    descriptor::{ConfigDescriptor, DeviceDescriptor, InterfaceDescriptor},
    Speed, UsbDevice,
};

/// A withdrawal owner that records that it ran.
struct Owner(Arc<AtomicUsize>);

impl Drop for Owner {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn owner(counter: &Arc<AtomicUsize>) -> Box<dyn core::any::Any + Send + Sync> {
    Box::new(Owner(counter.clone()))
}

fn device(address: u8, parent: Option<(u8, u8)>) -> UsbDevice {
    UsbDevice {
        address,
        port: usize::from(address),
        parent,
        speed: Speed::Super,
        device: DeviceDescriptor {
            bcd_usb: 0x0300,
            class: 0,
            subclass: 0,
            protocol: 0,
            max_packet_size_0: 9,
            vendor: 0x03f0,
            product: 0x7240,
            bcd_device: 0x0100,
            manufacturer_index: 0,
            product_index: 0,
            serial_index: 0,
            num_configurations: 1,
        },
        configurations: vec![ConfigDescriptor {
            configuration_value: 1,
            attributes: 0x80,
            max_power: 0x32,
            total_length: 32,
            interfaces: vec![InterfaceDescriptor {
                number: 0,
                alternate_setting: 0,
                class: 8,
                subclass: 6,
                protocol: 0x50,
                endpoints: Vec::new(),
            }],
        }],
        manufacturer: Some("HP".to_string()),
        product: Some("x5000m".to_string()),
        serial: None,
    }
}

fn bus() -> UsbBus {
    UsbBus::new(driver_base::DeviceHandle::synthetic(
        "usb-xhci",
        "platform/fee00000.usb/xhci",
    ))
}

/// Two devices cannot share an address, and a refused attach keeps the first.
#[def_test(serial)]
fn attach_rejects_a_second_device_at_the_same_address() {
    let mut bus = bus();
    assert!(bus.attach(device(3, None)).is_ok());
    assert_eq!(bus.attach(device(3, None)).unwrap_err(), KError::BadState);
    assert!(bus.contains(3));
    assert_eq!(bus.devices().count(), 1);

    assert!(bus.attach(device(4, None)).is_ok());
    assert_eq!(bus.devices().count(), 2);
}

/// A refused bind leaves no entry **and** hands the owners back instead of
/// dropping them while the table's lock is held.
#[def_test(serial)]
fn bind_is_all_or_nothing_and_returns_the_rejected_owners() {
    let mut bus = bus();
    assert!(bus.attach(device(3, None)).is_ok());

    let counter = Arc::new(AtomicUsize::new(0));
    let first = ClassBinding::new("usb-storage", 0, vec![owner(&counter)]);
    assert!(bus.bind(3, first).is_ok());
    assert_eq!(counter.load(Ordering::SeqCst), 0);

    let second = ClassBinding::new("usb-input", 0, vec![owner(&counter)]);
    let RejectedBinding { error, binding } = bus.bind(3, second).unwrap_err();
    assert_eq!(error, KError::BadState);
    // Still alive: the table did not drop it.
    assert_eq!(counter.load(Ordering::SeqCst), 0);
    drop_bindings(vec![binding]);
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    // The first binding survived the refusal.
    let entry = bus.devices().next().unwrap();
    assert!(entry.binding(0).is_some());
    assert_eq!(
        entry.binding(0).unwrap().class_name,
        "usb-storage"
    );

    // Binding an address that is not on the bus is a lookup failure, not a bind.
    let orphan = ClassBinding::new("usb-storage", 0, vec![owner(&counter)]);
    let RejectedBinding { error, binding } = bus.bind(9, orphan).unwrap_err();
    assert_eq!(error, KError::NotFound);
    drop_bindings(vec![binding]);
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

/// `detach` returns the owners: it must not withdraw them itself.
#[def_test(serial)]
fn detach_returns_the_bindings_without_dropping_them() {
    let mut bus = bus();
    assert!(bus.attach(device(3, None)).is_ok());
    assert!(bus.attach(device(4, None)).is_ok());

    let counter = Arc::new(AtomicUsize::new(0));
    assert!(
        bus.bind(3, ClassBinding::new("usb-storage", 0, vec![owner(&counter)]))
            .is_ok()
    );
    assert!(
        bus.bind(3, ClassBinding::new("usb-storage", 1, vec![owner(&counter)]))
            .is_ok()
    );

    let gone = bus.detach(3);
    assert_eq!(gone.len(), 2);
    assert_eq!(counter.load(Ordering::SeqCst), 0);
    assert!(!bus.contains(3));
    assert!(bus.contains(4));

    // Detaching something that is not there is a no-op, not an error.
    assert!(bus.detach(7).is_empty());

    drop_bindings(gone);
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

/// `detach_all` is the controller-reset path: every device and every owner.
#[def_test(serial)]
fn detach_all_empties_the_table() {
    let mut bus = bus();
    assert!(bus.attach(device(3, None)).is_ok());
    assert!(bus.attach(device(5, Some((3, 1)))).is_ok());

    let counter = Arc::new(AtomicUsize::new(0));
    assert!(
        bus.bind(3, ClassBinding::new("usb-storage", 0, vec![owner(&counter)]))
            .is_ok()
    );
    assert!(
        bus.bind(5, ClassBinding::new("usb-hub", 0, vec![owner(&counter)]))
            .is_ok()
    );
    // A child device keeps its parent link for the port scan.
    let child = bus.device(5).unwrap();
    assert_eq!(child.parent, Some((3, 1)));

    let all = bus.detach_all();
    assert_eq!(all.len(), 2);
    assert_eq!(bus.devices().count(), 0);
    assert_eq!(counter.load(Ordering::SeqCst), 0);
    drop_bindings(all);
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

/// The handle survives the probe it came from, which is the whole reason the
/// table can publish later.
#[def_test(serial)]
fn the_table_keeps_the_device_identity() {
    let bus = bus();
    assert_eq!(bus.handle().path(), "platform/fee00000.usb/xhci");
}

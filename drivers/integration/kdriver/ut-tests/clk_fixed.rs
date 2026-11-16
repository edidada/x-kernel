// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! End-to-end chain: DT `fixed-clock` node → provider driver probe →
//! `kclk` registry → capability-contract resolution → enabled clock.
//!
//! The fixture uses the production `DtBackend`, and the provider driver is
//! the production [`FixedClockDriver`] registered the same way
//! `init_drivers` registers it.

use alloc::{sync::Arc, vec::Vec};

use driver_os_api::ClockOp as _;
use driver_os_api_xkernel::XKERNEL_CLOCK_OP;
use kalloc_types::KBox;
use kdevice::{register_driver_object, reset_and_setup_platform_bus_for_tests};
use khal::firmware::devices::{FirmwareDevice, FirmwareSource};
use rs_fdtree::{DtBackend, LinuxFdt};
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::driver_registry::clk::fixed_clock_driver;

// -- Device-tree fixture ------------------------------------------------------

struct Dtb {
    structure: Vec<u8>,
    strings: Vec<u8>,
}

impl Dtb {
    fn new() -> Self {
        Self {
            structure: Vec::new(),
            strings: Vec::new(),
        }
    }

    fn word(&mut self, value: u32) {
        self.structure.extend_from_slice(&value.to_be_bytes());
    }

    fn align(&mut self) {
        while !self.structure.len().is_multiple_of(4) {
            self.structure.push(0);
        }
    }

    fn begin(&mut self, name: &str) {
        self.word(1); // FDT_BEGIN_NODE
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        self.align();
        self.word(4); // FDT_NOP padding
    }

    fn property(&mut self, name: &str, bytes: &[u8]) {
        self.word(3); // FDT_PROP
        self.word(bytes.len() as u32);
        self.word(self.strings.len() as u32);
        self.strings.extend_from_slice(name.as_bytes());
        self.strings.push(0);
        self.structure.extend_from_slice(bytes);
        self.align();
    }

    fn end(&mut self) {
        self.word(2); // FDT_END_NODE
    }

    fn finish(mut self) -> Vec<u8> {
        self.word(9); // FDT_END
        let strings_offset = 56 + self.structure.len();
        let mut bytes = Vec::new();
        for value in [
            0xd00d_feed,
            (strings_offset + self.strings.len()) as u32,
            56,
            strings_offset as u32,
            40,
            17,
            16,
            0,
            self.strings.len() as u32,
            self.structure.len() as u32,
        ] {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        bytes.extend_from_slice(&[0; 16]);
        bytes.extend_from_slice(&self.structure);
        bytes.extend_from_slice(&self.strings);
        bytes
    }
}

fn cells(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect()
}

fn u64_cells(value: u64) -> Vec<u8> {
    value.to_be_bytes().to_vec()
}

/// Fixture:
///
/// ```text
/// / {
///     uart_clk: clock-controller@0 {
///         compatible = "fixed-clock";
///         #clock-cells = <0>;
///         clock-frequency = /bits/ 64 <24000000>;
///     };
///     serial@1000 {
///         compatible = "test,clk-consumer";
///         clocks = <&uart_clk>;
///         clock-names = "uartclk";
///     };
/// };
/// ```
fn clock_tree() -> Vec<u8> {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.property("#address-cells", &cells(&[1]));
    blob.property("#size-cells", &cells(&[1]));
    blob.begin("clock-controller@0");
    blob.property("compatible", b"fixed-clock\0");
    blob.property("#clock-cells", &cells(&[0]));
    blob.property("clock-frequency", &u64_cells(24_000_000));
    blob.property("phandle", &cells(&[7]));
    blob.end();
    blob.begin("serial@1000");
    blob.property("compatible", b"test,clk-consumer\0");
    blob.property("clocks", &cells(&[7]));
    blob.property("clock-names", b"uartclk\0");
    blob.end();
    blob.end();
    blob.finish()
}

/// Leaks the fixture blob and backend; test-only boot-static storage.
fn fixture() -> (FwNode<'static>, FwNode<'static>) {
    let bytes: &'static [u8] = alloc::vec::Vec::leak(clock_tree());
    let tree = LinuxFdt::new(bytes).expect("fixture is a valid device tree");
    let backend: &'static DtBackend<'static> =
        KBox::leak(KBox::new(DtBackend::new(tree)).expect("fixture allocation failed"));
    let root = backend.root().expect("fixture has a root");
    let provider = root
        .descendants()
        .find(|node| node.name() == Some("clock-controller@0"))
        .expect("provider node exists");
    let consumer = root
        .descendants()
        .find(|node| node.firmware_ids().any(|id| id == "test,clk-consumer"))
        .expect("consumer node exists");
    (provider, consumer)
}

use khal::firmware::devices::FwNode;

// -- The chain ----------------------------------------------------------------

#[def_test(serial)]
fn fixed_clock_provider_resolves_through_the_capability_contract() {
    kclk::reset_providers();

    let (provider_node, consumer_node) = fixture();

    // The registry reset comes first: it wipes earlier registrations.
    let (_bus_type, bus) = reset_and_setup_platform_bus_for_tests("clk-fixed-bus");
    let mut backend = PlatformBackend::new();
    let mut context = EnumerationContext::new();
    let bus_id = bus.id();

    // The production provider driver registers the same way init_drivers does.
    register_driver_object(fixed_clock_driver());
    let device = FirmwareDevice {
        firmware_id: "fixed-clock",
        source: FirmwareSource::DeviceTree,
        node: provider_node,
        mmio: Ok(None),
        irq: Ok(None),
    };
    backend
        .register_firmware_device(&mut context, bus_id, &device)
        .expect("fixed-clock registers");
    assert_eq!(context.probe_pending(), 1, "provider driver binds");

    // Consumer side: connection name -> (provider, index) -> capability op.
    let spec = of::clocks::resolve_by_name(&consumer_node, "uartclk")
        .expect("clock-names parse")
        .expect("uartclk resolves");
    assert_eq!(spec.provider_id, provider_node.id().raw());

    let clk = XKERNEL_CLOCK_OP
        .clk_get(spec.provider_id, spec.index)
        .expect("provider is registered");
    assert!(!clk.is_enabled());
    assert_eq!(clk.get_rate(), 24_000_000);

    clk.prepare_enable().expect("enable succeeds");
    assert!(clk.is_enabled());
    clk.disable_unprepare();
    assert!(!clk.is_enabled());

    // An unknown provider reports NotFound, not a panic.
    let missing = XKERNEL_CLOCK_OP
        .clk_get(0xDEAD_0000, 0)
        .expect_err("unknown provider fails");
    assert!(matches!(missing, driver_os_api::ResError::NotFound));
    let _ = Arc::new(()); // keep Arc import used when features change
}

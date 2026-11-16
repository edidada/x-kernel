// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Clock binding parsing against the real device-tree backend.
//!
//! The fixture builds a small flattened device tree with the production
//! `DtBackend`, so the tests exercise the same phandle resolution and cell
//! arithmetic the boot firmware path uses.

use alloc::vec::Vec;

use fwnode::FwNode;
use rs_fdtree::{DtBackend, LinuxFdt};
use unittest::{assert, assert_eq, def_test};

use super::clocks;

/// Minimal flattened-device-tree writer (same shape as the kdriver fixture).
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

/// Fixture.
///
/// The rs-fdtree reference resolver requires a uniform stride across one
/// `clocks` list, so every provider here declares `#clock-cells = <1>`; the
/// zero-cell path is covered by the separate `bare` consumer whose single
/// entry resolves against `clk0`.
///
/// ```text
/// / {
///     serial@1000 {
///         compatible = "test,serial";
///         clocks = <&uart_clk 0>, <&cru 5>;
///         clock-names = "uartclk", "apb_pclk";
///     };
///     bare {
///         compatible = "test,bare";
///         clocks = <&clk0>;
///     };
///     uart_clk: clock-freq@0 {
///         compatible = "fixed-clock";
///         #clock-cells = <1>;
///         clock-frequency = /bits/ 64 <24000000>;
///     };
///     clk0: clock-freq@1 {
///         compatible = "fixed-clock";
///         #clock-cells = <0>;
///         clock-frequency = /bits/ 64 <48000000>;
///     };
///     cru {
///         compatible = "test,cru";
///         #clock-cells = <1>;
///         clock-output-names = "uart", "mmc";
///     };
///     mixed {
///         compatible = "test,mixed";
///         #clock-cells = <2>;
///     };
/// };
/// ```
fn clock_tree() -> Vec<u8> {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.property("#address-cells", &cells(&[1]));
    blob.property("#size-cells", &cells(&[1]));
    blob.begin("serial@1000");
    blob.property("compatible", b"test,serial\0");
    blob.property("clocks", &cells(&[1, 0, 2, 5]));
    blob.property("clock-names", b"uartclk\0apb_pclk\0");
    blob.end();
    blob.begin("bare");
    blob.property("compatible", b"test,bare\0");
    blob.property("clocks", &cells(&[3]));
    blob.end();
    blob.begin("clock-freq@0");
    blob.property("compatible", b"fixed-clock\0");
    blob.property("#clock-cells", &cells(&[1]));
    blob.property("clock-frequency", &u64_cells(24_000_000));
    blob.property("phandle", &cells(&[1]));
    blob.end();
    blob.begin("clock-freq@1");
    blob.property("compatible", b"fixed-clock\0");
    blob.property("#clock-cells", &cells(&[0]));
    blob.property("clock-frequency", &u64_cells(48_000_000));
    blob.property("phandle", &cells(&[3]));
    blob.end();
    blob.begin("cru");
    blob.property("compatible", b"test,cru\0");
    blob.property("#clock-cells", &cells(&[1]));
    blob.property("clock-output-names", b"uart\0mmc\0");
    blob.property("phandle", &cells(&[2]));
    blob.end();
    blob.begin("mixed");
    blob.property("compatible", b"test,mixed\0");
    blob.property("#clock-cells", &cells(&[2]));
    blob.property("phandle", &cells(&[3]));
    blob.end();
    blob.end();
    blob.finish()
}

/// Leaks the parsed tree into a `'static` backend, so the node borrows handed
/// out by the fixtures stay valid for the whole test run.
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: portable firmware fixture returns static FDT \
              node borrows and needs a leaked stable FDT owner; it cannot depend on the kernel \
              allocator"
)]
fn leak_backend(tree: LinuxFdt<'static>) -> &'static DtBackend<'static> {
    alloc::boxed::Box::leak(alloc::boxed::Box::new(DtBackend::new(tree)))
}

/// Leaks the fixture blob and backend; test-only boot-static storage.
fn fixture() -> (FwNode<'static>, FwNode<'static>, FwNode<'static>, FwNode<'static>, FwNode<'static>) {
    let bytes: &'static [u8] = alloc::vec::Vec::leak(clock_tree());
    let tree = LinuxFdt::new(bytes).expect("fixture is a valid device tree");
    let backend: &'static DtBackend<'static> = leak_backend(tree);
    let root = backend.root().expect("fixture has a root");
    let find = |compatible: &str| {
        root.descendants()
            .find(|node| node.firmware_ids().any(|id| id == compatible))
            .expect("fixture node exists")
    };
    let consumer = find("test,serial");
    let bare = find("test,bare");
    let fixed = root
        .descendants()
        .find(|node| node.name() == Some("clock-freq@0"))
        .expect("uart_clk exists");
    let clk0 = root
        .descendants()
        .find(|node| node.name() == Some("clock-freq@1"))
        .expect("clk0 exists");
    let cru = find("test,cru");
    (consumer, bare, fixed, clk0, cru)
}

#[def_test]
fn resolve_by_index_maps_provider_and_cells() {
    let (consumer, _, fixed, _, cru) = fixture();
    let first = clocks::resolve_by_index(&consumer, 0)
        .expect("clocks parse")
        .expect("entry 0 exists");
    assert_eq!(first.provider_id, fixed.id().raw());
    assert_eq!(first.index, 0);

    let second = clocks::resolve_by_index(&consumer, 1)
        .expect("clocks parse")
        .expect("entry 1 exists");
    assert_eq!(second.provider_id, cru.id().raw());
    assert_eq!(second.index, 5);
}

#[def_test]
fn resolve_zero_cell_provider_entry() {
    let (_, bare, _, clk0, _) = fixture();
    let resolved = clocks::resolve_by_index(&bare, 0)
        .expect("clocks parse")
        .expect("entry 0 exists");
    assert_eq!(resolved.provider_id, clk0.id().raw());
    assert_eq!(resolved.index, 0);
}

#[def_test]
fn resolve_by_index_out_of_range_is_none() {
    let (consumer, ..) = fixture();
    let resolved = clocks::resolve_by_index(&consumer, 9).expect("clocks parse");
    assert!(resolved.is_none());
}

#[def_test]
fn resolve_by_name_matches_connection_name() {
    let (consumer, _, fixed, _, cru) = fixture();
    let uartclk = clocks::resolve_by_name(&consumer, "uartclk")
        .expect("clock-names parse")
        .expect("uartclk exists");
    assert_eq!(uartclk.provider_id, fixed.id().raw());
    assert_eq!(uartclk.index, 0);

    let apb = clocks::resolve_by_name(&consumer, "apb_pclk")
        .expect("clock-names parse")
        .expect("apb_pclk exists");
    assert_eq!(apb.provider_id, cru.id().raw());
    assert_eq!(apb.index, 5);

    let missing = clocks::resolve_by_name(&consumer, "nope").expect("clock-names parse");
    assert!(missing.is_none());
}

#[def_test]
fn provider_declarations_are_readable() {
    let (_, bare, fixed, _, cru) = fixture();
    assert_eq!(
        clocks::frequency(&fixed),
        Some(24_000_000),
        "64-bit clock-frequency cell"
    );
    assert_eq!(
        clocks::frequency(&bare.parent().expect("bare parent").first_child().expect("first")),
        None,
        "consumer nodes carry no clock-frequency"
    );
    assert_eq!(clocks::cells(&cru).expect("cells"), Some(1));
    assert_eq!(
        clocks::output_name(&cru, 1),
        Some("mmc"),
        "clock-output-names entry"
    );
    assert_eq!(clocks::output_name(&cru, 9), None);
}

#[def_test]
fn mixed_clock_cells_are_rejected() {
    // Rebuild with a consumer that references the two-cell provider so the
    // uniform-cells check trips on entry 1.
    let mut blob = Dtb::new();
    blob.begin("");
    blob.property("#address-cells", &cells(&[1]));
    blob.property("#size-cells", &cells(&[1]));
    blob.begin("consumer");
    blob.property("compatible", b"test,serial\0");
    blob.property("clocks", &cells(&[1, 3, 4, 2]));
    blob.end();
    blob.begin("fixed");
    blob.property("compatible", b"fixed-clock\0");
    blob.property("#clock-cells", &cells(&[0]));
    blob.property("phandle", &cells(&[1]));
    blob.end();
    blob.begin("mixed");
    blob.property("compatible", b"test,mixed\0");
    blob.property("#clock-cells", &cells(&[2]));
    blob.property("phandle", &cells(&[3]));
    blob.end();
    blob.end();
    let bytes: &'static [u8] = alloc::vec::Vec::leak(blob.finish());
    let tree = LinuxFdt::new(bytes).expect("valid fixture");
    let backend: &'static DtBackend<'static> = leak_backend(tree);
    let consumer = backend
        .root()
        .expect("root")
        .descendants()
        .find(|node| node.firmware_ids().any(|id| id == "test,serial"))
        .expect("consumer");

    let error = clocks::resolve_by_index(&consumer, 1).expect_err("mixed cells rejected");
    assert!(matches!(error, fwnode::FwError::InvalidValue));
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Driver-facing resource helpers backed by the [`device_res`] provider model.
//!
//! The X-Kernel provider implementation lives in `device-res-xkernel`. This
//! module keeps the driver orchestration crate's `DriverResult` boundary while
//! acquiring resources through the OS-neutral `device_res` API.

use alloc::{boxed::Box, sync::Arc};
use core::ptr::NonNull;

use device_res::{
    DmaSpec, IrqHandler as DevIrqHandler, IrqResource, IrqThreadHandler as DevIrqThreadHandler,
    MmioRegion, ResError,
};
use device_res_xkernel::XKernelResourceProvider;
use driver_base::{DriverError, DriverResult};
use kdevice::DeviceObject;

static XKERNEL_RESOURCE_PROVIDER: XKernelResourceProvider = XKernelResourceProvider::new();

pub(crate) fn resource_provider() -> &'static XKernelResourceProvider {
    &XKERNEL_RESOURCE_PROVIDER
}

fn map_res_err(err: ResError) -> DriverError {
    match err {
        ResError::InvalidResource => DriverError::InvalidInput,
        ResError::MappingFailed => DriverError::Io,
        ResError::NoMemory => DriverError::NoMemory,
        ResError::Busy => DriverError::ResourceBusy,
        ResError::Unsupported => DriverError::Unsupported,
        // NoProvider is retryable (a provider has not probed yet — defer);
        // NotFound means the resource list has no such entry.
        ResError::NoProvider => DriverError::WouldBlock,
        ResError::NotFound => DriverError::InvalidInput,
    }
}

/// Map an MMIO region and tie its lifetime to `device`.
///
/// The mapping is released when the device's probe fails or the device is
/// removed. Returns the virtual base address for the region.
pub(crate) fn devm_iomap(
    device: &DeviceObject,
    region: MmioRegion,
    name: &'static str,
) -> DriverResult<NonNull<u8>> {
    device_res::devm_iomap_with_provider(resource_provider(), device, region, name)
        .map_err(map_res_err)
}

/// Register a context-carrying interrupt handler and tie its lifetime to
/// `device`.
///
/// The handler is released when the device's probe fails or the device is
/// removed.
pub(crate) fn devm_request_irq(
    device: &DeviceObject,
    irq: IrqResource,
    handler: Arc<dyn DevIrqHandler>,
) -> DriverResult<()> {
    device_res::devm_request_irq_with_provider(resource_provider(), device, irq, handler)
        .map_err(map_res_err)
}

/// Register primary and threaded interrupt handlers and tie their lifetime to
/// `device`.
///
/// The request is routed through the framework-owned `device_res` IRQ provider.
pub(crate) fn devm_request_threaded_irq(
    device: &DeviceObject,
    irq: IrqResource,
    primary: Arc<dyn DevIrqHandler>,
    thread: Arc<dyn DevIrqThreadHandler>,
    name: Option<&'static str>,
) -> DriverResult<()> {
    device_res::devm_request_threaded_irq_with_provider(
        resource_provider(),
        device,
        irq,
        primary,
        thread,
        name,
    )
    .map_err(map_res_err)
}

/// Register a default-primary threaded interrupt handler and tie its lifetime
/// to `device`.
///
/// The request is routed through the framework-owned `device_res` IRQ provider.
pub(crate) fn devm_request_threaded_irq_default(
    device: &DeviceObject,
    irq: IrqResource,
    thread: Arc<dyn DevIrqThreadHandler>,
    name: Option<&'static str>,
) -> DriverResult<()> {
    device_res::devm_request_threaded_irq_default_with_provider(
        resource_provider(),
        device,
        irq,
        thread,
        name,
    )
    .map_err(map_res_err)
}

/// Allocate a coherent DMA buffer and tie its lifetime to `device`.
///
/// The buffer is freed when the device's probe fails or the device is removed.
/// Returns the CPU virtual address and device bus address of the buffer.
pub(crate) fn devm_alloc_coherent(
    device: &DeviceObject,
    spec: DmaSpec,
) -> DriverResult<(NonNull<u8>, u64)> {
    device_res::devm_alloc_coherent_with_provider(resource_provider(), device, spec)
        .map_err(map_res_err)
}

/// Driver-facing device-managed resource methods.
///
/// Drivers use this extension trait to acquire resources from their
/// [`DeviceObject`] without naming the concrete host provider. The provider is
/// owned by the driver framework and passed explicitly into `device_res`.
pub trait DeviceResourceExt {
    /// Map an MMIO region and tie its lifetime to this device.
    fn devm_iomap(&self, region: MmioRegion, name: &'static str) -> DriverResult<NonNull<u8>>;

    /// Register a context-carrying interrupt handler and tie its lifetime to
    /// this device.
    fn devm_request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn DevIrqHandler>,
    ) -> DriverResult<()>;

    /// Register primary and threaded interrupt handlers and tie their lifetime
    /// to this device.
    fn devm_request_threaded_irq(
        &self,
        irq: IrqResource,
        primary: Arc<dyn DevIrqHandler>,
        thread: Arc<dyn DevIrqThreadHandler>,
        name: Option<&'static str>,
    ) -> DriverResult<()>;

    /// Register a default-primary threaded interrupt handler and tie its
    /// lifetime to this device.
    fn devm_request_threaded_irq_default(
        &self,
        irq: IrqResource,
        thread: Arc<dyn DevIrqThreadHandler>,
        name: Option<&'static str>,
    ) -> DriverResult<()>;

    /// Allocate a coherent DMA buffer and tie its lifetime to this device.
    fn devm_alloc_coherent(&self, spec: DmaSpec) -> DriverResult<(NonNull<u8>, u64)>;
}

pub fn devm_clk_get(
    device: &dyn device_res::DeviceResource,
    spec: device_res::ClkSpec,
) -> DriverResult<device_res::Clk> {
    let clk = device_res::Clk::get_with(resource_provider(), spec).map_err(map_res_err)?;
    let cleanup = clk.clone();
    device.register_cleanup(Box::new(move || drop(cleanup)));
    Ok(clk)
}

impl DeviceResourceExt for DeviceObject {
    fn devm_iomap(&self, region: MmioRegion, name: &'static str) -> DriverResult<NonNull<u8>> {
        devm_iomap(self, region, name)
    }

    fn devm_request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn DevIrqHandler>,
    ) -> DriverResult<()> {
        devm_request_irq(self, irq, handler)
    }

    fn devm_request_threaded_irq(
        &self,
        irq: IrqResource,
        primary: Arc<dyn DevIrqHandler>,
        thread: Arc<dyn DevIrqThreadHandler>,
        name: Option<&'static str>,
    ) -> DriverResult<()> {
        devm_request_threaded_irq(self, irq, primary, thread, name)
    }

    fn devm_request_threaded_irq_default(
        &self,
        irq: IrqResource,
        thread: Arc<dyn DevIrqThreadHandler>,
        name: Option<&'static str>,
    ) -> DriverResult<()> {
        devm_request_threaded_irq_default(self, irq, thread, name)
    }

    fn devm_alloc_coherent(&self, spec: DmaSpec) -> DriverResult<(NonNull<u8>, u64)> {
        devm_alloc_coherent(self, spec)
    }
}

#[cfg(unittest)]
pub mod clk_e2e_tests {
    use alloc::{sync::Arc, vec::Vec};

    use device_res::{ClkSpec, DeviceResource};
    use kspin::SpinNoIrq;
    use unittest::{TestResult, def_test, ktest_println};

    use crate::resource::devm_clk_get;

    // ---- mock controller: a table the "device" resolves clocks through ----

    static CCU: kclk::ClkTable = kclk::ClkTable::new(0x5A);
    /// Serializes tests touching the global kclk provider registry.
    static E2E_SERIAL: SpinNoIrq<()> = SpinNoIrq::new(());

    /// Build osc(24M) → pll(×50) → div(programmable) → uart gate and install
    /// the provider. Mirrors what a real CCU driver does in probe().
    fn install_mock_ccu() {
        use kclk::{ClkBranch, DividerFlags, GateFlags};

        CCU.clear();
        kclk::reset_providers();
        let branches = [
            ClkBranch::fixed_rate(0, "osc24m", 24_000_000),
            ClkBranch::fixed_factor(1, "pll1200", 50, 1, &["osc24m"]),
            // The clock the UART consumes: a divider (bits [7:4]) + gate
            // (bit 11) in one word — the standard composite node shape.
            ClkBranch::composite(
                3,
                "uart_clk",
                None,
                Some(kclk::DividerDesc {
                    offset: 0x30,
                    shift: 4,
                    width: 4,
                    table: None,
                    flags: DividerFlags::empty(),
                }),
                Some(kclk::GateDesc {
                    offset: 0x30,
                    bit: 11,
                    flags: GateFlags::empty(),
                }),
                &["pll1200"],
                kclk::ClkFlags::empty(),
            ),
        ];
        // The register window backs the divider + gate; a memory-backed
        // accessor is enough because only field math is exercised.
        let reg: Arc<dyn kclk::ClkRegAccess> = Arc::new(RamClkReg::new());
        kclk::register_table(&CCU, reg, &branches).unwrap();
        kclk::register_provider(&CCU);
    }

    /// Memory-backed register window standing in for the controller's MMIO.
    struct RamClkReg {
        words: SpinNoIrq<[u32; 16]>,
    }
    impl RamClkReg {
        fn new() -> Self {
            Self {
                words: SpinNoIrq::new([0; 16]),
            }
        }
    }
    impl kclk::ClkRegAccess for RamClkReg {
        fn reg_read(&self, offset: usize) -> u32 {
            self.words.lock()[offset / 4]
        }

        fn reg_write(&self, offset: usize, value: u32) {
            self.words.lock()[offset / 4] = value;
        }
    }

    // ---- mock device: the minimum a driver's probe sees ----

    struct UartDevice {
        cleanups: SpinNoIrq<Vec<alloc::boxed::Box<dyn FnOnce() + Send>>>,
    }
    impl UartDevice {
        fn new() -> Self {
            Self {
                cleanups: SpinNoIrq::new(Vec::new()),
            }
        }

        /// The remove path: LIFO devres drain (DeviceObject::run_cleanups).
        fn remove(&self) {
            let mut cleanups = self.cleanups.lock();
            while let Some(cleanup) = cleanups.pop() {
                cleanup();
            }
        }
    }
    impl DeviceResource for UartDevice {
        fn resources(&self) -> &[device_res::ResourceDesc] {
            &[]
        }

        fn register_cleanup(&self, cleanup: alloc::boxed::Box<dyn FnOnce() + Send>) {
            self.cleanups.lock().push(cleanup);
        }
    }

    // ---- the device driver's probe/remove, exactly as a real one writes it ----

    #[def_test(serial)]
    fn device_consumes_clock_end_to_end() -> TestResult {
        let _serial = E2E_SERIAL.lock();
        install_mock_ccu();

        // probe(): resolve the clock the DT would describe as
        //   clocks = <&ccu 3>; clock-names = "baudclk";
        // and the driver requests by SoC name (the contracts layer resolves it).
        let device = UartDevice::new();
        let clk = match devm_clk_get(&device, ClkSpec::by_phandle_index(0x5A, 3)) {
            Ok(clk) => clk,
            Err(e) => {
                ktest_println!("E2E FAIL: devm_clk_get returned {e:?}");
                return TestResult::Failed;
            }
        };
        ktest_println!("E2E: resolved {} via devm_clk_get", clk.name());
        if clk.name() != "uart_clk" {
            return TestResult::Failed;
        }

        // Turn the clock on — the driver's usual first action.
        if let Err(e) = clk.prepare_enable() {
            ktest_println!("E2E FAIL: prepare_enable {e:?}");
            return TestResult::Failed;
        }
        if !clk.is_enabled().unwrap_or(false) {
            ktest_println!("E2E FAIL: clock not enabled after prepare_enable");
            return TestResult::Failed;
        }

        // Program the UART divisor from the actual clock rate.
        // Boot state: div field 0 (÷1) of pll1200 → 1.2 GHz.
        let rate = clk.get_rate();
        ktest_println!("E2E: baudclk = {rate} Hz");
        if rate != 1_200_000_000 {
            return TestResult::Failed;
        }

        // DVFS: ask for 150 MHz (divider ÷8), read it back through the handle.
        let actual = match clk.set_rate(150_000_000) {
            Ok(r) => r,
            Err(e) => {
                ktest_println!("E2E FAIL: set_rate {e:?}");
                return TestResult::Failed;
            }
        };
        ktest_println!("E2E: set_rate(150 MHz) -> {actual} Hz");
        if !(140_000_000..=170_000_000).contains(&actual) {
            return TestResult::Failed;
        }
        let rounded = clk.round_rate(99_999_999).unwrap_or(0);
        ktest_println!("E2E: round_rate(100 MHz) -> {rounded} Hz");

        // A second consumer on the same clock: independent handle, shared gate.
        let clk2 = match devm_clk_get(&device, ClkSpec::by_name("uart_clk")) {
            Ok(c) => c,
            Err(e) => {
                ktest_println!("E2E FAIL: second devm_clk_get {e:?}");
                return TestResult::Failed;
            }
        };
        // Enable through the second handle, then release it: the first
        // consumer still holds a reference, so the shared gate must stay on.
        if let Err(e) = clk2.prepare_enable() {
            ktest_println!("E2E FAIL: second prepare_enable {e:?}");
            return TestResult::Failed;
        }
        clk2.disable_unprepare();
        if !clk.is_enabled().unwrap_or(false) {
            ktest_println!("E2E FAIL: HW gated while another consumer holds it");
            return TestResult::Failed;
        }

        // remove(): devres drains LIFO — the handles drop, the clock releases.
        drop(clk2);
        clk.disable_unprepare();
        drop(clk);
        device.remove();
        ktest_println!("E2E: device removed, devres drained");

        // The provider itself is gone from lookup.
        kclk::reset_providers();
        let gone = devm_clk_get(&device, ClkSpec::by_phandle_index(0x5A, 3));
        if gone.is_ok() {
            ktest_println!("E2E FAIL: lookup succeeded after provider removal");
            return TestResult::Failed;
        }
        ktest_println!("E2E: OK — full contracts→kdriver→kclk round trip");
        TestResult::Ok
    }
}

#[cfg(unittest)]
pub mod clk_vccu_tests {
    use alloc::{sync::Arc, vec::Vec};

    use device_res::{ClkSpec, DeviceResource};
    use kspin::SpinNoIrq;
    use unittest::{TestResult, def_test, ktest_println};

    use crate::resource::devm_clk_get;

    static VCCU: kclk::ClkTable = kclk::ClkTable::new(0xCC0);
    static VCCU_SERIAL: SpinNoIrq<()> = SpinNoIrq::new(());

    /// Plan-C "virtual CCU": a four-level tree — osc → PLL(settable) →
    /// mux(2 sources) → divider → uart gate — with all register-backed nodes
    /// accessed through the REAL MmioClkReg over a kernel-memory bank.
    /// This exercises the actual volatile-access path (not the trait mock)
    /// and the full controller surface: enable/disable, DVFS, parent
    /// switching, and the diagnostic/PM walks.
    struct VccuBank {
        base: usize,
    }

    impl VccuBank {
        /// Allocate a page-aligned kernel-memory register bank (the stand-in
        /// for the controller's MMIO window) and return it plus the base.
        fn alloc() -> Self {
            // A leaked, zeroed, page-aligned allocation stands in for the
            // mapped device window. Word-aligned volatile accesses to it are
            // sound by construction.
            let layout = alloc::alloc::Layout::from_size_align(0x100, 0x100).unwrap();
            // SAFETY: `layout` is non-zero and the global allocator
            // returns a valid pointer or null; alloc_zeroed gives a
            // page-aligned, zeroed block suitable for the register bank.
            let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
            Self { base: ptr as usize }
        }
    }

    fn build_vccu() -> &'static VccuBank {
        static BANK: SpinNoIrq<Option<&'static VccuBank>> = SpinNoIrq::new(None);
        let mut slot = BANK.lock();
        if slot.is_none() {
            let bank = VccuBank::alloc();
            *slot = Some(alloc::boxed::Box::leak(alloc::boxed::Box::new(bank)));
        }
        slot.unwrap()
    }

    fn install_vccu() {
        use kclk::{ClkBranch, DividerDesc, DividerFlags, GateDesc, GateFlags, MuxDesc};

        VCCU.clear();
        kclk::reset_providers();
        let bank = build_vccu();

        let branches = [
            // Level 0: 24 MHz crystal.
            ClkBranch::fixed_rate(0, "v_osc", 24_000_000),
            // Level 1: settable "PLL" — a divider against the crystal
            // (stand-in for a real PLL's programmable VCO).
            ClkBranch::fixed_factor(1, "v_pll", 50, 1, &["v_osc"]),
            // Level 2: 2-input mux selecting between PLL and crystal.
            ClkBranch::composite(
                2,
                "v_mux",
                Some(MuxDesc {
                    offset: 0x10,
                    shift: 0,
                    mask: 0x1,
                    table: None,
                    flags: kclk::MuxFlags::empty(),
                }),
                None,
                None,
                &["v_pll", "v_osc"],
                kclk::ClkFlags::SET_RATE_PARENT,
            ),
            // Level 3: the clock the "device" consumes — divider + gate.
            ClkBranch::composite(
                3,
                "v_uart",
                None,
                Some(DividerDesc {
                    offset: 0x20,
                    shift: 0,
                    width: 4,
                    table: None,
                    flags: DividerFlags::empty(),
                }),
                Some(GateDesc {
                    offset: 0x20,
                    bit: 8,
                    flags: GateFlags::empty(),
                }),
                &["v_mux"],
                kclk::ClkFlags::empty(),
            ),
        ];

        // SAFETY: `bank.base` is a valid, word-aligned, page-aligned kernel
        // allocation that lives for the process lifetime (a leaked static);
        // MmioClkReg only does word-aligned volatile accesses within [base,
        // base+0x100).
        let reg: Arc<dyn kclk::ClkRegAccess> =
            Arc::new(unsafe { kclk::MmioClkReg::new(bank.base) });
        kclk::register_table(&VCCU, reg, &branches).unwrap();
        kclk::register_provider(&VCCU);
    }

    struct Dev {
        cleanups: SpinNoIrq<Vec<alloc::boxed::Box<dyn FnOnce() + Send>>>,
    }
    impl Dev {
        fn new() -> Self {
            Self {
                cleanups: SpinNoIrq::new(Vec::new()),
            }
        }
    }
    impl DeviceResource for Dev {
        fn resources(&self) -> &[device_res::ResourceDesc] {
            &[]
        }

        fn register_cleanup(&self, c: alloc::boxed::Box<dyn FnOnce() + Send>) {
            self.cleanups.lock().push(c);
        }
    }

    #[def_test(serial)]
    fn virtual_ccu_full_tree_on_mmio_regbank() -> TestResult {
        let _serial = VCCU_SERIAL.lock();
        install_vccu();
        let device = Dev::new();

        // Resolve the leaf clock through the full stack.
        let clk = match devm_clk_get(&device, ClkSpec::by_phandle_index(0xCC0, 3)) {
            Ok(c) => c,
            Err(e) => {
                ktest_println!("VCCU FAIL: devm_clk_get {e:?}");
                return TestResult::Failed;
            }
        };
        if clk.name() != "v_uart" {
            ktest_println!("VCCU FAIL: wrong clock {}", clk.name());
            return TestResult::Failed;
        }

        // Enable through the whole chain (gate → mux → PLL → osc).
        if let Err(e) = clk.prepare_enable() {
            ktest_println!("VCCU FAIL: prepare_enable {e:?}");
            return TestResult::Failed;
        }
        ktest_println!("VCCU: v_uart enabled at {} Hz", clk.get_rate());

        // Boot state: PLL ×50 (1.2 GHz), mux→PLL, divider ÷1 → 1.2 GHz.
        if clk.get_rate() != 1_200_000_000 {
            ktest_println!("VCCU FAIL: boot rate {} != 1.2 GHz", clk.get_rate());
            return TestResult::Failed;
        }

        // DVFS at the leaf: set v_uart to 300 MHz via the leaf divider (÷4).
        let r = match clk.set_rate(300_000_000) {
            Ok(r) => r,
            Err(e) => {
                ktest_println!("VCCU FAIL: set_rate(12M) {e:?}");
                return TestResult::Failed;
            }
        };
        ktest_println!("VCCU: set_rate(300 MHz) → {r} Hz");

        // DVFS at a different divider point: 150 MHz (÷8).
        let r2 = match clk.set_rate(150_000_000) {
            Ok(r) => r,
            Err(e) => {
                ktest_println!("VCCU FAIL: set_rate(600M) {e:?}");
                return TestResult::Failed;
            }
        };
        ktest_println!("VCCU: set_rate(150 MHz) → {r2} Hz");
        if !(140_000_000..=160_000_000).contains(&r2) {
            return TestResult::Failed;
        }

        // Parent switch: flip the mux (a separate node with 2 sources) from
        // the PLL to the crystal. The leaf (v_uart) has one parent (the
        // mux); the switch belongs on the mux itself.
        let mux = match devm_clk_get(&device, ClkSpec::by_phandle_index(0xCC0, 2)) {
            Ok(m) => m,
            Err(e) => {
                ktest_println!("VCCU FAIL: resolve mux {e:?}");
                return TestResult::Failed;
            }
        };
        if let Err(e) = mux.set_parent(1) {
            ktest_println!("VCCU FAIL: mux.set_parent(1) {e:?}");
            return TestResult::Failed;
        }
        // Now on the crystal through the same divider chain.
        let after_switch = clk.get_rate();
        ktest_println!("VCCU: after parent switch, rate = {after_switch} Hz");
        if after_switch > 24_000_000 {
            return TestResult::Failed;
        }

        // clk_summary over the virtual CCU.
        let summary = kclk::clk_summary();
        let v_uart = summary.iter().find(|e| e.name == "v_uart");
        if v_uart.is_none() {
            ktest_println!("VCCU FAIL: v_uart not in clk_summary");
            return TestResult::Failed;
        }
        ktest_println!(
            "VCCU: clk_summary shows {} clocks (v_uart rate={})",
            summary.len(),
            v_uart.unwrap().rate
        );

        // clk_disable_unused: the crystal branch is held by our reference; a
        // sibling we never enabled would be gated. Just verify the walk runs.
        kclk::clk_disable_unused();
        ktest_println!("VCCU: clk_disable_unused completed");

        clk.disable_unprepare();
        if clk.is_enabled().unwrap_or(true) {
            ktest_println!("VCCU FAIL: still enabled after release");
            return TestResult::Failed;
        }

        kclk::reset_providers();
        ktest_println!("VCCU: OK — full virtual-CCU round trip on MmioClkReg");
        TestResult::Ok
    }
}

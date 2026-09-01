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
    MmioRegion, ResError, ResResult,
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
        ResError::NoProvider | ResError::NotFound => DriverError::InvalidInput,
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
/// Resolve a clock for `device` and tie the consumer handle's lifetime to it.
///
/// `spec` is normally built by the platform's firmware layer from the device's
/// DT `clocks` specifier (or by the SoC clock name). The underlying kclk
/// adapter lives in this crate's clock module.
pub fn devm_clk_get(
    device: &dyn device_res::DeviceResource,
    spec: device_res::ClkSpec,
) -> DriverResult<device_res::Clk> {
    let clk = device_res::Clk::get_with(clk_op_provider(), spec).map_err(map_res_err)?;
    let cleanup = clk.clone();
    device.register_cleanup(Box::new(move || drop(cleanup)));
    Ok(clk)
}

/// The host `ClkOp` implementation resolving clocks through `kclk`.
fn clk_op_provider() -> &'static dyn device_res::ClkOp {
    &KCLK_OP
}

static KCLK_OP: KClkOp = KClkOp;

/// `ClkOp` adapter delegating to the kclk provider registry.
struct KClkOp;

impl device_res::ClkOp for KClkOp {
    fn get_clock(
        &self,
        spec: device_res::ClkSpec,
    ) -> ResResult<alloc::sync::Arc<dyn device_res::ClkConsumer>> {
        use alloc::sync::Arc;

        use kclk::ClkHandle;

        struct KclkConsumer(ClkHandle);
        impl device_res::ClkConsumer for KclkConsumer {
            fn name(&self) -> &str {
                self.0.name()
            }

            fn get_rate(&self) -> u64 {
                self.0.get_rate()
            }

            fn prepare(&self) -> ResResult<()> {
                self.0.prepare().map_err(map_clk_err)
            }

            fn unprepare(&self) {
                let _ = self.0.unprepare();
            }

            fn enable(&self) -> ResResult<()> {
                self.0.enable().map_err(map_clk_err)
            }

            fn disable(&self) {
                self.0.disable()
            }

            fn prepare_enable(&self) -> ResResult<()> {
                self.0.prepare_enable().map_err(map_clk_err)
            }

            fn disable_unprepare(&self) {
                self.0.disable_unprepare()
            }

            fn is_enabled(&self) -> ResResult<bool> {
                self.0.is_enabled().map_err(map_clk_err)
            }

            fn is_prepared(&self) -> ResResult<bool> {
                self.0.is_prepared().map_err(map_clk_err)
            }

            fn get_parent(&self) -> Option<Arc<dyn device_res::ClkConsumer>> {
                self.0
                    .get_parent()
                    .map(|p| Arc::new(KclkConsumer(p)) as Arc<dyn device_res::ClkConsumer>)
            }

            fn round_rate(&self, rate: u64) -> ResResult<u64> {
                self.0.round_rate(rate).map_err(map_clk_err)
            }

            fn set_rate(&self, rate: u64) -> ResResult<u64> {
                self.0.set_rate(rate).map_err(map_clk_err)
            }

            fn set_rate_range(&self, min: u64, max: u64) -> ResResult<()> {
                self.0.set_rate_range(min, max).map_err(map_clk_err)
            }

            fn get_rate_range(&self) -> (u64, u64) {
                self.0.get_rate_range()
            }

            fn set_parent(&self, index: u8) -> ResResult<()> {
                self.0.set_parent(index).map_err(map_clk_err)
            }

            fn register_notifier(&self, cb: device_res::ClkNotifier) -> ResResult<u64> {
                let kcb: kclk::ClkNotifier = Arc::new(move |event| {
                    let mapped = match *event {
                        kclk::ClkEvent::PreRateChange { old, new } => {
                            device_res::ClkRateEvent::PreRateChange { old, new }
                        }
                        kclk::ClkEvent::PostRateChange { old, new } => {
                            device_res::ClkRateEvent::PostRateChange { old, new }
                        }
                        kclk::ClkEvent::RateChangeAborted { old, attempted } => {
                            device_res::ClkRateEvent::RateChangeAborted { old, attempted }
                        }
                    };
                    match cb(&mapped) {
                        device_res::NotifierVote::Proceed => kclk::NotifierResult::Proceed,
                        device_res::NotifierVote::Stop => kclk::NotifierResult::Stop,
                    }
                });
                Ok(self.0.register_notifier(kcb))
            }

            fn unregister_notifier(&self, id: u64) -> ResResult<()> {
                self.0.unregister_notifier(id);
                Ok(())
            }
        }

        /// Translate a kclk error into the resource vocabulary.
        fn map_clk_err(err: kclk::ClkError) -> ResError {
            match err {
                kclk::ClkError::NoProvider | kclk::ClkError::NotFound => ResError::NotFound,
                kclk::ClkError::NotSupported => ResError::Unsupported,
                kclk::ClkError::InvalidRate | kclk::ClkError::InvalidParent => {
                    ResError::InvalidResource
                }
                kclk::ClkError::Busy | kclk::ClkError::Aborted => ResError::Busy,
                kclk::ClkError::Io | kclk::ClkError::ParentError | kclk::ClkError::BadState => {
                    ResError::InvalidResource
                }
            }
        }

        let handle = match spec.name() {
            Some(name) => kclk::clk_get_by_name(name).map_err(map_clk_err)?,
            None => kclk::clk_get_by_index(spec.provider(), spec.index()).map_err(map_clk_err)?,
        };
        Ok(Arc::new(KclkConsumer(handle)))
    }
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

// ---------------------------------------------------------------------------
// End-to-end device consumption test (review option A): register a mock clock
// controller as a real kclk provider, then exercise the exact path a device
// driver takes — devm_clk_get → gating → rate queries → devfs cleanup.
// ---------------------------------------------------------------------------

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
                    flags: DividerFlags::ONE_BASED,
                }),
                Some(kclk::GateDesc {
                    offset: 0x30,
                    bit: 11,
                    flags: GateFlags::SET_TO_ENABLE,
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

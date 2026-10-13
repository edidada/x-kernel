// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bridge between the `kclk` common clock framework and the device model.
//!
//! The clock subsystem keeps its own boot-static provider registry
//! ([`kclk::register_provider`], keyed by DT phandle). This module is the
//! device-model projection of it:
//!
//! * [`ClkRegistry::get`] turns a firmware [`ClkSpec`] into a consumer
//!   handle, mapping `NoProvider` to [`ServiceError::Deferred`] so probes
//!   park instead of failing;
//! * [`ClkRegistry::register_provider`] publishes a controller and fires the
//!   wake hook, which re-probes exactly the parked consumers — the
//!   `of_clk_add_hw_provider` / `-EPROBE_DEFER` handshake without polling.
//!
//! A clock-controller driver on the new model therefore ends its probe with
//! `runtime.register_clk_provider(&lookup)`; a consumer driver just calls
//! `dev.clk("baudclk")`.

use alloc::{boxed::Box, sync::Arc};

use driver_core::{
    ClkHandle, ClkOps, ClkSpec, DepSource, RuntimeError, ServiceError, platform::PlatformBusData,
};
use kclk::{ClkError, ClkLookup};

/// Newtype adapting a [`kclk::ClkHandle`] to the device-model clock
/// operations (the orphan-rule bridge).
pub(crate) struct KclkHandle(kclk::ClkHandle);

impl ClkOps for KclkHandle {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn get_rate(&self) -> u64 {
        self.0.get_rate()
    }

    fn round_rate(&self, rate: u64) -> Result<u64, RuntimeError> {
        self.0.round_rate(rate).map_err(map_clk_error)
    }

    fn set_rate(&self, rate: u64) -> Result<u64, RuntimeError> {
        self.0.set_rate(rate).map_err(map_clk_error)
    }

    fn prepare(&self) -> Result<(), RuntimeError> {
        self.0.prepare().map_err(map_clk_error)
    }

    fn unprepare(&self) {
        let _ = self.0.unprepare();
    }

    fn enable(&self) -> Result<(), RuntimeError> {
        self.0.enable().map_err(map_clk_error)
    }

    fn disable(&self) {
        self.0.disable();
    }

    fn is_prepared(&self) -> bool {
        self.0.is_prepared().unwrap_or(false)
    }

    fn is_enabled(&self) -> bool {
        self.0.is_enabled().unwrap_or(false)
    }
}

fn map_clk_error(err: ClkError) -> RuntimeError {
    match err {
        ClkError::NoProvider | ClkError::NotFound => RuntimeError::NotFound,
        ClkError::NotSupported => RuntimeError::Unsupported,
        ClkError::InvalidRate | ClkError::InvalidParent | ClkError::Aborted => {
            RuntimeError::Invalid
        }
        ClkError::Busy => RuntimeError::Busy,
        ClkError::Io => RuntimeError::Io,
        ClkError::BadState => RuntimeError::BadState,
    }
}

/// Device-model view of the kclk provider registry: acquire with defer
/// semantics, publish with exact wakeups.
pub struct ClkRegistry {
    wake: Box<dyn Fn(DepSource) + Send + Sync>,
}

impl ClkRegistry {
    pub fn new(wake: impl Fn(DepSource) + Send + Sync + 'static) -> Self {
        Self {
            wake: Box::new(wake),
        }
    }

    /// Publish a clock controller (`of_clk_add_hw_provider` analogue) and
    /// wake every probe parked on it.
    ///
    /// The lookup must be boot-static, matching the kclk registry's
    /// "clocks live forever" model.
    pub fn register_provider(&self, provider: &'static dyn ClkLookup) {
        let provider_id = provider.provider_id();
        kclk::register_provider(provider);
        log::debug!("driver-host: clk provider {provider_id:#x} registered");
        (self.wake)(DepSource::clk(provider_id as u64));
    }

    /// Acquire a clock input, or report the dependency we are waiting for.
    pub fn get(&self, spec: ClkSpec) -> Result<ClkHandle, ServiceError> {
        let provider_id = u32::try_from(spec.provider).map_err(|_| RuntimeError::Invalid)?;
        match kclk::clk_get_by_index(provider_id, spec.index) {
            Ok(handle) => Ok(ClkHandle::new(Arc::new(KclkHandle(handle)), None)),
            Err(ClkError::NoProvider) => Err(ServiceError::Deferred(DepSource::clk(spec.provider))),
            Err(err) => Err(ServiceError::Fatal(map_clk_error(err))),
        }
    }
}

#[cfg(unittest)]
#[unittest::mod_test]
mod tests {
    use alloc::boxed::Box;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use driver_core::{
        BoundDevice, DeviceCtx, DeviceDomain, DeviceState, DriverDesc, DriverOps, ProbeError,
        ResourceKind, ResourceMap,
        desc::DeviceDescriptor,
        platform::{OfMatch, PlatformBus, PlatformId},
    };
    use kclk::{ClkBuilder, ClkTable, FixedRate, ProviderId};
    use unittest::{assert_eq, def_test};

    use super::*;

    /// The "clock controller" of this test: a static table with one
    /// fixed-rate output, standing in for a real CCU driver's probe result.
    static CCU_TABLE: ClkTable = ClkTable::new(0x55);

    struct TestCcu;

    static TEST_CCU: TestCcu = TestCcu;

    impl ClkLookup for TestCcu {
        fn provider_id(&self) -> ProviderId {
            0x55
        }

        fn get_by_index(&self, index: kclk::ClkIndex) -> Option<Arc<kclk::ClkCore>> {
            CCU_TABLE.get_by_index(index)
        }
    }

    fn install_ccu_output() {
        let osc = ClkBuilder::new("osc24m", FixedRate::new(24_000_000).into_ops())
            .build()
            .expect("fixed-rate clock builds");
        CCU_TABLE.register(0, "osc24m", osc);
    }

    /// The consumer: a serial-like driver that needs `baudclk` to probe.
    struct SerData {
        clk: ClkHandle,
    }

    struct SerDriver;

    static SER_IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("test,serial"))];

    static PROBES: AtomicUsize = AtomicUsize::new(0);

    impl DriverOps<PlatformBus> for SerDriver {
        type Data = SerData;

        fn probe(&self, dev: &mut DeviceCtx<'_, PlatformBus>) -> Result<SerData, ProbeError> {
            PROBES.fetch_add(1, Ordering::Relaxed);
            let clk = dev.clk("baudclk")?;
            clk.prepare_enable()?;
            Ok(SerData { clk })
        }

        fn remove(&self, _dev: &mut BoundDevice<'_, PlatformBus, SerData>) {}
    }

    fn serial_descriptor(provider: u64) -> DeviceDescriptor {
        let mut resources = ResourceMap::new();
        resources.push_named(
            "baudclk",
            ResourceKind::Clk(ClkSpec::by_phandle_index(provider, 0)),
        );
        DeviceDescriptor::new(
            <PlatformBus as driver_core::Bus>::ID,
            alloc::boxed::Box::new(PlatformBusData::firmware_node(
                1,
                &["test,serial"],
                driver_core::platform::FirmwareNodeInfo::EMPTY,
            )),
        )
        .with_resources(resources)
    }

    /// Full-chain acceptance: consumer probes before the controller and
    /// defers; publishing the provider wakes exactly it; the acquired rate
    /// comes from real kclk clock objects.
    #[def_test(serial)]
    fn consumer_defers_until_ccu_publishes_then_activates() {
        kclk::reset_providers();
        CCU_TABLE.clear();
        PROBES.store(0, Ordering::Relaxed);

        let domain: &'static DeviceDomain = Box::leak(Box::new(DeviceDomain::new()));
        let runtime = {
            crate::XKernelRuntime::new(move |dep| {
                domain.wake(dep);
            })
        };
        domain.set_services(runtime.clone());
        domain.register_driver(DriverDesc::new("test-serial", SER_IDS, SerDriver));

        // Controller not yet probed: the consumer must defer, not fail.
        let id = domain
            .add_device(serial_descriptor(0x55))
            .expect("add device");
        assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Deferred);
        assert_eq!(PROBES.load(Ordering::Relaxed), 1);
        assert!(!kclk::provider_installed());

        // Controller probe completes: publish + wake.
        install_ccu_output();
        runtime.register_clk_provider(&TEST_CCU);
        assert!(kclk::provider_installed());

        assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Active);
        assert_eq!(PROBES.load(Ordering::Relaxed), 2);
        let rate = domain.with_data::<SerData, _>(id, |data| data.clk.get_rate());
        assert_eq!(rate, Some(24_000_000));

        // A different missing provider still defers (exact wakeups).
        let other = domain
            .add_device(serial_descriptor(0x77))
            .expect("add device");
        assert_eq!(domain.snapshot(other).unwrap().state, DeviceState::Deferred);
    }
}

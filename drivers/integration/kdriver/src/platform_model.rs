// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ownership of the fixed boot-DT MMIO RNG devices selected for the new model.

use alloc::{boxed::Box, collections::BTreeMap, sync::Arc};

use driver_model::{
    Bus, BusInstanceId, DeviceDescriptor, DeviceRef, DmaInfo, IrqController, IrqSpec, IrqTrigger,
    MmioRegion, PlatformBus, PlatformBusData, ResourceKind, ResourceMap, RuntimeError,
    global_domain,
};
use khal::firmware::devices::{FirmwareDeviceView, InterruptControllerKind, InterruptTrigger};
use kspin::SpinNoPreempt;
use ksync::Mutex;
use lazyinit::LazyInit;

static PLATFORM_MODEL: LazyInit<Arc<PlatformModel>> = LazyInit::new();

/// Shared production owner for every public enumeration entry point.
///
/// The global domain has one native-service installation and one root bus.
/// All platform discovery holds this owner's discovery lock from ownership
/// checks through classification and reservation or legacy registration.
/// Root initialization and probing must occur after releasing that lock.
pub(crate) fn production_owner() -> Arc<PlatformModel> {
    PLATFORM_MODEL.call_once(PlatformModel::new);
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: call_once permanently published the \
                  production platform owner"
    )]
    PLATFORM_MODEL
        .get()
        .expect("platform owner initialized")
        .clone()
}

enum Slot {
    Reserved(MmioRegion),
    Registered(DeviceRef),
}

impl Slot {
    fn region(&self) -> Option<MmioRegion> {
        match self {
            Self::Reserved(region) => Some(*region),
            Self::Registered(device) => device.descriptor().resources.mmio(0),
        }
    }
}

pub(crate) struct PlatformModel {
    pub(crate) discovery: SpinNoPreempt<()>,
    root: Mutex<Option<Result<BusInstanceId, RuntimeError>>>,
    slots: SpinNoPreempt<BTreeMap<u32, Slot>>,
}

impl PlatformModel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            discovery: SpinNoPreempt::new(()),
            root: Mutex::new(None),
            slots: SpinNoPreempt::new(BTreeMap::new()),
        })
    }

    pub(crate) fn owns_range(&self, base: usize, size: usize) -> bool {
        self.slots.lock().values().any(|slot| {
            slot.region()
                .is_some_and(|region| overlaps(region, base, size))
        })
    }

    /// Initialize the production root once, outside all discovery locks.
    ///
    /// The singleton owns this bus and its native-service installation for the
    /// global domain's lifetime. Only its reserved QEMU nodes may be added to
    /// this bus; initialization failure is terminal rather than reinstalling
    /// services or registering another partially initialized root on retry.
    fn root(&self) -> Result<BusInstanceId, RuntimeError> {
        if !ktask::is_sleepable_context() || self.root.is_owned_by_current() {
            return Err(RuntimeError::BadState);
        }
        let mut root = self.root.lock();
        if let Some(result) = *root {
            return result;
        }
        let domain = global_domain();
        domain.set_services(crate::host_services::new_native_device_services());
        let instance = match domain.add_bus_instance(PlatformBus::ID, None, "firmware-platform") {
            Ok(instance) => instance,
            Err(_) => {
                *root = Some(Err(RuntimeError::BadState));
                return Err(RuntimeError::BadState);
            }
        };
        // SAFETY: this composition root routes only available QEMU Arm virt
        // modern RNG nodes, validates coherent identity DMA, installs native
        // services and reserves MMIO before registration. Rebind retains those
        // immutable descriptors; all matching production additions use this bus.
        if !unsafe { crate::driver_registry::virtio::rng_model::register(domain, instance) } {
            *root = Some(Err(RuntimeError::BadState));
            return Err(RuntimeError::BadState);
        }
        *root = Some(Ok(instance));
        Ok(instance)
    }

    pub(crate) fn stage(
        self: &Arc<Self>,
        view: &FirmwareDeviceView<'_>,
    ) -> Result<Option<StagedRng>, RuntimeError> {
        let description = view.description();
        let mmio = description.mmio.ok_or(RuntimeError::Invalid)?;
        if mmio.size < 0x100 || mmio.base.checked_add(mmio.size).is_none() {
            return Err(RuntimeError::Invalid);
        }
        let irq = description.irq.ok_or(RuntimeError::Invalid)?;
        let controller = match irq.controller {
            InterruptControllerKind::Gic => IrqController::Gic,
            InterruptControllerKind::Plic | InterruptControllerKind::Unknown => {
                return Err(RuntimeError::Invalid);
            }
        };
        let trigger = match irq.trigger {
            InterruptTrigger::EdgeRising => IrqTrigger::EdgeRising,
            InterruptTrigger::EdgeFalling => IrqTrigger::EdgeFalling,
            InterruptTrigger::LevelHigh => IrqTrigger::LevelHigh,
            InterruptTrigger::LevelLow => IrqTrigger::LevelLow,
            InterruptTrigger::Unknown(_) => return Err(RuntimeError::Invalid),
        };
        let dma = crate::firmware_dma::from_firmware_with_qemu_virt_mmio_policy(view, u64::MAX)
            .map_err(|_| RuntimeError::Invalid)?;
        if !dma.coherent || !dma.window.is_identity() {
            return Err(RuntimeError::Invalid);
        }
        let node = view.node_id();
        let compatibles: alloc::vec::Vec<_> = view.compatibles().collect();
        let bus_data = PlatformBusData::firmware_node(node, &compatibles);
        let region = MmioRegion::new(mmio.base, mmio.size);
        let mut resources = ResourceMap::new();
        resources.push_kind(ResourceKind::Mmio(region));
        resources.push_kind(ResourceKind::Irq(IrqSpec::firmware(
            controller, irq.irq, trigger,
        )));
        let mut slots = self.slots.lock();
        if slots.contains_key(&node)
            || slots.values().any(|slot| {
                slot.region()
                    .is_some_and(|owned| overlaps(owned, mmio.base, mmio.size))
            })
        {
            return Ok(None);
        }
        slots.insert(node, Slot::Reserved(region));
        Ok(Some(StagedRng {
            owner: self.clone(),
            node,
            bus_data,
            resources,
            dma,
        }))
    }
}

fn overlaps(region: MmioRegion, base: usize, size: usize) -> bool {
    region.size != 0
        && size != 0
        && region.base < base.saturating_add(size)
        && base < region.base.saturating_add(region.size)
}

pub(crate) struct StagedRng {
    owner: Arc<PlatformModel>,
    node: u32,
    bus_data: PlatformBusData,
    resources: ResourceMap,
    dma: DmaInfo,
}

impl StagedRng {
    pub(crate) fn register(self) -> Result<(), RuntimeError> {
        let instance = self.owner.root()?;
        let descriptor = DeviceDescriptor::builder()
            .bus_instance(instance)
            .bus_data(Box::new(self.bus_data.clone()))
            .resources(self.resources.clone())
            .dma(self.dma)
            .build();
        let device = global_domain()
            .add_device(descriptor)
            .map_err(|_| RuntimeError::BadState)?;
        self.owner
            .slots
            .lock()
            .insert(self.node, Slot::Registered(device));
        Ok(())
    }
}

impl Drop for StagedRng {
    fn drop(&mut self) {
        let mut slots = self.owner.slots.lock();
        if matches!(slots.get(&self.node), Some(Slot::Reserved(_))) {
            slots.remove(&self.node);
        }
    }
}

unittest::path_mod_test!(tests, "../ut-tests/platform_model.rs");

unittest::path_mod_test!(lifecycle_tests, "../ut-tests/model_mmio_lifecycle.rs");

unittest::path_mod_test!(pending_tests, "../ut-tests/model_mmio_pending.rs");

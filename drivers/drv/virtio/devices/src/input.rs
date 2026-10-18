// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO input driver adapter.
use alloc::{string::String, sync::Arc};

use kerrno::{KError, KResult};
use kspin::SpinNoIrq;
use subsys_input::{Event, EventType, InputDevice, InputDeviceId, InputEventNotifier};
use virtio_drivers::{
    Hal,
    device::input::{InputConfigSelect, VirtIOInput as InnerDev},
    transport::{InterruptStatus, Transport},
};

use crate::as_driver_error;

/// The VirtIO Input device driver.
///
/// Wraps `VirtIOInput` from `virtio-drivers` and implements the
/// [`InputDevice`] trait, providing event reading and capability query
/// for virtual input devices (keyboard, mouse, etc.).
///
/// # Type Parameters
///
/// - `H` - VirtIO HAL implementation for DMA allocation.
/// - `T` - Transport layer (MMIO or PCI).
///
/// # Example
///
/// ```ignore
/// let (kind, transport) = virtio::probe_pci_device::<HalImpl, _>(...).unwrap();
/// let input = VirtIoInputDev::<HalImpl, _>::try_new(transport, irq)?;
/// // the registration closure a driver passes to Device::request_irq:
/// // move || if input.ack_irq(virq) { IrqAction::Handled } else { IrqAction::NotHandled }
/// let event = input.read_event()?;
/// ```
pub struct VirtIoInputDev<H: Hal, T: Transport> {
    /// MMIO register lease: the mapping stays mapped as long as ANY
    /// reference to this device object (class publication, consumer clones)
    /// lives — the last drop releases the mapping (Linux: pci_release_regions
    /// runs after the last put_device).
    _mmio_regs: Option<driver_base::IoMap>,
    state: Arc<VirtIoInputState<H, T>>,
    device_id: InputDeviceId,
    name: String,
    irq: usize,
}

struct VirtIoInputState<H: Hal, T: Transport> {
    inner: SpinNoIrq<InnerDev<H, T>>,
    notifier: SpinNoIrq<Option<Arc<dyn InputEventNotifier>>>,
}

// SAFETY: moving the state does not move its DMA allocations or MMIO regions,
// and the queue pointers continue to target those stable allocations. The
// transport has no CPU-local ownership, and access after the move still goes
// through `SpinNoIrq`.
unsafe impl<H: Hal, T: Transport> Send for VirtIoInputState<H, T> {}
// SAFETY: shared references cannot access `InnerDev` without taking the same
// IRQ-disabling spin lock, including from the hard-IRQ acknowledgement path.
unsafe impl<H: Hal, T: Transport> Sync for VirtIoInputState<H, T> {}

impl<H: Hal, T: Transport> VirtIoInputDev<H, T> {
    /// Creates an IRQ-backed driver instance and reads its identity.
    ///
    /// # Errors
    ///
    /// Returns `KError` if device initialization or ID query fails.
    pub fn try_new(
        transport: T,
        irq: usize,
        mmio_regs: Option<driver_base::IoMap>,
    ) -> KResult<Self> {
        let mut device = InnerDev::new(transport).map_err(as_driver_error)?;
        let name = device.name().unwrap_or_else(|_| String::from("<unknown>"));
        let name = String::from(name.trim_end_matches('\0'));
        let ids = device.ids().map_err(as_driver_error)?;

        Ok(Self {
            _mmio_regs: mmio_regs,
            state: Arc::new(VirtIoInputState {
                inner: SpinNoIrq::new(device),
                notifier: SpinNoIrq::new(None),
            }),
            device_id: InputDeviceId {
                bus_type: ids.bustype,
                vendor: ids.vendor,
                product: ids.product,
                version: ids.version,
            },
            name,
            irq,
        })
    }

    /// Acknowledge one interrupt and report whether it was ours.
    ///
    /// The family-side half of `Device::request_irq` — see
    /// `VirtIoBlkDev::ack_irq` for why this is an inherent method and not a
    /// host registration trait.
    ///
    /// Unlike blk/rng, this one **does** filter on `irq`: virtio-input shares
    /// its line, so a handler that acked someone else's interrupt would steal
    /// it. `irq` is the LOGICAL virq the host dispatches (resolve a firmware
    /// route with `Device::resolve_irq`, never by reading its raw fields).
    ///
    /// Runs in hard-IRQ context: shares the transport lock with task-context
    /// event reads, and neither consumes events nor allocates.
    pub fn ack_irq(&self, irq: usize) -> bool {
        if irq != self.irq {
            return false;
        }
        let status = self.state.inner.lock().ack_interrupt();
        if status.is_empty() {
            return false;
        }
        if status.contains(InterruptStatus::QUEUE_INTERRUPT) {
            let notifier = self.state.notifier.lock().clone();
            if let Some(notifier) = notifier {
                notifier.notify_events();
            }
        }
        true
    }

    fn load_event_bits(&self, event_type: EventType, out: &mut [u8]) -> KResult<bool> {
        let written = self
            .state
            .inner
            .lock()
            .query_config_select(InputConfigSelect::EvBits, event_type as u8, out)
            .map_err(as_driver_error)?;
        Ok(written != 0)
    }
}

impl<H: Hal, T: Transport> VirtIoInputDev<H, T> {
    /// The input product name (from the virtio device IDs) — the same
    /// string the class publication and the devnode uevent carry.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The device's IRQ line (the ack protocol's source of truth).
    pub fn irq(&self) -> Option<usize> {
        Some(self.irq)
    }
}

impl<H: Hal, T: Transport> InputDevice for VirtIoInputDev<H, T> {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> InputDeviceId {
        self.device_id
    }

    fn physical_location(&self) -> &str {
        "virtio0/input0"
    }

    fn unique_id(&self) -> &str {
        "virtio"
    }

    fn get_event_bits(&self, ty: EventType, out: &mut [u8]) -> KResult<bool> {
        self.load_event_bits(ty, out)
    }

    fn read_event(&self) -> KResult<Event> {
        let Some(event) = self.state.inner.lock().pop_pending_event() else {
            return Err(KError::WouldBlock);
        };
        Ok(Event {
            event_type: event.event_type,
            code: event.code,
            value: event.value,
        })
    }

    fn set_event_notifier(&self, notifier: Option<Arc<dyn InputEventNotifier>>) -> KResult {
        *self.state.notifier.lock() = notifier;
        Ok(())
    }
}

#[unittest::mod_test]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::{assert, assert_eq, def_test};
    use virtio_drivers::transport::{DeviceType, InterruptStatus};

    use super::*;
    use crate::mock_virtio::{MockHal, MockTransport};

    struct CountingNotifier(AtomicUsize);

    impl InputEventNotifier for CountingNotifier {
        fn notify_events(&self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[def_test]
    fn test_virtio_input_init_failure() {
        let mut transport = MockTransport::new();
        transport.device_type = DeviceType::Input;
        let dev = VirtIoInputDev::<MockHal, MockTransport>::try_new(transport, 11, None);
        assert!(dev.is_err());
    }

    #[def_test]
    fn test_virtio_input_traits() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<VirtIoInputDev<MockHal, MockTransport>>();
    }

    fn input_transport() -> MockTransport {
        let transport = MockTransport::new_with_type(DeviceType::Input);
        let mut config = transport.config_space.borrow_mut();
        config[2] = 8;
        config[8..16].copy_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0]);
        drop(config);
        transport
    }

    #[def_test]
    fn test_virtio_input_irq_reports_line_and_ignores_empty_isr() {
        let transport = input_transport();
        let ack_count = transport.interrupt_ack_count.clone();
        let device =
            VirtIoInputDev::<MockHal, MockTransport>::try_new(transport, 11, None).unwrap();

        assert_eq!(device.irq(), Some(11));
        assert!(!device.ack_irq(11));
        assert_eq!(ack_count.load(Ordering::Relaxed), 1);
    }

    #[def_test]
    fn test_virtio_input_irq_handles_queue_and_config_isr() {
        let transport = input_transport();
        let ack_count = transport.interrupt_ack_count.clone();
        let interrupt_status = transport.interrupt_status.clone();
        let device =
            VirtIoInputDev::<MockHal, MockTransport>::try_new(transport, 11, None).unwrap();

        for status in [
            InterruptStatus::QUEUE_INTERRUPT,
            InterruptStatus::DEVICE_CONFIGURATION_INTERRUPT,
        ] {
            interrupt_status.store(status.bits() as usize, Ordering::Release);
            assert!(device.ack_irq(11));
        }
        assert_eq!(ack_count.load(Ordering::Relaxed), 2);
    }

    #[def_test]
    fn test_virtio_input_notifies_only_for_event_queue_interrupts() {
        let transport = input_transport();
        let interrupt_status = transport.interrupt_status.clone();
        let device =
            VirtIoInputDev::<MockHal, MockTransport>::try_new(transport, 11, None).unwrap();
        let notifier = Arc::new(CountingNotifier(AtomicUsize::new(0)));
        device.set_event_notifier(Some(notifier.clone())).unwrap();

        interrupt_status.store(
            InterruptStatus::DEVICE_CONFIGURATION_INTERRUPT.bits() as usize,
            Ordering::Release,
        );
        assert!(device.ack_irq(11));
        assert_eq!(notifier.0.load(Ordering::Relaxed), 0);

        interrupt_status.store(
            InterruptStatus::QUEUE_INTERRUPT.bits() as usize,
            Ordering::Release,
        );
        assert!(device.ack_irq(11));
        assert_eq!(notifier.0.load(Ordering::Relaxed), 1);
    }

    #[def_test]
    fn test_virtio_input_irq_rejects_other_line_without_ack() {
        let transport = input_transport();
        let ack_count = transport.interrupt_ack_count.clone();
        let interrupt_status = transport.interrupt_status.clone();
        let device =
            VirtIoInputDev::<MockHal, MockTransport>::try_new(transport, 11, None).unwrap();
        interrupt_status.store(
            InterruptStatus::QUEUE_INTERRUPT.bits() as usize,
            Ordering::Release,
        );

        assert!(!device.ack_irq(12));
        assert_eq!(ack_count.load(Ordering::Relaxed), 0);
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO input driver adapter.
use alloc::{collections::BTreeSet, string::String, sync::Arc, vec::Vec};

use device_res::{Irq, IrqEvent, IrqResource, IrqTrigger};
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use input::{Event, EventType, InputDevice, InputDeviceId};
use kspin::SpinNoIrq;
use log::info;
use virtio_drivers::{
    Hal,
    device::input::{InputConfigSelect, VirtIOInput as InnerDev},
    transport::Transport,
};

use crate::as_driver_error;

type EventCallback = Arc<dyn Fn() + Send + Sync>;
type EventCallbackSlot = Arc<SpinNoIrq<Option<EventCallback>>>;

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
/// let mut input = VirtIoInputDev::<HalImpl, _>::try_new(transport)?;
/// let event = input.read_event()?;
/// ```
pub struct VirtIoInputDev<H: Hal, T: Transport> {
    inner: Arc<SpinNoIrq<InnerDev<H, T>>>,
    event_callback: EventCallbackSlot,
    device_id: InputDeviceId,
    name: String,
}

trait InputIrqHandle: Send + Sync {
    fn irq(&self) -> usize;
    fn ack(&self) -> bool;
}

struct InputIrqHandleImpl<H: Hal, T: Transport> {
    irq: usize,
    inner: Arc<SpinNoIrq<InnerDev<H, T>>>,
    event_callback: EventCallbackSlot,
}

// SAFETY: InnerDev is accessed only through SpinNoIrq and the handle is kept
// alive for as long as the corresponding IRQ registration is active.
unsafe impl<H: Hal, T: Transport> Send for InputIrqHandleImpl<H, T> {}
// SAFETY: all shared device access is serialized by SpinNoIrq.
unsafe impl<H: Hal, T: Transport> Sync for InputIrqHandleImpl<H, T> {}

impl<H: Hal, T: Transport> InputIrqHandle for InputIrqHandleImpl<H, T> {
    fn irq(&self) -> usize {
        self.irq
    }

    fn ack(&self) -> bool {
        let handled = !self.inner.lock().ack_interrupt().is_empty();
        if handled && let Some(callback) = self.event_callback.lock().clone() {
            callback();
        }
        handled
    }
}

static INPUT_IRQ_HANDLES: SpinNoIrq<Vec<Arc<dyn InputIrqHandle>>> = SpinNoIrq::new(Vec::new());
static INPUT_IRQS: SpinNoIrq<BTreeSet<usize>> = SpinNoIrq::new(BTreeSet::new());
static INPUT_IRQ_GUARDS: SpinNoIrq<Vec<Irq>> = SpinNoIrq::new(Vec::new());

fn handle_input_irq(irq: usize) -> IrqEvent {
    // PCI INTx can be shared by multiple virtio-input functions. Do not use
    // Iterator::any here: its short-circuiting would leave later devices'
    // interrupt status asserted and cause an interrupt storm.
    let mut handled = false;
    for handle in INPUT_IRQ_HANDLES
        .lock()
        .iter()
        .filter(|handle| handle.irq() == irq)
    {
        handled |= handle.ack();
    }
    if handled {
        IrqEvent::from_sources(1)
    } else {
        IrqEvent::NOT_HANDLED
    }
}

fn register_input_irq<H: Hal + 'static, T: Transport + 'static>(
    irq: usize,
    inner: &Arc<SpinNoIrq<InnerDev<H, T>>>,
    event_callback: &EventCallbackSlot,
) -> DriverResult<()> {
    INPUT_IRQ_HANDLES
        .lock()
        .push(Arc::new(InputIrqHandleImpl::<H, T> {
            irq,
            inner: inner.clone(),
            event_callback: event_callback.clone(),
        }));
    if INPUT_IRQS.lock().insert(irq) {
        let guard = Irq::request(
            IrqResource::new(irq, IrqTrigger::Unknown(0)),
            Arc::new(handle_input_irq),
        )
        .map_err(|_| DriverError::ResourceBusy)?;
        INPUT_IRQ_GUARDS.lock().push(guard);
    }
    Ok(())
}

// SAFETY: VirtIoInputDev serializes access to the inner VirtIOInput through its
// own `SpinNoIrq` lock. The inner type is not auto Send/Sync due to
// PhantomData, but it is safe to transfer across threads and share behind that
// lock.
unsafe impl<H: Hal, T: Transport> Send for VirtIoInputDev<H, T> {}
// SAFETY: shared references to VirtIoInputDev still funnel all device access
// through the same `SpinNoIrq` lock, preserving synchronized access.
unsafe impl<H: Hal, T: Transport> Sync for VirtIoInputDev<H, T> {}

impl<H: Hal + 'static, T: Transport + 'static> VirtIoInputDev<H, T> {
    /// Creates a new driver instance, reads the device name and IDs, and
    /// initializes the VirtIO input device.
    ///
    /// # Errors
    ///
    /// Returns `DriverError` if device initialization or ID query fails.
    pub fn try_new(transport: T) -> DriverResult<Self> {
        Self::try_new_with_irq(transport, None)
    }

    /// Creates an input device while keeping its shared legacy interrupt
    /// masked until the virtqueue and its real handler are both ready.
    pub fn try_new_with_irq(transport: T, irq: Option<usize>) -> DriverResult<Self> {
        info!("virtio-input: starting initialization on IRQ {irq:?}");
        let has_registered_irq = irq.is_some_and(|irq| INPUT_IRQS.lock().contains(&irq));
        if has_registered_irq {
            let irq = irq.expect("has_registered_irq requires an IRQ");
            for guard in INPUT_IRQ_GUARDS
                .lock()
                .iter()
                .filter(|guard| guard.number() == irq)
            {
                guard.set_enabled(false);
            }
        }
        let irq_mask = if let Some(irq) = irq.filter(|_| !has_registered_irq) {
            let guard = Irq::request(
                IrqResource::new(irq, IrqTrigger::Unknown(0)),
                Arc::new(|_| IrqEvent::from_sources(1)),
            )
            .map_err(|_| DriverError::ResourceBusy)?;
            guard.set_enabled(false);
            Some(guard)
        } else {
            None
        };
        info!("virtio-input: IRQ masked for initialization on IRQ {irq:?}");
        let mut device = InnerDev::new(transport).map_err(as_driver_error)?;
        info!("virtio-input: virtqueues initialized on IRQ {irq:?}");
        let name = device.name().unwrap_or_else(|_| String::from("<unknown>"));
        let ids = device.ids().map_err(as_driver_error)?;

        let inner = Arc::new(SpinNoIrq::new(device));
        let input = Self {
            inner,
            event_callback: Arc::new(SpinNoIrq::new(None)),
            device_id: InputDeviceId {
                bus_type: ids.bustype,
                vendor: ids.vendor,
                product: ids.product,
                version: ids.version,
            },
            name,
        };
        // Release the temporary masking registration before installing the
        // persistent handler. Some IRQ providers tear down the line state
        // when a registration is released, so dropping it afterwards would
        // also make the newly installed handler unreachable.
        drop(irq_mask);
        input.register_irq(irq)?;
        info!("virtio-input: handler registered on IRQ {irq:?}");
        if let Some(irq) = irq {
            for guard in INPUT_IRQ_GUARDS
                .lock()
                .iter()
                .filter(|guard| guard.number() == irq)
            {
                guard.set_enabled(true);
            }
        }
        info!("virtio-input: initialization complete on IRQ {irq:?}");
        Ok(input)
    }

    /// Register the transport interrupt after construction when a PCI route
    /// is available. Input devices otherwise leave the virtqueue interrupt
    /// asserted with no handler, causing an interrupt storm.
    pub fn register_irq(&self, irq: Option<usize>) -> DriverResult<()> {
        if let Some(irq) = irq {
            register_input_irq(irq, &self.inner, &self.event_callback)?;
        }
        Ok(())
    }

    fn load_event_bits(&self, event_type: EventType, out: &mut [u8]) -> DriverResult<bool> {
        let written = self
            .inner
            .lock()
            .query_config_select(InputConfigSelect::EvBits, event_type as u8, out)
            .map_err(as_driver_error)?;
        Ok(written != 0)
    }
}

impl<H: Hal, T: Transport> Device for VirtIoInputDev<H, T> {
    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Input
    }
}

impl<H: Hal + 'static, T: Transport + 'static> InputDevice for VirtIoInputDev<H, T> {
    fn set_event_callback(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        *self.event_callback.lock() = Some(callback);
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

    fn get_event_bits(&self, ty: EventType, out: &mut [u8]) -> DriverResult<bool> {
        self.load_event_bits(ty, out)
    }

    fn read_event(&self) -> DriverResult<Event> {
        let Some(event) = self.inner.lock().pop_pending_event() else {
            return Err(DriverError::WouldBlock);
        };
        Ok(Event {
            event_type: event.event_type,
            code: event.code,
            value: event.value,
        })
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, def_test};

    use super::*;
    use crate::mock_virtio::{MockHal, MockTransport};

    #[def_test]
    fn test_virtio_input_init_failure() {
        let mut transport = MockTransport::new();
        transport.device_type = virtio_drivers::transport::DeviceType::Input;
        let dev = VirtIoInputDev::<MockHal, MockTransport>::try_new(transport);
        assert!(dev.is_err());
    }

    #[def_test]
    fn test_virtio_input_traits() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<VirtIoInputDev<MockHal, MockTransport>>();
    }
}

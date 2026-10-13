// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Virtio device-family constructors over the vendored `virtio-drivers`
//! transport, implementing the class contracts (`block`, `net`, `char`, ...).
//! Shared by the `drv/virtio` driver modules and the `driver-host` HAL.
//!
//! Like the [`virtio-drivers`] crate, you must implement the [`VirtIoHal`]
//! trait (alias of `virtio_drivers::Hal`), to allocate DMA regions and
//! translate between physical addresses (as seen by devices) and virtual
//! addresses (as seen by your program).
//!
//! # Interrupts
//!
//! Each device type exposes an inherent `ack_irq(irq) -> bool` (net exposes
//! `VirtIoNetIrqWorker::service() -> bool`). Those are the family-side half
//! of an interrupt: **the driver** registers it through
//! `DeviceCtx::request_irq` and maps the bool to `driver_core::IrqAction` in
//! its closure. Nothing in this crate implements a host registration trait or
//! depends on the host IRQ core — that is what keeps the `DevIrq` guard in
//! the driver's `Data`, where the rollback order is expressible.
//!
//! [`virtio-drivers`]: https://docs.rs/virtio-drivers/latest/virtio_drivers/

#![allow(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: case (b): legacy virtio has 14 diagnostic \
              sites, too many to judge individually in this migration; new code remains reviewed"
)]
#![no_std]
#![allow(
    clippy::unwrap_used,
    reason = "CG-021-no-panic-on-reachable-failure: case (b): 11 legacy diagnostic sites need \
              individual failure-contract review; new code remains subject to the \
              no-legitimate-failure-panic rule"
)]
#![cfg_attr(doc, feature(doc_cfg))]

#[cfg(feature = "alloc")]
extern crate alloc;
#[cfg(feature = "net")]
extern crate net as driver_net;

#[cfg(feature = "block")]
mod blk;
#[cfg(feature = "block")]
pub use self::blk::{PART_BITS as VIRTIO_BLK_PART_BITS, VIRTIO_BLK_MAJOR, VirtIoBlkDev};

#[cfg(feature = "gpu")]
mod gpu;
#[cfg(feature = "gpu")]
pub use self::gpu::VirtIoGpuDev;

#[cfg(feature = "input")]
mod input;
#[cfg(feature = "input")]
pub use self::input::VirtIoInputDev;

#[cfg(feature = "net")]
mod net;
#[cfg(feature = "net")]
pub use self::net::{VirtIoNetDev, VirtIoNetIrqWorker};

#[cfg(feature = "rng")]
mod rng;
#[cfg(feature = "rng")]
pub use self::rng::VirtIoRngDev;

mod pci;
pub use self::pci::{PciTransport, probe_pci_transport_only};

#[cfg(feature = "virtio-9p")]
mod virtio_9p;
#[cfg(feature = "virtio-9p")]
pub use self::virtio_9p::{VirtIo9pDev, Virtio9pDevice};

#[cfg(unittest)]
pub mod mock_virtio;
#[cfg(feature = "socket")]
mod vsock;
use core::{fmt::Debug, ops::BitAnd};

use bitflags::Flags;
use kerrno::KError;
use virtio_drivers::transport::DeviceType as VirtIoDevType;
pub use virtio_drivers::{
    BufferDirection, Hal as VirtIoHal, PhysAddr,
    transport::{DeviceStatus, InterruptStatus, Transport, mmio::MmioTransport},
};

#[cfg(feature = "socket")]
pub use self::vsock::VirtIoVsockDev;

/// Try to probe a VirtIO MMIO device from the given memory region.
///
/// If the device is recognized, returns the device type and a transport object
/// for later operations. Otherwise, returns [`None`].
///
/// # Arguments
///
/// - `reg_base` - Pointer to the MMIO register base of the device.
/// - `reg_size` - Size of the MMIO register region in bytes.
///
/// # Returns
///
/// `Some((VirtioDeviceKind, MmioTransport))` if the device is a recognized VirtIO
/// device, or `None` if the region does not contain a valid VirtIO header.
///
/// # Safety
///
/// The caller must ensure `reg_base` points to a valid VirtIO MMIO register
/// region of at least `reg_size` bytes, and that the memory remains valid
/// for the lifetime of the returned `MmioTransport`.
///
/// # Panics
///
/// Panics if `reg_base` is null.
/// `MmioTransport` with the `Send` the vendored crate deliberately leaves
/// out (it only declares `Sync`).
///
/// # SAFETY
///
/// The vendored `unsafe impl Sync` already asserts cross-thread *sharing*
/// of MMIO access is sound; `Send` (moving the handle between threads) is
/// the strictly weaker claim. The wrapped transport's MMIO window is
/// process-global hardware memory — moving the handle aliases nothing the
/// `Sync` impl does not already permit, and every device serializes its own
/// access. Only valid for the `'static` form returned by
/// [`probe_mmio_device`], whose mapping the caller guarantees for the
/// device lifetime.
pub struct SendMmioTransport(pub(crate) MmioTransport<'static>);

// SAFETY: see the type-level comment.
unsafe impl Send for SendMmioTransport {}

impl Transport for SendMmioTransport {
    fn device_type(&self) -> self::VirtIoDevType {
        self.0.device_type()
    }

    fn read_device_features(&mut self) -> u64 {
        self.0.read_device_features()
    }

    fn write_driver_features(&mut self, driver_features: u64) {
        self.0.write_driver_features(driver_features)
    }

    fn max_queue_size(&mut self, queue: u16) -> u32 {
        self.0.max_queue_size(queue)
    }

    fn notify(&mut self, queue: u16) {
        self.0.notify(queue)
    }

    fn get_status(&self) -> DeviceStatus {
        self.0.get_status()
    }

    fn set_status(&mut self, status: DeviceStatus) {
        self.0.set_status(status)
    }

    fn set_guest_page_size(&mut self, guest_page_size: u32) {
        self.0.set_guest_page_size(guest_page_size)
    }

    fn requires_legacy_layout(&self) -> bool {
        self.0.requires_legacy_layout()
    }

    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: PhysAddr,
        driver_area: PhysAddr,
        device_area: PhysAddr,
    ) {
        self.0
            .queue_set(queue, size, descriptors, driver_area, device_area)
    }

    fn queue_unset(&mut self, queue: u16) {
        self.0.queue_unset(queue)
    }

    fn queue_used(&mut self, queue: u16) -> bool {
        self.0.queue_used(queue)
    }

    fn ack_interrupt(&mut self) -> InterruptStatus {
        self.0.ack_interrupt()
    }

    fn begin_init<F: Flags<Bits = u64> + BitAnd<Output = F> + Debug>(&mut self, features: F) -> F {
        self.0.begin_init(features)
    }

    fn finish_init(&mut self) {
        self.0.finish_init()
    }

    fn read_config_generation(&self) -> u32 {
        self.0.read_config_generation()
    }

    fn read_config_space<T: zerocopy::FromBytes + zerocopy::IntoBytes>(
        &self,
        offset: usize,
    ) -> virtio_drivers::Result<T> {
        self.0.read_config_space(offset)
    }

    fn write_config_space<T: zerocopy::IntoBytes + zerocopy::Immutable>(
        &mut self,
        offset: usize,
        value: T,
    ) -> virtio_drivers::Result<()> {
        self.0.write_config_space(offset, value)
    }
}

/// Probe a  register window: validate the header magic and
/// version, read the device type, and build a transport over it.
///
/// # Panics
///
/// Panics if  is null.
///
/// # Safety
///
/// `reg_base` must point at a valid virtio-mmio register window of at
/// least `reg_size` bytes, mapped for the lifetime of the returned
/// transport (the `'static` in the return type is the caller's promise).
pub unsafe fn probe_mmio_device(
    reg_base: *mut u8,
    reg_size: usize,
) -> Option<(VirtioDeviceKind, SendMmioTransport)> {
    use core::ptr::NonNull;

    use virtio_drivers::transport::mmio::VirtIOHeader;

    let header = NonNull::new(reg_base as *mut VirtIOHeader).unwrap();
    // SAFETY: The caller guarantees `reg_base` points to a valid MMIO region
    // of at least `reg_size` bytes. `MmioTransport::new` only reads the
    // header and validates the magic number and version.
    let transport = SendMmioTransport(unsafe { MmioTransport::new(header, reg_size) }.ok()?);
    let dev_kind = as_device_kind(transport.device_type())?;
    Some((dev_kind, transport))
}

/// The virtio family's own device-kind vocabulary: what the probe
/// discovers and the family dispatcher routes on. It belongs here, with
/// the family — not to a global device vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtioDeviceKind {
    Block,
    Net,
    Hwrng,
    Display,
    Input,
    Vsock,
    Fs9p,
}

/// Map a virtio transport device type onto the family kind. Returns
/// `None` for console and other kinds the family does not carry.
pub const fn as_device_kind(t: VirtIoDevType) -> Option<VirtioDeviceKind> {
    use VirtIoDevType::*;
    match t {
        Block => Some(VirtioDeviceKind::Block),
        Network => Some(VirtioDeviceKind::Net),
        EntropySource => Some(VirtioDeviceKind::Hwrng),
        GPU => Some(VirtioDeviceKind::Display),
        Input => Some(VirtioDeviceKind::Input),
        Socket => Some(VirtioDeviceKind::Vsock),
        _9P => Some(VirtioDeviceKind::Fs9p),
        _ => None,
    }
}

/// Enabled exactly when at least one VirtIO device backend module is
/// compiled — the only consumers of the conversion.
#[cfg(any(
    feature = "block",
    feature = "gpu",
    feature = "input",
    feature = "net",
    feature = "rng",
    feature = "socket",
    feature = "virtio-9p",
))]
pub(crate) const fn as_driver_error(e: virtio_drivers::Error) -> KError {
    use virtio_drivers::{Error::*, device::socket::SocketError::*};
    match e {
        QueueFull => KError::BadState,
        NotReady => KError::WouldBlock,
        WrongToken => KError::BadState,
        AlreadyUsed => KError::AlreadyExists,
        InvalidParam => KError::InvalidInput,
        DmaError => KError::NoMemory,
        IoError => KError::Io,
        Unsupported => KError::Unsupported,
        ConfigSpaceTooSmall => KError::BadState,
        ConfigSpaceMissing => KError::BadState,
        SocketDeviceError(e) => match e {
            ConnectionExists => KError::AlreadyExists,
            NotConnected => KError::BadState,
            InvalidOperation | InvalidNumber | UnknownOperation(_) => KError::InvalidInput,
            OutputBufferTooShort(_) | BufferTooShort | BufferTooLong(..) => KError::InvalidInput,
            UnexpectedDataInPacket | PeerSocketShutdown => KError::Io,
            InsufficientBufferSpaceInPeer => KError::WouldBlock,
            RecycledWrongBuffer => KError::BadState,
        },
    }
}

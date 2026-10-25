// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Wrappers of some devices in the [`virtio-drivers`] crate, that implement
//! traits in the [`driver_base`] series crates.
//!
//! Like the [`virtio-drivers`] crate, you must implement the [`VirtIoHal`]
//! trait (alias of `virtio_drivers::Hal`), to allocate DMA regions and
//! translate between physical addresses (as seen by devices) and virtual
//! addresses (as seen by your program).
//! RNG additionally accepts a per-instance [`DmaBackend`] for queue and request
//! memory. Its default [`StaticHal`] preserves existing HAL-based callers.
//!
//! [`virtio-drivers`]: https://docs.rs/virtio-drivers/latest/virtio_drivers/
//! [`driver_base`]: driver_base

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
pub use self::net::VirtIoNetDev;

#[cfg(feature = "rng")]
mod device_dma;
#[cfg(feature = "rng")]
mod rng;
#[cfg(feature = "rng")]
pub use self::device_dma::DeviceDmaBackend;
#[cfg(feature = "rng")]
pub use self::rng::VirtIoRngDev;

mod pci;
pub use self::pci::{PciTransport, probe_pci_device};

#[cfg(feature = "virtio-9p")]
mod virtio_9p;
#[cfg(feature = "virtio-9p")]
pub use self::virtio_9p::{VirtIo9pDev, Virtio9pDevice};

#[cfg(unittest)]
pub mod mock_virtio;
#[cfg(feature = "socket")]
mod socket;
use driver_base::{DeviceKind, DriverError};
use virtio_drivers::transport::DeviceType as VirtIoDevType;
pub use virtio_drivers::{
    BufferDirection, DmaBackend, Hal as VirtIoHal, PhysAddr, StaticHal,
    transport::{
        Transport,
        mmio::{MmioTransport, MmioVersion},
    },
};

/// Classify a mapped VirtIO MMIO slot without constructing a transport.
///
/// Reads only magic, device ID and version. No register is written and no
/// transport destructor runs. Returns None for null/misaligned pointers,
/// short/overflowing extents, invalid identification, empty slots or device
/// types not supported by this wrapper. The result owns no mapping or device.
///
/// # Safety
/// For an extent that passes validation, `reg_base` must refer to a live,
/// readable MMIO mapping covering `reg_size` bytes for this call. Identification
/// registers must permit side-effect-free aligned u32 reads. For ordinary
/// memory fixtures, no concurrent mutation or conflicting references may exist.
/// Use this API before creating a transport: no live `UniqueMmioPointer`,
/// `MmioTransport` or other exclusive owner may cover any accessed register,
/// including through another mapping of the same physical region. Raw reads
/// do not bypass an existing owner's exclusivity; do not rescan an active
/// transport. The caller must exclude concurrent acquisition of such ownership.
pub unsafe fn classify_mmio_device(
    reg_base: *mut u8,
    reg_size: usize,
) -> Option<(DeviceKind, MmioVersion)> {
    use core::{
        mem::{align_of, size_of},
        ptr::NonNull,
    };

    use virtio_drivers::transport::mmio::VirtIOHeader;

    let header = NonNull::new(reg_base.cast::<VirtIOHeader>())?;
    if !(reg_base as usize).is_multiple_of(align_of::<VirtIOHeader>())
        || reg_size < size_of::<VirtIOHeader>()
        || reg_size > isize::MAX as usize
        || (reg_base as usize).checked_add(reg_size).is_none()
    {
        return None;
    }
    // SAFETY: extent and alignment are checked above; the caller retains the
    // readable mapping and excludes live or concurrently acquired exclusive
    // owners. The vendor helper reads only identification fields.
    let (device_type, version) = unsafe { VirtIOHeader::read_device_info(header) }.ok()?;
    Some((as_device_kind(device_type)?, version))
}

unittest::path_mod_test!(
    mmio_classification_tests,
    "../ut-tests/mmio_classification.rs"
);

#[cfg(feature = "socket")]
pub use self::socket::VirtIoVsockDev;

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
/// `Some((DeviceKind, MmioTransport))` if the device is a recognized VirtIO
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
pub unsafe fn probe_mmio_device(
    reg_base: *mut u8,
    reg_size: usize,
) -> Option<(DeviceKind, MmioTransport<'static>)> {
    use core::ptr::NonNull;

    use virtio_drivers::transport::mmio::VirtIOHeader;

    let header = NonNull::new(reg_base as *mut VirtIOHeader).unwrap();
    // SAFETY: The caller guarantees `reg_base` points to a valid MMIO region
    // of at least `reg_size` bytes. `MmioTransport::new` only reads the
    // header and validates the magic number and version.
    let transport = unsafe { MmioTransport::new(header, reg_size) }.ok()?;
    let dev_kind = as_device_kind(transport.device_type())?;
    Some((dev_kind, transport))
}

const fn as_device_kind(t: VirtIoDevType) -> Option<DeviceKind> {
    use VirtIoDevType::*;
    match t {
        Block => Some(DeviceKind::Block),
        Network => Some(DeviceKind::Net),
        EntropySource => Some(DeviceKind::Hwrng),
        GPU => Some(DeviceKind::Display),
        Input => Some(DeviceKind::Input),
        Socket => Some(DeviceKind::Vsock),
        _9P => Some(DeviceKind::Fs9p),
        _ => None,
    }
}

#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Error conversion is used only by enabled VirtIO device \
              backends"
)]
#[allow(
    dead_code,
    reason = "CG-024-lint-exceptions: dead_code: Error conversion is used only by enabled VirtIO \
              device backends"
)]
pub(crate) const fn as_driver_error(e: virtio_drivers::Error) -> DriverError {
    use virtio_drivers::{Error::*, device::socket::SocketError::*};
    match e {
        QueueFull => DriverError::BadState,
        NotReady => DriverError::WouldBlock,
        WrongToken => DriverError::BadState,
        AlreadyUsed => DriverError::AlreadyExists,
        InvalidParam => DriverError::InvalidInput,
        DmaError => DriverError::NoMemory,
        IoError => DriverError::Io,
        Unsupported => DriverError::Unsupported,
        ConfigSpaceTooSmall => DriverError::BadState,
        ConfigSpaceMissing => DriverError::BadState,
        SocketDeviceError(e) => match e {
            ConnectionExists => DriverError::AlreadyExists,
            NotConnected => DriverError::BadState,
            InvalidOperation | InvalidNumber | UnknownOperation(_) => DriverError::InvalidInput,
            OutputBufferTooShort(_) | BufferTooShort | BufferTooLong(..) => {
                DriverError::InvalidInput
            }
            UnexpectedDataInPacket | PeerSocketShutdown => DriverError::Io,
            InsufficientBufferSpaceInPeer => DriverError::WouldBlock,
            RecycledWrongBuffer => DriverError::BadState,
        },
    }
}

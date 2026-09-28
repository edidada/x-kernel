// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Structures and functions for PCI bus operations.
//!
//! Currently, it just re-exports structures from the crate [virtio-drivers][1]
//! and its module [`virtio_drivers::transport::pci::bus`][2].
//!
//! [1]: https://docs.rs/virtio-drivers/latest/virtio_drivers/
//! [2]: https://docs.rs/virtio-drivers/latest/virtio_drivers/transport/pci/bus/index.html

#![no_std]

extern crate alloc;

use core::{
    ptr::NonNull,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};

use khal::firmware::devices as fw;
// Re-export the OS-neutral firmware interrupt descriptor types so downstream
// crates (e.g. virtio) can consume them without taking a direct `khal`
// dependency.
pub use khal::firmware::devices::{InterruptControllerKind, InterruptTrigger};
use lazyinit::LazyInit;
pub use virtio_drivers::transport::pci::bus::{
    BarInfo, Cam, CapabilityInfo, Command, ConfigurationAccess, DeviceFunction, DeviceFunctionInfo,
    HeaderType, MemoryBarType, MmioCam, PciError, PciRoot, Status,
};

pub mod msix;
pub mod support;

pub mod discover;

use driver_core::{
    Bus, BusId, IdMatchEntry, MatchScore,
    desc::{DeviceDescriptor, DeviceIdentity},
};

/// The PCI bus type in the driver-core device model — the single authority
/// (Linux keeps `pci_bus_type` with the PCI subsystem in `drivers/pci`,
/// not in the driver core).
///
/// Matches descriptors with `DeviceLocation::Pci` against [`PciMatch`] id
/// tables. The discovery pipeline lives in [`discover`].
pub struct PciBus;

impl Bus for PciBus {
    type IdEntry = PciMatch;

    const ID: BusId = BusId::new("pci");

    fn accepts(desc: &DeviceDescriptor) -> bool {
        matches!(desc.location, driver_core::DeviceLocation::Pci { .. })
    }
}

/// One `struct pci_device_id` row, for PCI id tables.
#[derive(Debug, Clone, Copy)]
pub struct PciMatch {
    pub vendor_id: u16,
    pub device_id: u16,
}

impl PciMatch {
    /// Exact vendor + device match.
    pub const fn device(vendor_id: u16, device_id: u16) -> Self {
        Self {
            vendor_id,
            device_id,
        }
    }

    /// Any device from a vendor.
    pub const fn vendor(vendor_id: u16) -> Self {
        Self {
            vendor_id,
            device_id: u16::MAX,
        }
    }
}

impl IdMatchEntry for PciMatch {
    fn modalias(&self) -> Option<alloc::string::String> {
        Some(alloc::format!(
            "pci:v{vendor:08X}d{device:08X}",
            vendor = self.vendor_id,
            device = self.device_id
        ))
    }

    fn matches(&self, desc: &DeviceDescriptor) -> MatchScore {
        let DeviceIdentity::Pci(pci) = &desc.identity else {
            return MatchScore::NoMatch;
        };
        let device_match = self.device_id == u16::MAX || self.device_id == pci.device_id;
        if self.vendor_id == pci.vendor_id && device_match {
            if self.device_id == pci.device_id {
                MatchScore::Exact
            } else {
                MatchScore::Generic
            }
        } else {
            MatchScore::NoMatch
        }
    }
}

#[macro_use]
extern crate log;

static PCI_CONFIG_BASE_OVERRIDE: AtomicU64 = AtomicU64::new(0);
static PCI_BUS_END_OVERRIDE: AtomicU32 = AtomicU32::new(u32::MAX);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PciConfigSource {
    Static,
    Firmware,
    RuntimeOverride,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PciInitError {
    NoMemory,
    InvalidRange,
    MappingFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyInterruptRoute {
    pub irq: usize,
    pub trigger: fw::InterruptTrigger,
    pub controller: fw::InterruptControllerKind,
}

impl PciInitError {
    fn from_iomap(err: memspace::IoMapError) -> Self {
        match err {
            memspace::IoMapError::NoMemory => Self::NoMemory,
            memspace::IoMapError::InvalidRange => Self::InvalidRange,
            memspace::IoMapError::MappingFailed => Self::MappingFailed,
        }
    }
}

pub fn set_pci_config_space(base: u64, bus_end: u8) {
    PCI_CONFIG_BASE_OVERRIDE.store(base, Ordering::Relaxed);
    PCI_BUS_END_OVERRIDE.store(bus_end as u32, Ordering::Relaxed);
}

pub fn pci_config_space() -> (u64, u8, PciConfigSource) {
    let base = PCI_CONFIG_BASE_OVERRIDE.load(Ordering::Relaxed);
    let bus_end = PCI_BUS_END_OVERRIDE.load(Ordering::Relaxed);
    if base != 0 && bus_end != u32::MAX {
        log::info!(
            "PCI config from runtime override: ecam={:#x} bus_end={:#x}",
            base,
            bus_end
        );
        return (base, bus_end as u8, PciConfigSource::RuntimeOverride);
    }

    if let Some(host) = fw::pci_host()
        && host.bus_start == 0
    {
        log::info!(
            "PCI host from firmware: cam={:?} ecam={:#x} size={:#x} bus_range={:#x}..={:#x}",
            host.cam,
            host.ecam_base,
            host.ecam_size,
            host.bus_start,
            host.bus_end
        );
        return (host.ecam_base, host.bus_end, PciConfigSource::Firmware);
    }

    if kbuild_config::PCI_ECAM_BASE == 0 {
        log::info!("PCI: no firmware PCI host and PCI_ECAM_BASE=0; skipping PCI");
        return (0, 0, PciConfigSource::Static);
    }
    log::info!(
        "PCI config from kbuild: ecam={:#x} bus_end={:#x}",
        kbuild_config::PCI_ECAM_BASE as u64,
        kbuild_config::PCI_BUS_END as u8
    );
    (
        kbuild_config::PCI_ECAM_BASE as u64,
        kbuild_config::PCI_BUS_END as u8,
        PciConfigSource::Static,
    )
}

pub fn pci_ecam_size(bus_end: u8, cam: Cam) -> usize {
    let buses = bus_end as usize + 1;
    let bus_stride = match cam {
        Cam::MmioCam => 1usize << 16,
        Cam::Ecam => 1usize << 20,
    };
    buses * bus_stride
}

pub fn iomap_mmio(
    paddr: usize,
    size: usize,
    name: &'static str,
) -> Result<NonNull<u8>, PciInitError> {
    let vaddr = memspace::iomap_device(paddr.into(), size, name).map_err(|err| {
        log::warn!(
            "failed to iomap {name} at [PA:{:#x}, PA:{:#x}): {:?}",
            paddr,
            paddr.saturating_add(size),
            err
        );
        PciInitError::from_iomap(err)
    })?;
    NonNull::new(vaddr.as_mut_ptr()).ok_or(PciInitError::MappingFailed)
}

/// The single boot-lifetime owner of the mapped ECAM window.
///
/// Every [`PciBus`] is a handle over this one mapping. The `iomap_device`
/// reference taken at resolve time is deliberately never released: the
/// config window must outlive all `'static` config-space aliases handed
/// to transports, so the owner holds it for the system lifetime.
static ECAM_MAPPING: LazyInit<EcamOwner> = LazyInit::new();

/// Resolved state of the ECAM window, computed exactly once.
enum EcamOwner {
    /// No usable config space on this platform (PCI absent).
    Absent,
    /// The window exists but could not be mapped; the failure is cached
    /// so every later `PciBus::new` reports the same error.
    Failed(PciInitError),
    /// The mapped window.
    Mapped(EcamWindow),
}

struct EcamWindow {
    base: SendPtr,
    cam: Cam,
    config_base: u64,
    bus_end: u8,
    source: PciConfigSource,
}

/// The mapped ECAM base as plain shareable data.
///
/// The pointer itself is never dereferenced through this wrapper; every
/// consumer re-derives an  under its own SAFETY obligations.
/// Sharing the address across threads is sound for the same reason a
///  would be.
#[derive(Clone, Copy)]
struct SendPtr(NonNull<u8>);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

impl EcamOwner {
    fn resolve(cam: Cam) -> Self {
        let (config_base, bus_end, source) = pci_config_space();
        if config_base == 0 {
            log::info!("PCI: no config space available; skipping PCI bus init");
            return Self::Absent;
        }
        if source == PciConfigSource::Firmware
            && let Some(host) = fw::pci_host()
        {
            let expected = match cam {
                Cam::MmioCam => fw::PciHostCam::Cam,
                Cam::Ecam => fw::PciHostCam::Ecam,
            };
            if host.cam != expected {
                log::warn!(
                    "PCI firmware host advertises {:?} but build selected {:?}; falling back to \
                     static config",
                    host.cam,
                    cam
                );
                return Self::resolve_static(cam);
            }
            log::info!(
                "PCI bus using firmware host: cam={:?} ecam={:#x} bus_end={:#x}",
                cam,
                config_base,
                bus_end
            );
        }
        let ecam_size = pci_ecam_size(bus_end, cam);
        match iomap_mmio(config_base as usize, ecam_size, "pci-ecam") {
            Ok(base) => Self::Mapped(EcamWindow {
                base: SendPtr(base),
                cam,
                config_base,
                bus_end,
                source,
            }),
            Err(err) => Self::Failed(err),
        }
    }

    fn resolve_static(cam: Cam) -> Self {
        let config_base = kbuild_config::PCI_ECAM_BASE as u64;
        let bus_end = kbuild_config::PCI_BUS_END as u8;
        let ecam_size = pci_ecam_size(bus_end, cam);
        match iomap_mmio(config_base as usize, ecam_size, "pci-ecam") {
            Ok(base) => Self::Mapped(EcamWindow {
                base: SendPtr(base),
                cam,
                config_base,
                bus_end,
                source: PciConfigSource::Static,
            }),
            Err(err) => Self::Failed(err),
        }
    }
}

pub struct PciHost {
    root: PciRoot<MmioCam<'static>>,
    config_base: u64,
    bus_end: u8,
    source: PciConfigSource,
}

impl PciHost {
    /// Brings up the PCI bus over the resolved configuration window.
    ///
    /// # Errors
    ///
    /// Returns [`PciInitError::InvalidRange`] when no config space exists,
    /// and [`PciInitError::NoMemory`] / [`PciInitError::MappingFailed`]
    /// when the ECAM window cannot be mapped.
    /// Bring up (or attach to) the shared PCI bus over the resolved
    /// configuration window.
    ///
    /// The ECAM mapping has exactly one owner ([`ECAM_MAPPING`]) for the
    /// whole system lifetime: the first call resolves the window
    /// (firmware host, with the kbuild static fallback) and maps it once;
    /// every later call is a cheap handle over that same mapping — no
    /// extra `iomap_device` reference is taken and no side effect runs.
    /// This is the Linux `pci_remap_iospace()` model: the host bridge
    /// config window is a boot-lifetime resource and is never unmapped,
    /// which is also what keeps the `'static` config-space aliases (the
    /// vendored `MmioCam` copies inside transports, the x86 MSI-X state
    /// root) alive for as long as any device exists.
    ///
    /// # Errors
    ///
    /// Returns [`PciInitError::InvalidRange`] when no config space exists
    /// (cached: platforms without PCI fail once, not per probe), and
    /// [`PciInitError::NoMemory`] / [`PciInitError::MappingFailed`] when
    /// the ECAM window cannot be mapped.
    pub fn new(cam: Cam) -> Result<Self, PciInitError> {
        ECAM_MAPPING.call_once(|| EcamOwner::resolve(cam));
        match ECAM_MAPPING.get().expect("ECAM_MAPPING call_once ran") {
            EcamOwner::Absent => Err(PciInitError::InvalidRange),
            EcamOwner::Failed(err) => Err(*err),
            EcamOwner::Mapped(ecam) => {
                if ecam.cam != cam {
                    log::warn!(
                        "PCI: ECAM owner initialized for {:?} but {:?} requested",
                        ecam.cam,
                        cam
                    );
                    return Err(PciInitError::InvalidRange);
                }
                // SAFETY: the mapping is owned by `ECAM_MAPPING` for the
                // system lifetime and covers the whole window for `cam`.
                let mmio_cam = unsafe { MmioCam::new(ecam.base.0.as_ptr(), cam) };
                Ok(Self {
                    root: PciRoot::new(mmio_cam),
                    config_base: ecam.config_base,
                    bus_end: ecam.bus_end,
                    source: ecam.source,
                })
            }
        }
    }

    pub fn config_base(&self) -> u64 {
        self.config_base
    }

    pub fn bus_end(&self) -> u8 {
        self.bus_end
    }

    pub fn source(&self) -> PciConfigSource {
        self.source
    }

    /// Mutable access to the bus root (config space via the public
    /// `configuration_access` field of the vendored `PciRoot`).
    pub fn root_mut(&mut self) -> &mut PciRoot<MmioCam<'static>> {
        &mut self.root
    }
}

pub fn pci_bar_allocation_range() -> Option<(u64, u64, PciConfigSource)> {
    if let Some(range) = fw::pci_bar_range() {
        log::info!(
            "PCI BAR allocator from firmware: cpu_base={:#x} size={:#x} prefetchable={}",
            range.cpu_base,
            range.size,
            range.prefetchable
        );
        return Some((range.cpu_base, range.size, PciConfigSource::Firmware));
    }

    let range = kbuild_config::PCI_RANGES
        .get(1)
        .copied()
        .map(|(start, len)| (start, len, PciConfigSource::Static));
    if let Some((start, len, _)) = range {
        log::info!(
            "PCI BAR allocator from kbuild: cpu_base={:#x} size={:#x}",
            start,
            len
        );
    }
    range
}

pub fn legacy_interrupt_route<C: ConfigurationAccess>(
    root: &PciRoot<C>,
    bdf: DeviceFunction,
) -> Option<LegacyInterruptRoute> {
    let word = root.configuration_access.read_word(bdf, 0x3C);
    let pin = ((word >> 8) & 0xff) as u8;
    if pin == 0 || pin == 0xff {
        log::info!(
            "PCI legacy IRQ route: bdf={:?} pin not programmed (pin={:#x})",
            bdf,
            pin
        );
        return None;
    }
    let irq = fw::pci_legacy_irq(bdf.bus, bdf.device, bdf.function, pin)?;
    log::info!(
        "PCI legacy IRQ from firmware: bdf={:?} pin=INT{} irq={} trigger={:?} controller={:?}",
        bdf,
        pin,
        irq.irq,
        irq.trigger,
        irq.controller
    );
    Some(LegacyInterruptRoute {
        irq: irq.irq,
        trigger: irq.trigger,
        controller: irq.controller,
    })
}

>>>>>>> 3d5d64b1b (refactor(pci): drop PciConfigAccess, use the vendored root's public config field)
/// Sequential allocator for MMIO address space used by PCI Base Address Registers.
///
/// This allocator manages a contiguous range of physical addresses and dispenses
/// aligned chunks suitable for memory-mapped I/O regions. Each allocation must
/// be power-of-two sized and will be naturally aligned to that size.
pub struct PciRangeAllocator {
    _begin_addr: u64,
    end_addr: u64,
    cursor_addr: u64,
}

/// Assigns unassigned memory BARs from `allocator`, logs the final BAR
/// layout, and enables IO / memory / bus-master command bits.
///
/// # Errors
///
/// Returns [`PciInitError::NoMemory`] when the allocator cannot satisfy a
/// BAR request.
///
/// # Panics
///
/// Panics via `expect` when `allocator` is `None` while a memory BAR is
/// unassigned — configuring BARs without an allocation range is a
/// platform configuration error.
pub fn configure_device<C: ConfigurationAccess>(
    root: &mut PciRoot<C>,
    bdf: DeviceFunction,
    allocator: &mut Option<PciRangeAllocator>,
) -> Result<(), PciInitError> {
    let mut bar = 0;
    while bar < 6 {
        let info = match root.bar_info(bdf, bar).unwrap() {
            Some(info) => info,
            None => {
                bar += 1;
                continue;
            }
        };

        if let BarInfo::Memory {
            address_type,
            address,
            size,
            ..
        } = info
            && size > 0
            && address == 0
        {
            let new_addr = allocator
                .as_mut()
                .expect("No memory ranges available for PCI BARs!")
                .alloc_buf(size as _)
                .ok_or(PciInitError::NoMemory)?;
            if address_type == MemoryBarType::Width32 {
                root.set_bar_32(bdf, bar, new_addr as _);
            } else if address_type == MemoryBarType::Width64 {
                root.set_bar_64(bdf, bar, new_addr);
            }
        }

        let info = match root.bar_info(bdf, bar).unwrap() {
            Some(info) => info,
            None => {
                bar += 1;
                continue;
            }
        };
        let takes_two = info.takes_two_entries();
        match info {
            BarInfo::IO { address, size } => {
                if address > 0 && size > 0 {
                    log::debug!("  BAR {}: IO  [{:#x}, {:#x})", bar, address, address + size);
                }
            }
            BarInfo::Memory {
                address_type,
                prefetchable,
                address,
                size,
            } => {
                if address > 0 && size > 0 {
                    log::debug!(
                        "  BAR {}: MEM [{:#x}, {:#x}){}{}",
                        bar,
                        address,
                        address + size,
                        if address_type == MemoryBarType::Width64 {
                            " 64bit"
                        } else {
                            ""
                        },
                        if prefetchable { " pref" } else { "" },
                    );
                }
            }
        }

        bar += 1;
        if takes_two {
            bar += 1;
        }
    }

    let (_status, cmd) = root.get_status_command(bdf);
    root.set_command(
        bdf,
        cmd | Command::IO_SPACE | Command::MEMORY_SPACE | Command::BUS_MASTER,
    );
    Ok(())
}

impl PciRangeAllocator {
    /// Constructs a new allocator managing the specified address range.
    ///
    /// # Arguments
    ///
    /// * `start` - The starting physical address of the MMIO region
    /// * `length` - The total size in bytes of the allocatable region
    pub const fn new(start: u64, length: u64) -> Self {
        Self {
            _begin_addr: start,
            end_addr: start.saturating_add(length),
            cursor_addr: start,
        }
    }

    /// Attempts to allocate an aligned MMIO region of the requested size.
    ///
    /// # Arguments
    ///
    /// * `length` - The requested allocation size. Must be a power of two.
    ///
    /// # Returns
    ///
    /// * `Some(address)` - The base address of the allocated region, aligned to `length`
    /// * `None` - If `length` is not a power of two or insufficient space remains
    ///
    /// # Alignment guarantee
    ///
    /// The returned address is guaranteed to be a multiple of `length`, ensuring
    /// natural alignment for the allocated region.
    pub fn alloc_buf(&mut self, length: u64) -> Option<u64> {
        // Validate that length is a power of two (required for alignment)
        if length == 0 || (length & (length.wrapping_sub(1))) != 0 {
            return None;
        }

        // Calculate the aligned starting address for this allocation
        let aligned_addr = self.compute_aligned_address(length)?;

        // Verify that the full allocation fits within our managed range
        let allocation_end = aligned_addr.checked_add(length)?;
        if allocation_end > self.end_addr {
            return None;
        }

        // Commit the allocation by advancing our free pointer
        self.cursor_addr = allocation_end;
        Some(aligned_addr)
    }

    /// Computes the next address aligned to the given boundary.
    ///
    /// Returns `None` if alignment calculation would overflow.
    fn compute_aligned_address(&self, alignment: u64) -> Option<u64> {
        // Calculate the alignment mask (e.g., for alignment=4096, mask=4095)
        let alignment_mask = alignment.wrapping_sub(1);

        // Apply ceiling alignment: (addr + mask) & ~mask
        let adjusted = self.cursor_addr.checked_add(alignment_mask)?;
        Some(adjusted & !alignment_mask)
    }
}

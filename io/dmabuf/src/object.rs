// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The object behind a dma-buf descriptor.
//!
//! [`DmaBuf`] is the exporter's side of one shared buffer: the memory window a
//! device addresses, the attachments an importer takes, and the two CPU-access
//! hooks that make a cacheable buffer coherent with a non-coherent device.
//!
//! Everything here is deliberately narrow. Fences, reservations and the
//! `*_SYNC_FILE` commands are *not* part of the core contract: they are
//! exporter-specific today (only the DRM exporter has a reservation), and an
//! exporter that has none — a heap buffer, for instance — inherits correct
//! defaults instead of having to stub them out.

use alloc::sync::Arc;
use core::any::Any;

use kerrno::KResult;
use kpoll::{IoEvents, PollContext, PollRegisterError};
use memaddr::{PhysAddr, PhysAddrRange};

/// Stable identity of one export.
///
/// This is `struct dma_buf`'s address in Linux: two attachments belong to the
/// same buffer exactly when their [`DmaBufAttachment::export`] values are
/// equal. It is never zero, and it stays valid for as long as the object is
/// reachable.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExportId(u64);

impl ExportId {
    /// Wraps the exporter's identity value.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw identity value.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Identity of one importer, as the exporter knows it.
///
/// A display card, a media device and an NPU driver each report their own id,
/// so one export can tell their attachments apart.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ImporterId(u64);

impl ImporterId {
    /// Wraps the importer's identity value.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw identity value.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Direction of a CPU access, from the CPU's point of view.
///
/// This is the `direction` argument of `dma_buf_begin_cpu_access`, named from
/// the side that calls it: a [`Read`](Self::Read) access is the CPU reading
/// what a device wrote, which Linux spells `DMA_FROM_DEVICE`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuAccess {
    /// The CPU reads the buffer.
    Read,
    /// The CPU writes the buffer.
    Write,
    /// The CPU reads and writes the buffer.
    ReadWrite,
}

impl CpuAccess {
    /// Whether the access reads memory a device wrote.
    pub const fn reads_memory(self) -> bool {
        matches!(self, Self::Read | Self::ReadWrite)
    }

    /// Whether the access writes memory a device reads.
    pub const fn writes_memory(self) -> bool {
        matches!(self, Self::Write | Self::ReadWrite)
    }
}

/// One importer's attachment to an export (`struct dma_buf_attachment`).
///
/// The attachment only names the export and the importer, so it can be passed
/// back to the exporter to map or to detach. It holds no reference: a holder
/// must keep the [`SharedDmaBuf`](crate::SharedDmaBuf) alive for as long as it
/// uses a window obtained from [`DmaBuf::map`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaBufAttachment {
    /// The export this attachment was taken from.
    pub export: ExportId,
    /// The importer that took it.
    pub importer: ImporterId,
}

/// The physical window one mapping covers.
///
/// This is the single run an exporter exposes: `sg_table`'s one entry for a
/// contiguous buffer. An importer that can address the guest's physical memory
/// programs a device with [`phys_start`](Self::phys_start) directly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappedWindow {
    /// Physical address of the window's first byte.
    pub phys_start: PhysAddr,
    /// Length of the window in bytes.
    pub len_bytes: usize,
}

impl MappedWindow {
    /// Builds a window from its physical start and byte length.
    pub const fn new(phys_start: PhysAddr, len_bytes: usize) -> Self {
        Self {
            phys_start,
            len_bytes,
        }
    }

    /// Returns the window as an address range.
    pub fn range(self) -> PhysAddrRange {
        PhysAddrRange::from_start_size(self.phys_start, self.len_bytes)
    }

    /// Returns whether the window covers no bytes.
    pub const fn is_empty(self) -> bool {
        self.len_bytes == 0
    }
}

/// The object behind a dma-buf descriptor.
///
/// One export implements this trait; every descriptor of the buffer, every
/// importer attachment and the descriptor's slot hold the same [`Arc`]. Freeing
/// the memory is therefore the exporter's `Drop`, which runs exactly when the
/// last of those references goes away.
///
/// The trait carries defaults for every operation an exporter may not need:
///
/// * [`attach`](Self::attach) and [`detach`](Self::detach) need no bookkeeping
///   when one export serves every importer;
/// * [`map`](Self::map) answers [`window`](Self::window) when the address a
///   device uses does not depend on the attachment (no IOMMU);
/// * [`begin_cpu_access`](Self::begin_cpu_access) and
///   [`end_cpu_access`](Self::end_cpu_access) are no-ops for a buffer that is
///   coherent by construction;
/// * [`poll_state`](Self::poll_state) reports a ready descriptor and
///   [`register_poll`](Self::register_poll) has nothing to arm when the export
///   has no fences to wait for.
pub trait DmaBuf: Send + Sync {
    /// Returns the export as `Any`, for the exporter's own downcast.
    ///
    /// This is the exporter's equivalent of `dma_buf->priv`.
    fn as_any(&self) -> &dyn Any;

    /// Returns the buffer's stable identity.
    fn export_id(&self) -> ExportId;

    /// Returns the exporter's name for the buffer (`dma_buf->exp_name`).
    fn exporter_name(&self) -> &str;

    /// Returns the buffer's length in bytes, as `lseek(SEEK_END)` reports it.
    fn size_bytes(&self) -> u64;

    /// Returns the physical window the exporter's pages occupy.
    ///
    /// This is what a userspace mapping of the descriptor covers. A device
    /// driver should attach and [`map`](Self::map) instead, so the buffer
    /// cannot be released while the device still addresses it.
    fn window(&self) -> MappedWindow;

    /// Attaches `importer` to the buffer.
    ///
    /// The default records nothing: an export that serves every importer the
    /// same way still hands out a valid attachment, and dropping it is a
    /// no-op. An exporter with per-importer state (an IOMMU mapping, an
    /// address-space window) overrides this pair.
    ///
    /// # Errors
    ///
    /// The exporter refuses an importer it cannot serve.
    fn attach(&self, importer: ImporterId) -> KResult<DmaBufAttachment> {
        Ok(DmaBufAttachment {
            export: self.export_id(),
            importer,
        })
    }

    /// Detaches a previous attachment; a foreign attachment is ignored.
    fn detach(&self, _attachment: &DmaBufAttachment) {}

    /// Maps `attachment` to the window the importer may program.
    ///
    /// The window is only meaningful while the caller keeps this export alive;
    /// the returned value carries no lifetime of its own.
    ///
    /// # Errors
    ///
    /// The attachment does not belong to this export, or the exporter cannot
    /// describe a window for it.
    fn map(&self, attachment: &DmaBufAttachment) -> KResult<MappedWindow> {
        if attachment.export != self.export_id() {
            return Err(kerrno::KError::InvalidInput);
        }
        Ok(self.window())
    }

    /// Makes the CPU and a non-coherent device agree before the CPU reads.
    ///
    /// The exporter performs whatever maintenance its buffer needs (a heap
    /// buffer invalidates the data cache; a coherent buffer does nothing) and
    /// waits for outstanding device work. The default assumes the buffer is
    /// coherent by construction.
    ///
    /// # Errors
    ///
    /// The wait was interrupted, or the export cannot prepare the access.
    fn begin_cpu_access(&self, _access: CpuAccess) -> KResult<()> {
        Ok(())
    }

    /// Makes the CPU and a non-coherent device agree after the CPU wrote.
    ///
    /// # Errors
    ///
    /// The export cannot publish the writes.
    fn end_cpu_access(&self, _access: CpuAccess) -> KResult<()> {
        Ok(())
    }

    /// Returns the fence that orders any access to the buffer.
    ///
    /// An importer records it in its own reservation, so the export's
    /// outstanding work is ordered before the importer's first use. The
    /// default has no fence to record, which is the right answer for a buffer
    /// that is not produced by an engine.
    fn access_fence(&self) -> Option<FenceRef> {
        None
    }

    /// Records a diagnostics-only name (`DMA_BUF_SET_NAME`).
    ///
    /// The default accepts the rename and keeps nothing: the name never
    /// affects the memory.
    fn set_name(&self, _name: &str) {}

    /// Returns the descriptor readiness, or a ready set when the export has no
    /// fences.
    fn poll_state(&self) -> IoEvents {
        IoEvents::IN | IoEvents::OUT
    }

    /// Arms the export's fence callbacks for the requested directions.
    ///
    /// The default has nothing to arm, because
    /// [`poll_state`](Self::poll_state) is always ready.
    fn register_poll(
        &self,
        _context: &mut PollContext<'_>,
        _events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        Ok(())
    }

    /// Handles a descriptor command the export's reservation owns.
    ///
    /// Only `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` and
    /// `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` are forwarded here. The default
    /// refuses them, which is the correct answer for a buffer without fences.
    ///
    /// # Errors
    ///
    /// [`KError::OperationNotSupported`](kerrno::KError::OperationNotSupported)
    /// when the export keeps no reservation.
    fn sync_file_ioctl(&self, _cmd: u32, _arg: usize) -> KResult<usize> {
        Err(kerrno::KError::OperationNotSupported)
    }
}

/// An opaque fence an importer can record in its own reservation.
pub type FenceRef = Arc<dyn Any + Send + Sync>;

/// Type-erased handle to an export.
///
/// Every holder of a buffer — the descriptor, a mapping, an importer — keeps
/// one of these; the last one to go frees the memory.
pub type SharedDmaBuf = Arc<dyn DmaBuf>;

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! dma-buf: this card's exporter for the kernel-wide sharing object
//! (`io/dmabuf`).
//!
//! The card's buffers are guest memory, so they are shared by *importing the
//! memory*, not by exporting a device-private object: the exporter receives
//! the physical backing under a dma-buf fd, and an importer that runs on the
//! same bus gets that same backing back. The export therefore carries exactly
//! what a consumer needs and nothing it may not have:
//!
//! * the read-only physical pages;
//! * the buffer's [`DmaResv`], so every attachment shares one implicit ordering
//!   for the memory rather than each importer inventing its own;
//! * no host visibility of any kind — a resource the host cannot read from the
//!   guest is not exported at all.
//!
//! The descriptor, the ioctl numbers and the importer entry point belong to
//! `io/dmabuf`; this module only implements its [`dmabuf::DmaBuf`] contract
//! and adds the piece DRM has that a fence-less buffer does not: a reservation
//! that orders CPU access against the card's fences.
//!
//! An attachment records who holds the buffer and with which access. It stays
//! alive as long as the importer's GEM object does.

use alloc::{collections::BTreeMap, string::String, sync::Arc};

use dmabuf::{
    CpuAccess, DmaBuf as SharedDmaBuf, ExportId, MappedWindow,
    uapi::{
        DMA_BUF_IOCTL_EXPORT_SYNC_FILE, DMA_BUF_IOCTL_IMPORT_SYNC_FILE, DMA_BUF_SYNC_RW,
        DMA_BUF_SYNC_WRITE, DmaBufExportSyncFile, DmaBufImportSyncFile,
    },
};
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollContext, PollRegisterError};
use kspin::SpinNoIrq;
use memaddr::PhysAddr;
use posix_types::UserPtr;

use super::error::DrmError;
use crate::{
    prelude::*,
    sync::{
        dma_fence::{DmaFence, FenceCallback},
        dma_resv::{DmaResv, ResvUsage},
        fence_access::{fence_from_fd, install},
        fence_list::{FenceList, is_signaled},
        wakeable::Wakeable,
    },
};

/// The memory an export hands to an importer.
///
/// The backing is the exporter's pages. The `Arc` keeps the allocation alive
/// for the dma-buf's lifetime, so a GEM object may be destroyed while the
/// buffer is still exported.
#[derive(Clone)]
pub(crate) struct DmaBufBacking {
    /// Physical start of the exported window.
    pub(crate) start: u64,
    /// Length of the window in bytes.
    pub(crate) length: usize,
    /// The exporter's allocation, when the export owns its memory.
    ///
    /// It keeps the pages alive for every importer. An export of borrowed
    /// memory (a re-export of an import) has none: the export it came from
    /// stays alive through its own references.
    pub(crate) owner: Option<Arc<PageRef>>,
}

/// The device-independent part of an exported buffer.
pub(crate) struct DmaBuf {
    /// Exported memory.
    pub(crate) backing: DmaBufBacking,
    /// The owning `Arc`, set once by [`DmaBuf::new`].
    ///
    /// A buffer is only ever reachable as an `Arc<DmaBuf>`, and every holder
    /// that needs another handle receives it from here instead of rebuilding
    /// one — the reference counts stay the allocator's business.
    owner: SpinNoIrq<Weak<DmaBuf>>,
    /// Implicit ordering of the backing.
    pub(crate) resv: Arc<DmaResv>,
    /// Exporter's name for the buffer, reported in `DMA_BUF_IOCTL_INFO` and in
    /// `/proc` style diagnostics.
    pub(crate) name: String,
    /// Size in bytes, as `lseek` and `DMA_BUF_IOCTL_INFO` report it.
    pub(crate) size: u64,
    /// The object that exported this buffer, when it belongs to this driver.
    ///
    /// `drm_gem_prime_import` has a same-device fast path that returns this
    /// exact object, and the DRM object's resource id must stay the same or a
    /// renderer on another node (card0 vs renderD128) would receive a second
    /// resource that the exporter never rendered into.
    pub(crate) exporter: Option<alloc::sync::Weak<crate::gem::GemObject>>,
    /// Geometry of the exported framebuffer.
    ///
    /// A dma-buf has no geometry of its own in Linux; this driver exports only
    /// framebuffer-capable memory, and an importer needs the geometry to
    /// create its object without an ioctl that does not exist. The width is
    /// the pitch in pixels, so `width * 4 == pitch` and an importer can size a
    /// row without guessing from the byte length.
    pub(crate) height: u32,
    /// Visible width in pixels (`pitch / 4`).
    pub(crate) width: u32,
    /// The caller-set name (`dma_buf.name`).
    user_name: Mutex<String>,
    /// Readiness source for `EPOLLIN`: the exclusive work is done.
    ///
    /// This is `dma_buf`'s wait queue, which lives on the buffer rather than on
    /// each descriptor: fences complete on the object, so every descriptor of
    /// one buffer waits on the same source.
    read: Arc<Wakeable>,
    /// Readiness source for `EPOLLOUT`: every user of the buffer is done.
    write: Arc<Wakeable>,
    /// Importers that already attached to this buffer, by card identity.
    ///
    /// One importer has one attachment: a second import of the same buffer
    /// (through this export or a re-export of it) resolves to the existing
    /// import instead of creating a second mapping of one backing.
    attachments: Mutex<BTreeMap<u64, u64>>,
}

impl DmaBuf {
    /// Creates the exported view of one buffer.
    pub(crate) fn new(
        backing: DmaBufBacking,
        resv: Arc<DmaResv>,
        name: String,
        height: u32,
        width: u32,
        exporter: Option<alloc::sync::Weak<crate::gem::GemObject>>,
    ) -> Arc<Self> {
        let size = backing.length as u64;
        Arc::new_cyclic(|owner| Self {
            owner: SpinNoIrq::new(owner.clone()),
            backing,
            resv,
            name,
            size,
            height,
            width,
            exporter,
            user_name: Mutex::new(String::new()),
            read: Arc::new(Wakeable::new()),
            write: Arc::new(Wakeable::new()),
            attachments: Mutex::new(BTreeMap::new()),
        })
    }

    /// Another handle to this buffer.
    ///
    /// Only `None` between the cyclic construction and its publication, which
    /// no caller can observe: a buffer reaches callers only through the
    /// returned `Arc`.
    pub(crate) fn shared(&self) -> Option<Arc<Self>> {
        self.owner.lock().upgrade()
    }

    /// Attaches `importer` to the buffer, or resolves to its existing import.
    ///
    /// The returned attachment reports whether it created a new import and the
    /// mapping offset that import uses. An existing import keeps the offset of
    /// the first one, so one backing is never addressable under two names.
    pub(crate) fn attach_to(
        &self,
        importer: u64,
        allocate_offset: impl FnOnce(u64) -> DrmResult<u64>,
    ) -> DrmResult<Attachment> {
        let buf = self.shared().ok_or(DrmError::InvalidInput)?;
        let mut attachments = self.attachments.lock();
        if let Some(offset_bytes) = attachments.get(&importer) {
            return Ok(Attachment {
                buf,
                importer,
                offset_bytes: *offset_bytes,
                is_fresh: false,
            });
        }
        // The import's mapping offset must come from the importing card's VMA
        // namespace, exactly like an object the card allocated itself.
        let offset_bytes = allocate_offset(buf.backing.length as u64)?;
        attachments.insert(importer, offset_bytes);
        Ok(Attachment {
            buf,
            importer,
            offset_bytes,
            is_fresh: true,
        })
    }
}

/// One attachment of an exported buffer.
///
/// An importer holds the attachment for as long as its object references the
/// buffer; dropping it is the detach. This card is the only importer this
/// driver can create (the memory is guest memory, so every importer is the
/// same bus master), and the attachment records the importing card id.
pub(crate) struct Attachment {
    /// The exported buffer.
    pub(crate) buf: Arc<DmaBuf>,
    /// The card that attached.
    importer: u64,
    /// The mapping offset the import uses.
    offset_bytes: u64,
    /// Whether this attachment created the import.
    is_fresh: bool,
}

impl Attachment {
    /// The mapping offset of the import.
    pub(crate) const fn offset_bytes(&self) -> u64 {
        self.offset_bytes
    }

    /// Whether this attachment created the import.
    pub(crate) const fn is_fresh(&self) -> bool {
        self.is_fresh
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        // Detaching is what lets a later import of the same buffer create a
        // fresh one: the importer no longer holds the memory.
        let mut attachments = self.buf.attachments.lock();
        if attachments
            .get(&self.importer)
            .is_some_and(|offset| *offset == self.offset_bytes)
        {
            attachments.remove(&self.importer);
        }
    }
}

impl core::fmt::Debug for Attachment {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Attachment")
            .field("name", &self.buf.name)
            .finish()
    }
}

/// The fences a consumer with `write` in mind must wait for
/// (`dma_resv_usage_rw`).
///
/// A reader waits for the exclusive work, a writer for everything; this is the
/// same set `dma_resv_get_singleton` and `dma_resv_wait_timeout` return, and
/// the same one `poll()` registers on.
pub(crate) fn usage_fences(buf: &DmaBuf, write: bool) -> Arc<FenceList> {
    buf.resv.snapshot(if write {
        ResvUsage::Write
    } else {
        ResvUsage::Read
    })
}

/// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`: export the buffer's current fences.
///
/// The exported fence is `dma_resv_get_singleton` of the reservation with the
/// usage the flags name (a write access waits for everything, a read access
/// for the writers), or a stub when nothing is outstanding. The flags describe
/// the *waiting* direction, so they are not `DMA_BUF_SYNC_END`/`NOWAIT`.
fn export_sync_file(buf: &DmaBuf, request: &mut DmaBufExportSyncFile) -> KResult<()> {
    validate_sync_file_rw(request.flags as u64)?;
    let fences = usage_fences(buf, request.flags as u64 & DMA_BUF_SYNC_WRITE != 0);
    let fence = if fences.fences().is_empty() {
        private_stub()
    } else {
        merge_fences(&fences)
    };
    let cred = kprocess::current_cred();
    request.fd = install(&fence, &cred)?;
    Ok(())
}

/// `DMA_BUF_IOCTL_IMPORT_SYNC_FILE`: record a sync_file's fences.
///
/// This is `dma_buf_import_sync_file`: the descriptor names a sync_file, its
/// fence is unwrapped into its members, and every member is recorded with the
/// usage the flags name (a write access is exclusive work, a read access shared
/// work).
fn import_sync_file(buf: &DmaBuf, request: &DmaBufImportSyncFile) -> KResult<()> {
    validate_sync_file_rw(request.flags as u64)?;
    // Resolve the fence before touching the reservation, so a bad fd leaves
    // the buffer's ordering unchanged.
    let fence = fence_from_fd(request.fd).map_err(DrmError::to_vfs)?;
    let write = request.flags as u64 & DMA_BUF_SYNC_WRITE != 0;
    for member in crate::sync::fence_access::flatten(&fence) {
        if write {
            let _previous = buf.resv.add_write_fence(member);
        } else {
            let _previous = buf.resv.add_read_fence(member);
        }
    }
    Ok(())
}

/// Validates the read/write flags of the sync-file bridge.
///
/// Linux accepts only the direction bits here: any other bit is `EINVAL` and a
/// direction of zero is `EINVAL`, while `DMA_BUF_SYNC_RW` means "as a writer".
fn validate_sync_file_rw(flags: u64) -> KResult<()> {
    if flags & !DMA_BUF_SYNC_RW != 0 || flags & DMA_BUF_SYNC_RW == 0 {
        return Err(KError::InvalidInput);
    }
    Ok(())
}

/// Merges `fences` into the single fence a sync_file can carry
/// (`dma_fence_unwrap_merge` under `dma_resv_get_singleton`).
fn merge_fences(fences: &FenceList) -> Arc<DmaFence> {
    crate::sync::fence_access::unwrap_merge(fences.fences())
}

/// The stub an export hands out when the buffer has no outstanding fence.
fn private_stub() -> Arc<DmaFence> {
    crate::sync::fence_access::private_stub()
}

/// The name a buffer is exported under, `drm_<driver>` as Linux prints it.
pub(crate) fn export_name(driver: &str) -> String {
    alloc::format!("drm_{driver}")
}

impl SharedDmaBuf for DmaBuf {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn export_id(&self) -> ExportId {
        // The slot, every descriptor and every attachment hold an `Arc` to the
        // buffer, so its address is stable while the buffer can be resolved.
        ExportId::new(self as *const Self as u64)
    }

    fn exporter_name(&self) -> &str {
        &self.name
    }

    fn size_bytes(&self) -> u64 {
        self.size
    }

    fn window(&self) -> MappedWindow {
        // This driver exports guest memory only, so the window an importer
        // maps is the exporter's own pages: an importer that can address the
        // guest scans exactly the memory the producer filled.
        MappedWindow::new(
            PhysAddr::from(self.backing.start as usize),
            self.backing.length,
        )
    }

    /// `START` is `dma_buf_begin_cpu_access`: it waits for the reservation in
    /// the direction the access names, so the CPU never reads what the card is
    /// still writing.
    fn begin_cpu_access(&self, access: CpuAccess) -> KResult<()> {
        let fences = usage_fences(self, access.writes_memory());
        crate::sync::wait::wait_for_fences(&fences).map_err(DrmError::to_vfs)
    }

    /// `END` is `dma_buf_end_cpu_access`, which this driver leaves as a no-op:
    /// its producers record their work on the reservation themselves, so there
    /// is nothing to publish on end.
    fn end_cpu_access(&self, _access: CpuAccess) -> KResult<()> {
        Ok(())
    }

    /// `dma_resv_get_singleton` over this export's write list: the work a
    /// consumer must finish before the buffer is stable, or a fence that is
    /// already done when nothing is outstanding.
    fn access_fence(&self) -> Option<dmabuf::FenceRef> {
        let fences = self.resv.write_list();
        match fences.fences().last() {
            Some(fence) => Some(fence.clone() as dmabuf::FenceRef),
            None => Some(crate::sync::dma_fence::new_signaled_fence() as dmabuf::FenceRef),
        }
    }

    /// Renames the buffer (`dma_buf_set_name`).
    fn set_name(&self, name: &str) {
        *self.user_name.lock() = name.into();
    }

    /// Readable once the exclusive work completed, writable once every user is
    /// done — `dma_buf_poll` over the reservation.
    fn poll_state(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, is_signaled(&usage_fences(self, false)));
        events.set(IoEvents::OUT, is_signaled(&usage_fences(self, true)));
        events
    }

    fn register_poll(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            self.read.register(context, IoEvents::IN)?;
            self.arm(false);
        }
        if events.contains(IoEvents::OUT) {
            self.write.register(context, IoEvents::OUT)?;
            self.arm(true);
        }
        Ok(())
    }

    /// The two reservation commands (`dma_buf_export_sync_file`,
    /// `dma_buf_import_sync_file`).
    ///
    /// They are the exporter's because a buffer without a reservation has
    /// nothing to export or import; the descriptor's default answers
    /// `OperationNotSupported` for exactly that case.
    fn sync_file_ioctl(&self, cmd: u32, arg: usize) -> KResult<usize> {
        match cmd {
            DMA_BUF_IOCTL_EXPORT_SYNC_FILE => {
                let mut request: DmaBufExportSyncFile = UserPtr::from(arg)
                    .read_vm()
                    .map_err(|_| KError::BadAddress)?;
                export_sync_file(self, &mut request)?;
                UserPtr::from(arg)
                    .write_vm(request)
                    .map_err(|_| KError::BadAddress)?;
                Ok(0)
            }
            DMA_BUF_IOCTL_IMPORT_SYNC_FILE => {
                let request: DmaBufImportSyncFile = UserPtr::from(arg)
                    .read_vm()
                    .map_err(|_| KError::BadAddress)?;
                import_sync_file(self, &request)?;
                Ok(0)
            }
            _ => Err(KError::OperationNotSupported),
        }
    }
}

impl DmaBuf {
    /// Arms one direction's poll callback (`dma_buf_poll_add_cb`).
    fn arm(&self, write: bool) {
        let source = if write { &self.write } else { &self.read };
        if !source.arm() {
            return;
        }
        let fences = usage_fences(self, write);
        // `dma_buf_poll_add_cb` arms the first fence that accepts a callback;
        // that fence's completion wakes the queue and clears the arm bit.
        let pending = fences.fences().iter().find(|fence| !fence.is_signaled());
        match pending {
            Some(fence) => fence.add_callback(FenceCallback::dma_buf_wakeable(
                &Arc::downgrade(source),
                write,
            )),
            None => source.wake_direction(write),
        }
    }
}

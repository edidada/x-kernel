// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Descriptor behaviour against a stub export.
//!
//! The stub owns no memory, so these tests cover exactly what the descriptor
//! adds on top of an export: the fd's size answer, the importer entry point,
//! and the arithmetic that turns `mmap(offset)` into a physical range.

use alloc::sync::Arc;
use core::{
    any::Any,
    sync::atomic::{AtomicU64, Ordering},
};

use kcred::initial_cred;
use kpoll::IoEvents;
use kvfs::{MmapMapper, VfsError};
use memaddr::{PAGE_SIZE_4K, PhysAddr, PhysAddrRange};
use unittest::{assert, assert_eq, def_test};

use super::{DmaBufDescriptor, lookup, new_file};
use crate::{
    object::{DmaBuf, ExportId, MappedWindow},
    uapi::DMA_BUF_IOCTL_EXPORT_SYNC_FILE,
};

/// Physical base the stub's window starts at.
const WINDOW_START: usize = 0x4000_0000;

/// Every stub export gets its own identity, as a real exporter would.
static NEXT_EXPORT_ID: AtomicU64 = AtomicU64::new(1);

/// An export without fences, like a heap buffer: it implements the required
/// operations and inherits every default.
struct StubBuf {
    export_id: ExportId,
    window: MappedWindow,
}

impl StubBuf {
    fn new(len_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            export_id: ExportId::new(NEXT_EXPORT_ID.fetch_add(1, Ordering::Relaxed)),
            window: MappedWindow::new(PhysAddr::from(WINDOW_START), len_bytes),
        })
    }
}

impl DmaBuf for StubBuf {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn export_id(&self) -> ExportId {
        self.export_id
    }

    fn exporter_name(&self) -> &str {
        "stub"
    }

    fn size_bytes(&self) -> u64 {
        self.window.len_bytes as u64
    }

    fn window(&self) -> MappedWindow {
        self.window
    }
}

/// Records what the descriptor asked the mm layer for.
struct RecordingMapper {
    offset: usize,
    length: usize,
    range: Option<PhysAddrRange>,
    keep_alive: Option<Arc<dyn Any + Send + Sync>>,
}

impl RecordingMapper {
    fn new(offset: usize, length: usize) -> Self {
        Self {
            offset,
            length,
            range: None,
            keep_alive: None,
        }
    }
}

impl MmapMapper for RecordingMapper {
    fn map_physical(&mut self, range: PhysAddrRange) -> kvfs::VfsResult<()> {
        self.range = Some(range);
        Ok(())
    }

    fn map_physical_owned(
        &mut self,
        range: PhysAddrRange,
        owner: Arc<dyn Any + Send + Sync>,
    ) -> kvfs::VfsResult<()> {
        // The mm layer refuses a mapping longer than the backing it was given;
        // the descriptor's job is to hand it the exact remaining window.
        if range.is_empty() || range.size() < self.length {
            return Err(VfsError::InvalidInput);
        }
        self.range = Some(range);
        self.keep_alive = Some(owner);
        Ok(())
    }

    fn map_file_backed(&mut self) -> kvfs::VfsResult<()> {
        Err(VfsError::InvalidInput)
    }

    fn map_anonymous_shared(&mut self) -> kvfs::VfsResult<()> {
        Err(VfsError::InvalidInput)
    }

    fn offset(&self) -> usize {
        self.offset
    }
}

fn descriptor(len_bytes: usize) -> (Arc<StubBuf>, Arc<kvfs::VfsFile>) {
    let object = StubBuf::new(len_bytes);
    let file = new_file(object.clone(), initial_cred()).expect("a descriptor must be created");
    (object, file)
}

/// `lseek(SEEK_END)` is how a client sizes a buffer, and `SEEK_SET(0)` is how
/// it reads the base without moving the offset.
#[def_test]
fn seek_end_reports_the_export_size() {
    let (_, file) = descriptor(3 * PAGE_SIZE_4K);
    let want = 3 * PAGE_SIZE_4K as u64;
    assert_eq!(file.llseek(0, 2).expect("SEEK_END"), want);
    assert_eq!(file.llseek(0, 0).expect("SEEK_SET(0)"), 0);
}

/// Every other `whence`, and a non-zero offset, is `EINVAL`.
#[def_test]
fn seek_rejects_other_whence_and_offsets() {
    let (_, file) = descriptor(PAGE_SIZE_4K);
    // SEEK_CUR is not a dma-buf question.
    assert!(matches!(file.llseek(0, 1), Err(VfsError::InvalidInput)));
    assert!(matches!(file.llseek(1, 2), Err(VfsError::InvalidInput)));
}

/// The descriptor publishes its export on the shared slot, which is how an
/// importer that did not create the buffer finds it.
#[def_test]
fn descriptor_publishes_the_export_for_importers() {
    let (object, file) = descriptor(PAGE_SIZE_4K);
    let found = lookup(&file).expect("the descriptor must publish its export");
    assert_eq!(found.export_id(), object.export_id());
    let found = found
        .as_any()
        .downcast_ref::<StubBuf>()
        .expect("the published object is the exporter's own type");
    assert_eq!(
        found as *const StubBuf,
        Arc::as_ptr(&object),
        "lookup returns the same export, not a copy"
    );
}

/// A fence-less export is always ready and has no sync-file bridge: the
/// defaults are the right answer, not stubs the exporter has to write.
#[def_test]
fn fence_less_export_is_ready_and_has_no_sync_file_bridge() {
    let (object, _file) = descriptor(PAGE_SIZE_4K);
    let ready = object.poll_state();
    assert!(ready.contains(IoEvents::IN) && ready.contains(IoEvents::OUT));
    assert!(matches!(
        object.sync_file_ioctl(DMA_BUF_IOCTL_EXPORT_SYNC_FILE, 0),
        Err(VfsError::OperationNotSupported)
    ));
}

/// A page-aligned sub-range maps the buffer from the requested byte offset to
/// its end, and the mapping keeps the export alive by holding the descriptor.
#[def_test]
fn sub_range_mapping_starts_at_the_requested_offset() {
    let (_, file) = descriptor(2 * PAGE_SIZE_4K);
    let mut mapper = RecordingMapper::new(PAGE_SIZE_4K, PAGE_SIZE_4K);
    file.mmap(&mut mapper).expect("a sub-range must map");
    let range = mapper.range.expect("the descriptor must map a range");
    assert_eq!(
        range.start.as_usize(),
        WINDOW_START + PAGE_SIZE_4K,
        "the mapping starts at window + offset"
    );
    assert_eq!(range.size(), PAGE_SIZE_4K);
    assert!(
        mapper
            .keep_alive
            .expect("a user mapping must keep the export alive")
            .downcast::<DmaBufDescriptor>()
            .is_ok(),
        "the keep-alive is the descriptor the VMA can drop"
    );
}

/// Offsets outside the window name no backing bytes, so they are refused
/// instead of being clipped into a neighbouring allocation.
#[def_test]
fn mapping_outside_the_window_is_refused() {
    let len = 2 * PAGE_SIZE_4K;
    let (_, file) = descriptor(len);

    let mut past_end = RecordingMapper::new(len, PAGE_SIZE_4K);
    assert!(matches!(
        file.mmap(&mut past_end),
        Err(VfsError::InvalidInput)
    ));

    let mut unaligned = RecordingMapper::new(PAGE_SIZE_4K + 1, PAGE_SIZE_4K);
    assert!(matches!(
        file.mmap(&mut unaligned),
        Err(VfsError::InvalidInput)
    ));
    assert!(past_end.range.is_none() && unaligned.range.is_none());
}

/// The window the descriptor hands the mm layer is the *remaining* one, so a
/// mapping longer than the tail of the buffer is refused rather than silently
/// covering memory past it.
#[def_test]
fn mapping_longer_than_the_remaining_window_is_refused() {
    let (_, file) = descriptor(2 * PAGE_SIZE_4K);
    let mut oversized = RecordingMapper::new(PAGE_SIZE_4K, 2 * PAGE_SIZE_4K);
    assert!(matches!(
        file.mmap(&mut oversized),
        Err(VfsError::InvalidInput)
    ));
    assert!(oversized.range.is_none());
}
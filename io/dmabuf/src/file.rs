// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The descriptor a shared buffer is handed out through.
//!
//! A dma-buf fd is an anonymous-inode file whose slot carries the export
//! object. The descriptor owns nothing else: `mmap` publishes the object's
//! physical window, `lseek(SEEK_END)` reports its length, and the ioctls
//! forward to the object (see [`crate::ioctl`]).
//!
//! Two references keep the memory alive, and they are both `Arc`s of the same
//! descriptor:
//!
//! * the open file description holds one, so the buffer lives as long as the
//!   descriptor;
//! * every user mapping holds one (`MmapMapper::map_physical_owned`), so
//!   closing the descriptor cannot free pages a mapping still uses, and `fork`
//!   clones the reference rather than the buffer.

use alloc::sync::Arc;

use anon_inodefs::AnonInodeFs;
use kcred::Cred;
use kpoll::{IoEvents, PollContext, PollRegisterError};
use kvfs::{FMode, FileOperations, MmapMapper, OpenFlags, VfsError, VfsFile, VfsResult};
use memaddr::{PAGE_SIZE_4K, PhysAddr, PhysAddrRange};

use crate::object::SharedDmaBuf;

/// The state every descriptor (and every mapping of it) shares.
pub(crate) struct DmaBufDescriptor {
    object: SharedDmaBuf,
}

impl DmaBufDescriptor {
    fn new(object: SharedDmaBuf) -> Arc<Self> {
        Arc::new(Self { object })
    }
}

/// Creates the descriptor one export is shared through with read-write access.
///
/// The caller installs the returned file in a descriptor table; this crate
/// does not know which process that is. Exporters that hand out an fd with a
/// caller-chosen access mode (`/dev/dma_heap/*`'s `fd_flags`) use
/// [`new_file_with_mode`].
///
/// # Errors
///
/// Forwards anonymous-inode file creation failures.
pub fn new_file(object: SharedDmaBuf, cred: Arc<Cred>) -> VfsResult<Arc<VfsFile>> {
    new_file_with_mode(object, cred, OpenFlags::READ_WRITE)
}

/// Creates the descriptor with the access mode `open_flags` names.
///
/// `READ` and `LSEEK` are always present: the VFS requires read access for any
/// `mmap` of a regular file, and a client sizes the buffer with
/// `lseek(SEEK_END)`. The caller's access mode then decides `WRITE`, so an
/// `O_RDONLY` dma-buf descriptor cannot establish a
/// `MAP_SHARED | PROT_WRITE` mapping, while an `O_WRONLY` descriptor remains
/// mappable instead of failing every `mmap` for lack of `READ`.
///
/// # Errors
///
/// Forwards anonymous-inode file creation failures.
pub fn new_file_with_mode(
    object: SharedDmaBuf,
    cred: Arc<Cred>,
    open_flags: OpenFlags,
) -> VfsResult<Arc<VfsFile>> {
    let descriptor = DmaBufDescriptor::new(object);
    let file = AnonInodeFs::global().get_file(
        descriptor.object.exporter_name(),
        Arc::new(DmaBufFileOps),
        descriptor.clone(),
        // `dma_buf_getfile` hands out a regular file, so `lseek` works: a
        // client sizes a buffer with `lseek(SEEK_END)` before mapping it, and
        // `VfsFile::llseek` refuses the syscall with ESPIPE when the open mode
        // lacks the flag, before this file's `llseek` op is ever reached.
        // `READ` is unconditional because `validate_file_mapping_access`
        // requires it for every file mapping, including a write-only one; the
        // access mode only gates `WRITE` below.
        FMode::READ | FMode::from_open_flags(open_flags) | FMode::LSEEK,
        open_flags,
        cred,
    )?;
    // `dma_buf_getfile` sets `inode->i_size`, so `stat` and `SEEK_END` agree.
    // The anonymous inode is shared with every other anonymous file descriptor
    // (eventfd, signalfd, timerfd, ...), so this also reports the buffer's
    // length on those descriptors until the inode becomes per-file; see the
    // design notes for that follow-up.
    file.inode().set_size(descriptor.object.size_bytes());
    // Publish the export on the descriptor's slot: any importer — this card's
    // `PRIME_FD_TO_HANDLE`, another display engine, a media device — finds the
    // object without knowing which exporter created the descriptor.
    file.set_slot(kvfs::DMA_BUF_SLOT, descriptor);
    Ok(file)
}

/// Returns the export behind `file`.
///
/// # Errors
///
/// [`VfsError::BadFileDescriptor`] when `file` is not a descriptor of this
/// kind.
pub(crate) fn descriptor_of(file: &VfsFile) -> VfsResult<Arc<DmaBufDescriptor>> {
    file.private_data_get::<DmaBufDescriptor>()
        .ok_or(VfsError::BadFileDescriptor)
}

/// Returns the export object behind `file`.
pub(crate) fn object_of(file: &VfsFile) -> VfsResult<SharedDmaBuf> {
    Ok(descriptor_of(file)?.object.clone())
}

/// Operations of one dma-buf descriptor.
struct DmaBufFileOps;

impl FileOperations for DmaBufFileOps {
    fn llseek(&self, file: &VfsFile, offset: i64, whence: i32) -> VfsResult<u64> {
        const SEEK_SET: i32 = 0;
        const SEEK_END: i32 = 2;
        let object = object_of(file)?;
        // `dma_buf_llseek` discovers the end of the buffer and also accepts
        // `SEEK_SET(0)`, which is how a client asks for the base without
        // moving the offset. Any other `whence`, or a non-zero offset, is
        // `EINVAL`.
        let base = match whence {
            SEEK_END => object.size_bytes(),
            SEEK_SET => 0,
            _ => return Err(VfsError::InvalidInput),
        };
        if offset != 0 {
            return Err(VfsError::InvalidInput);
        }
        Ok(base)
    }

    fn ioctl(&self, file: &VfsFile, _cred: &kcred::Cred, cmd: u32, arg: usize) -> VfsResult<usize> {
        crate::ioctl::handle(file, cmd, arg)
    }

    fn mmap(&self, file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        let descriptor = descriptor_of(file)?;
        let window = descriptor.object.window();
        // `dma_buf_mmap_internal`: the offset is a byte position inside the
        // buffer, not a lookup key, so a client may map a sub-range. A
        // non-aligned offset, or one past the window, names no backing bytes.
        let offset = mapper.offset();
        if !offset.is_multiple_of(PAGE_SIZE_4K) || offset >= window.len_bytes {
            return Err(VfsError::InvalidInput);
        }
        let start = window
            .phys_start
            .as_usize()
            .checked_add(offset)
            .ok_or(VfsError::InvalidInput)?;
        let range =
            PhysAddrRange::from_start_size(PhysAddr::from(start), window.len_bytes - offset);
        // The mapping takes its own reference to the descriptor, so closing the
        // file cannot free pages the mapping still references and `fork`
        // clones the reference instead of the buffer.
        mapper.map_physical_owned(range, descriptor)
    }

    fn poll(&self, file: &VfsFile) -> Option<IoEvents> {
        descriptor_of(file).ok().map(|d| d.object.poll_state())
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if let Ok(descriptor) = descriptor_of(file) {
            descriptor.object.register_poll(context, events)?;
        }
        Ok(())
    }
}

/// Returns the export behind a descriptor, whoever exported it.
///
/// This is the importer's entry point (`dma_buf_get`): asking the descriptor
/// is what lets an export from another subsystem be imported at all, and a
/// descriptor without an object is reported as "not a dma-buf" rather than as
/// a malformed request.
pub fn lookup(file: &VfsFile) -> Option<SharedDmaBuf> {
    file.slot_get::<DmaBufDescriptor>(kvfs::DMA_BUF_SLOT)
        .map(|descriptor| descriptor.object.clone())
}

unittest::path_mod_test!(tests, "../ut-tests/descriptor.rs");

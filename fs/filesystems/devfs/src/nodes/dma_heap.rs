// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `/dev/dma_heap/` — heap allocators that hand out dma-buf file descriptors.
//!
//! Linux exposes one node per heap (`/dev/dma_heap/system`, `/dev/dma_heap/cma`,
//! …) and callers open them by name. Vendor runtimes reach for `cma` first and
//! fall back to `system`, so both names exist here — but both land on the same
//! allocator. This kernel has one pool of physically contiguous DMA memory, so
//! a separate "CMA" would be a name without a difference, and pretending
//! otherwise would invite callers to depend on the distinction.
//!
//! Everything the caller does afterwards happens on the returned descriptor
//! (`DMA_BUF_IOCTL_SYNC`, `mmap`), not on this node — the buffer object and the
//! descriptor belong to the `dmabuf` crate, and this node only selects the heap
//! and installs the descriptor.

use alloc::sync::Arc;

use dmabuf::{
    heap, new_file_with_mode,
    uapi::{DMA_HEAP_IOCTL_ALLOC, DmaHeapAllocationData},
};
use kvfs::{
    DeviceFileOps, DeviceId, DirMapping, NodeFlags, OpenFlags, SimpleDir, SimpleFs, VfsError,
    VfsFile, VfsResult,
};
use posix_types::UserPtr;

use crate::{DeviceFile, add_device_entry};

/// `/dev/dma_heap/system` — the generic heap.
const HEAP_SYSTEM: DeviceId = DeviceId::new(10, 243);
/// `/dev/dma_heap/cma` — the contiguous-reserve heap, aliased to the generic one.
const HEAP_CMA: DeviceId = DeviceId::new(10, 244);

/// One heap node. The two nodes differ only in the name they report.
struct DmaHeap {
    name: &'static str,
}

/// The allocation ioctl needs a current process thread: it reads that thread's
/// subjective credential, allocates contiguous DMA pages (and may therefore
/// sleep), and installs the descriptor in its descriptor table. IRQ and
/// softirq context must not reach it, and neither may a caller holding a
/// spinlock or an IRQ-disable/preempt-disable guard; `DeviceFileOps::ioctl`
/// is entered from the syscall path, which meets this contract.
impl DeviceFileOps for DmaHeap {
    fn ioctl(
        &self,
        _file: &VfsFile,
        _cred: &kcred::Cred,
        cmd: u32,
        arg: usize,
    ) -> VfsResult<usize> {
        if cmd != DMA_HEAP_IOCTL_ALLOC {
            // Linux's `dma_heap_ioctl` first bounds `_IOC_NR(cmd)` against its
            // one-entry command table and returns `-EINVAL` for anything
            // outside it (drivers/dma-buf/dma-heap.c); the internal `default:`
            // ENOTTY arm is unreachable for a user command. Match the errno
            // userspace actually observes on Linux.
            debug!("dma_heap({}): unimplemented ioctl {cmd:#010x}", self.name);
            return Err(VfsError::InvalidInput);
        }

        let req_ptr = UserPtr::<DmaHeapAllocationData>::from(arg);
        let mut req = req_ptr.read_vm().map_err(|_| VfsError::BadAddress)?;

        // `fd` is an output and `heap_flags` is heap-private input. Neither
        // has a meaning for this allocator, and accepting a set value would
        // tell the caller its request was understood.
        if req.fd != 0 || req.heap_flags != 0 {
            debug!(
                "dma_heap({}): allocation request carries fd/heap flags (fd={}, \
                 heap_flags={:#x}); refused",
                self.name, req.fd, req.heap_flags
            );
            return Err(VfsError::InvalidInput);
        }
        // Linux accepts every access mode here, including the zero-filled
        // `O_RDONLY` that callers which only care about `O_CLOEXEC` leave
        // behind; only bits outside `O_ACCMODE | O_CLOEXEC` are invalid.
        let flags = OpenFlags::from_bits(req.fd_flags).ok_or(VfsError::InvalidInput)?;
        let allowed = OpenFlags::WRITE_ONLY | OpenFlags::READ_WRITE | OpenFlags::CLOSE_ON_EXEC;
        if !allowed.contains(flags) {
            return Err(VfsError::InvalidInput);
        }
        // On a 32-bit target `len` must fit the address space; Linux checks the
        // same narrowing in `dma_heap_ioctl_allocate`.
        let len = usize::try_from(req.len).map_err(|_| VfsError::InvalidInput)?;

        // `VfsError` is `KError`, so the dma-buf layer's results pass straight
        // through — no translation table between the two.
        let buf = heap::alloc(self.name, len)?;
        let file = new_file_with_mode(buf, kprocess::current_cred(), flags)?;

        // The descriptor is installed on the *calling* process; this node has
        // no idea which one that is, and does not need to.
        let fd = kprocess::current_files()
            .add_file(file.clone(), flags.contains(OpenFlags::CLOSE_ON_EXEC))?;

        req.fd = fd as u32;
        if req_ptr.write_vm(req).is_err() {
            // The output structure is the only way userspace learns this fd;
            // if it cannot be written, remove the descriptor here rather than
            // leaking the fd slot and the whole DMA allocation for the
            // process lifetime. `close_file_if` compares the open file under
            // the table lock, so a slot another task already closed and
            // reused is left alone.
            if let Err(error) = kprocess::current_files().close_file_if(fd, &file) {
                // The rollback failed: the descriptor and its DMA allocation
                // now stay leaked for the process lifetime, which needs
                // operator attention rather than a development diagnostic.
                warn!(
                    "dma_heap({}): cannot close leaked fd {fd}: {error:?}",
                    self.name
                );
            }
            return Err(VfsError::BadAddress);
        }
        Ok(0)
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    // `DirMapping` is itself a `SimpleDirOps`, so the subdirectory is just
    // another mapping wrapped in a directory node.
    let mut heap = DirMapping::new();
    add_device_entry(
        &mut heap,
        "system",
        DeviceFile::new_character(
            fs.clone(),
            HEAP_SYSTEM,
            Arc::new(DmaHeap { name: "system" }),
        ),
    );
    add_device_entry(
        &mut heap,
        "cma",
        DeviceFile::new_character(fs.clone(), HEAP_CMA, Arc::new(DmaHeap { name: "cma" })),
    );
    root.add("dma_heap", SimpleDir::new_maker(fs, Arc::new(heap)));
}

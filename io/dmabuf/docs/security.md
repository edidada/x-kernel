# dmabuf — Security

This analysis covers the shared object/descriptor core and the optional `heap`
exporter. The `/dev/dma_heap/*` device nodes themselves are `devfs`'s, and its
security analysis covers the node-level request validation; this document covers
the allocation and cache-maintenance contract the heap object implements.

## Trust boundaries

Everything that arrives through a dma-buf descriptor's ioctls is untrusted
userspace input: the command argument pointer, the sync flags and the requested
mapping offset. Each is validated before use.

The heap allocation ioctl adds its own inputs: the requested length, the
`fd_flags` access mode, the heap-private flags, and the output `fd` field. The
node validates them (see `fs/filesystems/devfs/docs/security.md`) and this crate
still bounds the length and the access mode it is handed.

The kernel-internal side is a trust boundary of a different kind: an exporter
promises that the window it reports stays valid while an attachment exists, and
an importer must not take ownership of that memory. Neither side is checked by
this crate; the contracts are reviewed per exporter.

## Threats and controls

| ID | Threat | Impact | Control |
|----|--------|--------|---------|
| T-01 | Malformed ioctl argument pointer | Fault or disclosure while copying the UAPI structure | Arguments are copied with the typed user-copy helpers; a fault becomes `EFAULT`, never a raw dereference. A name longer than the 31 characters Linux accepts (`DMA_BUF_NAME_LEN` counts the terminator) is `EINVAL`, as `strndup_user` distinguishes the two. |
| T-02 | Sync flags outside the ABI, or no direction at all | Cache maintenance in the wrong direction, or a silently ignored request | `classify_sync` rejects any bit outside `DMA_BUF_SYNC_RW\|DMA_BUF_SYNC_END` and any request without a direction, as `dma_buf_ioctl` does. |
| T-03 | `mmap` offset outside the buffer, or not page aligned | Mapping unrelated physical memory into the process | The offset must be page aligned and inside the exporter's window; the range handed to the mm layer is the *remaining* window, so an over-long request is refused rather than clipped. |
| T-04 | Closing the descriptor while a mapping survives | Use-after-free of pages userspace still maps | The VMA owns a reference to the descriptor (`map_physical_owned`); the exporter's memory is freed only when the last descriptor or mapping reference drops. |
| T-05 | `fork` of a mapping | The child losing its backing at the parent's exit | The linear mapping is cloned with its owner `Arc`, so each VMA owns one reference. |
| T-06 | A descriptor that is not a dma-buf passed to `lookup` | An importer treating unrelated memory as a buffer | `lookup` only reads the `DMA_BUF_SLOT` the exporter published; a descriptor without it resolves to `None`, which importers report as "not a dma-buf". |
| T-07 | Importer outlives the export it attached to | Use-after-free of the exporter's pages (or virtual-address aliasing with an IOMMU) | The importer's object holds the `SharedDmaBuf` it resolved, next to the attachment token, for as long as it programs a window. The attachment and the window are tokens without a reference or a lifetime, so this crate cannot enforce it. |
| T-08 | Stale CPU view vs device writes | The CPU reading data the device has not published (or the device reading stale lines) | Explicit `begin/end_cpu_access`; the exporter performs the maintenance its allocation needs. Skipping it is a caller error documented on the contract, not something the descriptor can detect. |
| T-09 | Descriptor creation writes the shared anonymous inode's size | One buffer's length reported by unrelated eventfd/signalfd/timerfd descriptors | The inode size is still set because `stat` must report the buffer's length; the shared inode is the `anon_inodefs` limitation, and per-file inodes are the fix. Until then the exposure is a size, not memory content. |
| T-10 | Zero, overflowing, or non-narrowing heap length | A degenerate allocation, a bogus `Layout`, or a length that does not fit the address space | `heap::alloc` rejects zero, rounds up with `checked_add` (an overflow is `EINVAL`), and builds the layout with `Layout::from_size_align`; the devfs node refuses a `u64` length that does not fit `usize`. |
| T-11 | Heap request with a preset output `fd` or heap-private flag | Accepting a request the allocator does not understand | The devfs node requires both fields to be zero and answers `EINVAL` before allocating. |
| T-12 | `fd_flags` outside the access mode and `O_CLOEXEC` | A descriptor with unintended seek/close or mapping semantics | The node accepts only `O_ACCMODE \| O_CLOEXEC`. `new_file_with_mode` always advertises `READ` (the VFS requires it for every file `mmap`) and derives `WRITE` from the mode, so an `O_RDONLY` heap fd cannot publish writes through a shared mapping and an `O_WRONLY` fd is still mappable. |
| T-13 | Uninitialized heap allocation exposed through `mmap` | Disclosure of a previous owner's data or allocator metadata | The full page-rounded allocation is zeroed and cleaned to the Point of Coherency before the object is published; a later `SYNC START \| READ` invalidation refetches the zeros instead of reviving stale cache lines. |
| T-14 | Output copy fault after the heap descriptor is installed | A leaked fd and allocation, or closing an unrelated descriptor if the number is reused | The node rolls back with `TaskFiles::close_file_if`, which compares the open file under the table lock and removes the slot only while it still names the file this request installed. |

## Unsafe code

The descriptor core has no hand-written `unsafe` blocks: user copies go through
the typed `UserPtr` helpers, and the physical window is plain data.

The `heap` exporter has three, each carrying a `SAFETY:` justification:

- `kdma::allocate_dma_memory_cacheable` hands ownership of the pages to the
  `HeapBuffer`, whose `Drop` releases them exactly once with the same layout;
- `write_bytes` zeroes the live allocation before any reference escapes;
- `kdma::deallocate_dma_memory` returns the same allocation in `Drop`.

The derive-generated unsafe is `bytemuck::AnyBitPattern` on `DmaBufSync`,
`DmaBufExportSyncFile`, `DmaBufImportSyncFile` and `DmaHeapAllocationData`.
Every bit pattern is valid for these `#[repr(C)]` types, there is no padding,
and the derive is what lets the typed user-copy helpers read them without a
hand-written layout claim.

## Known limitations

- `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` installs the sync_file descriptor before the
  result is copied back. A readable-but-not-writable argument page therefore
  returns `EFAULT` with the fd already installed. Linux copies out first and
  installs afterwards; this kernel has no reserve-then-install fd API, so the
  fix belongs to the owning exporter (`io/drmdevice`).
- The `*_SYNC_FILE` commands are exported as a raw `(cmd, arg)` hook on
  `DmaBuf` because the reservation they touch lives in the exporter. The hook
  exists until the fence layer is shared by more than one producer.

## Invariants relied on by callers

- `DmaBuf::window` reports memory the exporter owns and keeps alive for the
  object's lifetime; a caller that programs it keeps the export alive itself.
- `DmaBuf::attach` refuses an importer it cannot serve rather than returning a
  token that `map` will later accept.
- `ExportId` is unique among live exports; importers compare it to decide
  whether two attachments name the same buffer.
- `map_physical_owned` is called with the descriptor, so a mapping never
  outlives the memory it covers.
- `HeapBuffer::window` reports the page-aligned length every interface uses,
  and the allocation behind it is freed exactly once, when the last descriptor
  or mapping reference drops.
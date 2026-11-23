# dmabuf — Security

## Trust boundaries

Everything that arrives through a dma-buf descriptor's ioctls is untrusted
userspace input: the command argument pointer, the sync flags and the requested
mapping offset. Each is validated before use.

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

## Unsafe code

No hand-written `unsafe` blocks: user copies go through the typed `UserPtr`
helpers, and the physical window is plain data.

The only unsafe code is derive-generated: `DmaBufSync`,
`DmaBufExportSyncFile` and `DmaBufImportSyncFile` derive
`bytemuck::AnyBitPattern`, which emits `unsafe impl AnyBitPattern` for them.
Every bit pattern is valid for these `#[repr(C)]` types (two `u32`/`i32`
fields, or one `u64`), there is no padding, and the derive is what lets the
typed user-copy helpers read them without a hand-written layout claim.

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
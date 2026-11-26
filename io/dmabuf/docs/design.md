# dmabuf — Design

## Purpose

`dmabuf` is the kernel's shared-buffer layer: one allocation that a device
addresses, the CPU reads and writes, and userspace reaches through a file
descriptor.

```text
exporter ── implements ──▶ DmaBuf          identity, size, window, attachments,
                                           CPU-access hooks
descriptor ── holds ─────▶ Arc<dyn DmaBuf> fd, ioctls, lseek, mmap, poll, slot
importer ── lookup() ────▶ Arc<dyn DmaBuf> attach → map → program the device
```

The userspace ABI is Linux's (`include/uapi/linux/dma-buf.h`):

```text
mmap()                      → CPU access to the buffer
lseek(SEEK_END)             → buffer size
DMA_BUF_IOCTL_SYNC          → CPU access begin/end
DMA_BUF_IOCTL_SET_NAME      → diagnostics-only rename
DMA_BUF_IOCTL_EXPORT/IMPORT_SYNC_FILE → reservation bridge (exporters with fences)
```

## Where it sits

```text
arch/karch    cache-maintenance instructions
      ▲
io/kdma       DMA memory capability: contiguous pages, bus address,
              cacheability, bounce mappings
      ▲
io/dmabuf     this crate: the sharing object, its descriptor, and
      ▲       the cacheable heap exporter (`heap` feature)
      │
      ├── io/drmdevice     (exports GEM buffers)
      └── io/dmabuf::heap  (exports heap buffers)
```

The split is by ownership domain, not by filename:

- `kdma` answers *can a device reach this memory, and at which address*;
- `dmabuf` answers *who may share it, who keeps it alive, what userspace sees*;
- `karch` supplies the cache primitives; the **policy** — when to clean, when
  to invalidate — belongs to the exporter that chose a cacheable allocation;
- `io/drmdevice` keeps what only DRM has: a reservation that orders CPU access
  against the card's fences.

`io/` rather than `drivers/`: this crate owns a userspace device ABI and depends
on the VFS and on DMA memory. `drivers/` is the portable driver framework, whose
contract and driver crates must stay OS-neutral and must not implement
`FileOperations`.

## Object model

- **`DmaBuf`** — the contract an exporter implements. Only four things are
  mandatory: an `as_any` narrowing step, an `ExportId`, an exporter name and a
  byte size, plus the physical `window` the exporter's pages occupy.
- **Defaults carry the fence-less case.** `attach`/`detach` need no bookkeeping
  when one export serves every importer; `map` answers `window` when the
  address is attachment-independent; `begin/end_cpu_access` do nothing for a
  coherent buffer; `poll_state` reports a ready descriptor and `register_poll`
  arms nothing; `access_fence` has no fence to record; `sync_file_ioctl` answers
  `OperationNotSupported`. A coherent heap buffer therefore implements five
  methods and is correct; the cacheable heap exporter additionally overrides
  `begin/end_cpu_access`, and a GPU buffer overrides the ones its hardware
  needs.
- **`DmaBufAttachment`** names the export and the importer. An exporter with
  per-importer state (an IOMMU window, an address-space mapping) overrides
  `attach`/`detach`/`map`; the default attachment is a capacity-free token that
  holds no reference.
- **`MappedWindow`** is the physical window a mapping covers: `sg_table`'s
  single run for a contiguous buffer. It is two integers, so the importer must
  keep the `SharedDmaBuf` it resolved alive for as long as it programs the
  window.

## Ownership

The exporter owns the memory; every other holder owns a reference to the
descriptor (`Arc<DmaBufDescriptor>`), which in turn owns the object:

| Holder | Lives as long as |
|--------|------------------|
| open file description | the fd is open (one `Arc`) |
| user mapping | the VMA exists (`MmapMapper::map_physical_owned`) |
| importer | its object holds the `SharedDmaBuf` from `lookup` |

The attachment and the window are tokens, not references: only the importer's
own object can keep an export alive, which is what the in-tree importer
(`ForeignAttachment`, holding the export next to the token) does.

`munmap`, `execve` and exit drop exactly one reference per mapping; `fork`
clones the reference, not the buffer. The last reference runs the exporter's
`Drop`, which returns the pages exactly once.

This is also why the descriptor publishes ownership to the mapping: a buffer
freed while userspace still has it mapped is a use-after-free, and the fix
belongs at the mapping, not in every exporter.

## CPU access and cacheability

A buffer shared with userspace as ordinary memory must stay cacheable — two
cacheabilities for one physical page are unpredictable on AArch64 — so a
non-coherent device needs explicit maintenance:

- `DMA_BUF_SYNC(START|READ)` → `begin_cpu_access(Read)`: the exporter
  invalidates the CPU view and waits for outstanding device work;
- `DMA_BUF_SYNC(END|WRITE)` → `end_cpu_access(Write)`: the exporter cleans the
  CPU writes before the device reads.

The descriptor classifies the request and calls the object; it never touches a
cache itself. An exporter that allocated coherent (uncached) memory leaves the
defaults in place.

## Heap exporter

The `heap` feature adds the allocation half of the dma-buf ABI:

```text
open("/dev/dma_heap/system") → DMA_HEAP_IOCTL_ALLOC → dma-buf descriptor
mmap()/lseek()/DMA_BUF_IOCTL_SYNC on that descriptor
```

`heap::alloc` allocates a physically contiguous, **cacheable** run through
`kdma` and returns a `HeapBuffer` implementing `DmaBuf`. The descriptor,
`mmap`, `lseek` and the ioctls are the same ones every other exporter uses;
`devfs` owns the `/dev/dma_heap/{system,cma}` nodes and only selects the heap
name.

A request is rounded up to a whole page before it becomes the buffer's length,
as `dma_heap_buffer_alloc` does. `lseek(SEEK_END)`, `mmap` and `SYNC` therefore
speak about one length, and a sub-page request cannot expose a mapped tail that
`SYNC` never reaches. The full page-rounded allocation is zeroed and cleaned to
the Point of Coherency before the object is published, so userspace cannot
observe a previous owner's data and a later invalidation cannot revive it.

The fd's access mode comes from `fd_flags`: the descriptor is created with
`new_file_with_mode`. `READ` is always advertised because the VFS requires it
for any `mmap` of a regular file, and the access mode only gates `WRITE`, so an
`O_RDONLY` heap fd cannot establish a `MAP_SHARED | PROT_WRITE` mapping while
an `O_WRONLY` heap fd remains mappable. `O_CLOEXEC` is applied when the
descriptor is installed.

`system` and `cma` name the same allocator: the kernel has one pool of
physically contiguous DMA memory, so a separate CMA policy would be a name
without a difference. There is no per-heap flag; `heap_flags` must be zero.

## Importer entry point

`lookup(file)` reads the descriptor's `kvfs::DMA_BUF_SLOT`, so an importer
resolves any exporter's buffer without knowing which subsystem created the
descriptor — the kernel-side equivalent of following `file->f_op`. This is what
lets `PRIME_FD_TO_HANDLE` import a buffer this card did not export.

## Extension points

| Need | Where it goes |
|------|---------------|
| Heap allocation ABI (`/dev/dma_heap/*`) | the `heap` module (feature) over `kdma`; `devfs` owns the nodes |
| A portable exporter/importer | the `DmaBuf` trait moves to `drivers/contracts/dmabuf`; the descriptor stays here |
| A second fence producer | the fence layer moves to `sync/kfence`; `sync_file_ioctl` stops being a raw-command seam |
| IOMMU / scatter-gather | `window` becomes attachment-scoped, or `map` grows a multi-run form |

## Non-responsibilities

- The descriptor does no allocation: the exporter allocates (`kdma` for heap
  buffers, `kalloc` for GEM), this crate only shares it. The optional `heap`
  module is the one exporter that lives here, because it is the generic
  allocation ABI rather than a device driver.
- No fence framework: exporters that have reservations bring them.
- No per-heap policy, no IOMMU management.
- No `fstat`-based sizing decision: the descriptor sets the anonymous inode's
  size so `stat` and `SEEK_END` agree, as Linux does. The anonymous inode is
  shared with every other anonymous file descriptor, so that size is visible on
  those descriptors too until the inode becomes per-file; that follow-up belongs
  to `anon_inodefs`, not to this crate.

## Linux correspondence

| Linux | Here |
|-------|------|
| `struct dma_buf`, `dma_buf->size`, `exp_name` | `DmaBuf::{export_id, size_bytes, exporter_name}` |
| `struct dma_buf_attachment` | `DmaBufAttachment` |
| `dma_buf_attach` / `detach` / `map_attachment` | `DmaBuf::{attach, detach, map}` |
| `dma_buf_ops.begin_cpu_access` / `end_cpu_access` | `DmaBuf::{begin_cpu_access, end_cpu_access}` |
| `dma_buf_getfile` | `new_file`, `new_file_with_mode` |
| `dma-heap.c`, `dma_heap_allocation_data` | `heap`, `uapi::DMA_HEAP_IOCTL_ALLOC` |
| `dma_buf_get` | `lookup` |
| `dma_buf_ioctl`, `dma_buf_llseek`, `dma_buf_mmap_internal`, `dma_buf_poll` | `src/ioctl.rs`, `src/file.rs` |
| `DMA_BUF_IOCTL_*` | `uapi` (numbers pinned by assertions) |
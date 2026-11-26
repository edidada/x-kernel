// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! dma-buf: DMA buffers shared between devices and userspace through a file
//! descriptor.
//!
//! # What this crate owns
//!
//! One shared buffer has three faces, and this crate owns the two that are not
//! the memory itself:
//!
//! ```text
//! exporter ── implements ──▶ DmaBuf  (object: window, attachments, CPU access)
//! descriptor ── holds ─────▶ Arc<dyn DmaBuf>  (fd, ioctls, lseek, mmap, slot)
//! importer ── lookup() ───▶ Arc<dyn DmaBuf>  (attach → map → program device)
//! ```
//!
//! * [`object`]: the [`DmaBuf`] contract an exporter implements — identity,
//!   size, physical window, attachments, and the two CPU-access hooks.
//! * [`file`]: the descriptor — a dma-buf fd, its `lseek`/`mmap`/`poll`
//!   behaviour, and [`lookup`], the importer's door into any exporter's
//!   buffer.
//! * [`uapi`]: the ioctl numbers, structures and flags (Linux
//!   `include/uapi/linux/dma-buf.h`).
//!
//! # Where it sits
//!
//! ```text
//! arch/karch    cache-maintenance instructions
//!       ▲
//! io/kdma       DMA memory capability: contiguous pages, bus address,
//!               cacheability, bounce mappings
//!       ▲
//! io/dmabuf     this crate: the sharing object and its descriptor
//!       ▲                    ▲
//! io/drmdevice (exports GEM buffers)     io/dmabuf::heap (exports heap buffers)
//! ```
//!
//! The split follows ownership domains, not filenames:
//!
//! * `kdma` answers "can a device reach this memory, and at which address";
//! * this crate answers "who may share it, who keeps it alive, and what does
//!   userspace see";
//! * `karch` supplies the cache primitives; the *policy* (when to clean, when
//!   to invalidate) belongs to the exporter that asked for a cacheable buffer.
//!
//! It owns a userspace device-node ABI, depends on the VFS and on DMA memory,
//! and is consumed by `io/drmdevice` and `fs/devfs`, so it lives with the host
//! IO subsystems rather than in `drivers/`, whose crates must stay OS-neutral.
//! If a *portable* exporter ever needs the object contract, the trait moves to
//! `drivers/contracts/dmabuf` and this crate keeps the descriptor — a pure
//! move.
//!
//! # Ownership
//!
//! The exporter owns the memory; the descriptor reference keeps it alive:
//!
//! * the open file description holds one `Arc` of the descriptor;
//! * every user mapping holds one (`MmapMapper::map_physical_owned`), so
//!   `munmap` ends the last reference and `fork` clones it instead of the
//!   buffer.
//!
//! Dropping the last reference runs the exporter's `Drop`, which is where the
//! pages are returned exactly once.
//!
//! An importer is the one holder this crate cannot count: `DmaBufAttachment`
//! is a token that names the export, not a reference, and `MappedWindow` is
//! two integers. An importer must therefore keep the [`SharedDmaBuf`] it got
//! from [`lookup`] alive for as long as it programs the window.
//!
//! # CPU access and cacheability
//!
//! A buffer shared with userspace as ordinary memory is cacheable, so a
//! non-coherent device needs explicit maintenance. That maintenance is the
//! exporter's [`DmaBuf::begin_cpu_access`] / [`DmaBuf::end_cpu_access`]; the
//! descriptor only classifies the request and calls them. An exporter that
//! allocated coherent (uncached) memory leaves the defaults in place.
//!
//! # Extension points
//!
//! * [`heap`] (feature `heap`): the cacheable exporter over `kdma` and the
//!   `/dev/dma_heap` allocation ABI. `devfs` owns the device nodes; this
//!   module owns the buffer object and the ioctl numbers.
//! * Fences: exporters that keep a reservation implement
//!   [`DmaBuf::poll_state`], [`DmaBuf::register_poll`] and
//!   [`DmaBuf::sync_file_ioctl`]. Exporters without fences inherit correct
//!   defaults (always ready, no sync-file bridge).
//! * A portable object contract (`drivers/contracts/dmabuf`) once a portable
//!   exporter exists.
//!
//! # Non-goals
//!
//! No IOMMU mapping, no scatter-gather, no per-heap policy, and no fence
//! framework: today an exporter that needs fences already has its own, and a
//! kernel-wide fence layer is a separate ownership decision.

#![no_std]

extern crate alloc;

mod file;
#[cfg(feature = "heap")]
pub mod heap;
mod ioctl;
mod object;
pub mod uapi;

pub use file::{lookup, new_file, new_file_with_mode};
pub use object::{
    CpuAccess, DmaBuf, DmaBufAttachment, ExportId, FenceRef, ImporterId, MappedWindow, SharedDmaBuf,
};

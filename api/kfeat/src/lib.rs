// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Top-level feature selection for the XHyper EL2 runtime substrate.
//!
//! # Cargo Features
//!
//! - CPU
//!     - `smp`: Enable SMP (symmetric multiprocessing) support.
//!     - `fp-simd`: Enable floating point and SIMD support.
//! - Interrupts:
//!     - `irq`: Enable interrupt handling support.
//!     - `ipi`: Enable Inter-Processor Interrupts (IPIs).
//! - Memory
//!     - `alloc`: Enable dynamic memory allocation.
//!     - `alloc_tlsf`: Use the TLSF allocator.
//!     - `alloc_slab`: Use the slab allocator.
//!     - `alloc_buddy`: Use the buddy system allocator.
//!     - `paging`: Enable page table manipulation.
//!     - `tls`: Enable thread-local storage.
//!
//! Only the allocator, AArch64 platform, SMP, console, and RR scheduler are
//! aggregated here. POSIX, syscall, filesystem, network, TEE, and VMM business
//! crates are deliberately outside the image dependency graph.
#![no_std]

#[cfg(feature = "platform_kplat_aarch64")]
extern crate kplat_aarch64;

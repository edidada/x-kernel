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
//! Allocator, all four architecture platform crates, SMP, hardware driver
//! mappings, and the RR scheduler remain available here. POSIX, syscall,
//! filesystem, socket/process networking, TEE, and VMM business crates are
//! deliberately outside the default image dependency graph. Kernel-native
//! block and network device drivers remain selectable independently.
#![no_std]

#[cfg(feature = "platform_kplat_aarch64")]
extern crate kplat_aarch64;
#[cfg(feature = "platform_kplat_loongarch64")]
extern crate kplat_loongarch64;
#[cfg(feature = "platform_kplat_riscv64")]
extern crate kplat_riscv64;
#[cfg(feature = "platform_kplat_x86_64")]
extern crate kplat_x86_64;

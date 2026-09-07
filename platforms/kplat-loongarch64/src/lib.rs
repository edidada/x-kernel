// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#![cfg(target_arch = "loongarch64")]

#[macro_use]
extern crate log;
#[macro_use]
extern crate kplat;

// Secondary boot mailboxes do not yet provide the runtime Generic IPI
// protocol needed by SMP scheduling, TLB shootdown and CPU stop.
const _: () = assert!(
    kbuild_config::NR_CPUS == 1,
    "LoongArch runtime IPI is not implemented"
);

mod init;
mod irq;
#[cfg(feature = "smp")]
mod mp;
mod power;
mod time;

kplat::default_dma_if_impl!();
kplat::default_mmio_if_impl!();

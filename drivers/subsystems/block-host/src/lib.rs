// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Block-class dispatch SERVICES: host-side waiting, deferred completion
//! reclaiming and IRQ wiring.
//!
//! The block CONTRACTS (`BlockDeviceOperations`, `BlockWaiter`,
//! `BlockSignals`, `BlockCompletionOperations`) live in `subsys-block` and
//! stay host-free; this crate is the only place that may name `kirq`/`ktask`/
//! `kpoll`/`khal`. The split is what turns "contracts are host-free" from a
//! review rule into a compile-time property.

#![no_std]

extern crate alloc;

pub mod reclaimer;
pub mod wait;

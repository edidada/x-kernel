// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! driver-core serial drivers: one module per UART family.

#![no_std]

extern crate alloc;

mod common;

#[cfg(any(feature = "ns16550-mmio", feature = "ns16550-ioport"))]
pub mod ns16550;
#[cfg(feature = "pl011")]
pub mod pl011;

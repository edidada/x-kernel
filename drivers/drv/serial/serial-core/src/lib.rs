#![no_std]

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

// (doc comment must precede everything; keep the crate doc at top)

// The serial family library: per-instance UART ports over the PL011 and
// NS16550 (MMIO or IO-port) backends.
//
// One [`SerialPort`] per physical UART. The stdout port is constructed
// during platform early boot and handed to the driver-model serial driver
// through [`take_early_port`], so printk and the driver share the same
// hardware instance; auxiliary ports become standalone character devices.
// Emergency output uses immutable register addresses outside the normal
// port lock, so an NMI can print even after interrupting a lock holder.

mod ns16550_mmio;
mod serial;

pub use ns16550_mmio::SerialRegWidth;
// Cross-crate test fixtures (ktty's connection tests) — compiled only in
// the unittest build, the same cfg the path_mod_test! macro gates on.
#[cfg(unittest)]
pub use serial::test_support;
pub use serial::{
    SerialIdent, SerialPort, SerialRole, register_early_stdout, stdout_irq, stdout_port,
    take_early_port,
};

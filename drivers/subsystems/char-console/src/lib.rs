// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Console service over the char class.
//!
//! The boot console itself (device-tree stdout bring-up, the printk sink and
//! early console input) lives in `drivers/boot-console` and runs before the
//! driver model exists. This crate owns the model-side half that starts when
//! the adopted stdout port is published into [`subsys_char::class::CHAR`]:
//!
//! * the console view over the char class (which publications are console
//!   devices);
//! * the adopted stdout identity (`stdout_console`, `stdout_console_id`);
//! * ownership of the guards the stdout binding transfers with it
//!   (`adopt_stdout_guards`), so a console publication never outlives its
//!   input IRQ or register mapping.
//!
//! The split is the L0-b rule in practice: the contract lives in
//! `subsys-char`; this crate is the console service over it, and the boot
//! backend is a separate host-side tree pinned before `driver-init`.

#![no_std]

extern crate alloc;

pub mod runtime;

pub use runtime::{
    adopt_stdout_guards, register_stdout_console, stdout_console, stdout_console_id,
    unregister_console_device,
};

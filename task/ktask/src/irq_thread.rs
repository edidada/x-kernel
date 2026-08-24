// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! KIRQ threaded-handler task provider.

use alloc::{boxed::Box, string::String};

#[kiface::provide]
impl kirq::IrqThreadTaskIf {
    fn spawn_irq_thread(name: String, entry: Box<dyn FnOnce() + Send>) -> bool {
        crate::spawn_with_name(entry, name);
        true
    }
}

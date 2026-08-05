// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#![no_main]
#![doc = "XHyper EL2 runtime entry."]

#[macro_use]
extern crate klogger;

// Keep the feature aggregation crate linked into the final image.
extern crate kfeat;
extern crate kruntime;

mod image_metadata;

#[kiface::provide]
impl kruntime::SystemInitEntry {
    fn enter() {
        image_metadata::print();
        kprintln!("XHyper EL2 runtime initialized; entering scheduler idle loop");
        ktask::future::block_on(core::future::pending::<()>());
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `/proc/devices` and `/proc/bus/*` — the driver-core domain projection.

use alloc::{format, string::String, vec::Vec};
use core::fmt::Write;

use kvfs::{DirMapping, SeqFileInode, SeqIterator};

/// `/proc/devices`: one line per bound device, grouped by class view.
pub struct DevicesIter {
    text: String,
    done: bool,
}

impl DevicesIter {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            done: false,
        }
    }
}

impl SeqIterator for DevicesIter {
    type Item = String;

    fn start(&mut self) -> Option<Self::Item> {
        self.rewind();
        self.next()
    }

    fn rewind(&mut self) {
        self.text.clear();
        self.done = false;

        let mut block = Vec::new();
        let mut other = Vec::new();

        for device in driver_core::global_topology_summary() {
            let driver = device.driver.unwrap_or("<none>");
            let identity = device
                .identity
                .unwrap_or_else(|| format!("{:?}", device.bus));
            let line = format!(
                "  {:>3} {:<12} {} [{}]\n",
                device.id,
                device.bus.name(),
                identity,
                driver,
            );
            if device.state == driver_core::DeviceState::Active {
                block.push(line);
            } else {
                other.push(line);
            }
        }

        let s = &mut self.text;
        s.push_str("Active devices:\n");
        for line in block {
            s.push_str(&line);
        }
        s.push_str("\nOther devices:\n");
        for line in other {
            s.push_str(&line);
        }
    }

    fn next(&mut self) -> Option<String> {
        if self.done {
            return None;
        }
        self.done = true;
        Some(core::mem::take(&mut self.text))
    }

    fn show(&self, item: &String, buf: &mut String) -> core::fmt::Result {
        buf.push_str(item);
        Ok(())
    }
}

/// `/proc/bus/devices`: bus-grouped listing.
pub struct BusDevicesIter {
    text: String,
    done: bool,
}

impl BusDevicesIter {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            done: false,
        }
    }
}

impl SeqIterator for BusDevicesIter {
    type Item = String;

    fn start(&mut self) -> Option<Self::Item> {
        self.rewind();
        self.next()
    }

    fn rewind(&mut self) {
        self.text.clear();
        self.done = false;

        let s = &mut self.text;
        for device in driver_core::global_topology_summary() {
            let identity = device.identity.unwrap_or_default();
            let driver = device.driver.unwrap_or("<none>");
            let _ = core::writeln!(
                s,
                "{:<12} {:>3} {} [{}] driver={}",
                device.bus.name(),
                device.id,
                identity,
                device.state.as_str(),
                driver,
            );
        }
    }

    fn next(&mut self) -> Option<String> {
        if self.done {
            return None;
        }
        self.done = true;
        Some(core::mem::take(&mut self.text))
    }

    fn show(&self, item: &String, buf: &mut String) -> core::fmt::Result {
        buf.push_str(item);
        Ok(())
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: alloc::sync::Arc<kvfs::SimpleFs>) {
    root.add(
        "devices",
        SeqFileInode::new_regular(fs.clone(), || Ok(DevicesIter::new())),
    );
    root.add("bus", {
        let mut bus = kvfs::DirMapping::new();
        bus.add(
            "devices",
            SeqFileInode::new_regular(fs.clone(), || Ok(BusDevicesIter::new())),
        );
        kvfs::SimpleDir::new_maker(fs.clone(), alloc::sync::Arc::new(bus))
    });
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Dynamic devtmpfs projection for device-core node assignments.

use alloc::{borrow::Cow, boxed::Box, collections::BTreeMap, string::String, sync::Arc, vec::Vec};

#[cfg(feature = "input")]
use device_class::find_input_device;
use device_class::{ClassDeviceId, find_display_device};
use devnode::{DevNode, DevNodeEvent, subscribe};
use klazy::Once;
use ksync::Mutex;
use kvfs::{
    Dentry, DeviceId, SimpleDirLookup, SimpleDirOps, SimpleFs, VfsError, VfsResult, cdev_del,
};
use lazyinit::LazyInit;

#[cfg(feature = "input")]
use crate::nodes::event::EventDev;
use crate::{DeviceFile, device_dentry};

type DeviceRemovalCallback = Arc<dyn Fn() + Send + Sync>;

struct DevNodeRecord {
    node: DevNode,
    file: Arc<DeviceFile>,
    on_remove: DeviceRemovalCallback,
}

struct DeviceProjection {
    fs: Arc<SimpleFs>,
    records: BTreeMap<ClassDeviceId, DevNodeRecord>,
}

impl DeviceProjection {
    fn new(fs: Arc<SimpleFs>) -> Self {
        Self {
            fs,
            records: BTreeMap::new(),
        }
    }
}

static DEVICES: LazyInit<Mutex<DeviceProjection>> = LazyInit::new();
static DEVICE_INITIALIZATION: Once<()> = Once::new();

fn projection() -> &'static Mutex<DeviceProjection> {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Filesystem initialization installs the \
                  permanent devtmpfs projection before device callbacks"
    )]
    DEVICES
        .get()
        .expect("dynamic devtmpfs projection must initialize with its filesystem")
}

#[cfg(feature = "input")]
fn input_file(
    fs: Arc<SimpleFs>,
    number: DeviceId,
    id: ClassDeviceId,
) -> Option<(Arc<DeviceFile>, DeviceRemovalCallback)> {
    let device = find_input_device(id)?;
    // The endpoint keeps the driver's event queue draining even with no
    // open fd (main !887); the EventDev pairs view + endpoint.
    let endpoint = match inputdev::input_endpoint(device.clone()) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            warn!("devtmpfs: input endpoint for {id:?} failed: {error:?}");
            return None;
        }
    };
    let event = Arc::new(EventDev::new(device, endpoint));
    let weak_event = Arc::downgrade(&event);
    let on_remove: DeviceRemovalCallback = Arc::new(move || {
        if let Some(event) = weak_event.upgrade() {
            event.detach();
        }
    });
    Some((
        DeviceFile::new_character(fs, number, event).ok()?,
        on_remove,
    ))
}

fn display_file(
    fs: Arc<SimpleFs>,
    number: DeviceId,
    id: ClassDeviceId,
) -> Option<(Arc<DeviceFile>, DeviceRemovalCallback)> {
    let device = find_display_device(id)?;
    let card = drmdevice::DrmCard::new(device);
    let weak_card = Arc::downgrade(&card);
    let on_remove: DeviceRemovalCallback = Arc::new(move || {
        if let Some(card) = weak_card.upgrade() {
            card.notify_removed();
        }
    });
    Some((DeviceFile::new_character(fs, number, card).ok()?, on_remove))
}

fn node_key(node: &DevNode) -> ClassDeviceId {
    // Node ids ARE the publishing class device's raw id (kclass passes
    // `parent_ref().id().raw()`), so the record key must preserve it:
    // `find_input_device`/`find_display_device` look the CLASS registry up
    // by this id, and a hashed derivation would never match — the /dev
    // input/drm projections silently never materialized.
    ClassDeviceId::from_raw(node.id())
}

fn add_device(projection: &mut DeviceProjection, node: DevNode) {
    if projection.records.contains_key(&node_key(&node)) {
        return;
    }
    let number = node.device_number();
    let number = DeviceId::new(number.major(), number.minor());
    let file = match node.subsystem() {
        #[cfg(feature = "input")]
        "input" => input_file(projection.fs.clone(), number, node_key(&node)),
        "drm" => display_file(projection.fs.clone(), number, node_key(&node)),
        "block" => {
            let file = DeviceFile::new_block(projection.fs.clone(), number);
            Some((file, Arc::new(|| {}) as DeviceRemovalCallback))
        }
        _ => None,
    };
    let Some((file, on_remove)) = file else {
        warn!(
            "devtmpfs: class device for node {:?} is unavailable",
            node_key(&node)
        );
        return;
    };
    projection.records.insert(
        node_key(&node),
        DevNodeRecord {
            node,
            file,
            on_remove,
        },
    );
}

fn remove_device(node: &DevNode) {
    let record = {
        let mut projection = projection().lock();
        let Some(record) = projection.records.get(&node_key(node)) else {
            return;
        };
        if record.node != *node {
            return;
        }
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: The same locked projection map was \
                      checked for this node immediately before removal"
        )]
        projection
            .records
            .remove(&node_key(node))
            .expect("checked device projection must remain present")
    };
    (record.on_remove)();
    // Only character-class nodes registered through cdev_add; block nodes
    // carry just an inode (DeviceFile::new_block) — a cdev_del on their
    // dev_t would miss (warning spam) or, worse, evict an unrelated
    // character device that happens to share the number.
    if node.subsystem() != "block" {
        let number = node.device_number();
        if cdev_del(DeviceId::new(number.major(), number.minor()), 1).is_none() {
            warn!("devtmpfs: missing character device during removal: {number:?}");
        }
    }
}

fn handle_device_event(event: DevNodeEvent) {
    match event {
        DevNodeEvent::Added(node) => add_device(&mut projection().lock(), node),
        DevNodeEvent::Removing(_) => {}
        DevNodeEvent::Removed(node) => remove_device(&node),
    }
}

pub(crate) fn initialize(fs: Arc<SimpleFs>) {
    DEVICE_INITIALIZATION.call_once(|| {
        DEVICES.init_once(Mutex::new(DeviceProjection::new(fs)));

        let callback = Arc::new(handle_device_event);
        let mut projection = projection().lock();
        let nodes = subscribe(callback);
        for node in nodes {
            add_device(&mut projection, node);
        }
    });
}

#[derive(Clone, Copy)]
pub(crate) struct DynamicDeviceDir {
    subsystem: &'static str,
}

impl DynamicDeviceDir {
    pub(crate) const fn input() -> Self {
        Self { subsystem: "input" }
    }

    pub(crate) const fn drm() -> Self {
        Self { subsystem: "drm" }
    }

    fn local_name<'a>(&self, node: &'a DevNode) -> Option<&'a str> {
        if node.subsystem() != self.subsystem {
            return None;
        }
        node.devname().rsplit('/').next()
    }
}

impl SimpleDirOps for DynamicDeviceDir {
    fn child_names<'a>(&'a self) -> VfsResult<Box<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        let names: Vec<Cow<'a, str>> = projection()
            .lock()
            .records
            .values()
            .filter_map(|record| self.local_name(&record.node))
            .map(|name| Cow::Owned(String::from(name)))
            .collect();
        Ok(Box::new(names.into_iter()))
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        let file = projection()
            .lock()
            .records
            .values()
            .find(|record| self.local_name(&record.node) == Some(name))
            .map(|record| record.file.clone())
            .ok_or(VfsError::NotFound)?;
        Ok(device_dentry(lookup, name, file))
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }
}

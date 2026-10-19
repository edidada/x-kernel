// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux-compatible, read-mostly device discovery filesystem.
//!
//! This crate projects [`devnode::DevNode`] registrations into dynamic
//! `/sys/devices`, `/sys/class`, `/sys/dev/char`, and `/sys/bus` trees. It also
//! exposes device attributes and accepts validated user-triggered kobject
//! uevents for device-management clients.
//! Mount the filesystem through the VFS using [`FILE_SYSTEM_TYPE`]; callers do
//! not construct its singleton superblock directly.
//!
//! # Usage
//!
//! From userspace after the filesystem type has been registered:
//!
//! ```text
//! mount -t sysfs sysfs /sys
//! ```
//!
//! The architecture and trust boundaries are described in
//! `fs/filesystems/sysfs/docs/design.md` and
//! `fs/filesystems/sysfs/docs/security.md`.

#![no_std]

extern crate alloc;

use alloc::{
    borrow::Cow,
    boxed::Box,
    collections::BTreeMap,
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use devnode::{DevNode, DevNodeEvent, PublicationId, subscribe, uevent};
use klazy::Once;
use ksync::{Mutex, RwLock};
use kvfs::{
    CommandFile, Dentry, FileSystemType, FsContext, FsContextOperations, Mount, NodePermission,
    NodeType, SimpleDir, SimpleDirLookup, SimpleDirOps, SimpleFile, SimpleFileOperation, SimpleFs,
    SuperBlock, SuperBlockFlags, VfsError, VfsResult, get_tree_nodev,
};
use lazyinit::LazyInit;
use uevent_netlink::UeventAction;

const SYSFS_MAGIC: u32 = 0x6265_6572;
const READ_ONLY_PERMISSION: NodePermission = NodePermission::from_bits_truncate(0o444);
const UEVENT_PERMISSION: NodePermission = NodePermission::from_bits_truncate(0o644);

static SYSFS: Once<(Arc<SuperBlock>, Arc<Mount>)> = Once::new();
static DEVICE_PROJECTION_INITIALIZATION: Once<()> = Once::new();
static DEVICE_PROJECTION: LazyInit<Mutex<DeviceProjection>> = LazyInit::new();
static DEVICE_PROJECTION_EVENTS: LazyInit<Mutex<()>> = LazyInit::new();

struct ProjectedDevice {
    node: DevNode,
    access: RwLock<()>,
}

impl ProjectedDevice {
    fn new(node: DevNode) -> Self {
        Self {
            node,
            access: RwLock::new(()),
        }
    }

    fn with_available<R>(&self, operation: impl FnOnce(&DevNode) -> R) -> VfsResult<R> {
        if !self.node.is_available() {
            return Err(VfsError::NoSuchDevice);
        }
        let _access = self.access.read();
        if !self.node.is_available() {
            return Err(VfsError::NoSuchDevice);
        }
        Ok(operation(&self.node))
    }
}

struct DirectoryEntry {
    inode: Once<Arc<kvfs::VfsInode>>,
}

impl DirectoryEntry {
    const fn new() -> Self {
        Self { inode: Once::new() }
    }
}

#[derive(Clone)]
enum TreeEntry {
    Directory(Arc<DirectoryEntry>),
    /// A file owned by a registered character device (`class/<class>/<name>`).
    CharacterAttribute(kvfs::CharacterDeviceRegistration, Vec<u8>),
    Attribute(Arc<ProjectedDevice>, Vec<u8>),
    Symlink(String),
    Uevent(Arc<ProjectedDevice>),
}

impl TreeEntry {
    fn has_same_identity(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Directory(left), Self::Directory(right)) => Arc::ptr_eq(left, right),
            (
                Self::CharacterAttribute(left, left_data),
                Self::CharacterAttribute(right, right_data),
            ) => left.same_registration(right) && left_data == right_data,
            (Self::Attribute(left, left_data), Self::Attribute(right, right_data)) => {
                Arc::ptr_eq(left, right) && left_data == right_data
            }
            (Self::Symlink(left), Self::Symlink(right)) => left == right,
            (Self::Uevent(left), Self::Uevent(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }
}

struct DeviceProjection {
    nodes: BTreeMap<PublicationId, Arc<ProjectedDevice>>,
    characters: Vec<kvfs::CharacterDeviceRegistration>,
    tree: BTreeMap<String, TreeEntry>,
}

impl DeviceProjection {
    fn new() -> Self {
        let nodes = BTreeMap::new();
        let characters = Vec::new();
        let tree = build_tree(&nodes, &characters);
        Self {
            nodes,
            characters,
            tree,
        }
    }

    fn rebuild_tree(&mut self) -> Vec<String> {
        let mut tree = build_tree(&self.nodes, &self.characters);
        for (path, entry) in &mut tree {
            if let TreeEntry::Directory(directory) = entry
                && let Some(TreeEntry::Directory(previous)) = self.tree.get(path)
            {
                *directory = previous.clone();
            }
        }
        let changed = changed_paths(&self.tree, &tree);
        self.tree = tree;
        changed
    }
}

fn device_projection() -> &'static Mutex<DeviceProjection> {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Device projection initialization installs \
                  both permanent locks before callbacks or lookups are exposed"
    )]
    DEVICE_PROJECTION
        .get()
        .expect("sysfs device projection must initialize before lookup")
}

fn device_projection_events() -> &'static Mutex<()> {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Device projection initialization installs \
                  both permanent locks before callbacks or lookups are exposed"
    )]
    DEVICE_PROJECTION_EVENTS
        .get()
        .expect("sysfs device projection event serialization must initialize before callbacks")
}

/// Initializes the device-node projection independently of mounting sysfs.
///
/// Boot calls this before driver discovery so every lifecycle add event first
/// creates its sysfs projection. Mount-time initialization remains as an
/// idempotent fallback for nonstandard callers.
pub fn init_device_projection() {
    DEVICE_PROJECTION_INITIALIZATION.call_once(|| {
        DEVICE_PROJECTION.init_once(Mutex::new(DeviceProjection::new()));
        DEVICE_PROJECTION_EVENTS.init_once(Mutex::new(()));

        let mut projection = device_projection().lock();
        // devnode installs the subscriber and captures the snapshot under its
        // registry lock, but invokes callbacks only after releasing that lock.
        // Holding this mutex therefore serializes callbacks without creating a
        // lock cycle and prevents a stale snapshot from overtaking removal.
        let nodes = subscribe(Arc::new(handle_device_event));
        let characters = kvfs::subscribe_character_devices(Arc::new(handle_character_event));
        for node in nodes {
            projection
                .nodes
                .entry(node.id())
                .or_insert_with(|| Arc::new(ProjectedDevice::new(node)));
        }
        projection.characters = characters;
        projection.rebuild_tree();
        drop(projection);
    });
}

fn handle_character_event() {
    let _event = device_projection_events().lock();
    let mut projection = device_projection().lock();
    projection.characters = kvfs::registered_character_devices();
    let changed = projection.rebuild_tree();
    drop(projection);
    invalidate_changed_paths(&changed);
}

fn handle_device_event(event: DevNodeEvent) {
    let _event = device_projection_events().lock();
    match event {
        DevNodeEvent::Added(node) => {
            let mut projection = device_projection().lock();
            let is_same_publication = projection
                .nodes
                .get(&node.id())
                .is_some_and(|projected| projected.node == node);
            if !is_same_publication {
                projection
                    .nodes
                    .insert(node.id(), Arc::new(ProjectedDevice::new(node)));
            }
            let changed = projection.rebuild_tree();
            drop(projection);
            invalidate_changed_paths(&changed);
        }
        DevNodeEvent::Removing(node) => {
            let projected = device_projection()
                .lock()
                .nodes
                .get(&node.id())
                .filter(|projected| projected.node == node)
                .cloned();
            if let Some(projected) = projected {
                // devnode marks the node unavailable before this callback.
                // The exclusive guard waits for reads or uevent writes that
                // already passed their availability check.
                let _access = projected.access.write();
            }
        }
        DevNodeEvent::Removed(node) => {
            let mut projection = device_projection().lock();
            // A device with the same core ID may have been republished while
            // removal callbacks ran. Do not let the stale event delete its
            // replacement projection.
            if projection
                .nodes
                .get(&node.id())
                .is_some_and(|projected| projected.node == node)
            {
                projection.nodes.remove(&node.id());
                let changed = projection.rebuild_tree();
                drop(projection);
                invalidate_changed_paths(&changed);
            }
        }
    }
}

fn get_tree(
    context: &mut FsContext<'_>,
    _lookup_root: &kvfs::Path,
    _lookup_pwd: &kvfs::Path,
) -> VfsResult<Arc<SuperBlock>> {
    get_tree_nodev(context, |file_system_type, flags| {
        Ok(new_sysfs_with_type(file_system_type, flags))
    })
}

static FS_CONTEXT_OPERATIONS: FsContextOperations = FsContextOperations::new(get_tree);

fn init_fs_context(context: &mut FsContext<'_>) -> VfsResult<()> {
    context.set_operations(&FS_CONTEXT_OPERATIONS);
    Ok(())
}

/// Registered sysfs filesystem type.
pub static FILE_SYSTEM_TYPE: FileSystemType = FileSystemType::nodev("sysfs", init_fs_context);

#[macros::register_init]
fn init_sysfs() {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Each statically named filesystem is \
                  registered once by its unique linker initialization entry"
    )]
    kvfs::register_filesystem(&FILE_SYSTEM_TYPE).expect("sysfs filesystem type must register once");
}

fn new_sysfs_with_type(
    file_system_type: &'static FileSystemType,
    superblock_flags: SuperBlockFlags,
) -> Arc<SuperBlock> {
    init_device_projection();
    let (super_block, _internal_mount) = SYSFS.call_once(|| {
        let super_block = SimpleFs::new_with_superblock_flags(
            file_system_type,
            SYSFS_MAGIC,
            superblock_flags,
            |fs| {
                SimpleDir::new_maker(
                    fs.clone(),
                    Arc::new(SysfsDirectory {
                        fs,
                        path: String::new(),
                    }),
                )
            },
        );
        let internal_mount = Mount::new_root(&super_block);
        (super_block, internal_mount)
    });
    super_block.clone()
}

fn add_directory(tree: &mut BTreeMap<String, TreeEntry>, path: &str) {
    let mut current = String::new();
    for component in path.split('/').filter(|component| !component.is_empty()) {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(component);
        tree.entry(current.clone())
            .or_insert_with(|| TreeEntry::Directory(Arc::new(DirectoryEntry::new())));
    }
}

fn add_entry(tree: &mut BTreeMap<String, TreeEntry>, path: String, entry: TreeEntry) {
    if let Some((parent, _)) = path.rsplit_once('/') {
        add_directory(tree, parent);
    }
    tree.insert(path, entry);
}

fn changed_paths(
    previous: &BTreeMap<String, TreeEntry>,
    current: &BTreeMap<String, TreeEntry>,
) -> Vec<String> {
    let mut changed: Vec<String> = previous
        .iter()
        .filter(|(path, entry)| {
            current
                .get(*path)
                .is_none_or(|current| !entry.has_same_identity(current))
        })
        .map(|(path, _)| path.clone())
        .collect();
    changed.extend(
        current
            .keys()
            .filter(|path| !previous.contains_key(*path))
            .cloned(),
    );
    changed.sort_by(|left, right| {
        left.matches('/')
            .count()
            .cmp(&right.matches('/').count())
            .then(left.cmp(right))
    });

    let mut topmost = Vec::<String>::new();
    for path in changed {
        let has_changed_ancestor = topmost.iter().any(|ancestor| {
            path.strip_prefix(ancestor)
                .is_some_and(|remainder| remainder.starts_with('/'))
        });
        if !has_changed_ancestor {
            topmost.push(path);
        }
    }
    topmost
}

fn invalidate_changed_paths(paths: &[String]) {
    let Some((super_block, _internal_mount)) = SYSFS.get() else {
        return;
    };
    let root = super_block.root_dir();
    for path in paths {
        let mut parent = root.clone();
        let mut components = path.split('/').peekable();
        while let Some(component) = components.next() {
            if components.peek().is_none() {
                // Invalidation is idempotent; an absent cached entry is already the goal.
                let _ = parent.invalidate_cached_child(component);
                break;
            }
            let Some(child) = parent.lookup_cache(component) else {
                break;
            };
            if !child.is_really_positive() || !child.can_lookup() {
                // Invalidation is idempotent; an absent cached entry is already the goal.
                let _ = parent.invalidate_cached_child(component);
                break;
            }
            parent = child;
        }
    }
}

fn relative_symlink(source: &str, target: &str) -> String {
    let source_parent = source.rsplit_once('/').map_or("", |(parent, _)| parent);
    let source_components: Vec<&str> = source_parent
        .split('/')
        .filter(|component| !component.is_empty())
        .collect();
    let target_components: Vec<&str> = target
        .split('/')
        .filter(|component| !component.is_empty())
        .collect();
    let common_components = source_components
        .iter()
        .zip(&target_components)
        .take_while(|(source, target)| source == target)
        .count();

    let mut result = String::new();
    for _ in common_components..source_components.len() {
        result.push_str("../");
    }
    let target_remainder = target_components[common_components..].join("/");
    if target_remainder.is_empty() {
        result.pop();
    } else {
        result.push_str(&target_remainder);
    }
    if result.is_empty() {
        ".".to_string()
    } else {
        result
    }
}

fn bus_name(physical_path: &str) -> &'static str {
    if physical_path.starts_with("pci") {
        "pci"
    } else {
        "platform"
    }
}

fn append_line(value: &str) -> Vec<u8> {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(b'\n');
    bytes
}

fn bitmap_text(bytes: &[u8]) -> Vec<u8> {
    append_line(&uevent::bitmap_value(bytes))
}

fn add_common_device_entries(
    tree: &mut BTreeMap<String, TreeEntry>,
    projected: &Arc<ProjectedDevice>,
    leaf: &str,
) {
    let node = &projected.node;
    let number = node.device_number();
    add_entry(
        tree,
        format!("{leaf}/dev"),
        TreeEntry::Attribute(
            projected.clone(),
            append_line(&format!("{}:{}", number.major(), number.minor())),
        ),
    );
    add_entry(
        tree,
        format!("{leaf}/uevent"),
        TreeEntry::Uevent(projected.clone()),
    );
    let subsystem_link = format!("{leaf}/subsystem");
    add_entry(
        tree,
        subsystem_link.clone(),
        TreeEntry::Symlink(relative_symlink(
            &subsystem_link,
            &format!("class/{}", node.subsystem()),
        )),
    );
    let character_device_link = format!("dev/char/{}:{}", number.major(), number.minor());
    add_entry(
        tree,
        character_device_link.clone(),
        TreeEntry::Symlink(relative_symlink(&character_device_link, leaf)),
    );
}

fn add_input_entries(tree: &mut BTreeMap<String, TreeEntry>, projected: &Arc<ProjectedDevice>) {
    let node = &projected.node;
    // Both paths come from the node's canonical sysfs path (set by the
    // publisher, shared with the uevent DEVPATH): the event device IS that
    // path, the input parent is it minus the last segment. No local
    // re-derivation — a second path construction would drift again.
    let sysfs = node.sysfs_path();
    let event = format!("devices/{sysfs}");
    let input = match sysfs.rsplit_once('/') {
        Some((parent, _)) => format!("devices/{parent}"),
        None => event.clone(),
    };
    add_directory(tree, &event);
    add_directory(tree, &input);
    add_common_device_entries(tree, projected, &event);
    let event_device_link = format!("{event}/device");
    add_entry(
        tree,
        event_device_link.clone(),
        TreeEntry::Symlink(relative_symlink(&event_device_link, &input)),
    );
    // In-class names for the class links come from the canonical path's
    // segments: `.../inputN/eventN` → class/input/inputN and
    // class/input/eventN.
    let input_name = input.rsplit('/').next().unwrap_or(&input);
    let event_name = event.rsplit('/').next().unwrap_or(&event);
    let input_class_link = format!("class/input/{input_name}");
    add_entry(
        tree,
        input_class_link.clone(),
        TreeEntry::Symlink(relative_symlink(&input_class_link, &input)),
    );
    let event_class_link = format!("class/input/{event_name}");
    add_entry(
        tree,
        event_class_link.clone(),
        TreeEntry::Symlink(relative_symlink(&event_class_link, &event)),
    );
    let input_subsystem_link = format!("{input}/subsystem");
    add_entry(
        tree,
        input_subsystem_link.clone(),
        TreeEntry::Symlink(relative_symlink(&input_subsystem_link, "class/input")),
    );
    // Input-parent attributes come from the class properties the device_class
    // publisher already produces (single data source: NAME/PHYS/UNIQ,
    // PRODUCT = bustype/vendor/product/version hex, and the capability
    // bitmaps EV/KEY/REL/ABS/...). `sysfs:`-prefixed properties, when a
    // producer adds them later, take precedence for the same file.
    let mut file_attrs: Vec<(String, String)> = Vec::new();
    for (key, value) in node.properties() {
        match key.as_str() {
            "NAME" => file_attrs.push((String::from("name"), value.clone())),
            "PHYS" => file_attrs.push((String::from("phys"), value.clone())),
            "UNIQ" => file_attrs.push((String::from("uniq"), value.clone())),
            "PROP" => file_attrs.push((String::from("properties"), value.clone())),
            "EV" | "KEY" | "REL" | "ABS" | "MSC" | "SW" | "LED" | "SND" | "FF" | "FF_STATUS" => {
                file_attrs.push((
                    format!("capabilities/{}", key.to_lowercase()),
                    value.clone(),
                ));
            }
            "PRODUCT" => {
                // bustype/vendor/product/version, slash-separated hex.
                let parts: Vec<&str> = value.split('/').collect();
                if parts.len() == 4 {
                    for (file, part) in [
                        ("bustype", 0),
                        ("vendor", 1),
                        ("product", 2),
                        ("version", 3),
                    ] {
                        file_attrs.push((format!("id/{file}"), String::from(parts[part])));
                    }
                }
            }
            _ => {
                if let Some(file) = key.strip_prefix("sysfs:") {
                    file_attrs.push((String::from(file), value.clone()));
                }
            }
        }
    }
    for (file, value) in file_attrs {
        add_entry(
            tree,
            format!("{input}/{file}"),
            TreeEntry::Attribute(projected.clone(), append_line(&value)),
        );
    }
}

fn add_drm_entries(tree: &mut BTreeMap<String, TreeEntry>, projected: &Arc<ProjectedDevice>) {
    let node = &projected.node;
    let physical = node.physical_path();
    // The canonical sysfs path (`{phys}/drm/cardN`, shared with the uevent
    // DEVPATH) is carried by the node; the in-class name for the
    // `class/drm` link is its last segment.
    let card = format!("devices/{}", node.sysfs_path());
    let sysfs_name = node
        .sysfs_path()
        .rsplit('/')
        .next()
        .unwrap_or(node.sysfs_path());
    add_directory(tree, &card);
    add_common_device_entries(tree, projected, &card);
    let physical_device = format!("devices/{physical}");
    let card_device_link = format!("{card}/device");
    add_entry(
        tree,
        card_device_link.clone(),
        TreeEntry::Symlink(relative_symlink(&card_device_link, &physical_device)),
    );
    let class_link = format!("class/drm/{sysfs_name}");
    add_entry(
        tree,
        class_link.clone(),
        TreeEntry::Symlink(relative_symlink(&class_link, &card)),
    );
}

fn build_tree(
    nodes: &BTreeMap<PublicationId, Arc<ProjectedDevice>>,
    characters: &[kvfs::CharacterDeviceRegistration],
) -> BTreeMap<String, TreeEntry> {
    let mut tree = BTreeMap::new();
    for directory in [
        "devices",
        "class/input",
        "class/drm",
        "dev/char",
        "fs/cgroup",
        "fs/bpf",
        "bus/pci/drivers",
        "bus/platform/drivers",
    ] {
        add_directory(&mut tree, directory);
    }

    for registration in characters {
        if !registration.is_registered() {
            continue;
        }
        let device = registration.device_id();
        let class = format!("class/{}", registration.class());
        let leaf = format!(
            "devices/virtual/{}/{}",
            registration.class(),
            registration.name()
        );
        add_directory(&mut tree, &class);
        add_directory(&mut tree, &leaf);
        add_entry(
            &mut tree,
            format!("{leaf}/dev"),
            TreeEntry::CharacterAttribute(
                registration.clone(),
                append_line(&format!("{}:{}", device.major(), device.minor())),
            ),
        );
        add_entry(
            &mut tree,
            format!("{leaf}/uevent"),
            TreeEntry::CharacterAttribute(
                registration.clone(),
                format!(
                    "MAJOR={}\nMINOR={}\nDEVNAME={}\n",
                    device.major(),
                    device.minor(),
                    registration.name()
                )
                .into_bytes(),
            ),
        );
        for (source, target) in [
            (format!("{class}/{}", registration.name()), leaf.clone()),
            (
                format!("dev/char/{}:{}", device.major(), device.minor()),
                leaf.clone(),
            ),
            (format!("{leaf}/subsystem"), class),
        ] {
            add_entry(
                &mut tree,
                source.clone(),
                TreeEntry::Symlink(relative_symlink(&source, &target)),
            );
        }
    }

    for projected in nodes.values() {
        let node = &projected.node;
        let physical = node.physical_path();
        let bus = bus_name(node.physical_path());
        add_directory(&mut tree, &format!("devices/{physical}"));
        if let Some(driver) = node.driver_name() {
            add_directory(&mut tree, &format!("bus/{bus}/drivers/{driver}"));
        }
        let subsystem_link = format!("devices/{physical}/subsystem");
        add_entry(
            &mut tree,
            subsystem_link.clone(),
            TreeEntry::Symlink(relative_symlink(&subsystem_link, &format!("bus/{bus}"))),
        );
        let driver_link = format!("devices/{physical}/driver");
        add_entry(
            &mut tree,
            driver_link.clone(),
            TreeEntry::Symlink(relative_symlink(
                &driver_link,
                &format!(
                    "bus/{bus}/drivers/{}",
                    node.driver_name().unwrap_or("(none)")
                ),
            )),
        );
        match node.subsystem() {
            "input" => add_input_entries(&mut tree, projected),
            "drm" => add_drm_entries(&mut tree, projected),
            _ => {}
        }
    }
    tree
}

struct SysfsDirectory {
    fs: Arc<SimpleFs>,
    path: String,
}

impl DirectoryEntry {
    fn inode(&self, fs: Arc<SimpleFs>, path: String) -> Arc<kvfs::VfsInode> {
        self.inode
            .call_once(|| {
                SimpleDir::new_inode_with_owner(
                    fs.clone(),
                    Arc::new(SysfsDirectory { fs, path }),
                    NodePermission::from_bits_truncate(0o755),
                    0,
                    0,
                )
            })
            .clone()
    }
}

impl SysfsDirectory {
    fn child_path(&self, name: &str) -> String {
        if self.path.is_empty() {
            name.to_string()
        } else {
            format!("{}/{name}", self.path)
        }
    }
}

impl SimpleDirOps for SysfsDirectory {
    fn child_names<'a>(&'a self) -> VfsResult<Box<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        let prefix = if self.path.is_empty() {
            String::new()
        } else {
            format!("{}/", self.path)
        };
        let mut names = BTreeMap::<String, ()>::new();
        let projection = device_projection().lock();
        for path in projection.tree.keys() {
            let Some(remainder) = path.strip_prefix(&prefix) else {
                continue;
            };
            let name = remainder.split('/').next().unwrap_or(remainder);
            if !name.is_empty() {
                names.insert(name.to_string(), ());
            }
        }
        drop(projection);
        let names: Vec<Cow<'a, str>> = names.into_keys().map(Cow::Owned).collect();
        Ok(Box::new(names.into_iter()))
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        let path = self.child_path(name);
        let entry = device_projection()
            .lock()
            .tree
            .get(&path)
            .cloned()
            .ok_or(VfsError::NotFound)?;
        match entry {
            TreeEntry::CharacterAttribute(registration, contents) => lookup.file(
                name,
                SimpleFile::new_regular_with_permission(
                    self.fs.clone(),
                    READ_ONLY_PERMISSION,
                    move || {
                        if registration.is_registered() {
                            Ok(contents.clone())
                        } else {
                            Err(VfsError::NoSuchDevice)
                        }
                    },
                ),
            ),
            TreeEntry::Directory(directory) => {
                Ok(lookup.dir_from_inode(name, directory.inode(self.fs.clone(), path)))
            }
            TreeEntry::Attribute(projected, contents) => lookup.file(
                name,
                SimpleFile::new_regular_with_permission(
                    self.fs.clone(),
                    READ_ONLY_PERMISSION,
                    move || projected.with_available(|_| contents.clone()),
                ),
            ),
            TreeEntry::Symlink(target) => lookup.file(
                name,
                SimpleFile::new(self.fs.clone(), NodeType::Symlink, move || {
                    Ok(target.clone())
                }),
            ),
            TreeEntry::Uevent(projected) => {
                let file = CommandFile::new(move |operation| -> VfsResult<Option<Vec<u8>>> {
                    match operation {
                        SimpleFileOperation::Read => projected
                            .with_available(uevent::uevent_file_contents)
                            .map(Some),
                        SimpleFileOperation::Write { data, .. } => projected
                            .with_available(|node| -> VfsResult<()> {
                                let action = core::str::from_utf8(data)
                                    .map_err(|_| VfsError::InvalidInput)?
                                    .trim_matches(['\0', '\n', '\r', ' ']);
                                let action = UeventAction::try_from(action)
                                    .map_err(|()| VfsError::InvalidInput)?;
                                uevent_netlink::publish(action, node);
                                Ok(())
                            })
                            .and_then(|result| result)
                            .map(|()| None),
                    }
                });
                lookup.file(
                    name,
                    SimpleFile::new_regular_with_permission(
                        self.fs.clone(),
                        UEVENT_PERMISSION,
                        file,
                    ),
                )
            }
        }
    }
}

unittest::path_mod_test!(sysfs_unittest, "../ut-tests/lib.rs");

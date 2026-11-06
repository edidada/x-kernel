// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Address registration and lookup for Unix sockets.
//!
//! Each socket owns a registration guard that cannot be cloned. Handles returned
//! by lookup do not keep the address registered after the socket closes.

use alloc::sync::Arc;

use hashbrown::{HashMap, hash_map::Entry};
use kerrno::{KError, KResult};
use klazy::lazy_static;
use ksync::Mutex;
use kvfs::{
    DeviceId, Filename, LookupFlags, LookupIntent, NodePermission, NodeType, Path, Permission,
    WeakVfsInode,
};

use super::{UnixAddr, UnixSocketType, transport::BoundEndpoint};

type AbstractKey = (Arc<[u8]>, UnixSocketType);
// None marks a name reserved while its transport is being set up. Removing the
// table entry frees the name even if lookup handles still exist.
type Registration = Mutex<Option<BoundEndpoint>>;

struct PathEntry {
    inode: WeakVfsInode,
    registration: Arc<Registration>,
}

enum BindingKey {
    Abstract(AbstractKey),
    Path(usize),
}

/// Owns one address reservation and releases it if bind fails or the socket closes.
pub(super) struct BindingGuard {
    key: BindingKey,
    registration: Arc<Registration>,
}

impl BindingGuard {
    pub(super) fn publish(&self, endpoint: BoundEndpoint) {
        *self.registration.lock() = Some(endpoint);
    }
}

impl Drop for BindingGuard {
    fn drop(&mut self) {
        // Remove this registration only if it still owns the table entry.
        // Another socket may have reused the inode key or abstract name.
        match &self.key {
            BindingKey::Abstract(key) => {
                let mut bindings = ABSTRACT_BINDINGS.lock();
                if bindings
                    .get(key)
                    .is_some_and(|entry| Arc::ptr_eq(entry, &self.registration))
                {
                    bindings.remove(key);
                }
            }
            BindingKey::Path(key) => {
                let mut bindings = PATH_BINDINGS.lock();
                if bindings
                    .get(key)
                    .is_some_and(|entry| Arc::ptr_eq(&entry.registration, &self.registration))
                {
                    bindings.remove(key);
                }
            }
        }
        // Drop the endpoint after releasing the namespace locks. Earlier lookup
        // handles can still use it until the transport closes.
        let endpoint = self.registration.lock().take();
        drop(endpoint);
    }
}

lazy_static! {
    static ref ABSTRACT_BINDINGS: Mutex<HashMap<AbstractKey, Arc<Registration>>> =
        Mutex::new(HashMap::new());
    static ref PATH_BINDINGS: Mutex<HashMap<usize, PathEntry>> = Mutex::new(HashMap::new());
}

fn reserve_abstract(name: Arc<[u8]>, kind: UnixSocketType) -> KResult<BindingGuard> {
    let key = (name, kind);
    let registration = Arc::new(Mutex::new(None));
    match ABSTRACT_BINDINGS.lock().entry(key.clone()) {
        Entry::Occupied(_) => return Err(KError::AddrInUse),
        Entry::Vacant(entry) => {
            entry.insert(registration.clone());
        }
    }
    Ok(BindingGuard {
        key: BindingKey::Abstract(key),
        registration,
    })
}

fn create_bind_location(
    root: &Path,
    pwd: &Path,
    path: &str,
    mode: NodePermission,
    cred: &kcred::Cred,
) -> KResult<Path> {
    let (parent, name) = Filename::new(path)
        .parent_at(root, pwd, LookupIntent::Open, cred)?
        .into_normal()?;
    parent
        .mknod(&name, NodeType::Socket, mode, DeviceId::default(), cred)
        .map_err(|error| {
            if error.canonicalize() == KError::AlreadyExists {
                KError::AddrInUse
            } else {
                error
            }
        })
}

/// Reserves a name. Lookup can find its endpoint only after setup is complete.
pub(super) fn reserve(
    addr: &UnixAddr,
    kind: UnixSocketType,
    mode: NodePermission,
    cred: &kcred::Cred,
) -> KResult<BindingGuard> {
    let guard = match addr {
        UnixAddr::Unbound => return Err(KError::InvalidInput),
        UnixAddr::Abstract(name) => reserve_abstract(name.clone(), kind)?,
        UnixAddr::Path(path) => {
            let fs_context = kprocess::current_fs_context();
            let fs = fs_context.lock();
            let loc = create_bind_location(fs.root(), fs.pwd(), path, mode, cred)?;
            drop(fs);
            let key = loc.inode_key();
            let registration = Arc::new(Mutex::new(None));
            PATH_BINDINGS.lock().insert(
                key,
                PathEntry {
                    inode: loc.weak_inode(),
                    registration: registration.clone(),
                },
            );
            BindingGuard {
                key: BindingKey::Path(key),
                registration,
            }
        }
    };
    Ok(guard)
}

/// Returns a transport endpoint after releasing all filesystem and registry locks.
pub(super) fn lookup(
    addr: &UnixAddr,
    kind: UnixSocketType,
    cred: &kcred::Cred,
) -> KResult<BoundEndpoint> {
    let registration = match addr {
        UnixAddr::Unbound => return Err(KError::InvalidInput),
        UnixAddr::Abstract(name) => ABSTRACT_BINDINGS
            .lock()
            .get(&(name.clone(), kind))
            .cloned()
            .ok_or(KError::ConnectionRefused)?,
        UnixAddr::Path(path) => {
            let fs_context = kprocess::current_fs_context();
            let fs = fs_context.lock();
            let loc = Filename::new(path.as_ref()).lookup_at(
                fs.root(),
                fs.pwd(),
                LookupIntent::Open,
                LookupFlags::follow(),
                cred,
            )?;
            drop(fs);
            loc.permission(Permission::MAY_WRITE, cred)?;
            if loc.getattr()?.mode.node_type() != NodeType::Socket {
                return Err(KError::NotASocket);
            }
            let key = loc.inode_key();
            let mut bindings = PATH_BINDINGS.lock();
            let entry = bindings.get(&key).ok_or(KError::ConnectionRefused)?;
            if !entry
                .inode
                .upgrade()
                .is_some_and(|inode| Arc::ptr_eq(&inode, &loc.inode()))
            {
                bindings.remove(&key);
                return Err(KError::ConnectionRefused);
            }
            entry.registration.clone()
        }
    };
    let endpoint = registration
        .lock()
        .clone()
        .ok_or(KError::ConnectionRefused)?;
    Ok(endpoint)
}

unittest::path_mod_test!(tests, "../../ut-tests/unix/namespace/tests.rs");

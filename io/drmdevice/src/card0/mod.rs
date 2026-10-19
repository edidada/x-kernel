// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal DRM primary character device.
//!
//! Single-CRTC, single-connector, single-plane resource-backed DRM/KMS driver.
//! Covers legacy libdrm (`CREATE_DUMB → ADDFB2 → SETCRTC → PAGE_FLIP`) and the
//! atomic-KMS path (`MODE_ATOMIC` + blob properties) used by modern compositors.

use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use bytemuck::bytes_of;
use display::{DisplayDevice, ScanoutFormat, ScanoutRect, ScanoutResource};
use kalloc::GlobalPage;
use kclass::{ClassDevice, DisplayDeviceImpl};
use khal::mem::v2p;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::Mutex;
use kvfs::{
    DeviceFileOps, MmapMapper, NodeFlags, VfsError, VfsFile, VfsFileBuilder, VfsInode, VfsResult,
};
use memaddr::{PAGE_SIZE_4K, PhysAddrRange};
use posix_types::{UserPtr, UserRead, UserWrite};

use super::{consts::*, drm::*};
pub(crate) mod auth;
pub(crate) mod blob;
pub(crate) mod fb;
pub(crate) mod gem;
pub(crate) mod ids;
pub(crate) mod ioctl;
pub(crate) mod kms;
pub(crate) mod present;

pub(crate) use auth::*;
pub(crate) use blob::*;
pub(crate) use fb::*;
pub(crate) use gem::*;
pub(crate) use ids::*;
pub(crate) use ioctl::*;
pub(crate) use kms::*;
pub(crate) use present::*;

#[repr(transparent)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite)]
pub(crate) struct DrmAuthMagic(DrmAuth);

#[repr(transparent)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite)]
pub(crate) struct DrmPrimeFdToHandle(DrmPrimeHandle);

#[repr(transparent)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite)]
pub(crate) struct DrmModeSetCrtc(DrmModeCrtc);

#[repr(transparent)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite)]
pub(crate) struct DrmModeRmFb(u32);

// VERSION reports full lengths even for a length-only or truncated query.
// Capture the original capacity before updating the output length; this ioctl
// does not append a NUL terminator.
fn copy_version_field(user_ptr: UserPtr<u8>, capacity: usize, value: &str) -> VfsResult<usize> {
    let copy_len = capacity.min(value.len());
    if copy_len != 0 && !user_ptr.is_null() {
        user_ptr
            .write_vm_slice(&value.as_bytes()[..copy_len])
            .map_err(|_| VfsError::BadAddress)?;
    }
    Ok(value.len())
}

fn ioctl_drm_version(
    _dev: &DrmCard,
    _file: &Arc<DrmFile>,
    version: &mut DrmVersion,
) -> VfsResult<usize> {
    version.version_major = DRIVER_VERSION_MAJOR;
    version.version_minor = DRIVER_VERSION_MINOR;
    version.version_patchlevel = DRIVER_VERSION_PATCHLEVEL;
    version.name_len = copy_version_field(version.name, version.name_len, DRIVER_NAME)?;
    version.date_len = copy_version_field(version.date, version.date_len, DRIVER_DATE)?;
    version.desc_len = copy_version_field(version.desc, version.desc_len, DRIVER_DESC)?;
    Ok(0)
}

fn ioctl_drm_set_version(
    dev: &DrmCard,
    file: &Arc<DrmFile>,
    set_version: &mut DrmSetVersion,
) -> VfsResult<usize> {
    let requested = *set_version;
    // Like Linux drm_setversion's done path, return supported versions even
    // for lower-version requests and validation errors, not the input pair.
    set_version.drm_di_major = 1;
    set_version.drm_di_minor = 4;
    set_version.drm_dd_major = DRIVER_VERSION_MAJOR;
    set_version.drm_dd_minor = DRIVER_VERSION_MINOR;

    let masters = dev.masters.lock();
    if !masters
        .active
        .as_ref()
        .is_some_and(|active| Weak::ptr_eq(&active.owner, &Arc::downgrade(file)))
    {
        return Err(VfsError::PermissionDenied);
    }
    let binding = file.binding.lock();
    // Linux treats interface major -1 as a query and ignores its minor.
    // See drm_setversion in https://github.com/torvalds/linux/blob/master/drivers/gpu/drm/drm_ioctl.c.
    if requested.drm_di_major != -1 {
        if requested.drm_di_major != 1 || !(0..=4).contains(&requested.drm_di_minor) {
            return Err(VfsError::InvalidInput);
        }
        if requested.drm_di_minor >= 1 {
            *binding.master.unique.lock() = Some(format!("{}:0", DRIVER_NAME));
        }
    }
    // Linux establishes the bus identity before validating the driver
    // version, and still copies supported versions out on an error.
    if requested.drm_dd_major != -1
        && (requested.drm_dd_major != DRIVER_VERSION_MAJOR
            || requested.drm_dd_minor < 0
            || requested.drm_dd_minor > DRIVER_VERSION_MINOR)
    {
        return Err(VfsError::InvalidInput);
    }
    Ok(0)
}

fn ioctl_drm_unique(
    dev: &DrmCard,
    file: &Arc<DrmFile>,
    unique: &mut DrmUnique,
) -> VfsResult<usize> {
    let identity = {
        let _masters = dev.masters.lock();
        let binding = file.binding.lock();
        binding.master.unique.lock().clone()
    };
    let unique_str = identity.as_deref().unwrap_or("");
    // GET_UNIQUE copies only when the complete identity fits, unlike VERSION.
    if !unique_str.is_empty() && unique.unique_len >= unique_str.len() {
        unique
            .unique
            .write_vm_slice(unique_str.as_bytes())
            .map_err(|_| VfsError::BadAddress)?;
    }
    unique.unique_len = unique_str.len();
    Ok(0)
}

fn ioctl_drm_get_cap(
    _dev: &DrmCard,
    _file: &Arc<DrmFile>,
    cap: &mut DrmGetCap,
) -> VfsResult<usize> {
    cap.value = match cap.capability {
        DRM_CAP_DUMB_BUFFER => 1,
        // The driver exposes a single primary plane and no cursor
        // commands (the virtio-gpu cursor path is disabled), so these
        // are client-compatibility values: libdrm-based clients treat
        // 0 as "cursor unsupported" and bail out during init. See
        // DRM_CURSOR_COMPAT_SIZE in consts.rs.
        DRM_CAP_CURSOR_WIDTH | DRM_CAP_CURSOR_HEIGHT => DRM_CURSOR_COMPAT_SIZE,
        DRM_CAP_TIMESTAMP_MONOTONIC => 1,
        DRM_CAP_CRTC_IN_VBLANK_EVENT => 1,
        DRM_CAP_ADDFB2_MODIFIERS => 1,
        _ => 0,
    };
    Ok(0)
}

pub(crate) struct DrmFile {
    masters: Arc<Mutex<MasterState>>,
    binding: Mutex<MasterBinding>,
    /// Capabilities granted through `DRM_IOCTL_SET_CLIENT_CAP`, as a bitmask
    /// indexed by the Linux `DRM_CLIENT_CAP_*` value.
    ///
    /// Linux keeps this on `drm_file`, i.e. per open file description, and so
    /// does this driver: two clients of the same card must not share the
    /// `atomic` / `universal_planes` bits they negotiated independently.
    ///
    /// Only `DRM_CLIENT_CAP_ATOMIC` is read back, by `DrmModeAtomic`, which
    /// answers `EOPNOTSUPP` until the client has negotiated it.  The other
    /// accepted bits are recorded because clients branch on the answer to the
    /// setter, not because anything here acts on them: plane enumeration is the
    /// same with or without `DRM_CLIENT_CAP_UNIVERSAL_PLANES`.
    client_caps: AtomicU64,
}

impl Drop for DrmFile {
    fn drop(&mut self) {
        // Also rolls back ownership if the file builder is never published.
        let mut masters = self.masters.lock();
        if masters
            .active
            .as_ref()
            .is_some_and(|active| core::ptr::eq(active.owner.as_ptr(), self))
        {
            masters.active = None;
        }
    }
}

/// One DRM primary character device bound to one display backend.
pub struct DrmCard {
    display: ClassDevice<DisplayDeviceImpl>,
    masters: Arc<Mutex<MasterState>>,
    events: Mutex<VecDeque<DrmEventVblank>>,
    poll_rx: PollSet,
    sequence: AtomicU32,
    state: Mutex<ModesetState>,
    /// Serializes "publish the state, then show it" across the property entry
    /// points.
    ///
    /// `MODE_ATOMIC` and `set_one_property` both end with the same two steps:
    /// store the committed `ModesetState`, then present its `plane_fb_id`.
    /// Presenting after releasing `state` keeps the display call out of that
    /// lock, but it also lets two committers interleave so that the older
    /// framebuffer reaches the screen last; the scanout then keeps disagreeing
    /// with what `MODE_OBJ_GETPROPERTIES` reports, and `present_fb` cannot tell
    /// anyone because an unknown framebuffer is a silent no-op.
    ///
    /// Always taken before `state`, never while holding it, so the order is
    /// `commit_lock` -> `state` -> the inner maps `apply_prop` touches.
    commit_lock: Mutex<()>,
    /// The framebuffer `present_fb` last put on screen.
    ///
    /// `state.plane_fb_id` records what a client asked for, but `MODE_DIRTYFB`
    /// presents without touching it, so the binding alone cannot answer "is
    /// this framebuffer still on screen?".
    presented_fb: AtomicU32,
    /// The scanout resource `present_fb` last handed to the host, or 0.
    ///
    /// Retirement has to keep this resource alive: this driver has no "stop
    /// scanning" call, so the host keeps scanning the last resource it was
    /// given until another one is presented.  "No framebuffer references it"
    /// is therefore not enough on its own.
    presented_resource: AtomicU32,
    /// Resources whose scanout state is unknown.
    ///
    /// `present_fb` issues `TRANSFER_TO_HOST_2D`, `SET_SCANOUT` and
    /// `RESOURCE_FLUSH`; when one of them fails there is no way to tell how far
    /// the host got, so the new resource joins this set and the previous scanout
    /// target is still in `presented_resource`; both are held back from
    /// retirement.  A set rather than a slot, because several failed presents can
    /// pile up before one succeeds.  The next successful present settles all of
    /// them at once: this driver has a single scanout slot (`SET_SCANOUT` replaces
    /// the target), so nothing in the set can still be on screen afterwards.
    unsettled: Mutex<BTreeSet<u32>>,
    /// Dumb buffers whose handle was closed while a registered framebuffer
    /// still named their scanout resource.
    ///
    /// A framebuffer only copies the resource id out of the dumb buffer, so the
    /// resource and its backing have to outlive the handle.  Parked here and
    /// destroyed by the last framebuffer that references them (`RMFB` /
    /// `MODE_CLOSEFB`), which is the ordering the GEM object refcount gives
    /// Linux for free.
    retired_dumbs: Mutex<BTreeMap<u32, DumbBuffer>>,
    dumbs: Mutex<BTreeMap<DumbHandle, DumbBuffer>>,
    next_dumb_handle: AtomicU32,
    next_offset: AtomicU64,
    fbs: Mutex<BTreeMap<FbId, Framebuffer>>,
    next_fb_id: AtomicU32,
    next_resource_id: AtomicU32,
    blobs: Mutex<BTreeMap<BlobId, Arc<Vec<u8>>>>,
    mode_id_blob_ref: Mutex<Option<Arc<Vec<u8>>>>,
    next_blob_id: AtomicU32,
    system_blobs: Mutex<BTreeMap<BlobId, Arc<Vec<u8>>>>,
    in_formats_blob: AtomicU32,
    system_blobs_init: Mutex<()>,
    retained_pages: Mutex<BTreeMap<u64, Arc<GlobalPage>>>,
}

impl DrmCard {
    /// Create a new DRM device instance.
    pub fn new(display: ClassDevice<DisplayDeviceImpl>) -> Arc<Self> {
        Arc::new(Self {
            display,
            masters: Arc::new(Mutex::new(MasterState::default())),
            events: Mutex::new(VecDeque::with_capacity(MAX_EVENTS)),
            poll_rx: PollSet::new(),
            sequence: AtomicU32::new(0),
            state: Mutex::new(ModesetState::default()),
            commit_lock: Mutex::new(()),
            presented_fb: AtomicU32::new(0),
            presented_resource: AtomicU32::new(0),
            unsettled: Mutex::new(BTreeSet::new()),
            retired_dumbs: Mutex::new(BTreeMap::new()),
            dumbs: Mutex::new(BTreeMap::new()),
            next_dumb_handle: AtomicU32::new(FIRST_DUMB_HANDLE),
            next_offset: AtomicU64::new(DUMB_BUFFER_OFFSET_STRIDE),
            fbs: Mutex::new(BTreeMap::new()),
            next_fb_id: AtomicU32::new(FIRST_FB_ID),
            next_resource_id: AtomicU32::new(0x1000),
            blobs: Mutex::new(BTreeMap::new()),
            mode_id_blob_ref: Mutex::new(None),
            next_blob_id: AtomicU32::new(FIRST_BLOB_ID),
            system_blobs: Mutex::new(BTreeMap::new()),
            in_formats_blob: AtomicU32::new(0),
            system_blobs_init: Mutex::new(()),
            retained_pages: Mutex::new(BTreeMap::new()),
        })
    }

    /// Wake poll waiters after the bound display is removed.
    pub fn notify_removed(&self) {
        self.poll_rx.wake();
    }

    fn display(&self) -> VfsResult<&ClassDevice<DisplayDeviceImpl>> {
        self.display
            .is_available()
            .then_some(&self.display)
            .ok_or(VfsError::NoSuchDevice)
    }

    fn ensure_in_formats_blob(&self) -> u32 {
        let cur = self.in_formats_blob.load(Ordering::Acquire);
        if cur != 0 {
            return cur;
        }
        let _guard = self.system_blobs_init.lock();
        let cur = self.in_formats_blob.load(Ordering::Acquire);
        if cur != 0 {
            return cur;
        }
        let bytes = build_in_formats_blob();
        let id = BlobId::from_wire(self.next_blob_id.fetch_add(1, Ordering::Relaxed));
        self.system_blobs.lock().insert(id, Arc::new(bytes));
        self.in_formats_blob.store(id.wire(), Ordering::Release);
        id.wire()
    }
}

fn report_user_array<T: UserWrite + Copy>(
    user_ptr: UserPtr<T>,
    cap: u32,
    src: &[T],
) -> VfsResult<u32> {
    if !user_ptr.is_null() {
        let to_write = (cap as usize).min(src.len());
        user_ptr
            .write_vm_slice(&src[..to_write])
            .map_err(|_| VfsError::BadAddress)?;
    }
    Ok(src.len() as u32)
}

impl DeviceFileOps for DrmCard {
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        self.display()?;
        let mut masters = self.masters.lock();
        if masters
            .active
            .as_ref()
            .is_some_and(|active| active.owner.strong_count() == 0)
        {
            masters.active = None;
        }
        let is_master = masters.active.is_none();
        let master = match &masters.active {
            Some(active) => active.master.clone(),
            None => Arc::new(DrmMaster::default()),
        };
        let state = Arc::new(DrmFile {
            masters: self.masters.clone(),
            binding: Mutex::new(MasterBinding {
                master: master.clone(),
                is_master,
            }),
            client_caps: AtomicU64::new(0),
        });
        if is_master {
            masters.active = Some(ActiveMaster {
                master,
                owner: Arc::downgrade(&state),
            });
        }
        file.set_private_data(state);
        Ok(())
    }

    fn release(&self, _inode: &VfsInode, file: &VfsFile) -> VfsResult<()> {
        let state = file
            .private_data_get::<DrmFile>()
            .ok_or(VfsError::InvalidInput)?;
        let mut masters = self.masters.lock();
        if masters
            .active
            .as_ref()
            .is_some_and(|active| Weak::ptr_eq(&active.owner, &Arc::downgrade(&state)))
        {
            masters.active = None;
        }
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        self.display()?;
        if buf.is_empty() {
            return Ok(0);
        }
        let evsz = core::mem::size_of::<DrmEventVblank>();
        if buf.len() < evsz {
            return Err(VfsError::InvalidInput);
        }
        let mut events = self.events.lock();
        let mut written = 0;
        while written + evsz <= buf.len() {
            let Some(ev) = events.pop_front() else {
                break;
            };
            buf[written..written + evsz].copy_from_slice(bytes_of(&ev));
            written += evsz;
        }
        if written == 0 {
            Err(VfsError::WouldBlock)
        } else {
            Ok(written)
        }
    }

    fn ioctl(&self, file: &VfsFile, _cred: &kcred::Cred, cmd: u32, arg: usize) -> VfsResult<usize> {
        self.display()?;
        let state = file
            .private_data_get::<DrmFile>()
            .ok_or(VfsError::InvalidInput)?;
        match cmd {
            DRM_IOCTL_SET_MASTER => {
                let mut masters = self.masters.lock();
                if let Some(active) = &masters.active {
                    return if Weak::ptr_eq(&active.owner, &Arc::downgrade(&state)) {
                        Ok(0)
                    } else {
                        Err(VfsError::ResourceBusy)
                    };
                }
                let mut binding = state.binding.lock();
                if !binding.is_master {
                    binding.master = Arc::new(DrmMaster::default());
                    binding.is_master = true;
                }
                masters.active = Some(ActiveMaster {
                    master: binding.master.clone(),
                    owner: Arc::downgrade(&state),
                });
                Ok(0)
            }
            DRM_IOCTL_DROP_MASTER => {
                let mut masters = self.masters.lock();
                if !masters
                    .active
                    .as_ref()
                    .is_some_and(|active| Weak::ptr_eq(&active.owner, &Arc::downgrade(&state)))
                {
                    return Err(VfsError::InvalidInput);
                }
                masters.active = None;
                Ok(0)
            }
            cmd => drm_uniform_ioctl(self, &state, cmd, arg),
        }
    }

    fn mmap(&self, _file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        self.display()?;
        let offset = mapper.offset() as u64;
        let dumbs = self.dumbs.lock();
        let Some(b) = dumbs.values().find(|b| b.offset == offset) else {
            return Err(VfsError::InvalidInput);
        };
        let phys = v2p(b.pages.start_va());
        let size = b.pages.size();
        self.retained_pages.lock().insert(offset, b.pages.clone());
        let range = PhysAddrRange::from_start_size(
            memaddr::PhysAddr::from(phys.as_usize().wrapping_sub(offset as usize)),
            size + offset as usize,
        );
        mapper.map_physical(range)
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }

    fn poll(&self, _file: &VfsFile) -> IoEvents {
        Pollable::poll(self)
    }

    fn register_poll(
        &self,
        _file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        Pollable::register(self, context, events)
    }
}

impl Pollable for DrmCard {
    fn poll(&self) -> IoEvents {
        if self.display().is_err() {
            return IoEvents::ERR | IoEvents::HUP;
        }
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, !self.events.lock().is_empty());
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.poll_rx)?;
        }
        Ok(())
    }
}

const fn enum_entry(value: u64, name: &[u8]) -> DrmModePropertyEnum {
    let mut e = DrmModePropertyEnum {
        value,
        name: [0; DRM_PROP_NAME_LEN],
    };
    let n = if name.len() < DRM_PROP_NAME_LEN - 1 {
        name.len()
    } else {
        DRM_PROP_NAME_LEN - 1
    };
    let mut i = 0;
    while i < n {
        e.name[i] = name[i];
        i += 1;
    }
    e
}

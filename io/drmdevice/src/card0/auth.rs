// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! auth subsystem of the virtual DRM card, split out of card0.rs.

use super::*;

pub(crate) fn ioctl_drm_set_client_cap(
    _dev: &DrmCard,
    file: &Arc<DrmFile>,
    cap: &mut DrmSetClientCap,
) -> VfsResult<usize> {
    // Mirror Linux `drm_setclientcap()`: reject an unknown capability with
    // EOPNOTSUPP and an out-of-range boolean with EINVAL, then remember what
    // was granted.
    //
    // Answering Ok(0) to everything - which is what this used to do - turns
    // every "does the driver support X?" probe into a false positive. Clients
    // legitimately branch on this: a libdrm client that is told `atomic` was
    // accepted will go on to issue atomic commits.  The granted bits are read
    // back by `DrmModeAtomic`; see `DrmFile::client_caps` for why the other
    // two are recorded rather than enforced.
    //
    // `STEREO_3D` and `UNIVERSAL_PLANES` are accepted even though nothing here
    // implements stereoscopy or gated plane enumeration.  Linux's
    // `drm_setclientcap()` does not ask the driver either: both are per-file
    // client flags that govern what the client itself may send, so a driver
    // that answered EOPNOTSUPP would be the one out of spec -- and a client
    // that probes them before doing anything else would give up.  Only
    // `ATOMIC` changes what this kernel acts on.
    match cap.capability {
        DRM_CLIENT_CAP_STEREO_3D | DRM_CLIENT_CAP_UNIVERSAL_PLANES | DRM_CLIENT_CAP_ATOMIC => {}
        _ => return Err(VfsError::OperationNotSupported),
    }
    if cap.value > 1 {
        return Err(VfsError::InvalidInput);
    }
    // Capability values are small and defined above, so the shift is in
    // range by construction.
    let bit = 1u64 << cap.capability;
    if cap.value == 1 {
        file.client_caps.fetch_or(bit, Ordering::Relaxed);
    } else {
        file.client_caps.fetch_and(!bit, Ordering::Relaxed);
    }
    Ok(0)
}

pub(crate) fn ioctl_drm_auth(
    _dev: &DrmCard,
    _file: &Arc<DrmFile>,
    auth: &mut DrmAuth,
) -> VfsResult<usize> {
    auth.magic = 1;
    Ok(0)
}

pub(crate) fn ioctl_drm_auth_magic(
    _dev: &DrmCard,
    _file: &Arc<DrmFile>,
    _auth: &mut DrmAuthMagic,
) -> VfsResult<usize> {
    Ok(0)
}

#[derive(Default)]
pub(crate) struct DrmMaster {
    pub(crate) unique: Mutex<Option<String>>,
}

pub(crate) struct MasterBinding {
    pub(crate) master: Arc<DrmMaster>,
    // A former owner can reacquire its master after DROP_MASTER.
    pub(crate) is_master: bool,
}

pub(crate) struct ActiveMaster {
    pub(crate) master: Arc<DrmMaster>,
    pub(crate) owner: Weak<DrmFile>,
}

#[derive(Default)]
pub(crate) struct MasterState {
    pub(crate) active: Option<ActiveMaster>,
}

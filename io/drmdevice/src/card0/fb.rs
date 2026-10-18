// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! fb subsystem of the virtual DRM card, split out of card0.rs.

use super::*;

pub(crate) fn ioctl_drm_mode_dirty_fb(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    dirty: &mut DrmModeDirtyFb,
) -> VfsResult<usize> {
    // Linux's drm_mode_dirtyfb_ioctl() reports an unknown fb_id as -ENOENT
    // (drm_framebuffer_lookup), and callers distinguish the two: Xorg keeps
    // its damage tracking enabled only when the generic path answers with
    // ENOENT rather than EINVAL. present_fb is best-effort, so validate the
    // identifier before calling it.
    let fb_id = FbId::from_wire(dirty.fb_id);
    if !dev.fbs.lock().contains_key(&fb_id) {
        return Err(VfsError::NotFound);
    }
    dev.present_fb(fb_id);
    Ok(0)
}

pub(crate) struct Framebuffer {
    pub(crate) resource_id: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

pub(crate) fn ioctl_drm_mode_rm_fb(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    fb_id: &mut DrmModeRmFb,
) -> VfsResult<usize> {
    dev.fbs.lock().remove(&FbId::from_wire(fb_id.0));
    {
        let mut legacy = dev.legacy_crtc.lock();
        if legacy.fb_id == fb_id.0 {
            *legacy = LegacyCrtcState::default();
        }
    }
    Ok(0)
}

pub(crate) fn ioctl_drm_mode_fb_cmd(
    dev: &DrmCard,
    file: &Arc<DrmFile>,
    f: &mut DrmModeFbCmd,
) -> VfsResult<usize> {
    // Legacy ADDFB. Translate the request into the modern form and reuse
    // that implementation, so the two entry points cannot drift apart.
    //
    // bpp 24 is accepted on purpose: `DrmModeCreateDumb` backs every request
    // with a 32bpp BGRA buffer and reports `pitch = width * 4`, and callers
    // hand that same bpp straight back here.
    //
    // Unlike ADDFB2 there is no pixel_format field; the legacy ABI implies
    // XRGB8888 for depth 24, which is what the driver implements.
    // ADDFB always registers XRGB8888 below, so only accept the depths that
    // format actually describes: 24 (RGB) and 32 (XRGB with a spare byte).
    if f.handle == 0 || !matches!(f.bpp, 24 | 32) || !matches!(f.depth, 24 | 32) {
        return Err(VfsError::InvalidInput);
    }
    let mut modern = DrmModeFbCmd2 {
        fb_id: f.fb_id,
        width: f.width,
        height: f.height,
        pixel_format: DRM_FORMAT_XRGB8888,
        flags: 0,
        handles: [f.handle, 0, 0, 0],
        pitches: [f.pitch, 0, 0, 0],
        offsets: [0; 4],
        abi_pad: 0,
        modifier: [0; 4],
    };
    <DrmModeFbCmd2 as DrmIoctl>::handle(dev, file, &mut modern)?;
    f.fb_id = modern.fb_id;
    Ok(0)
}

pub(crate) fn ioctl_drm_mode_fb_cmd2(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    f: &mut DrmModeFbCmd2,
) -> VfsResult<usize> {
    let handle = f.handles[0];
    if handle == 0 || f.offsets[0] != 0 {
        return Err(VfsError::InvalidInput);
    }
    for i in 1..4 {
        if f.handles[i] != 0 || f.pitches[i] != 0 || f.offsets[i] != 0 {
            return Err(VfsError::InvalidInput);
        }
    }
    if !matches!(f.pixel_format, DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888) {
        return Err(VfsError::InvalidInput);
    }
    let (resource_id, width, height, pitch) = {
        let dumbs = dev.dumbs.lock();
        let Some(b) = dumbs.get(&DumbHandle::from_wire(handle)) else {
            return Err(VfsError::InvalidInput);
        };
        (b.resource_id, b.width, b.height, b.pitch)
    };
    // The scanout resource is created at exactly the dumb buffer's size
    // and pitch (no ANY_LAYOUT negotiation), and TRANSFER_TO_HOST_2D reads
    // the backing row-by-row with the resource pitch. A sub-size
    // framebuffer would be misread (garbled scanout) or rejected by the
    // device, so the framebuffer must match the resource layout exactly.
    if f.width != width || f.height != height || f.pitches[0] != pitch {
        return Err(VfsError::InvalidInput);
    }
    if f.flags & DRM_MODE_FB_MODIFIERS != 0 {
        for i in 0..4 {
            if f.handles[i] == 0 {
                continue;
            }
            let m = f.modifier[i];
            if m != DRM_FORMAT_MOD_LINEAR && m != DRM_FORMAT_MOD_INVALID {
                return Err(VfsError::InvalidInput);
            }
        }
    }
    let fb_id = FbId::from_wire(dev.next_fb_id.fetch_add(1, Ordering::Relaxed));
    dev.fbs.lock().insert(
        fb_id,
        Framebuffer {
            resource_id,
            width: f.width,
            height: f.height,
        },
    );
    f.fb_id = fb_id.wire();
    Ok(0)
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! fb subsystem of the virtual DRM card, split out of card0.rs.

use super::*;

impl DrmIoctl for DrmModeDirtyFb {
    const CMD: u32 = iowr::<DrmModeDirtyFb>(DRM_TYPE, 0xB1);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, dirty: &mut Self) -> VfsResult<usize> {
        // Linux's drm_mode_dirtyfb_ioctl() reports an unknown fb_id as -ENOENT
        // (drm_framebuffer_lookup), and callers distinguish the two: Xorg keeps
        // its damage tracking enabled only when the generic path answers with
        // ENOENT rather than EINVAL. present_fb is best-effort, so validate the
        // identifier before calling it.
        //
        // Serialize with `MODE_CLOSEFB`: it decides "still on screen" partly from
        // the `presented_fb` marker, which the present below updates.
        let _commit = dev.commit_lock.lock();
        let fb_id = FbId::from_wire(dirty.fb_id);
        if !dev.fbs.lock().contains_key(&fb_id) {
            return Err(VfsError::NotFound);
        }
        dev.present_fb(fb_id);
        Ok(0)
    }
}

pub(crate) struct Framebuffer {
    pub(crate) resource_id: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl DrmIoctl for DrmModeRmFb {
    const CMD: u32 = iowr::<u32>(DRM_TYPE, 0xAF);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, fb_id: &mut Self) -> VfsResult<usize> {
        let fb_id = FbId::from_wire(fb_id.0);
        // Same lock as `MODE_CLOSEFB`: the retirement below reads the scanout
        // markers, which a concurrent present updates under this lock.
        let _commit = dev.commit_lock.lock();
        let removed = dev.fbs.lock().remove(&fb_id);
        // Detach the committed plane binding without presenting: the framebuffer
        // is already gone, so there is nothing left to put on screen, and `RMFB`
        // does not refresh a different framebuffer that may still be bound.
        let mut state = dev.state.lock();
        if state.plane_fb_id == fb_id.wire() {
            state.plane_fb_id = 0;
        }
        drop(state);
        // This may have been the last framebuffer naming a dumb buffer whose
        // handle was closed earlier; that is when its resource is destroyed.
        if let Some(fb) = removed {
            retire_unused_dumb(dev, fb.resource_id);
        }
        Ok(0)
    }
}

impl DrmIoctl for DrmModeCloseFb {
    // DRM_IOCTL_MODE_CLOSEFB = DRM_IOWR(0xD0, struct drm_mode_closefb): 8 bytes.
    const CMD: u32 = iowr::<DrmModeCloseFb>(DRM_TYPE, 0xD0);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, args: &mut Self) -> VfsResult<usize> {
        // `drm_mode_closefb_ioctl()`: a non-zero `pad` is EINVAL, an unknown
        // framebuffer is ENOENT, and a framebuffer that is still on screen is
        // EBUSY.  That in-use check is the whole difference from RMFB (0xAF),
        // which this driver implements as an unconditional removal: CLOSEFB
        // tells a client its framebuffer is still on screen instead of pulling
        // the scanout out from under it.
        //
        // "On screen" has two sources: the plane binding committed through
        // `MODE_SETCRTC` / `MODE_PAGE_FLIP` / `MODE_ATOMIC`, and whatever
        // `present_fb` last put up -- `MODE_DIRTYFB` presents without committing
        // a binding.
        if args.pad != 0 {
            return Err(VfsError::InvalidInput);
        }
        // `MODE_SETCRTC` and the commit path take this lock too, so the binding
        // cannot change between the checks below and the removal.
        let _commit = dev.commit_lock.lock();
        let fb_id = FbId::from_wire(args.fb_id);
        let Some(resource_id) = dev.fbs.lock().get(&fb_id).map(|fb| fb.resource_id) else {
            return Err(VfsError::NotFound);
        };
        let bound = dev.state.lock().plane_fb_id == args.fb_id;
        let presented = dev.presented_fb.load(Ordering::Relaxed) == args.fb_id;
        if bound || presented {
            return Err(VfsError::ResourceBusy);
        }
        dev.fbs.lock().remove(&fb_id);
        // May have been the last framebuffer naming a parked dumb buffer.
        retire_unused_dumb(dev, resource_id);
        Ok(0)
    }
}

impl DrmIoctl for DrmModeFbCmd {
    const CMD: u32 = iowr::<DrmModeFbCmd>(DRM_TYPE, 0xAE);

    fn handle(dev: &DrmCard, file: &Arc<DrmFile>, f: &mut Self) -> VfsResult<usize> {
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
}

impl DrmIoctl for DrmModeFbCmd2 {
    const CMD: u32 = iowr::<DrmModeFbCmd2>(DRM_TYPE, 0xB8);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, f: &mut Self) -> VfsResult<usize> {
        let handle = f.handles[0];
        // The dumb handle is read here and the framebuffer is registered further
        // down; `GEM_CLOSE` / `DESTROY_DUMB` take the same lock, so the handle
        // cannot be closed (and its resource destroyed) in between.
        let _commit = dev.commit_lock.lock();
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
}

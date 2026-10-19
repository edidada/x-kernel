// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! gem subsystem of the virtual DRM card, split out of card0.rs.

use super::*;

pub(crate) fn ioctl_drm_prime_handle(
    _dev: &DrmCard,
    _file: &Arc<DrmFile>,
    prime: &mut DrmPrimeHandle,
) -> VfsResult<usize> {
    prime.fd = prime.handle as i32;
    Ok(0)
}

pub(crate) fn ioctl_drm_prime_fd_to_handle(
    _dev: &DrmCard,
    _file: &Arc<DrmFile>,
    prime: &mut DrmPrimeFdToHandle,
) -> VfsResult<usize> {
    prime.0.handle = prime.0.fd as u32;
    Ok(0)
}

pub(crate) struct DumbBuffer {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pitch: u32,
    pub(crate) offset: u64,
    pub(crate) resource_id: u32,
    pub(crate) pages: Arc<GlobalPage>,
}

pub(crate) fn ioctl_drm_mode_create_dumb(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    c: &mut DrmModeCreateDumb,
) -> VfsResult<usize> {
    // The backing store is always BGRA8888 (32bpp) and `bytes_per_pixel` is
    // therefore constant, but 24bpp requests must still be accepted: Linux's
    // generic dumb-buffer ioctl accepts them, and libdrm-based clients on
    // the non-atomic path (Xorg's modesetting driver) ask for 24bpp and then
    // hand that same value back to `ADDFB`. Rejecting it fails their
    // initialisation before any framebuffer exists.
    //
    // `bpp == 0`, `bpp > 64` and the multiple-of-8 rule are still enforced,
    // because the depth comes from user space and an unknown depth cannot be
    // described to the scanout device.
    if c.width == 0 || c.height == 0 || c.flags != 0 || !matches!(c.bpp, 24 | 32) {
        return Err(VfsError::InvalidInput);
    }
    if c.width > 16384 || c.height > 16384 {
        return Err(VfsError::InvalidInput);
    }
    let bytes_per_pixel = 4;
    let pitch = c
        .width
        .checked_mul(bytes_per_pixel)
        .ok_or(VfsError::InvalidInput)?;
    let size = (pitch as u64)
        .checked_mul(c.height as u64)
        .ok_or(VfsError::InvalidInput)?;
    if size as usize > DUMB_BUFFER_MAX_SIZE {
        return Err(VfsError::NoMemory);
    }
    c.pitch = pitch;
    c.size = size;
    let size_aligned = (size as usize).next_multiple_of(PAGE_SIZE_4K);
    let pages = size_aligned / PAGE_SIZE_4K;
    let mut backing =
        GlobalPage::alloc_contiguous(pages, PAGE_SIZE_4K).map_err(|_| VfsError::NoMemory)?;
    backing.zero();
    let paddr = v2p(backing.start_va()).as_usize() as u64;
    let resource_id = dev.next_resource_id.fetch_add(1, Ordering::Relaxed);
    dev.display()?
        .create_scanout_resource(
            ScanoutResource {
                id: resource_id,
                width: c.width,
                height: c.height,
                pitch: c.pitch,
                format: ScanoutFormat::Bgra8888,
            },
            paddr,
            size_aligned as u32,
        )
        .map_err(|_| VfsError::Io)?;
    let pages_arc = Arc::new(backing);
    let offset = dev
        .next_offset
        .fetch_add(DUMB_BUFFER_OFFSET_STRIDE, Ordering::Relaxed);
    let handle = dev.next_dumb_handle.fetch_add(1, Ordering::Relaxed);

    dev.dumbs.lock().insert(
        DumbHandle::from_wire(handle),
        DumbBuffer {
            width: c.width,
            height: c.height,
            pitch: c.pitch,
            offset,
            resource_id,
            pages: pages_arc,
        },
    );
    c.handle = handle;
    Ok(0)
}

pub(crate) fn ioctl_drm_mode_map_dumb(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    m: &mut DrmModeMapDumb,
) -> VfsResult<usize> {
    let offset = dev
        .dumbs
        .lock()
        .get(&DumbHandle::from_wire(m.handle))
        .map(|b| b.offset)
        .ok_or(VfsError::InvalidInput)?;
    m.offset = offset;
    Ok(0)
}

/// Drop a dumb buffer's handle and release the host scanout resource behind it.
///
/// `DESTROY_DUMB` and `GEM_CLOSE` converge on the same state in Linux --
/// `drm_mode_destroy_dumb_ioctl()` and `drm_gem_close_ioctl()` both end in
/// `drm_gem_handle_delete()` -- so they share this path instead of keeping two
/// copies of the teardown.  Returns `false` when the handle was never allocated;
/// the two callers report that case differently, which is the one place where
/// the legacy and the GEM spelling disagree.
fn take_dumb(dev: &DrmCard, handle: DumbHandle) -> VfsResult<bool> {
    // One critical section owns both the existence test and the removal, so two
    // concurrent closes of the same handle cannot both reach the teardown.
    let Some(dumb) = dev.dumbs.lock().remove(&handle) else {
        return Ok(false);
    };
    // Resolve the display device before anything becomes visible: if it is gone,
    // fail the ioctl instead of dropping the backing while the host still holds a
    // scanout resource for it (dangling backing + leak).  The lookup happens after
    // the handle test so that an unknown handle stays a silent miss -- the legacy
    // `DESTROY_DUMB` contract -- even when the device has already been removed.
    let device = match dev.display() {
        Ok(device) => device,
        Err(err) => {
            dev.dumbs.lock().insert(handle, dumb);
            return Err(err);
        }
    };
    // A registered framebuffer only copies `resource_id` out of the dumb buffer
    // and keeps no reference to the resource or its pages, so destroying them
    // here would leave every framebuffer that still names the resource pointing
    // at a detached scanout and freed backing -- and one of them may be on
    // screen.  Park the buffer instead and let the last framebuffer that
    // references it retire the resource; Linux gets this ordering from the GEM
    // object refcount.
    park_or_destroy(dev, device, dumb);
    Ok(true)
}

/// Is `resource_id` on the scanout, or one whose fate a failed present left open?
fn resource_on_scanout(dev: &DrmCard, resource_id: u32) -> bool {
    dev.presented_resource.load(Ordering::Relaxed) == resource_id
        || dev.unsettled.lock().contains(&resource_id)
}

/// Park `dumb` while anything still needs its resource, or destroy it.
///
/// Two things keep the resource alive: a registered framebuffer that copied its
/// id out, and the host scanning it -- `present_fb` leaves the host pointed at
/// the last resource it was given and this driver has no call that stops that.
/// Parked buffers are retired by `DrmCard::sweep_retired_dumbs()` once another
/// resource is presented.
///
/// The reference test and the insertion into the park table happen under the same
/// `fbs` lock -- the order `retire_unused_dumb()` takes them in.  Without that, a
/// close of the last framebuffer could slip in between the test and the
/// insertion, and the parked entry would never be retired.
fn park_or_destroy(dev: &DrmCard, device: &ClassDevice<DisplayDeviceImpl>, dumb: DumbBuffer) {
    {
        let fbs = dev.fbs.lock();
        let referenced = fbs.values().any(|fb| fb.resource_id == dumb.resource_id);
        let scanned = resource_on_scanout(dev, dumb.resource_id);
        if referenced || scanned {
            dev.retired_dumbs.lock().insert(dumb.resource_id, dumb);
            return;
        }
    }
    destroy_dumb(device, dumb);
}

/// Detach the scanout resource behind `dumb` and release its backing pages.
fn destroy_dumb(device: &ClassDevice<DisplayDeviceImpl>, dumb: DumbBuffer) {
    if let Err(err) = device.destroy_scanout_resource(dumb.resource_id) {
        log::warn!(
            "drm: failed to destroy scanout resource {}: {:?}",
            dumb.resource_id,
            err
        );
    }
    // `dumb` (and the backing pages it owns) is released here.
}

/// Retire a parked dumb buffer once no framebuffer references its resource.
///
/// Called by the framebuffer teardown paths, which are the only places that can
/// take the last reference away.
pub(crate) fn retire_unused_dumb(dev: &DrmCard, resource_id: u32) {
    // The host may still be scanning this resource even when no framebuffer
    // names it any more: `RMFB` drops the framebuffer without stopping the
    // scanout, and this driver has no call that would.  Such a buffer is left
    // parked and retired by the next present of a different resource.
    if resource_on_scanout(dev, resource_id) {
        return;
    }
    // Same lock order as `park_or_destroy()`: `fbs` first, then the park table,
    // so a concurrent park cannot be missed by this take.
    let parked = {
        let fbs = dev.fbs.lock();
        if fbs.values().any(|fb| fb.resource_id == resource_id) {
            return;
        }
        dev.retired_dumbs.lock().remove(&resource_id)
    };
    let Some(dumb) = parked else {
        return;
    };
    // The display may already be gone; the resource went with it, and only the
    // backing pages are left to release.
    if let Ok(device) = dev.display() {
        destroy_dumb(device, dumb);
    }
}

impl DrmCard {
    /// Retire parked dumb buffers that nothing references and nothing scans.
    ///
    /// Called after a present: that is the moment the set of resources the host
    /// is scanning changes, so it is when a parked buffer can become safe to
    /// destroy.
    pub(crate) fn sweep_retired_dumbs(&self) {
        // Called from every present, including the per-frame page-flip path.  A
        // parked entry can legitimately live as long as its framebuffer does (the
        // close-the-handle-first sequence), so the empty check is only the cheap
        // exit for clients that never close a handle early.
        if self.retired_dumbs.lock().is_empty() {
            return;
        }
        let presented = self.presented_resource.load(Ordering::Relaxed);
        // Lock order matches the retirement paths: `fbs` -> `retired_dumbs` ->
        // `unsettled`.  `park_or_destroy()` reads the unsettled set through
        // `resource_on_scanout()` while holding `fbs`, and never takes `fbs` from
        // inside any of them, so this nesting cannot invert.
        let victims: Vec<u32> = {
            let fbs = self.fbs.lock();
            let retired = self.retired_dumbs.lock();
            let unsettled = self.unsettled.lock();
            let referenced: BTreeSet<u32> = fbs.values().map(|fb| fb.resource_id).collect();
            retired
                .keys()
                .copied()
                .filter(|resource_id| {
                    *resource_id != presented
                        && !unsettled.contains(resource_id)
                        && !referenced.contains(resource_id)
                })
                .collect()
        };
        if victims.is_empty() {
            return;
        }
        let Ok(device) = self.display() else {
            return;
        };
        for resource_id in victims {
            // Take the entry out under the lock and destroy it outside, like the
            // other two retirement sites: `destroy_dumb` calls into the display
            // backend and drops up to 8 MiB of backing.
            let parked = self.retired_dumbs.lock().remove(&resource_id);
            if let Some(dumb) = parked {
                destroy_dumb(device, dumb);
            }
        }
    }
}

pub(crate) fn ioctl_drm_mode_destroy_dumb(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    d: &mut DrmModeDestroyDumb,
) -> VfsResult<usize> {
    // Legacy entry point: an unknown handle stays a silent success, because
    // callers free a dumb buffer without ever reading the result.  GEM_CLOSE
    // below is the entry point that is allowed to report the miss.
    //
    // `ADDFB2` holds this lock from reading a handle to registering the
    // framebuffer, which is what keeps a close from destroying a resource
    // the new framebuffer is about to name.
    let _commit = dev.commit_lock.lock();
    take_dumb(dev, DumbHandle::from_wire(d.handle))?;
    Ok(0)
}

pub(crate) fn ioctl_drm_gem_close(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    args: &mut DrmGemClose,
) -> VfsResult<usize> {
    // `drm_gem_close_ioctl()` rejects a non-zero `pad` and reports an unknown
    // handle as EINVAL (through `drm_gem_handle_delete()`), not ENOENT.
    if args.pad != 0 {
        return Err(VfsError::InvalidInput);
    }
    // See `MODE_DESTROY_DUMB`: this lock orders the close against `ADDFB2`.
    let _commit = dev.commit_lock.lock();
    if !take_dumb(dev, DumbHandle::from_wire(args.handle))? {
        return Err(VfsError::InvalidInput);
    }
    Ok(0)
}

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

pub(crate) fn ioctl_drm_mode_destroy_dumb(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    d: &mut DrmModeDestroyDumb,
) -> VfsResult<usize> {
    let Some(resource_id) = dev
        .dumbs
        .lock()
        .get(&DumbHandle::from_wire(d.handle))
        .map(|b| b.resource_id)
    else {
        return Ok(0);
    };
    // Resolve the display device *before* mutating guest state: if it is
    // gone, fail the ioctl instead of dropping the backing while the host
    // still holds a scanout resource for it (dangling backing + leak).
    let device = dev.display()?;
    dev.dumbs.lock().remove(&DumbHandle::from_wire(d.handle));
    if let Err(err) = device.destroy_scanout_resource(resource_id) {
        log::warn!(
            "drm: failed to destroy scanout resource {}: {:?}",
            resource_id,
            err
        );
    }
    Ok(0)
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! blob subsystem of the virtual DRM card, split out of card0.rs.

use super::*;

pub(crate) fn ioctl_drm_mode_create_blob(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    c: &mut DrmModeCreateBlob,
) -> VfsResult<usize> {
    if c.length == 0 || c.length as usize > MAX_BLOB_BYTES {
        return Err(VfsError::InvalidInput);
    }
    let len = c.length as usize;
    let bytes = c.data.load_vm_vec(len).map_err(|_| VfsError::BadAddress)?;
    let id = dev.next_blob_id.fetch_add(1, Ordering::Relaxed);
    dev.blobs
        .lock()
        .insert(BlobId::from_wire(id), Arc::new(bytes));
    c.blob_id = id;
    Ok(0)
}

pub(crate) fn ioctl_drm_mode_destroy_blob(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    d: &mut DrmModeDestroyBlob,
) -> VfsResult<usize> {
    if dev
        .system_blobs
        .lock()
        .contains_key(&BlobId::from_wire(d.blob_id))
    {
        return Err(VfsError::PermissionDenied);
    }
    dev.blobs
        .lock()
        .remove(&BlobId::from_wire(d.blob_id))
        .ok_or(VfsError::NotFound)?;
    Ok(0)
}

pub(crate) fn ioctl_drm_mode_get_blob(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    g: &mut DrmModeGetBlob,
) -> VfsResult<usize> {
    let bytes = if let Some(b) = dev.blobs.lock().get(&BlobId::from_wire(g.blob_id)).cloned() {
        b
    } else if g.blob_id == dev.state.lock().crtc_mode_id
        && let Some(b) = dev.mode_id_blob_ref.lock().clone()
    {
        b
    } else if let Some(b) = dev
        .system_blobs
        .lock()
        .get(&BlobId::from_wire(g.blob_id))
        .cloned()
    {
        b
    } else {
        return Err(VfsError::NotFound);
    };
    if !g.data.is_null() && g.length > 0 {
        let n = (g.length as usize).min(bytes.len());
        g.data
            .write_vm_slice(&bytes[..n])
            .map_err(|_| VfsError::BadAddress)?;
    }
    g.length = bytes.len() as u32;
    Ok(0)
}

pub(crate) fn build_in_formats_blob() -> Vec<u8> {
    #[repr(C)]
    #[derive(Clone, Copy, bytemuck::NoUninit)]
    struct Header {
        version: u32,
        flags: u32,
        count_formats: u32,
        formats_offset: u32,
        count_modifiers: u32,
        modifiers_offset: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy, bytemuck::NoUninit)]
    struct ModifierEntry {
        formats: u64,
        offset: u32,
        _pad: u32,
        modifier: u64,
    }
    let n_formats = SUPPORTED_FORMATS.len() as u32;
    let formats_off = core::mem::size_of::<Header>() as u32;
    let modifiers_off = formats_off + n_formats * 4;
    let hdr = Header {
        version: 1,
        flags: 0,
        count_formats: n_formats,
        formats_offset: formats_off,
        count_modifiers: 1,
        modifiers_offset: modifiers_off,
    };
    let format_mask = (1u64 << n_formats) - 1;
    let me = ModifierEntry {
        formats: format_mask,
        offset: 0,
        _pad: 0,
        modifier: DRM_FORMAT_MOD_LINEAR,
    };
    let mut buf = Vec::with_capacity(
        core::mem::size_of::<Header>()
            + (n_formats as usize) * 4
            + core::mem::size_of::<ModifierEntry>(),
    );
    buf.extend_from_slice(bytes_of(&hdr));
    for fmt in SUPPORTED_FORMATS {
        buf.extend_from_slice(&fmt.to_le_bytes());
    }
    buf.extend_from_slice(bytes_of(&me));
    buf
}

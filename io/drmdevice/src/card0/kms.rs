// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! kms subsystem of the virtual DRM card, split out of card0.rs.

use super::*;

pub(crate) fn ioctl_drm_mode_card_res(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    r: &mut DrmModeCardRes,
) -> VfsResult<usize> {
    let (w, h) = display_resolution(dev);
    r.min_width = w;
    r.max_width = w;
    r.min_height = h;
    r.max_height = h;

    r.count_fbs = 0;
    r.count_crtcs = report_user_array(r.crtc_id_ptr, r.count_crtcs, &[CRTC_ID])?;
    r.count_encoders = report_user_array(r.encoder_id_ptr, r.count_encoders, &[ENCODER_ID])?;
    r.count_connectors =
        report_user_array(r.connector_id_ptr, r.count_connectors, &[CONNECTOR_ID])?;
    Ok(0)
}

pub(crate) fn ioctl_drm_mode_crtc(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    crtc: &mut DrmModeCrtc,
) -> VfsResult<usize> {
    if crtc.crtc_id != CRTC_ID {
        return Err(VfsError::InvalidInput);
    }
    let legacy = dev.legacy_crtc.lock().clone();
    crtc.gamma_size = 0;
    if legacy.fb_id != 0 {
        crtc.x = legacy.x;
        crtc.y = legacy.y;
        crtc.fb_id = legacy.fb_id;
        crtc.mode_valid = legacy.mode_valid;
        crtc.mode = if legacy.mode_valid != 0 {
            legacy.mode
        } else {
            DrmModeModeInfo::default()
        };
        crtc.count_connectors = report_user_array(
            crtc.set_connectors_ptr,
            crtc.count_connectors,
            &legacy.connectors,
        )?;
    } else {
        crtc.x = 0;
        crtc.y = 0;
        crtc.fb_id = 0;
        crtc.mode_valid = 1;
        crtc.mode = current_mode(dev);
        let empty: &[u32] = &[];
        crtc.count_connectors =
            report_user_array(crtc.set_connectors_ptr, crtc.count_connectors, empty)?;
    }
    Ok(0)
}

/// The fields of a legacy cursor request that this driver acts on.
///
/// `MODE_CURSOR` and `MODE_CURSOR2` differ only by the hotspot offsets, which a
/// driver with no hardware cursor plane has nothing to apply, so both handlers
/// reduce to the same request.  Grouping them also keeps `set_cursor` under
/// clippy's argument-count threshold.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CursorRequest {
    pub(crate) crtc_id: u32,
    pub(crate) flags: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) handle: u32,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct LegacyCrtcState {
    pub(crate) fb_id: u32,
    pub(crate) connectors: Vec<u32>,
    pub(crate) mode: DrmModeModeInfo,
    pub(crate) mode_valid: u32,
    pub(crate) x: u32,
    pub(crate) y: u32,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ModesetState {
    pub(crate) crtc_active: u64,
    pub(crate) crtc_mode_id: u32,
    pub(crate) conn_crtc_id: u32,
    pub(crate) plane_fb_id: u32,
    pub(crate) plane_crtc_id: u32,
    pub(crate) plane_src_x: u64,
    pub(crate) plane_src_y: u64,
    pub(crate) plane_src_w: u64,
    pub(crate) plane_src_h: u64,
    pub(crate) plane_crtc_x: i64,
    pub(crate) plane_crtc_y: i64,
    pub(crate) plane_crtc_w: u64,
    pub(crate) plane_crtc_h: u64,
}

pub(crate) fn ioctl_drm_mode_set_crtc(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    crtc: &mut DrmModeSetCrtc,
) -> VfsResult<usize> {
    let c = &crtc.0;
    if c.crtc_id != CRTC_ID {
        return Err(VfsError::InvalidInput);
    }
    if c.fb_id == 0 && c.count_connectors == 0 {
        *dev.legacy_crtc.lock() = LegacyCrtcState::default();
        return Ok(0);
    }
    if c.fb_id == 0 || !dev.fbs.lock().contains_key(&FbId::from_wire(c.fb_id)) {
        return Err(VfsError::InvalidInput);
    }
    if c.count_connectors == 0 || c.set_connectors_ptr.is_null() {
        return Err(VfsError::InvalidInput);
    }
    if c.count_connectors > 16 {
        return Err(VfsError::InvalidInput);
    }
    let connectors: Vec<u32> = c
        .set_connectors_ptr
        .load_vm_vec(c.count_connectors as usize)?;
    for &id in &connectors {
        if id != CONNECTOR_ID {
            return Err(VfsError::InvalidInput);
        }
    }
    *dev.legacy_crtc.lock() = LegacyCrtcState {
        fb_id: c.fb_id,
        connectors,
        mode: c.mode,
        mode_valid: c.mode_valid,
        x: c.x,
        y: c.y,
    };
    dev.present_fb(FbId::from_wire(c.fb_id));
    Ok(0)
}

pub(crate) fn ioctl_drm_mode_get_encoder(
    _dev: &DrmCard,
    _file: &Arc<DrmFile>,
    e: &mut DrmModeGetEncoder,
) -> VfsResult<usize> {
    if e.encoder_id != ENCODER_ID {
        return Err(VfsError::InvalidInput);
    }
    e.encoder_type = DRM_MODE_ENCODER_VIRTUAL;
    e.crtc_id = CRTC_ID;
    e.possible_crtcs = 1;
    e.possible_clones = 0;
    Ok(0)
}

pub(crate) fn ioctl_drm_mode_get_connector(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    c: &mut DrmModeGetConnector,
) -> VfsResult<usize> {
    if c.connector_id != CONNECTOR_ID {
        return Err(VfsError::InvalidInput);
    }
    c.encoder_id = ENCODER_ID;
    c.connector_type = DRM_MODE_CONNECTOR_VIRTUAL;
    c.connector_type_id = 1;
    c.connection = DRM_MODE_CONNECTED;
    let (w, h) = display_resolution(dev);
    c.mm_width = w;
    c.mm_height = h;
    c.subpixel = 0;
    c.count_encoders = report_user_array(c.encoders_ptr, c.count_encoders, &[ENCODER_ID])?;
    if !c.modes_ptr.is_null() && c.count_modes > 0 {
        c.modes_ptr
            .write_vm(current_mode(dev))
            .map_err(|_| VfsError::BadAddress)?;
    }
    c.count_modes = 1;
    c.count_props = 0;
    Ok(0)
}

pub(crate) fn ioctl_drm_mode_crtc_page_flip(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    f: &mut DrmModeCrtcPageFlip,
) -> VfsResult<usize> {
    if f.crtc_id != CRTC_ID || !dev.fbs.lock().contains_key(&FbId::from_wire(f.fb_id)) {
        return Err(VfsError::InvalidInput);
    }
    dev.present_fb(FbId::from_wire(f.fb_id));
    if f.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
        dev.queue_flip_event(f.user_data);
    }
    Ok(0)
}

pub(crate) fn ioctl_drm_mode_get_plane_res(
    _dev: &DrmCard,
    _file: &Arc<DrmFile>,
    r: &mut DrmModeGetPlaneRes,
) -> VfsResult<usize> {
    let planes: &[u32] = &[PLANE_ID];
    r.count_planes = report_user_array(r.plane_id_ptr, r.count_planes, planes)?;
    Ok(0)
}

pub(crate) fn ioctl_drm_mode_get_plane(
    _dev: &DrmCard,
    _file: &Arc<DrmFile>,
    p: &mut DrmModeGetPlane,
) -> VfsResult<usize> {
    if p.plane_id != PLANE_ID {
        return Err(VfsError::InvalidInput);
    }
    p.crtc_id = CRTC_ID;
    p.fb_id = 0;
    p.possible_crtcs = 1;
    p.gamma_size = 0;
    p.count_format_types =
        report_user_array(p.format_type_ptr, p.count_format_types, SUPPORTED_FORMATS)?;
    Ok(0)
}

/// DRM_IOCTL_MODE_SETPROPERTY = DRM_IOWR(0xAB, struct
///  // drm_mode_connector_set_property): a 16-byte write-read.
pub(crate) fn ioctl_drm_mode_connector_set_property(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    req: &mut DrmModeConnectorSetProperty,
) -> VfsResult<usize> {
    // `drm_connector_property_set_ioctl()` is only a wrapper over
    // `drm_mode_obj_set_property_ioctl()` with `obj_type` fixed to
    // `DRM_MODE_OBJECT_CONNECTOR`, so the connector is the only target the
    // request can name, and the write below is the one the object form
    // performs.
    if req.connector_id != CONNECTOR_ID {
        return Err(VfsError::NotFound);
    }
    set_one_property(
        dev,
        DrmObjectType::Connector,
        req.connector_id,
        req.prop_id,
        req.value,
    )
}

/// DRM_IOCTL_MODE_OBJ_SETPROPERTY = DRM_IOWR(0xBA, struct
/// drm_mode_obj_set_property): 24 bytes after alignment, not 20.
pub(crate) fn ioctl_drm_mode_obj_set_property(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    req: &mut DrmModeObjSetProperty,
) -> VfsResult<usize> {
    // The object type travels in the request, so it has to agree with the id.
    // This is the check MODE_ATOMIC applies to every entry of its object list,
    // and it is what stops a client from writing a plane property onto the CRTC.
    let Some(obj_type) = DrmObjectType::from_wire(req.obj_type) else {
        return Err(VfsError::NotFound);
    };
    if DrmObjectType::of_resource_id(req.obj_id) != Some(obj_type) {
        return Err(VfsError::NotFound);
    }
    set_one_property(dev, obj_type, req.obj_id, req.prop_id, req.value)
}

/// Write one property from a non-atomic entry point.
///
/// The sequence is a copy of the one `MODE_ATOMIC` performs inline: apply to a
/// copy of the state, commit the copy only if the write succeeded, publish a new
/// mode blob if the write produced one, and refresh the scanout whenever the
/// committed plane framebuffer is non-zero.  `MODE_ATOMIC` is deliberately left
/// alone here: it carries `TEST_ONLY` and page-flip-event handling that this
/// helper has no business absorbing.  Both here and there the commit is
/// serialized by `commit_lock`, so a second committer cannot slip its own
/// framebuffer in between the store and the present.
///
/// Errnos follow Linux's `drm_mode_obj_set_property_ioctl()`, whose `ret` starts
/// at `-EINVAL`: a request that names an unknown object, or an object type that
/// does not match the id, fails the `drm_mode_object_find()` lookup and answers
/// `ENOENT`; a known object carrying an unknown property id reaches `out_unref`
/// with `ret` untouched and answers `EINVAL`, as does an invalid value for a
/// known property.  `DRM_IOCTL_MODE_SETPROPERTY` only wraps that function with
/// `obj_type = DRM_MODE_OBJECT_CONNECTOR`, so these two entry points and
/// `MODE_ATOMIC` all answer alike and a client cannot tell which one it used.
pub(crate) fn set_one_property(
    dev: &DrmCard,
    obj_type: DrmObjectType,
    obj_id: u32,
    prop_id: u32,
    value: u64,
) -> VfsResult<usize> {
    // The store below and the present after it have to stay one operation for
    // other committers, or the older framebuffer can reach the screen last.
    let _commit = dev.commit_lock.lock();
    let mut state = dev.state.lock();
    let mut proposed = *state;
    let mut new_mode_blob: Option<Option<Arc<Vec<u8>>>> = None;
    if !dev.apply_prop(
        obj_type,
        obj_id,
        prop_id,
        value,
        &mut proposed,
        &mut new_mode_blob,
    )? {
        // Unknown property on a known object: Linux's
        // `drm_mode_obj_set_property_ioctl()` leaves `ret` at its `-EINVAL`
        // initialiser on this path; only a failed object lookup is `ENOENT`.
        return Err(VfsError::InvalidInput);
    }
    let current_fb = proposed.plane_fb_id;
    *state = proposed;
    // Publish the blob before releasing `state`.  `apply_prop` takes the same two
    // locks in this order, so doing the store here keeps that order; releasing
    // `state` first would let a second writer commit its own `crtc_mode_id` in
    // the gap, and `GET_BLOB` would then find a blob id that no longer matches the
    // reference and answer with another mode's timings instead of an error.
    if let Some(new_ref) = new_mode_blob {
        *dev.mode_id_blob_ref.lock() = new_ref;
    }
    drop(state);
    if current_fb != 0 {
        dev.present_fb(FbId::from_wire(current_fb));
    }
    Ok(0)
}

pub(crate) fn ioctl_drm_mode_obj_get_properties(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    q: &mut DrmModeObjGetProperties,
) -> VfsResult<usize> {
    let state = *dev.state.lock();
    let (prop_ids, prop_vals): (&[u32], Vec<u64>) = match (q.obj_type, q.obj_id) {
        (DRM_MODE_OBJECT_PLANE, PLANE_ID) => {
            let blob_id = dev.ensure_in_formats_blob() as u64;
            (PLANE_PROPS, plane_prop_values(&state, blob_id))
        }
        (DRM_MODE_OBJECT_CRTC, CRTC_ID) => (CRTC_PROPS, crtc_prop_values(&state)),
        (DRM_MODE_OBJECT_CONNECTOR, CONNECTOR_ID) => (CONN_PROPS, conn_prop_values(&state)),
        _ => return Err(VfsError::NotFound),
    };
    report_user_array(q.props_ptr, q.count_props, prop_ids)?;
    report_user_array(q.prop_values_ptr, q.count_props, &prop_vals)?;
    q.count_props = prop_ids.len() as u32;
    Ok(0)
}

pub(crate) fn ioctl_drm_mode_get_property(
    _dev: &DrmCard,
    _file: &Arc<DrmFile>,
    g: &mut DrmModeGetProperty,
) -> VfsResult<usize> {
    let meta = property_meta(g.prop_id).ok_or(VfsError::NotFound)?;

    g.flags = meta.flags;
    g.name = [0; DRM_PROP_NAME_LEN];
    let nb = meta.name.as_bytes();
    let n = nb.len().min(DRM_PROP_NAME_LEN - 1);
    g.name[..n].copy_from_slice(&nb[..n]);

    match meta.kind {
        PropKind::Enum(enums) => {
            g.count_values = enums.len() as u32;
            g.count_enum_blobs = report_user_array(g.enum_blob_ptr, g.count_enum_blobs, enums)?;
        }
        PropKind::RangeU64 { min, max } => {
            let limits = [min, max];
            g.count_values = report_user_array(g.values_ptr, g.count_values, &limits)?;
            g.count_enum_blobs = 0;
        }
        PropKind::Object | PropKind::Blob => {
            g.count_values = 0;
            g.count_enum_blobs = 0;
        }
    }
    Ok(0)
}

pub(crate) fn ioctl_drm_mode_atomic(
    dev: &DrmCard,
    file: &Arc<DrmFile>,
    a: &mut DrmModeAtomic,
) -> VfsResult<usize> {
    // This is the consumer of the bitmap SET_CLIENT_CAP maintains: a client
    // that never negotiated `DRM_CLIENT_CAP_ATOMIC` is told the atomic path
    // is unavailable so it falls back to the legacy one, instead of being
    // handed an interface it did not ask for.
    if file.client_caps.load(Ordering::Relaxed) & (1u64 << DRM_CLIENT_CAP_ATOMIC) == 0 {
        return Err(VfsError::OperationNotSupported);
    }

    let known = DRM_MODE_ATOMIC_TEST_ONLY
        | DRM_MODE_ATOMIC_NONBLOCK
        | DRM_MODE_ATOMIC_ALLOW_MODESET
        | DRM_MODE_PAGE_FLIP_EVENT;
    if a.flags & !known != 0 {
        return Err(VfsError::InvalidInput);
    }

    let n = a.count_objs as usize;
    let objs: Vec<u32> = a.objs_ptr.load_vm_vec(n)?;
    let counts: Vec<u32> = a.count_props_ptr.load_vm_vec(n)?;
    let total_props: usize = counts.iter().map(|c| *c as usize).sum();
    let props: Vec<u32> = a.props_ptr.load_vm_vec(total_props)?;
    let values: Vec<u64> = a.prop_values_ptr.load_vm_vec(total_props)?;

    // Same ordering as `set_one_property`: one committer at a time, so the
    // commit and the present below cannot be split by another writer.
    let _commit = dev.commit_lock.lock();
    let mut state = dev.state.lock();
    let mut proposed = *state;
    let mut new_mode_blob: Option<Option<Arc<Vec<u8>>>> = None;
    let mut idx = 0;
    for (obj_i, &obj_id) in objs.iter().enumerate() {
        let obj_type = DrmObjectType::of_resource_id(obj_id).ok_or(VfsError::NotFound)?;
        for _ in 0..counts[obj_i] {
            let prop_id = props[idx];
            let value = values[idx];
            idx += 1;
            if !dev.apply_prop(
                obj_type,
                obj_id,
                prop_id,
                value,
                &mut proposed,
                &mut new_mode_blob,
            )? {
                return Err(VfsError::InvalidInput);
            }
        }
    }

    if a.flags & DRM_MODE_ATOMIC_TEST_ONLY != 0 {
        return Ok(0);
    }

    let current_fb = proposed.plane_fb_id;
    *state = proposed;
    drop(state);
    if let Some(new_ref) = new_mode_blob {
        *dev.mode_id_blob_ref.lock() = new_ref;
    }
    if current_fb != 0 {
        dev.present_fb(FbId::from_wire(current_fb));
    }
    if a.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
        dev.queue_flip_event(a.user_data);
    }
    Ok(0)
}

pub(crate) fn ioctl_drm_mode_cursor(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    cursor: &mut DrmModeCursor,
) -> VfsResult<usize> {
    dev.set_cursor(CursorRequest {
        crtc_id: cursor.crtc_id,
        flags: cursor.flags,
        width: cursor.width,
        height: cursor.height,
        handle: cursor.handle,
    })
}

pub(crate) fn ioctl_drm_mode_cursor2(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    cursor: &mut DrmModeCursor2,
) -> VfsResult<usize> {
    // The hotspot is only meaningful to a hardware cursor plane, which this
    // driver does not expose; validating it would reject requests the device
    // is happy to treat as a plain move.
    dev.set_cursor(CursorRequest {
        crtc_id: cursor.crtc_id,
        flags: cursor.flags,
        width: cursor.width,
        height: cursor.height,
        handle: cursor.handle,
    })
}

impl DrmCard {
    /// Answer a legacy `MODE_CURSOR` / `MODE_CURSOR2` request.
    ///
    /// The bound display exposes a single primary plane, so there is no hardware
    /// cursor to program.  A bare move is answered with success -- libdrm-based
    /// clients such as Xorg's modesetting driver issue it once the CRTC is up and
    /// treat a failure as an initialisation error -- but a request that installs
    /// an image is refused with `EOPNOTSUPP`.  Accepting the image would tell the
    /// client its hardware cursor works, and it would then stop drawing the
    /// pointer itself while nothing appeared on screen.
    ///
    /// Validation otherwise matches Linux's `drm_mode_cursor_common()`: the CRTC
    /// must exist, and a request that carries a handle must reference a live dumb
    /// buffer of non-zero size.  Position and hotspot are accepted and dropped,
    /// which is the same observable behaviour as a driver whose cursor plane is
    /// disabled.
    pub(crate) fn set_cursor(&self, request: CursorRequest) -> VfsResult<usize> {
        if request.crtc_id != CRTC_ID {
            return Err(VfsError::InvalidInput);
        }
        if request.flags & !(DRM_MODE_CURSOR_BO | DRM_MODE_CURSOR_MOVE) != 0 {
            return Err(VfsError::InvalidInput);
        }
        // A BO request carries the cursor image.  Validate it first, so a
        // malformed request is reported as such: the handle must name a buffer
        // this device allocated, and width/height are the only size the request
        // provides.  A BO with `handle == 0` hides the cursor and carries no
        // image, which Linux accepts; MOVE only relocates, so its width/height
        // are ignored.
        if request.flags & DRM_MODE_CURSOR_BO != 0 && request.handle != 0 {
            if request.width == 0
                || request.height == 0
                || !self
                    .dumbs
                    .lock()
                    .contains_key(&DumbHandle::from_wire(request.handle))
            {
                return Err(VfsError::InvalidInput);
            }
            // The image itself cannot be shown: there is no cursor plane, so
            // answering Ok would tell the client its hardware cursor is installed
            // and it would stop drawing the pointer while nothing appeared on
            // screen.  Refusing is what keeps the software cursor, and it is the
            // same answer this driver gave before MODE_CURSOR existed.
            return Err(VfsError::OperationNotSupported);
        }
        Ok(0)
    }

    pub(crate) fn apply_prop(
        &self,
        obj_type: DrmObjectType,
        _obj_id: u32,
        prop_id: u32,
        value: u64,
        s: &mut ModesetState,
        new_mode_blob: &mut Option<Option<Arc<Vec<u8>>>>,
    ) -> VfsResult<bool> {
        match (obj_type, prop_id) {
            (DrmObjectType::Plane, PROP_PLANE_TYPE) => {
                if value != DRM_PLANE_TYPE_PRIMARY {
                    return Err(VfsError::InvalidInput);
                }
            }
            (DrmObjectType::Plane, PROP_PLANE_FB_ID) => {
                let fb = value as u32;
                if fb != 0 && !self.fbs.lock().contains_key(&FbId::from_wire(fb)) {
                    return Err(VfsError::InvalidInput);
                }
                s.plane_fb_id = fb;
            }
            (DrmObjectType::Plane, PROP_PLANE_CRTC_ID) => {
                let c = value as u32;
                if c != 0 && c != CRTC_ID {
                    return Err(VfsError::InvalidInput);
                }
                s.plane_crtc_id = c;
            }
            (DrmObjectType::Plane, PROP_PLANE_SRC_X) => s.plane_src_x = value,
            (DrmObjectType::Plane, PROP_PLANE_SRC_Y) => s.plane_src_y = value,
            (DrmObjectType::Plane, PROP_PLANE_SRC_W) => s.plane_src_w = value,
            (DrmObjectType::Plane, PROP_PLANE_SRC_H) => s.plane_src_h = value,
            (DrmObjectType::Plane, PROP_PLANE_CRTC_X) => {
                s.plane_crtc_x = checked_i32(value)? as i64;
            }
            (DrmObjectType::Plane, PROP_PLANE_CRTC_Y) => {
                s.plane_crtc_y = checked_i32(value)? as i64;
            }
            (DrmObjectType::Plane, PROP_PLANE_CRTC_W) => s.plane_crtc_w = value,
            (DrmObjectType::Plane, PROP_PLANE_CRTC_H) => s.plane_crtc_h = value,
            (DrmObjectType::Crtc, PROP_CRTC_ACTIVE) => {
                if value > 1 {
                    return Err(VfsError::InvalidInput);
                }
                s.crtc_active = value;
            }
            (DrmObjectType::Crtc, PROP_CRTC_MODE_ID) => {
                let blob = value as u32;
                let arc = if blob == 0 {
                    None
                } else {
                    let arc = self
                        .blobs
                        .lock()
                        .get(&BlobId::from_wire(blob))
                        .cloned()
                        .or_else(|| {
                            if s.crtc_mode_id == blob {
                                self.mode_id_blob_ref.lock().clone()
                            } else {
                                None
                            }
                        });
                    Some(arc.ok_or(VfsError::InvalidInput)?)
                };
                s.crtc_mode_id = blob;
                *new_mode_blob = Some(arc);
            }
            (DrmObjectType::Connector, PROP_CONN_CRTC_ID) => {
                let c = value as u32;
                if c != 0 && c != CRTC_ID {
                    return Err(VfsError::InvalidInput);
                }
                s.conn_crtc_id = c;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}

pub(crate) fn plane_prop_values(s: &ModesetState, in_formats: u64) -> Vec<u64> {
    vec![
        DRM_PLANE_TYPE_PRIMARY,
        s.plane_fb_id as u64,
        s.plane_crtc_id as u64,
        s.plane_src_x,
        s.plane_src_y,
        s.plane_src_w,
        s.plane_src_h,
        s.plane_crtc_x as u64,
        s.plane_crtc_y as u64,
        s.plane_crtc_w,
        s.plane_crtc_h,
        in_formats,
    ]
}

pub(crate) fn crtc_prop_values(s: &ModesetState) -> Vec<u64> {
    vec![s.crtc_active, s.crtc_mode_id as u64]
}

pub(crate) fn conn_prop_values(s: &ModesetState) -> Vec<u64> {
    vec![s.conn_crtc_id as u64]
}

pub(crate) struct PropMeta {
    pub(crate) name: &'static str,
    pub(crate) flags: u32,
    pub(crate) kind: PropKind,
}

pub(crate) enum PropKind {
    Enum(&'static [DrmModePropertyEnum]),
    RangeU64 { min: u64, max: u64 },
    Object,
    Blob,
}

pub(crate) const PLANE_TYPE_ENUMS: &[DrmModePropertyEnum] = &[
    enum_entry(0, b"Overlay"),
    enum_entry(1, b"Primary"),
    enum_entry(2, b"Cursor"),
];

fn property_meta(id: u32) -> Option<PropMeta> {
    let atomic = DRM_MODE_PROP_ATOMIC;
    let meta = match id {
        PROP_PLANE_TYPE => PropMeta {
            name: "type",
            flags: DRM_MODE_PROP_ENUM | DRM_MODE_PROP_IMMUTABLE,
            kind: PropKind::Enum(PLANE_TYPE_ENUMS),
        },
        PROP_PLANE_FB_ID => PropMeta {
            name: "FB_ID",
            flags: DRM_MODE_PROP_OBJECT | atomic,
            kind: PropKind::Object,
        },
        PROP_PLANE_CRTC_ID => PropMeta {
            name: "CRTC_ID",
            flags: DRM_MODE_PROP_OBJECT | atomic,
            kind: PropKind::Object,
        },
        PROP_PLANE_SRC_X => range_u32("SRC_X", atomic),
        PROP_PLANE_SRC_Y => range_u32("SRC_Y", atomic),
        PROP_PLANE_SRC_W => range_u32("SRC_W", atomic),
        PROP_PLANE_SRC_H => range_u32("SRC_H", atomic),
        PROP_PLANE_CRTC_X => range_u32("CRTC_X", atomic),
        PROP_PLANE_CRTC_Y => range_u32("CRTC_Y", atomic),
        PROP_PLANE_CRTC_W => range_u32("CRTC_W", atomic),
        PROP_PLANE_CRTC_H => range_u32("CRTC_H", atomic),
        PROP_PLANE_IN_FORMATS => PropMeta {
            name: "IN_FORMATS",
            flags: DRM_MODE_PROP_BLOB | DRM_MODE_PROP_IMMUTABLE,
            kind: PropKind::Blob,
        },
        PROP_CRTC_ACTIVE => PropMeta {
            name: "ACTIVE",
            flags: DRM_MODE_PROP_RANGE | atomic,
            kind: PropKind::RangeU64 { min: 0, max: 1 },
        },
        PROP_CRTC_MODE_ID => PropMeta {
            name: "MODE_ID",
            flags: DRM_MODE_PROP_BLOB | atomic,
            kind: PropKind::Blob,
        },
        PROP_CONN_CRTC_ID => PropMeta {
            name: "CRTC_ID",
            flags: DRM_MODE_PROP_OBJECT | atomic,
            kind: PropKind::Object,
        },
        _ => return None,
    };
    Some(meta)
}

pub(crate) fn range_u32(name: &'static str, atomic: u32) -> PropMeta {
    PropMeta {
        name,
        flags: DRM_MODE_PROP_RANGE | atomic,
        kind: PropKind::RangeU64 {
            min: 0,
            max: u32::MAX as u64,
        },
    }
}

pub(crate) fn checked_i32(value: u64) -> VfsResult<i32> {
    let v = value as i64;
    if (i32::MIN as i64..=i32::MAX as i64).contains(&v) {
        Ok(v as i32)
    } else {
        Err(VfsError::InvalidInput)
    }
}

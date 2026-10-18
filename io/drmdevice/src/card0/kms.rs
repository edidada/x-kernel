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
    let state = *dev.state.lock();
    // The primary plane feeds the single CRTC: legacy scanout offsets are the
    // atomic source coordinates in 16.16, the connector binding lives in the
    // connector's CRTC_ID property, and the legacy mode is the blob that
    // SETCRTC registered under `MODE_ID`.
    crtc.gamma_size = 0;
    crtc.x = (state.plane_src_x >> 16) as u32;
    crtc.y = (state.plane_src_y >> 16) as u32;
    crtc.fb_id = state.plane_fb_id;
    let mode = if state.crtc_mode_id != 0 {
        dev.mode_id_blob_ref.lock().as_deref().and_then(|bytes| {
            bytemuck::try_from_bytes::<DrmModeModeInfo>(bytes)
                .ok()
                .copied()
        })
    } else {
        None
    };
    // A live modeset reports the committed mode; an idle CRTC reports the
    // panel's fixed mode, exactly as the legacy-only reader used to.
    crtc.mode_valid = 1;
    crtc.mode = mode.unwrap_or_else(|| current_mode(dev));
    let connectors: &[u32] = if state.conn_crtc_id == CONNECTOR_ID {
        &[CONNECTOR_ID]
    } else {
        &[]
    };
    crtc.count_connectors =
        report_user_array(crtc.set_connectors_ptr, crtc.count_connectors, connectors)?;
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
        // The legacy encoding of "off"; the atomic one is ACTIVE=0 with the
        // rest detached. Same treatment, one state.
        dev.commit(|_, s, _| {
            s.crtc_active = 0;
            s.plane_fb_id = 0;
            s.plane_crtc_id = 0;
            s.conn_crtc_id = 0;
            s.crtc_mode_id = 0;
            Ok(())
        })?;
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
    // Legacy SETCRTC carries the mode inline; the atomic path models modes as
    // blobs, so register it as one (the same store `CREATE_BLOB` uses) and
    // then commit through `apply_prop`, exactly like an atomic client would.
    // The blob outlives the commit so `GET_BLOB` can serve it back; a session
    // only modesets a handful of times, so the growth is bounded in practice.
    let mode_blob = BlobId::from_wire(dev.next_blob_id.fetch_add(1, Ordering::Relaxed));
    dev.blobs
        .lock()
        .insert(mode_blob, Arc::new(bytemuck::bytes_of(&c.mode).to_vec()));
    dev.commit(|dev, s, new_mode_blob| {
        for (obj_type, obj_id, prop_id, value) in [
            (
                DrmObjectType::Plane,
                PLANE_ID,
                PROP_PLANE_FB_ID,
                c.fb_id as u64,
            ),
            (
                DrmObjectType::Plane,
                PLANE_ID,
                PROP_PLANE_CRTC_ID,
                CRTC_ID as u64,
            ),
            (
                DrmObjectType::Plane,
                PLANE_ID,
                PROP_PLANE_SRC_X,
                (c.x as u64) << 16,
            ),
            (
                DrmObjectType::Plane,
                PLANE_ID,
                PROP_PLANE_SRC_Y,
                (c.y as u64) << 16,
            ),
            (DrmObjectType::Crtc, CRTC_ID, PROP_CRTC_ACTIVE, 1),
            (
                DrmObjectType::Crtc,
                CRTC_ID,
                PROP_CRTC_MODE_ID,
                mode_blob.wire() as u64,
            ),
            (
                DrmObjectType::Connector,
                CONNECTOR_ID,
                PROP_CONN_CRTC_ID,
                CONNECTOR_ID as u64,
            ),
        ] {
            if !dev.apply_prop(obj_type, obj_id, prop_id, value, s, new_mode_blob)? {
                return Err(VfsError::InvalidInput);
            }
        }
        Ok(())
    })?;
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
    dev.commit(|_, s, _| {
        s.plane_fb_id = f.fb_id;
        Ok(())
    })?;
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
/// The whole commit funnels through [`DrmCard::commit`], so this and the
/// atomic path share one store/publish/present sequence and one
/// serialization point.
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
    dev.commit(|dev, s, new_mode_blob| {
        if !dev.apply_prop(obj_type, obj_id, prop_id, value, s, new_mode_blob)? {
            // Unknown property on a known object: Linux's
            // `drm_mode_obj_set_property_ioctl()` leaves `ret` at its `-EINVAL`
            // initialiser on this path; only a failed object lookup is `ENOENT`.
            return Err(VfsError::InvalidInput);
        }
        Ok(())
    })?;
    Ok(0)
}

pub(crate) fn ioctl_drm_mode_obj_get_properties(
    dev: &DrmCard,
    _file: &Arc<DrmFile>,
    q: &mut DrmModeObjGetProperties,
) -> VfsResult<usize> {
    let state = *dev.state.lock();
    let (descs, in_formats) = match (q.obj_type, q.obj_id) {
        (DRM_MODE_OBJECT_PLANE, PLANE_ID) => (PLANE_PROPS, dev.ensure_in_formats_blob() as u64),
        (DRM_MODE_OBJECT_CRTC, CRTC_ID) => (CRTC_PROPS, 0),
        (DRM_MODE_OBJECT_CONNECTOR, CONNECTOR_ID) => (CONN_PROPS, 0),
        _ => return Err(VfsError::NotFound),
    };
    let prop_ids: Vec<u32> = descs.iter().map(|p| p.id).collect();
    let prop_vals: Vec<u64> = descs.iter().map(|p| (p.read)(&state, in_formats)).collect();
    report_user_array(q.props_ptr, q.count_props, &prop_ids)?;
    report_user_array(q.prop_values_ptr, q.count_props, &prop_vals)?;
    q.count_props = descs.len() as u32;
    Ok(0)
}

pub(crate) fn ioctl_drm_mode_get_property(
    _dev: &DrmCard,
    _file: &Arc<DrmFile>,
    g: &mut DrmModeGetProperty,
) -> VfsResult<usize> {
    let desc = prop_desc_by_id(g.prop_id).ok_or(VfsError::NotFound)?;

    g.flags = desc.flags;
    g.name = [0; DRM_PROP_NAME_LEN];
    let nb = desc.name.as_bytes();
    let n = nb.len().min(DRM_PROP_NAME_LEN - 1);
    g.name[..n].copy_from_slice(&nb[..n]);

    match desc.kind {
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

    if a.flags & DRM_MODE_ATOMIC_TEST_ONLY != 0 {
        // Dry run: apply every entry onto a scratch copy of the state and
        // throw the copy away. No commit, no present.
        let mut probe = *dev.state.lock();
        let mut scratch_blob: Option<Option<Arc<Vec<u8>>>> = None;
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
                    &mut probe,
                    &mut scratch_blob,
                )? {
                    return Err(VfsError::InvalidInput);
                }
            }
        }
        return Ok(0);
    }

    // NONBLOCK and ALLOW_MODESET are accepted: this driver commits
    // synchronously and never blocks on a modeset, so there is nothing to
    // defer and nothing to reject.
    dev.commit(|dev, s, new_mode_blob| {
        let mut idx = 0;
        for (obj_i, &obj_id) in objs.iter().enumerate() {
            let obj_type = DrmObjectType::of_resource_id(obj_id).ok_or(VfsError::NotFound)?;
            for _ in 0..counts[obj_i] {
                let prop_id = props[idx];
                let value = values[idx];
                idx += 1;
                if !dev.apply_prop(obj_type, obj_id, prop_id, value, s, new_mode_blob)? {
                    return Err(VfsError::InvalidInput);
                }
            }
        }
        Ok(())
    })?;
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

    /// Serialised KMS commit: the single entry point through which changes to
    /// `ModesetState` reach the card. Takes `commit_lock`, applies `mutate`
    /// onto a copy of the state, publishes a new mode blob while still holding
    /// `state` (the order documented on the field), then presents the
    /// committed plane framebuffer after the lock is released. Every property
    /// writer — atomic, legacy SETCRTC/PAGE_FLIP, single-property — funnels
    /// through here, so no two committers can interleave store and present.
    pub(crate) fn commit<F>(&self, mutate: F) -> VfsResult<()>
    where
        F: FnOnce(&DrmCard, &mut ModesetState, &mut Option<Option<Arc<Vec<u8>>>>) -> VfsResult<()>,
    {
        let _commit = self.commit_lock.lock();
        let mut state = self.state.lock();
        let mut proposed = *state;
        let mut new_mode_blob: Option<Option<Arc<Vec<u8>>>> = None;
        mutate(self, &mut proposed, &mut new_mode_blob)?;
        let current_fb = proposed.plane_fb_id;
        *state = proposed;
        if let Some(new_ref) = new_mode_blob {
            *self.mode_id_blob_ref.lock() = new_ref;
        }
        drop(state);
        if current_fb != 0 {
            self.present_fb(FbId::from_wire(current_fb));
        }
        Ok(())
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
        let Some(desc) = props_for(obj_type).iter().find(|p| p.id == prop_id) else {
            return Ok(false);
        };
        // `type` carries DRM_MODE_PROP_IMMUTABLE yet keeps a writer on
        // purpose: it only accepts the pinned primary plane type, which is
        // what the previous hand-written arm did. Linux rejects writes to
        // any immutable property outright.
        let Some(write) = desc.write else {
            return Err(VfsError::InvalidInput);
        };
        write(self, s, value, new_mode_blob)?;
        Ok(true)
    }
}

/// One KMS property: identity (id, name, flags, kind), how to read its
/// current value out of the modeset state, and how to validate and write a
/// new one.  `MODE_OBJ_GETPROPERTIES`, `MODE_GETPROPERTY` and the write path
/// (`apply_prop`) all derive from these tables, so a property can never be
/// readable but unwritable, or carry metadata that disagrees with its value.
pub(crate) struct PropDesc {
    pub(crate) id: u32,
    pub(crate) name: &'static str,
    pub(crate) flags: u32,
    pub(crate) kind: PropKind,
    /// Reads the current value.  The second argument is the driver-wide
    /// `IN_FORMATS` blob id, consumed only by that property.
    pub(crate) read: fn(&ModesetState, u64) -> u64,
    /// Validates and writes a new value; `None` marks a write-rejected
    /// property.
    pub(crate) write: Option<WriteProp>,
}

pub(crate) type WriteProp =
    fn(&DrmCard, &mut ModesetState, u64, &mut Option<Option<Arc<Vec<u8>>>>) -> VfsResult<()>;

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

fn read_plane_type(_s: &ModesetState, _in_formats: u64) -> u64 {
    DRM_PLANE_TYPE_PRIMARY
}

fn read_plane_fb(s: &ModesetState, _in_formats: u64) -> u64 {
    s.plane_fb_id as u64
}

fn read_plane_crtc(s: &ModesetState, _in_formats: u64) -> u64 {
    s.plane_crtc_id as u64
}

fn read_src_x(s: &ModesetState, _in_formats: u64) -> u64 {
    s.plane_src_x
}

fn read_src_y(s: &ModesetState, _in_formats: u64) -> u64 {
    s.plane_src_y
}

fn read_src_w(s: &ModesetState, _in_formats: u64) -> u64 {
    s.plane_src_w
}

fn read_src_h(s: &ModesetState, _in_formats: u64) -> u64 {
    s.plane_src_h
}

fn read_crtc_x(s: &ModesetState, _in_formats: u64) -> u64 {
    s.plane_crtc_x as u64
}

fn read_crtc_y(s: &ModesetState, _in_formats: u64) -> u64 {
    s.plane_crtc_y as u64
}

fn read_crtc_w(s: &ModesetState, _in_formats: u64) -> u64 {
    s.plane_crtc_w
}

fn read_crtc_h(s: &ModesetState, _in_formats: u64) -> u64 {
    s.plane_crtc_h
}

fn read_in_formats(_s: &ModesetState, in_formats: u64) -> u64 {
    in_formats
}

fn read_crtc_active(s: &ModesetState, _in_formats: u64) -> u64 {
    s.crtc_active
}

fn read_crtc_mode_id(s: &ModesetState, _in_formats: u64) -> u64 {
    s.crtc_mode_id as u64
}

fn read_conn_crtc(s: &ModesetState, _in_formats: u64) -> u64 {
    s.conn_crtc_id as u64
}

fn write_plane_type(
    _dev: &DrmCard,
    _s: &mut ModesetState,
    value: u64,
    _new_mode_blob: &mut Option<Option<Arc<Vec<u8>>>>,
) -> VfsResult<()> {
    if value != DRM_PLANE_TYPE_PRIMARY {
        return Err(VfsError::InvalidInput);
    }
    Ok(())
}

fn write_plane_fb(
    dev: &DrmCard,
    s: &mut ModesetState,
    value: u64,
    _new_mode_blob: &mut Option<Option<Arc<Vec<u8>>>>,
) -> VfsResult<()> {
    let fb = value as u32;
    if fb != 0 && !dev.fbs.lock().contains_key(&FbId::from_wire(fb)) {
        return Err(VfsError::InvalidInput);
    }
    s.plane_fb_id = fb;
    Ok(())
}

fn write_plane_crtc(
    _dev: &DrmCard,
    s: &mut ModesetState,
    value: u64,
    _new_mode_blob: &mut Option<Option<Arc<Vec<u8>>>>,
) -> VfsResult<()> {
    let c = value as u32;
    if c != 0 && c != CRTC_ID {
        return Err(VfsError::InvalidInput);
    }
    s.plane_crtc_id = c;
    Ok(())
}

fn write_src_x(
    _dev: &DrmCard,
    s: &mut ModesetState,
    value: u64,
    _new_mode_blob: &mut Option<Option<Arc<Vec<u8>>>>,
) -> VfsResult<()> {
    s.plane_src_x = value;
    Ok(())
}

fn write_src_y(
    _dev: &DrmCard,
    s: &mut ModesetState,
    value: u64,
    _new_mode_blob: &mut Option<Option<Arc<Vec<u8>>>>,
) -> VfsResult<()> {
    s.plane_src_y = value;
    Ok(())
}

fn write_src_w(
    _dev: &DrmCard,
    s: &mut ModesetState,
    value: u64,
    _new_mode_blob: &mut Option<Option<Arc<Vec<u8>>>>,
) -> VfsResult<()> {
    s.plane_src_w = value;
    Ok(())
}

fn write_src_h(
    _dev: &DrmCard,
    s: &mut ModesetState,
    value: u64,
    _new_mode_blob: &mut Option<Option<Arc<Vec<u8>>>>,
) -> VfsResult<()> {
    s.plane_src_h = value;
    Ok(())
}

fn write_crtc_x(
    _dev: &DrmCard,
    s: &mut ModesetState,
    value: u64,
    _new_mode_blob: &mut Option<Option<Arc<Vec<u8>>>>,
) -> VfsResult<()> {
    s.plane_crtc_x = checked_i32(value)? as i64;
    Ok(())
}

fn write_crtc_y(
    _dev: &DrmCard,
    s: &mut ModesetState,
    value: u64,
    _new_mode_blob: &mut Option<Option<Arc<Vec<u8>>>>,
) -> VfsResult<()> {
    s.plane_crtc_y = checked_i32(value)? as i64;
    Ok(())
}

fn write_crtc_w(
    _dev: &DrmCard,
    s: &mut ModesetState,
    value: u64,
    _new_mode_blob: &mut Option<Option<Arc<Vec<u8>>>>,
) -> VfsResult<()> {
    s.plane_crtc_w = value;
    Ok(())
}

fn write_crtc_h(
    _dev: &DrmCard,
    s: &mut ModesetState,
    value: u64,
    _new_mode_blob: &mut Option<Option<Arc<Vec<u8>>>>,
) -> VfsResult<()> {
    s.plane_crtc_h = value;
    Ok(())
}

fn write_crtc_active(
    _dev: &DrmCard,
    s: &mut ModesetState,
    value: u64,
    _new_mode_blob: &mut Option<Option<Arc<Vec<u8>>>>,
) -> VfsResult<()> {
    if value > 1 {
        return Err(VfsError::InvalidInput);
    }
    s.crtc_active = value;
    Ok(())
}

fn write_crtc_mode_id(
    dev: &DrmCard,
    s: &mut ModesetState,
    value: u64,
    new_mode_blob: &mut Option<Option<Arc<Vec<u8>>>>,
) -> VfsResult<()> {
    let blob = value as u32;
    let arc = if blob == 0 {
        None
    } else {
        let arc = dev
            .blobs
            .lock()
            .get(&BlobId::from_wire(blob))
            .cloned()
            .or_else(|| {
                if s.crtc_mode_id == blob {
                    dev.mode_id_blob_ref.lock().clone()
                } else {
                    None
                }
            });
        Some(arc.ok_or(VfsError::InvalidInput)?)
    };
    s.crtc_mode_id = blob;
    *new_mode_blob = Some(arc);
    Ok(())
}

fn write_conn_crtc(
    _dev: &DrmCard,
    s: &mut ModesetState,
    value: u64,
    _new_mode_blob: &mut Option<Option<Arc<Vec<u8>>>>,
) -> VfsResult<()> {
    let c = value as u32;
    if c != 0 && c != CRTC_ID {
        return Err(VfsError::InvalidInput);
    }
    s.conn_crtc_id = c;
    Ok(())
}

const fn range_prop(
    id: u32,
    name: &'static str,
    read: fn(&ModesetState, u64) -> u64,
    write: Option<WriteProp>,
) -> PropDesc {
    PropDesc {
        id,
        name,
        flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_ATOMIC,
        kind: PropKind::RangeU64 {
            min: 0,
            max: u32::MAX as u64,
        },
        read,
        write,
    }
}

pub(crate) static PLANE_PROPS: &[PropDesc] = &[
    PropDesc {
        id: PROP_PLANE_TYPE,
        name: "type",
        flags: DRM_MODE_PROP_ENUM | DRM_MODE_PROP_IMMUTABLE,
        kind: PropKind::Enum(PLANE_TYPE_ENUMS),
        read: read_plane_type,
        write: Some(write_plane_type),
    },
    PropDesc {
        id: PROP_PLANE_FB_ID,
        name: "FB_ID",
        flags: DRM_MODE_PROP_OBJECT | DRM_MODE_PROP_ATOMIC,
        kind: PropKind::Object,
        read: read_plane_fb,
        write: Some(write_plane_fb),
    },
    PropDesc {
        id: PROP_PLANE_CRTC_ID,
        name: "CRTC_ID",
        flags: DRM_MODE_PROP_OBJECT | DRM_MODE_PROP_ATOMIC,
        kind: PropKind::Object,
        read: read_plane_crtc,
        write: Some(write_plane_crtc),
    },
    range_prop(PROP_PLANE_SRC_X, "SRC_X", read_src_x, Some(write_src_x)),
    range_prop(PROP_PLANE_SRC_Y, "SRC_Y", read_src_y, Some(write_src_y)),
    range_prop(PROP_PLANE_SRC_W, "SRC_W", read_src_w, Some(write_src_w)),
    range_prop(PROP_PLANE_SRC_H, "SRC_H", read_src_h, Some(write_src_h)),
    range_prop(PROP_PLANE_CRTC_X, "CRTC_X", read_crtc_x, Some(write_crtc_x)),
    range_prop(PROP_PLANE_CRTC_Y, "CRTC_Y", read_crtc_y, Some(write_crtc_y)),
    range_prop(PROP_PLANE_CRTC_W, "CRTC_W", read_crtc_w, Some(write_crtc_w)),
    range_prop(PROP_PLANE_CRTC_H, "CRTC_H", read_crtc_h, Some(write_crtc_h)),
    PropDesc {
        id: PROP_PLANE_IN_FORMATS,
        name: "IN_FORMATS",
        flags: DRM_MODE_PROP_BLOB | DRM_MODE_PROP_IMMUTABLE,
        kind: PropKind::Blob,
        read: read_in_formats,
        write: None,
    },
];

pub(crate) static CRTC_PROPS: &[PropDesc] = &[
    PropDesc {
        id: PROP_CRTC_ACTIVE,
        name: "ACTIVE",
        flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_ATOMIC,
        kind: PropKind::RangeU64 { min: 0, max: 1 },
        read: read_crtc_active,
        write: Some(write_crtc_active),
    },
    PropDesc {
        id: PROP_CRTC_MODE_ID,
        name: "MODE_ID",
        flags: DRM_MODE_PROP_BLOB | DRM_MODE_PROP_ATOMIC,
        kind: PropKind::Blob,
        read: read_crtc_mode_id,
        write: Some(write_crtc_mode_id),
    },
];

pub(crate) static CONN_PROPS: &[PropDesc] = &[PropDesc {
    id: PROP_CONN_CRTC_ID,
    name: "CRTC_ID",
    flags: DRM_MODE_PROP_OBJECT | DRM_MODE_PROP_ATOMIC,
    kind: PropKind::Object,
    read: read_conn_crtc,
    write: Some(write_conn_crtc),
}];

pub(crate) fn props_for(obj_type: DrmObjectType) -> &'static [PropDesc] {
    match obj_type {
        DrmObjectType::Plane => PLANE_PROPS,
        DrmObjectType::Crtc => CRTC_PROPS,
        DrmObjectType::Connector => CONN_PROPS,
    }
}

pub(crate) fn prop_desc_by_id(id: u32) -> Option<&'static PropDesc> {
    PLANE_PROPS
        .iter()
        .chain(CRTC_PROPS)
        .chain(CONN_PROPS)
        .find(|p| p.id == id)
}

pub(crate) fn checked_i32(value: u64) -> VfsResult<i32> {
    let v = value as i64;
    if (i32::MIN as i64..=i32::MAX as i64).contains(&v) {
        Ok(v as i32)
    } else {
        Err(VfsError::InvalidInput)
    }
}

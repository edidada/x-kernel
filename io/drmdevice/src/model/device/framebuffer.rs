// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Framebuffer-registry methods of the card.
//!
//! [`DrmDevice`] owns the id -> framebuffer registry and the plane binding;
//! the ADDFB/RMFB/CLOSEFB/DIRTYFB ioctls in `framebuffer.rs` go through these
//! methods.

use crate::prelude::*;

impl DrmDevice {
    /// Removes `fb` unless it is bound or on screen (`MODE_CLOSEFB`).
    ///
    /// The ownership check, the "still in use" check and the removal happen
    /// under the commit lock, so a concurrent commit cannot bind the
    /// framebuffer between them.  A file may only close a framebuffer it
    /// created; Linux reports another file's id as `ENOENT` rather than
    /// revealing that it exists.
    pub(crate) fn close_framebuffer(&self, file: &Arc<DrmFile>, fb: FbId) -> DrmResult<()> {
        let _commit = self.commit.lock();
        if !file.fbs.lock().contains(&fb) || !self.framebuffers.lock().contains(fb) {
            return Err(DrmError::NotFound);
        }
        if self.kms_state().plane_fb_id == fb || self.is_on_scanout(fb) {
            return Err(DrmError::Busy);
        }
        self.unbind_and_remove(fb).ok_or(DrmError::NotFound)?;
        Ok(())
    }

    /// Whether `fb` names a live framebuffer.
    pub(crate) fn has_framebuffer(&self, fb: FbId) -> bool {
        self.framebuffers.lock().contains(fb)
    }

    /// Object, width and height behind `fb`.
    pub(crate) fn framebuffer_object(&self, fb: FbId) -> Option<(Arc<GemObject>, u32, u32)> {
        self.framebuffers
            .lock()
            .get(fb)
            .map(|fb| (fb.object.clone(), fb.object.width(), fb.object.height()))
    }

    /// Registers a framebuffer and records it on the creating file.
    pub(crate) fn add_framebuffer(
        &self,
        file: &Arc<DrmFile>,
        object: Arc<GemObject>,
    ) -> DrmResult<FbId> {
        let id = self
            .framebuffers
            .lock()
            .insert(Framebuffer {
                object,
                owner: Arc::downgrade(file),
            })
            .ok_or(DrmError::NoMemory)?;
        file.fbs.lock().insert(id);
        Ok(id)
    }

    /// Removes a framebuffer owned by `file` and unbinds it from the plane.
    ///
    /// Runs under the commit lock: the ownership test, unbinding the plane,
    /// dropping the registry entry and updating the creating file are one
    /// step, so a concurrent commit cannot publish or re-present a
    /// framebuffer that is being removed.  Checking ownership before touching
    /// the scanout or committed state also means a failed `RMFB` (unknown or
    /// foreign id, including zero) leaves everything unchanged.
    /// The scanout resource stays retained until another present replaces it —
    /// the host may still be scanning it.
    pub(crate) fn remove_framebuffer(&self, file: &Arc<DrmFile>, fb: FbId) -> DrmResult<()> {
        let _commit = self.commit.lock();
        if !file.fbs.lock().contains(&fb) {
            return Err(DrmError::NotFound);
        }
        self.unbind_and_remove(fb).ok_or(DrmError::NotFound)?;
        Ok(())
    }

    /// Unbinds and removes a framebuffer; the caller holds the commit lock.
    fn unbind_and_remove(&self, fb: FbId) -> Option<()> {
        if self.scanout.lock().is_bound(fb) {
            self.scanout.lock().unbind();
        }
        {
            let mut state = self.state.lock();
            if state.plane_fb_id == fb {
                state.plane_fb_id = FbId::default();
                state.plane_crtc_id = CrtcId::default();
            }
        }
        let removed = self.framebuffers.lock().remove(fb)?;
        if let Some(owner) = removed.owner.upgrade() {
            owner.fbs.lock().remove(&fb);
        }
        Some(())
    }

    /// Whether `fb` is currently bound to the plane.
    pub(crate) fn is_on_scanout(&self, fb: FbId) -> bool {
        self.scanout.lock().is_bound(fb)
    }
}

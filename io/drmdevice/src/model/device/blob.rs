// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Property-blob registry methods of the card (`drm_property.c` state).
//!
//! System blobs (the IN_FORMATS table) and client blobs share one registry on
//! [`DrmDevice`]; the legacy `SETCRTC` mode blob keeps its own slot so the
//! next modeset can retire it.

use crate::prelude::*;

impl DrmDevice {
    /// Registers a blob; system blobs cannot be destroyed by clients.
    ///
    /// `creator` records the blob on the creating file so `release()` can
    /// destroy it (`drm_property_destroy_user_blobs()` in Linux).
    pub(crate) fn create_blob(
        &self,
        creator: Option<&DrmFile>,
        bytes: Arc<Vec<u8>>,
        system: bool,
    ) -> DrmResult<(BlobId, Arc<Blob>)> {
        let blob = Arc::new(Blob { bytes, system });
        let id = self
            .blobs
            .lock()
            .insert(blob.clone())
            .ok_or(DrmError::NoMemory)?;
        if let Some(file) = creator {
            file.blobs.lock().insert(id, blob.clone());
        }
        Ok((id, blob))
    }

    /// Bytes of one blob.
    pub(crate) fn blob_bytes(&self, id: BlobId) -> Option<Arc<Vec<u8>>> {
        self.blob(id).map(|blob| blob.bytes())
    }

    /// The blob behind `id`, if the id is still registered.
    pub(crate) fn blob(&self, id: BlobId) -> Option<Arc<Blob>> {
        self.blobs.lock().get(id).cloned()
    }

    /// Removes one blob.
    pub(crate) fn destroy_blob(&self, id: BlobId) -> Option<()> {
        self.blobs.lock().remove(id).map(|_| ())
    }

    /// Destroys a client blob (`MODE_DESTROY_BLOB`) owned by `creator`.
    ///
    /// A caller that names an existing blob missing from its own set gets
    /// `EPERM`, like Linux's `drm_mode_destroyblob_ioctl()`; a missing id is
    /// `ENOENT`. Driver-owned blobs are refused as well, and a stale file entry
    /// whose registry id already retired is dropped before `ENOENT` is returned.
    ///
    /// The ownership check, legacy-slot clear, device-registry removal and the
    /// creating file's bookkeeping run under one commit lock. Splitting them
    /// would leave a window where the id is gone from the device but still owned
    /// by the file, or vice versa. As in Linux, the committed state keeps its
    /// decoded mode, so only the id disappears.
    ///
    /// `creator` is the file that registered the blob; `release` passes it so
    /// close also empties the per-file set.
    pub(crate) fn destroy_client_blob(
        &self,
        creator: Option<&DrmFile>,
        id: BlobId,
    ) -> DrmResult<()> {
        let _commit = self.commit.lock();
        self.destroy_client_blob_locked(creator, id)
    }

    /// Body of [`Self::destroy_client_blob`] for callers that already hold
    /// `commit`.
    fn destroy_client_blob_locked(&self, creator: Option<&DrmFile>, id: BlobId) -> DrmResult<()> {
        let Some(blob) = self.blob(id) else {
            // A legacy-slot retirement can remove the device entry while the
            // creating file still names it; release must clear that stale entry.
            if let Some(file) = creator {
                file.blobs.lock().remove(&id);
            }
            return Err(DrmError::NotFound);
        };
        if let Some(file) = creator
            && !file.blobs.lock().contains_key(&id)
        {
            return Err(DrmError::NotPermitted);
        }
        if blob.is_system() {
            return Err(DrmError::NotPermitted);
        }
        {
            let mut legacy = self.legacy_mode.lock();
            if legacy
                .as_ref()
                .is_some_and(|(legacy_id, _)| *legacy_id == id)
            {
                legacy.take();
            }
        }
        let removed = self.destroy_blob(id).is_some();
        // Clear the file registration even if the device entry was already
        // gone (for example a legacy mode retired it); the set must not keep a
        // stale id alive until the file itself drops.
        if let Some(file) = creator {
            file.blobs.lock().remove(&id);
        }
        if !removed {
            return Err(DrmError::NotFound);
        }
        Ok(())
    }

    /// Destroys every blob created by `file` in one commit section.
    ///
    /// This is `drm_property_destroy_user_blobs()` on close: the file's set is
    /// snapshotted under `commit`, then each id is removed from the device
    /// registry and the file set by [`Self::destroy_client_blob_locked`].
    pub(crate) fn destroy_file_blobs(&self, file: &DrmFile) {
        let _commit = self.commit.lock();
        let owned: Vec<BlobId> = file.blobs.lock().keys().copied().collect();
        for id in owned {
            let _ = self.destroy_client_blob_locked(Some(file), id);
        }
    }

    /// The IN_FORMATS blob, created on first use.
    pub(crate) fn ensure_in_formats_blob(&self) -> DrmResult<BlobId> {
        let mut cached = self.in_formats.lock();
        if let Some((id, _)) = cached.as_ref() {
            return Ok(*id);
        }
        let (id, blob) = self.create_blob(None, Arc::new(build_in_formats_blob()), true)?;
        *cached = Some((id, blob));
        Ok(id)
    }

    /// Retires the legacy-mode blob after a modeset that disabled the CRTC.
    pub(crate) fn clear_legacy_mode_blob(&self) {
        let previous = self.legacy_mode.lock().take();
        if let Some((previous, _)) = previous {
            self.destroy_blob(previous);
        }
    }

    /// The legacy-mode blob the driver currently owns, if any.
    #[cfg(unittest)]
    pub(crate) fn legacy_mode_blob(&self) -> Option<BlobId> {
        self.legacy_mode.lock().as_ref().map(|(id, _)| *id)
    }

    /// Records `blob` as the legacy-mode blob and frees the previous one.
    ///
    /// Called after a successful `SETCRTC`: the driver created that blob, so it
    /// is the driver's to retire when the modeset it belongs to is replaced.
    /// Only the id is retired — the new state keeps the blob itself alive.
    pub(crate) fn replace_legacy_mode_blob(&self, id: BlobId, blob: Arc<Blob>) {
        let previous = self.legacy_mode.lock().replace((id, blob));
        if let Some((previous, _)) = previous
            && previous != id
        {
            self.destroy_blob(previous);
        }
    }
}

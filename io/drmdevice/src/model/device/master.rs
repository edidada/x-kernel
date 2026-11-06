// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Card-ownership methods of the card.
//!
//! Opening a card decides who becomes its master; [`DrmDevice::is_master`]
//! answers the permission query. The master transitions and the token
//! registry live in `auth.rs`.

use crate::prelude::*;

impl DrmDevice {
    /// Whether `file` currently owns the card.
    ///
    /// The device is the single authority for this question: the per-file
    /// authentication state only remembers which master object the file is
    /// bound to (for re-acquisition), never whether it may issue master-only
    /// commands.
    pub(crate) fn is_master(&self, file: &Arc<DrmFile>) -> bool {
        self.master
            .lock()
            .active
            .as_ref()
            .is_some_and(|active| Weak::ptr_eq(&active.owner, &Arc::downgrade(file)))
    }

    /// Creates the per-open file state for one minor.
    ///
    /// Shared by the `DeviceFileOps::open` adapter and the unit tests, so the
    /// master/mastership rules are exercised exactly as production does.
    pub(crate) fn open_file(self: &Arc<Self>, kind: MinorKind) -> DrmResult<Arc<DrmFile>> {
        // Opening a node whose display disappeared must fail with ENODEV, and
        // must not hand out mastership of a dead card.
        self.backend()?;
        // A render client is never master and must not inherit the card's
        // identity: it gets a private master so GET_UNIQUE answers "no
        // identity" and SET_VERSION/AUTH_MAGIC cannot touch the shared one.
        let state = if kind == MinorKind::Render {
            DrmFile::new(
                self.clone(),
                kind,
                AuthState {
                    master: Arc::new(DrmMaster::default()),
                    owns_master_object: false,
                    magic: None,
                },
            )
        } else {
            // Ownership is decided and bound while the master lock is held, so
            // two concurrent opens cannot both believe they became master.
            let mut masters = self.master.lock();
            if masters
                .active
                .as_ref()
                .is_some_and(|active| active.owner.strong_count() == 0)
                && let Some(released) = masters.active.take()
            {
                invalidate_magic_tokens(&released.master);
            }
            match &masters.active {
                Some(active) => {
                    let master = active.master.clone();
                    drop(masters);
                    DrmFile::new(
                        self.clone(),
                        kind,
                        AuthState {
                            master,
                            owns_master_object: false,
                            magic: None,
                        },
                    )
                }
                None => {
                    let master = Arc::new(DrmMaster::default());
                    let state = DrmFile::new(
                        self.clone(),
                        kind,
                        AuthState {
                            master: master.clone(),
                            owns_master_object: true,
                            magic: None,
                        },
                    );
                    masters.active = Some(ActiveMaster {
                        master,
                        owner: Arc::downgrade(&state),
                    });
                    drop(masters);
                    state
                }
            }
        };
        Ok(state)
    }
}

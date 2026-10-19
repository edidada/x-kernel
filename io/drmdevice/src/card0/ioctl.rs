// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ioctl subsystem of the virtual DRM card, split out of card0.rs.

use super::*;

/// DRM ioctl handler trait. `const CMD` and the uniform dispatch function are
/// generated from the `for_each_drm_ioctl!` table at the bottom of this file.
pub(crate) trait DrmIoctl: Sized {
    const CMD: u32;
    fn handle(dev: &DrmCard, file: &Arc<DrmFile>, arg: &mut Self) -> VfsResult<usize>;

    fn handle_raw(dev: &DrmCard, file: &Arc<DrmFile>, arg: UserPtr<Self>) -> VfsResult<usize>
    where
        Self: UserRead + UserWrite,
    {
        let mut ptr: Self = arg.read_vm().map_err(|_| kvfs::VfsError::BadAddress)?;
        let result = Self::handle(dev, file, &mut ptr);
        arg.write_vm(ptr).map_err(|_| kvfs::VfsError::BadAddress)?;
        result
    }
}

/// Single declaration point for every uniform DRM ioctl: the wire ioctl
/// number, its transfer direction, the request type, and the handler. This
/// one table drives both the `impl DrmIoctl` wiring and the generated
/// dispatch function, so adding a command is a single line here and the
/// number, the size-derived command word, the dispatch arm and the handler
/// can no longer drift apart.
macro_rules! for_each_drm_ioctl {
    ($mac:ident) => {
        $mac! {
            (0x00, iowr, DrmVersion, ioctl_drm_version),
            (0x07, iowr, DrmSetVersion, ioctl_drm_set_version),
            (0x01, iowr, DrmUnique, ioctl_drm_unique),
            (0x0c, iowr, DrmGetCap, ioctl_drm_get_cap),
            (0x0d, iow, DrmSetClientCap, ioctl_drm_set_client_cap),
            (0x09, iow, DrmGemClose, ioctl_drm_gem_close),
            (0x02, iowr, DrmAuth, ioctl_drm_auth),
            (0x03, iowr, DrmAuthMagic, ioctl_drm_auth_magic),
            (0xB1, iowr, DrmModeDirtyFb, ioctl_drm_mode_dirty_fb),
            (0x2d, iowr, DrmPrimeHandle, ioctl_drm_prime_handle),
            (0x2e, iowr, DrmPrimeFdToHandle, ioctl_drm_prime_fd_to_handle),
            (0xA0, iowr, DrmModeCardRes, ioctl_drm_mode_card_res),
            (0xA1, iowr, DrmModeCrtc, ioctl_drm_mode_crtc),
            (0xA2, iowr, DrmModeSetCrtc, ioctl_drm_mode_set_crtc),
            (0xA6, iowr, DrmModeGetEncoder, ioctl_drm_mode_get_encoder),
            (0xA7, iowr, DrmModeGetConnector, ioctl_drm_mode_get_connector),
            (0xAF, iowr, DrmModeRmFb, ioctl_drm_mode_rm_fb),
            (0xB0, iowr, DrmModeCrtcPageFlip, ioctl_drm_mode_crtc_page_flip),
            (0xB2, iowr, DrmModeCreateDumb, ioctl_drm_mode_create_dumb),
            (0xB3, iowr, DrmModeMapDumb, ioctl_drm_mode_map_dumb),
            (0xB4, iowr, DrmModeDestroyDumb, ioctl_drm_mode_destroy_dumb),
            (0xB5, iowr, DrmModeGetPlaneRes, ioctl_drm_mode_get_plane_res),
            (0xB6, iowr, DrmModeGetPlane, ioctl_drm_mode_get_plane),
            (0xAB, iowr, DrmModeConnectorSetProperty, ioctl_drm_mode_connector_set_property),
            (0xBA, iowr, DrmModeObjSetProperty, ioctl_drm_mode_obj_set_property),
            (0xB9, iowr, DrmModeObjGetProperties, ioctl_drm_mode_obj_get_properties),
            (0xAA, iowr, DrmModeGetProperty, ioctl_drm_mode_get_property),
            (0x3A, iowr, DrmWaitVblank, ioctl_drm_wait_vblank),
            (0xBC, iowr, DrmModeAtomic, ioctl_drm_mode_atomic),
            (0xBD, iowr, DrmModeCreateBlob, ioctl_drm_mode_create_blob),
            (0xBE, iowr, DrmModeDestroyBlob, ioctl_drm_mode_destroy_blob),
            (0xAC, iowr, DrmModeGetBlob, ioctl_drm_mode_get_blob),
            (0xA3, iowr, DrmModeCursor, ioctl_drm_mode_cursor),
            (0xBB, iowr, DrmModeCursor2, ioctl_drm_mode_cursor2),
            (0xAE, iowr, DrmModeFbCmd, ioctl_drm_mode_fb_cmd),
            (0xB8, iowr, DrmModeFbCmd2, ioctl_drm_mode_fb_cmd2),
            (0xD0, iowr, DrmModeCloseFb, ioctl_drm_mode_close_fb),
        }
    };
}

macro_rules! drm_impl_and_dispatch {
    ($(($nr:literal, $dir:ident, $ty:ty, $handle:path)),* $(,)?) => {
        $(
            impl DrmIoctl for $ty {
                const CMD: u32 = $dir::<$ty>(DRM_TYPE, $nr);

                fn handle(dev: &DrmCard, file: &Arc<DrmFile>, req: &mut Self) -> VfsResult<usize> {
                    $handle(dev, file, req)
                }
            }
        )*

        pub(crate) fn drm_uniform_ioctl(
            dev: &DrmCard,
            state: &Arc<DrmFile>,
            cmd: u32,
            arg: usize,
        ) -> VfsResult<usize> {
            match cmd {
                $(
                    <$ty as DrmIoctl>::CMD => {
                        let user_ptr = UserPtr::<$ty>::from(arg);
                        <$ty as DrmIoctl>::handle_raw(dev, state, user_ptr)
                    }
                )*
                _ => Err(kvfs::VfsError::OperationNotSupported),
            }
        }
    };
}

for_each_drm_ioctl!(drm_impl_and_dispatch);

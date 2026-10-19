// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ioctl subsystem of the virtual DRM card, split out of card0.rs.

use super::*;

/// DRM ioctl handler trait: binds a wire command word to its request type
/// and its handler.  Every request type declares `impl DrmIoctl for <Type>`
/// next to the code that implements it, so the command number, the struct
/// it is decoded into, and the handler stay one greppable unit;
/// `drm_uniform_ioctl` below is the single dispatch point over the `CMD`s.
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

/// Decode and run the ioctl named by `cmd`; unknown commands answer
/// `ENOSYS`, like `drm_ioctl()` falling off its table.
#[expect(
    clippy::too_many_lines,
    reason = "linear DRM ioctl dispatch table: one short match arm per command, arms share no \
              state"
)]
pub(crate) fn drm_uniform_ioctl(
    dev: &DrmCard,
    file: &Arc<DrmFile>,
    cmd: u32,
    arg: usize,
) -> VfsResult<usize> {
    match cmd {
        <DrmVersion as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmVersion>::from(arg);
            <DrmVersion as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmSetVersion as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmSetVersion>::from(arg);
            <DrmSetVersion as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmUnique as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmUnique>::from(arg);
            <DrmUnique as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmGetCap as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmGetCap>::from(arg);
            <DrmGetCap as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmSetClientCap as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmSetClientCap>::from(arg);
            <DrmSetClientCap as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmGemClose as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmGemClose>::from(arg);
            <DrmGemClose as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmAuth as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmAuth>::from(arg);
            <DrmAuth as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmAuthMagic as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmAuthMagic>::from(arg);
            <DrmAuthMagic as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeDirtyFb as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeDirtyFb>::from(arg);
            <DrmModeDirtyFb as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmPrimeHandle as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmPrimeHandle>::from(arg);
            <DrmPrimeHandle as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmPrimeFdToHandle as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmPrimeFdToHandle>::from(arg);
            <DrmPrimeFdToHandle as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeCardRes as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeCardRes>::from(arg);
            <DrmModeCardRes as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeCrtc as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeCrtc>::from(arg);
            <DrmModeCrtc as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeSetCrtc as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeSetCrtc>::from(arg);
            <DrmModeSetCrtc as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeGetEncoder as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeGetEncoder>::from(arg);
            <DrmModeGetEncoder as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeGetConnector as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeGetConnector>::from(arg);
            <DrmModeGetConnector as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeRmFb as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeRmFb>::from(arg);
            <DrmModeRmFb as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeCrtcPageFlip as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeCrtcPageFlip>::from(arg);
            <DrmModeCrtcPageFlip as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeCreateDumb as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeCreateDumb>::from(arg);
            <DrmModeCreateDumb as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeMapDumb as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeMapDumb>::from(arg);
            <DrmModeMapDumb as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeDestroyDumb as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeDestroyDumb>::from(arg);
            <DrmModeDestroyDumb as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeGetPlaneRes as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeGetPlaneRes>::from(arg);
            <DrmModeGetPlaneRes as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeGetPlane as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeGetPlane>::from(arg);
            <DrmModeGetPlane as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeConnectorSetProperty as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeConnectorSetProperty>::from(arg);
            <DrmModeConnectorSetProperty as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeObjSetProperty as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeObjSetProperty>::from(arg);
            <DrmModeObjSetProperty as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeObjGetProperties as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeObjGetProperties>::from(arg);
            <DrmModeObjGetProperties as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeGetProperty as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeGetProperty>::from(arg);
            <DrmModeGetProperty as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmWaitVblank as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmWaitVblank>::from(arg);
            <DrmWaitVblank as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeAtomic as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeAtomic>::from(arg);
            <DrmModeAtomic as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeCreateBlob as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeCreateBlob>::from(arg);
            <DrmModeCreateBlob as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeDestroyBlob as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeDestroyBlob>::from(arg);
            <DrmModeDestroyBlob as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeGetBlob as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeGetBlob>::from(arg);
            <DrmModeGetBlob as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeCursor as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeCursor>::from(arg);
            <DrmModeCursor as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeCursor2 as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeCursor2>::from(arg);
            <DrmModeCursor2 as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeFbCmd as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeFbCmd>::from(arg);
            <DrmModeFbCmd as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeFbCmd2 as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeFbCmd2>::from(arg);
            <DrmModeFbCmd2 as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        <DrmModeCloseFb as DrmIoctl>::CMD => {
            let user_ptr = UserPtr::<DrmModeCloseFb>::from(arg);
            <DrmModeCloseFb as DrmIoctl>::handle_raw(dev, file, user_ptr)
        }
        _ => Err(kvfs::VfsError::OperationNotSupported),
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Framebuffer device emulation over scanout.
//!
//! This mirrors the Linux `drm_fbdev` model: `/dev/fb0` is not a separate
//! hardware path but a compatibility shim built on top of the display
//! device's scanout interface. At [`fb_init`] the primary display device is
//! asked for its resolution, a contiguous shadow buffer is allocated in guest
//! memory, and that buffer is bound to the host as a 2D scanout resource via
//! [`DisplayDevice::create_scanout_resource`]. Userspace reads and writes land
//! in the shadow; the shadow can be pushed to the visible scanout on demand
//! via [`fb_present`].
//!
//! mmap clients such as Links2 write pixels without `write()` or
//! `FBIOPAN_DISPLAY`. While at least one `/dev/fb0` file is open and no DRM
//! client currently owns the scanout, a low-frequency refresh task presents
//! the shadow. The task sleeps when idle so it is not a busy loop.
//!
//! This works uniformly for any [`DisplayDevice`]: scanout-only devices (e.g.
//! virtio-gpu) and any future directly-mapped device both get a working
//! `/dev/fb0` without the driver having to expose a directly-mapped buffer.

#![no_std]

#[macro_use]
extern crate log;

extern crate alloc;

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use display::{DisplayDevice, DisplayInfo, ScanoutFormat, ScanoutRect, ScanoutResource};
use kalloc::GlobalPage;
use kclass::{ClassDevice, DisplayDeviceImpl, display_devices as class_display_devices};
use khal::mem::v2p;
use kspin::SpinNoIrq;
use ktime_types::TimeSpan;
use lazyinit::LazyInit;
use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};

/// Bytes per pixel for the shadow scanout buffer. Matches the BGRA8888 format
/// advertised to the display device.
const BYTES_PER_PIXEL: usize = 4;

/// Driver-local resource id for the framebuffer's scanout resource.
const FB_RESOURCE_ID: u32 = 1;

/// Present interval while a framebuffer client is mapped or open.
const REFRESH_ACTIVE_INTERVAL: TimeSpan = TimeSpan::from_millis(33);
/// Sleep interval when no fbdev client is present or DRM owns the scanout.
const REFRESH_IDLE_INTERVAL: TimeSpan = TimeSpan::from_millis(250);

/// Runtime state for the emulated framebuffer, established once during
/// [`fb_init`] against the primary display device.
struct FbEmulation {
    info: DisplayInfo,
    /// Primary display device the shadow is bound to, cached so a present pays
    /// one Arc clone instead of re-snapshotting (and allocating) the whole
    /// class registry on every [`fb_present`] call. The device outlives fbdev
    /// (no teardown), so the cached handle cannot dangle.
    device: ClassDevice<DisplayDeviceImpl>,
    /// Guest-mapped shadow buffer; userspace reads/writes land here. Kept alive
    /// for the framebuffer lifetime, so the host scanout resource's backing
    /// memory stays valid.
    shadow: GlobalPage,
}

static FB: LazyInit<SpinNoIrq<Option<Arc<FbEmulation>>>> = LazyInit::new();
static FB_CLIENTS: AtomicUsize = AtomicUsize::new(0);
static DRM_OWNS_SCANOUT: AtomicBool = AtomicBool::new(false);
static REFRESH_STARTED: AtomicBool = AtomicBool::new(false);

/// Initialize the framebuffer emulation subsystem against the primary display
/// device.
///
/// Allocates a shadow buffer sized for the primary display's resolution at
/// BGRA8888 and binds it as a host-visible scanout resource. On any failure the
/// emulation is left unavailable (rather than panicking), so a misbehaving
/// display driver cannot take down boot; `/dev/fb0` simply will not appear.
pub fn fb_init() {
    info!("Initialize framebuffer subsystem...");

    FB.init_once(SpinNoIrq::new(None));

    if let Err(e) = try_setup() {
        warn!("fbdev emulation unavailable: {:?}", e);
    }
}

fn try_setup() -> Result<(), ()> {
    // Pick the primary display device. Display device registration races with
    // fb_init in general, but the kclass registry is seeded before fb_init
    // runs in the boot sequence; a missing device here means no display at all.
    let device = class_display_devices().into_iter().next().ok_or(())?;
    let info = device.info();
    if info.width == 0 || info.height == 0 {
        warn!("primary display reported zero resolution; skipping fbdev");
        return Err(());
    }

    // Size the shadow for the full visible surface at BGRA8888, page-aligned so
    // it can be both mapped to userspace and described to the host as a single
    // contiguous backing region.
    let size = (info.width as usize)
        .checked_mul(info.height as usize)
        .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
        .ok_or(())?;
    let pages = size.div_ceil(PAGE_SIZE_4K);

    let mut shadow = GlobalPage::alloc_contiguous(pages, PAGE_SIZE_4K).map_err(|_| {
        warn!("failed to allocate {}-page fbdev shadow buffer", pages);
    })?;
    shadow.zero();

    // Bind the shadow to the host as a scanout resource before moving it into
    // the FbEmulation; the host then has a stable guest backing for the
    // lifetime of fbdev. `shadow.start_va` stays valid because it is owned by
    // the FbEmulation we install below.
    let paddr = v2p(shadow.start_va()).as_usize() as u64;
    let resource = ScanoutResource {
        id: FB_RESOURCE_ID,
        width: info.width,
        height: info.height,
        // The shadow is packed (no row padding), so pitch == width * bpp.
        pitch: (info.width as usize * BYTES_PER_PIXEL) as u32,
        format: ScanoutFormat::Bgra8888,
    };
    if let Err(e) = device.create_scanout_resource(resource, paddr, (pages * PAGE_SIZE_4K) as u32) {
        warn!("create_scanout_resource failed on primary display: {:?}", e);
        return Err(());
    }

    *FB.lock() = Some(Arc::new(FbEmulation {
        info,
        device,
        shadow,
    }));
    info!(
        "fbdev emulation ready: {}x{} ({} bytes shadow)",
        info.width, info.height, size
    );
    Ok(())
}

/// Returns whether fbdev emulation is ready (a primary display device was
/// probed and its shadow scanout resource was established successfully).
pub fn fb_available() -> bool {
    FB.is_inited() && FB.lock().is_some()
}

/// Display information for the primary framebuffer device.
///
/// # Panics
///
/// Panics if fbdev emulation is not available; callers must gate on
/// [`fb_available`].
pub fn fb_info() -> DisplayInfo {
    FB.lock()
        .as_ref()
        .expect("fb_info called without fb_available")
        .info
}

/// Virtual address of the emulated framebuffer's shadow buffer.
///
/// Returns `None` if emulation is unavailable. The returned address remains
/// valid for as long as fbdev emulation stays available (i.e. for kernel
/// lifetime, since teardown is not supported).
pub fn fb_shadow_vaddr() -> Option<VirtAddr> {
    FB.lock().as_ref().map(|fb| fb.shadow.start_va())
}

/// Physical address of the emulated framebuffer's shadow buffer, for `mmap`.
pub fn fb_shadow_paddr() -> Option<PhysAddr> {
    let vaddr = fb_shadow_vaddr()?;
    Some(v2p(vaddr))
}

/// Size in bytes of the emulated framebuffer's shadow buffer.
pub fn fb_shadow_size() -> Option<usize> {
    FB.lock().as_ref().map(|fb| fb.shadow.size())
}

/// Records that a `/dev/fb0` file description was opened.
///
/// Starts the mmap refresh task on the first client. The task keeps presenting
/// the shadow until DRM claims the scanout or every fbdev client closes.
pub fn fb_client_open() {
    FB_CLIENTS.fetch_add(1, Ordering::Relaxed);
    start_refresh_task();
}

/// Records that a `/dev/fb0` file description was released.
pub fn fb_client_release() {
    let _ = FB_CLIENTS.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
        Some(count.saturating_sub(1))
    });
}

/// Called when a DRM client presents or takes master so fbdev refresh does not
/// steal the physical scanout from a compositor.
pub fn drm_claim_scanout() {
    DRM_OWNS_SCANOUT.store(true, Ordering::Release);
}

/// Called when the last DRM client drops master or closes `/dev/dri/card0`.
pub fn drm_release_scanout() {
    DRM_OWNS_SCANOUT.store(false, Ordering::Release);
}

/// Returns whether a DRM client currently owns the physical scanout.
pub fn drm_owns_scanout() -> bool {
    DRM_OWNS_SCANOUT.load(Ordering::Acquire)
}

fn start_refresh_task() {
    if REFRESH_STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    ktask::spawn_with_name(
        || loop {
            let has_clients = FB_CLIENTS.load(Ordering::Relaxed) > 0;
            if has_clients && !drm_owns_scanout() {
                let _ = fb_present();
                ktask::sleep(REFRESH_ACTIVE_INTERVAL);
            } else {
                ktask::sleep(REFRESH_IDLE_INTERVAL);
            }
        },
        "fb-refresh".into(),
    );
}

/// Push the shadow buffer to the visible scanout.
///
/// Present is skipped while DRM owns the scanout. Returns `true` on success;
/// transient host failures are not fatal.
pub fn fb_present() -> bool {
    if drm_owns_scanout() {
        return false;
    }
    let Some(fb) = FB.lock().as_ref().map(Arc::clone) else {
        return false;
    };
    let rect = ScanoutRect {
        x: 0,
        y: 0,
        width: fb.info.width,
        height: fb.info.height,
    };
    fb.device
        .present_scanout_resource(FB_RESOURCE_ID, rect)
        .is_ok()
}

/// Copy `len` bytes from the shadow at `offset` into `dst`.
///
/// Concurrent mmap writers may race this copy; that matches legacy fbdev.
///
/// # Safety
///
/// `src` must be valid for reads of `src_len` bytes for the kernel lifetime.
pub unsafe fn copy_from_shadow(
    src: *const u8,
    src_len: usize,
    dst: &mut [u8],
    offset: u64,
) -> usize {
    copy_from_shadow_ptr(src, src_len, dst.as_mut_ptr(), dst.len(), offset)
}

/// Copy `src` into the shadow at `offset`. Returns the number of bytes written,
/// or `None` when `offset` is at or past the end of the buffer.
///
/// # Safety
///
/// `dst` must be valid for writes of `dst_len` bytes for the kernel lifetime.
pub unsafe fn copy_to_shadow(
    dst: *mut u8,
    dst_len: usize,
    src: &[u8],
    offset: u64,
) -> Option<usize> {
    if offset >= dst_len as u64 {
        return None;
    }
    let start = offset as usize;
    let len = src.len().min(dst_len - start);
    // SAFETY: the caller guarantees `dst` is valid for `dst_len` bytes, and
    // `start + len` is in bounds by the checks above. mmap clients may write
    // the same bytes concurrently; fbdev permits that data race.
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr(), dst.add(start), len);
    }
    Some(len)
}

fn copy_from_shadow_ptr(
    src: *const u8,
    src_len: usize,
    dst: *mut u8,
    dst_len: usize,
    offset: u64,
) -> usize {
    if offset >= src_len as u64 {
        return 0;
    }
    let start = offset as usize;
    let len = dst_len.min(src_len - start);
    // SAFETY: `start + len` is in bounds of `src_len`, and the caller of the
    // public wrapper guarantees both regions are valid for that many bytes.
    unsafe {
        core::ptr::copy_nonoverlapping(src.add(start), dst, len);
    }
    len
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::{copy_from_shadow, copy_to_shadow};

    #[def_test]
    fn copy_from_shadow_honors_offset_and_eof() {
        let src = *b"abcdefghijkl";
        let mut dst = [0u8; 4];
        // SAFETY: `src` is a local array.
        let copied = unsafe { copy_from_shadow(src.as_ptr(), src.len(), &mut dst, 3) };
        assert_eq!(copied, 4);
        assert_eq!(&dst, b"defg");

        // SAFETY: `src` and `dst` are local initialized arrays. The source
        // pointer is valid for `src.len()` bytes, and the offset is checked
        // before any copy, so the EOF call performs no out-of-bounds access.
        let copied = unsafe { copy_from_shadow(src.as_ptr(), src.len(), &mut dst, 12) };
        assert_eq!(copied, 0);

        let mut short = [0u8; 8];
        // SAFETY: `src` and `short` are local initialized arrays. Both
        // pointers remain valid for their declared lengths, and the helper
        // clamps the two-byte copy to the source range starting at offset 10.
        let copied = unsafe { copy_from_shadow(src.as_ptr(), src.len(), &mut short, 10) };
        assert_eq!(copied, 2);
        assert_eq!(&short[..2], b"kl");
    }

    #[def_test]
    fn copy_to_shadow_honors_offset_and_rejects_past_end() {
        let mut dst = *b"xxxxxxxxxxxx";
        // SAFETY: `dst` is a local array.
        let written = unsafe { copy_to_shadow(dst.as_mut_ptr(), dst.len(), b"YZ", 4) };
        assert_eq!(written, Some(2));
        assert_eq!(&dst, b"xxxxYZxxxxxx");

        // SAFETY: `dst` is a local initialized array valid for `dst.len()`
        // bytes. Offset 12 is at the end, so the helper returns before using
        // the pointer.
        let written = unsafe { copy_to_shadow(dst.as_mut_ptr(), dst.len(), b"nope", 12) };
        assert_eq!(written, None);

        // SAFETY: `dst` is a local initialized array valid for 12 bytes, and
        // the helper bounds the two-byte copy to the range starting at 10.
        let written = unsafe { copy_to_shadow(dst.as_mut_ptr(), dst.len(), b"END", 10) };
        assert_eq!(written, Some(2));
        assert_eq!(&dst[10..], b"EN");
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! present subsystem of the virtual DRM card, split out of card0.rs.

use super::*;

pub(crate) fn display_resolution(dev: &DrmCard) -> (u32, u32) {
    dev.display()
        .map(|display| {
            let info = display.info();
            (info.width, info.height)
        })
        .unwrap_or((640, 480))
}

pub(crate) fn current_mode(dev: &DrmCard) -> DrmModeModeInfo {
    let (w, h) = display_resolution(dev);
    let mut name = [0u8; 32];
    let s = b"current";
    name[..s.len()].copy_from_slice(s);

    let hdisplay = w as u16;
    let hsync_start = hdisplay + CVT_RB_HFRONT_PORCH;
    let hsync_end = hsync_start + CVT_RB_HSYNC_WIDTH;
    let htotal = hsync_end + CVT_RB_HBACK_PORCH;

    let vdisplay = h as u16;
    let vsync_start = vdisplay + CVT_RB_VFRONT_PORCH;
    let vsync_end = vsync_start + CVT_RB_VSYNC_WIDTH;
    let vtotal = vsync_end + CVT_RB_VBACK_PORCH;

    let vrefresh: u32 = DEFAULT_VREFRESH;
    let clock = ((htotal as u32) * (vtotal as u32) * vrefresh) / 1000;

    DrmModeModeInfo {
        clock,
        hdisplay,
        hsync_start,
        hsync_end,
        htotal,
        hskew: 0,
        vdisplay,
        vsync_start,
        vsync_end,
        vtotal,
        vscan: 0,
        vrefresh,
        flags: 0,
        kind: 0,
        name,
    }
}

impl DrmIoctl for DrmWaitVblank {
    const CMD: u32 = iowr::<DrmWaitVblank>(DRM_TYPE, 0x3A);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, request: &mut Self) -> VfsResult<usize> {
        let is_relative = request.rep_type & DRM_VBLANK_RELATIVE != 0;
        let current = dev.sequence.load(Ordering::Acquire);
        let target = if is_relative {
            current.wrapping_add(request.sequence)
        } else {
            request.sequence
        };
        let raw_wait = target.wrapping_sub(current);
        let wait_count = if raw_wait == 0 || raw_wait >= i32::MAX as u32 {
            1
        } else {
            raw_wait
        };

        const FRAME_PERIOD_NS: u64 = ktime_types::NANOS_PER_SEC / 60;
        let delay =
            ktime_types::TimeSpan::from_nanos(FRAME_PERIOD_NS.saturating_mul(wait_count as u64));
        ktask::sleep(delay);
        dev.sequence.fetch_add(wait_count, Ordering::AcqRel);

        let now = khal::time::monotonic_time();
        let now = now.span_since_origin();
        *request = DrmWaitVblank {
            rep_type: 0,
            sequence: dev.sequence.load(Ordering::Acquire),
            tv_sec: now.as_secs() as i64,
            tv_usec: now.subsec_micros() as i64,
        };
        Ok(0)
    }
}

impl DrmCard {
    pub(crate) fn present_fb(&self, fb_id: FbId) {
        let (resource_id, width, height) = match self.fbs.lock().get(&fb_id) {
            Some(fb) => (fb.resource_id, fb.width, fb.height),
            None => return,
        };
        let Ok(device) = self.display() else {
            return;
        };
        let rect = ScanoutRect {
            x: 0,
            y: 0,
            width,
            height,
        };
        if let Err(err) = device.present_scanout_resource(resource_id, rect) {
            log::warn!(
                "drm: failed to present scanout resource {}: {:?}",
                resource_id,
                err
            );
            // The host may already have switched (a later command in the same
            // call can be the one that failed), so the new resource is held back
            // as well: retirement skips both markers until a present succeeds.
            self.unsettled.lock().insert(resource_id);
            return;
        }
        // Remember what is actually being scanned out: `MODE_CLOSEFB` has to
        // refuse to close a framebuffer that a page flip or a dirtyfb put up
        // there, and neither of those records itself anywhere else.  The
        // resource is recorded too, because retirement has to keep scanning
        // targets alive after their framebuffer is gone.
        self.presented_fb.store(fb_id.wire(), Ordering::Relaxed);
        self.presented_resource
            .store(resource_id, Ordering::Relaxed);
        // A successful `SET_SCANOUT` replaces the one and only scanout target, so
        // every earlier "unknown" resource is now provably off screen: the set can
        // be dropped wholesale, otherwise an entry whose framebuffer is closed
        // later would never be retired.  The resource just presented is tracked by
        // `presented_resource` from here on.
        self.unsettled.lock().clear();
        // A new scanout target is up, so parked buffers that nothing else
        // names may now be retired.
        self.sweep_retired_dumbs();
    }

    pub(crate) fn queue_flip_event(&self, user_data: u64) {
        let seq = self
            .sequence
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let now = khal::time::monotonic_time();
        let now = now.span_since_origin();
        let ev = DrmEventVblank {
            base: DrmEvent {
                event_type: DRM_EVENT_FLIP_COMPLETE,
                length: core::mem::size_of::<DrmEventVblank>() as u32,
            },
            user_data,
            tv_sec: now.as_secs() as u32,
            tv_usec: now.subsec_micros(),
            sequence: seq,
            crtc_id: CRTC_ID,
        };
        let enqueued = {
            let mut queue = self.events.lock();
            if queue.len() >= MAX_EVENTS {
                false
            } else {
                queue.push_back(ev);
                true
            }
        };
        if enqueued {
            self.poll_rx.wake();
        }
    }
}

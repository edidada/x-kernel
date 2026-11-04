// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Rockchip RK3588 VOP2 display controller.
//!
//! The VOP2 is the compositor that scans frames out to the panel interfaces
//! (DSI, DP, HDMI, eDP). On this kernel it is used in its simplest form: one
//! video port drives one (E)smart window that scans a single frame buffer, and
//! every other layer of the composition stack (plane setup, scaling, format
//! conversion) stays at its firmware-programmed default.
//!
//! Register semantics follow the (E)smart window layout documented in
//! `drivers/gpu/drm/rockchip/rockchip_drm_vop2.h`:
//!
//! * window registers are double buffered and only become active when the
//!   owning video port consumes a `CFG_DONE` commit at frame start;
//! * `CFG_DONE` needs both the VP bit and its write mask in bits 16..=31;
//! * a video port must be in standby while its window is re-programmed,
//!   otherwise the pending update is never latched and the read-back keeps
//!   returning the previous configuration.
//!
//! Cache maintenance for the scan-out buffer belongs to the owner of that
//! buffer (the framebuffer layer), not to this driver: the controller reads
//! physical memory and has no visibility into how the CPU mapped it.
//!
//! # Register offsets versus device tree
//!
//! The register map in [`regs`] is RK3588 TRM data, expressed with the
//! `tock-registers` interface so the compiler checks the layout and the call
//! sites use named fields instead of offsets. Firmware does not describe those
//! offsets, and the Linux driver keeps the same values in its per-SoC register
//! tables. The device tree carries what is instance- or board-specific
//! instead: the window base and size (the `reg` property the platform glue
//! maps), the panel timing (`display-timings`) and the port-to-panel wiring
//! (`remote-endpoint` graph), both of which the glue resolves before
//! constructing this driver. Supporting another VOP2 generation means adding a
//! per-SoC register module selected by the matched compatible string, not
//! reading it from firmware.

#![no_std]

mod regs;

use device_res::{TimeOp, TimeSpan};
use display::{
    Device, DeviceKind, DisplayDevice, DisplayInfo, DriverError, DriverResult, ScanoutFormat,
    ScanoutRect, ScanoutResource,
};
use regs::{CFG_DONE, DSP_CTRL, REGION0_CTRL, Vop2Registers};
use tock_registers::interfaces::{ReadWriteable, Readable, Writeable};

/// Time the controller needs between two window-programming steps.
///
/// The controller latches pending window state at frame start, so the standby
/// entry, the commit and the resume each have to be separated by at least one
/// frame. One 60 Hz frame is 16.7 ms, and the margin covers a slower panel
/// clock.
const FRAME_INTERVAL: TimeSpan = TimeSpan::from_millis(20);

/// Scan-out configuration of one VOP2 video port.
#[derive(Debug, Clone, Copy)]
pub struct Vop2Port {
    /// Video port index (0..=3 on RK3588).
    pub id: u8,
    /// Visible resolution of the panel connected to this port.
    pub info: DisplayInfo,
}

/// VOP2 display controller instance.
///
/// One instance owns one video port and the (E)smart window that feeds it.
pub struct Vop2Display {
    regs: &'static Vop2Registers,
    port: Vop2Port,
    /// Monotonic clock capability used to wait out display frames.
    clock: &'static dyn TimeOp,
}

impl Vop2Display {
    /// Builds a display device over an already-mapped VOP2 register window.
    ///
    /// # Safety
    ///
    /// `base` must be a kernel virtual address that maps the VOP2 register
    /// window, is exclusively owned by this driver instance, and stays mapped
    /// for the lifetime of the returned object.
    pub unsafe fn new(base: usize, port: Vop2Port, clock: &'static dyn TimeOp) -> Self {
        // SAFETY: the caller guarantees `base` maps the controller's register
        // window exclusively and for the lifetime of this object; the register
        // interface only performs volatile accesses inside that window.
        let regs = unsafe { &*(base as *const Vop2Registers) };
        Self { regs, port, clock }
    }

    /// Commits the pending window registers to the active bank at the next frame.
    fn commit(&self) {
        let port = 1u32 << self.port.id;
        self.regs
            .cfg_done
            .set(CFG_DONE::GLB_EN::SET.value | port | (port << 16));
    }

    /// Points the port's window at `paddr` as a full-screen 1:1 scan-out.
    fn program(&self, paddr: usize, width: u32, height: u32, pitch: u32) -> DriverResult {
        if width == 0 || height == 0 || !pitch.is_multiple_of(4) || pitch / 4 < width {
            return Err(DriverError::InvalidInput);
        }
        let vp = self.regs.vp(self.port.id);
        let win = self.regs.window(self.port.id);

        // The window registers only latch while the video port is idle.
        vp.dsp_ctrl.modify(DSP_CTRL::STANDBY::SET);
        self.settle_frame();

        win.region0_ctrl.write(REGION0_CTRL::ENABLE::CLEAR);
        win.yrgb_mst.set(paddr as u32);
        win.cbr_mst.set(0);
        win.vir.set(pitch / 4);
        win.act_info.set(((height - 1) << 16) | (width - 1));
        win.dsp_info.set(((height - 1) << 16) | (width - 1));
        win.dsp_st.set(0);
        // Source and destination sizes match, so the scaler stays bypassed.
        win.scl_ctrl.set(0);
        win.scl_factor_yrgb.set(0);
        win.scl_factor_cbr.set(0);
        win.region0_ctrl.write(REGION0_CTRL::ENABLE::SET);

        // The port latches the pending bank at the next frame start, so the
        // update cannot be confirmed synchronously; the caller can poll
        // [`Vop2Display::latched_scanout`] once frames have gone by.
        self.commit();
        self.settle_frame();
        vp.dsp_ctrl.modify(DSP_CTRL::STANDBY::CLEAR);
        self.commit();
        self.settle_frame();
        Ok(())
    }

    /// Lets at least one display frame elapse.
    ///
    /// The writes issued before this call are posted to the controller; the
    /// wait gives it the frame boundary it needs to act on them.
    fn settle_frame(&self) {
        self.clock.delay(FRAME_INTERVAL);
    }

    /// Scan-out address currently latched in the window register bank.
    ///
    /// A re-programmed window only becomes active at the next frame start, so
    /// this reads the previous value until then.
    pub fn latched_scanout(&self) -> u32 {
        self.regs.window(self.port.id).yrgb_mst.get()
    }

    /// Controller version register, useful to confirm the SoC generation.
    pub fn version(&self) -> u32 {
        self.regs.version.get()
    }

    /// Reports the active scan-out geometry programmed into the port.
    ///
    /// Useful for diagnostics when the controller is inherited from firmware:
    /// the values are the ones the port is actually scanning.
    pub fn active_geometry(&self) -> (u32, u32) {
        let vp = self.regs.vp(self.port.id);
        (
            (vp.post_dsp_hact_info.get() & 0xffff) + 1,
            (vp.post_dsp_vact_info.get() & 0xffff) + 1,
        )
    }
}

// SAFETY: the register block is memory-mapped device state, not Rust-owned
// memory. Every access goes through the volatile register interface, the
// driver never forms `&mut` aliases to it, and ordering against the hardware is
// the driver's own responsibility, so sharing the handle between threads is
// sound.
unsafe impl Send for Vop2Display {}
// SAFETY: see the `Send` implementation above; the handle is an immutable view
// of device registers whose mutation is serialized by the hardware protocol.
unsafe impl Sync for Vop2Display {}

impl Device for Vop2Display {
    fn name(&self) -> &str {
        "rk3588-vop2"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Display
    }
}

impl DisplayDevice for Vop2Display {
    fn info(&self) -> DisplayInfo {
        self.port.info
    }

    fn need_flush(&self) -> bool {
        // The controller scans guest memory directly; the framebuffer layer
        // owns the cache maintenance for that buffer.
        false
    }

    fn flush(&self) -> DriverResult {
        Ok(())
    }

    fn create_scanout_resource(
        &self,
        resource: ScanoutResource,
        paddr: u64,
        _length: u32,
    ) -> DriverResult {
        // The window only handles one memory layout here; refuse anything else
        // rather than scanning it with the wrong channel order.
        if resource.format != ScanoutFormat::Bgra8888 {
            return Err(DriverError::Unsupported);
        }
        self.program(
            paddr as usize,
            resource.width,
            resource.height,
            resource.pitch,
        )
    }

    fn present_scanout_resource(&self, _resource_id: u32, _rect: ScanoutRect) -> DriverResult {
        Ok(())
    }

    fn flush_scanout_resource(&self, _resource_id: u32, _rect: ScanoutRect) -> DriverResult {
        Ok(())
    }
}

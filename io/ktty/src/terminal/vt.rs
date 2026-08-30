// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Single-console Linux VT compatibility for framebuffer clients such as Links2.
//!
//! This is not a full virtual-terminal subsystem. X-Kernel exposes one console
//! (vt 1). The ioctls below store process-control state and answer queries so
//! userspace framebuffer drivers can finish their Linux VT handshake.

use bytemuck::AnyBitPattern;
use kerrno::KError;

/// Linux `VT_AUTO`.
pub const VT_AUTO: i8 = 0;
/// Linux `VT_PROCESS`.
pub const VT_PROCESS: i8 = 1;
/// Linux `VT_ACKACQ` (also the `VT_RELDISP` "switch-to complete" argument).
pub const VT_ACKACQ: usize = 2;

/// Linux `KD_TEXT`.
pub const KD_TEXT: u32 = 0;
/// Linux `KD_GRAPHICS`.
pub const KD_GRAPHICS: u32 = 1;
/// Linux `KD_TEXT0` (obsolete, mapped to [`KD_TEXT`]).
pub const KD_TEXT0: u32 = 2;
/// Linux `KD_TEXT1` (obsolete, mapped to [`KD_TEXT`]).
pub const KD_TEXT1: u32 = 3;

/// The only emulated console number (`fg_console + 1` in Linux).
pub const ACTIVE_VT: u16 = 1;

/// Linux `MAX_NR_CONSOLES`.
const MAX_NR_CONSOLES: usize = 63;

/// Linux `struct vt_mode`.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, AnyBitPattern)]
pub struct VtMode {
    /// `VT_AUTO` or `VT_PROCESS`.
    pub mode: i8,
    /// Hang on writes when the vt is not active (ignored here).
    pub waitv: i8,
    /// Signal raised on a release request.
    pub relsig: i16,
    /// Signal raised on acquisition.
    pub acqsig: i16,
    /// Unused; Linux stores 0.
    pub frsig: i16,
}

impl Default for VtMode {
    fn default() -> Self {
        Self {
            mode: VT_AUTO,
            waitv: 0,
            relsig: 0,
            acqsig: 0,
            frsig: 0,
        }
    }
}

/// Linux `struct vt_stat`.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, AnyBitPattern)]
pub struct VtStat {
    /// Active vt number (1-based).
    pub v_active: u16,
    /// Signal to send (unused by Links2).
    pub v_signal: u16,
    /// Bitmask of open vts. Bit 0 is `/dev/tty0`; bit 1 is vt 1.
    pub v_state: u16,
}

/// Per-tty Linux VT compatibility state.
#[derive(Debug, Clone, Copy)]
pub struct VtState {
    mode: VtMode,
    kd_mode: u32,
    /// Pending destination vt, or `-1` when no switch is in progress.
    newvt: i32,
}

impl Default for VtState {
    fn default() -> Self {
        Self {
            mode: VtMode::default(),
            kd_mode: KD_TEXT,
            newvt: -1,
        }
    }
}

impl VtState {
    /// Returns the current `VT_GETMODE` payload.
    pub fn mode(&self) -> VtMode {
        self.mode
    }

    /// Applies `VT_SETMODE`.
    pub fn set_mode(&mut self, mut mode: VtMode) -> Result<(), KError> {
        if mode.mode != VT_AUTO && mode.mode != VT_PROCESS {
            return Err(KError::InvalidInput);
        }
        // Linux ignores `frsig` and always stores 0.
        mode.frsig = 0;
        self.mode = mode;
        self.newvt = -1;
        Ok(())
    }

    /// Returns the current `KDGETMODE` value.
    pub fn kd_mode(&self) -> u32 {
        self.kd_mode
    }

    /// Applies `KDSETMODE`.
    pub fn set_kd_mode(&mut self, mode: usize) -> Result<(), KError> {
        let kd_mode = match mode as u32 {
            KD_GRAPHICS => KD_GRAPHICS,
            KD_TEXT0 | KD_TEXT1 | KD_TEXT => KD_TEXT,
            _ => return Err(KError::InvalidInput),
        };
        self.kd_mode = kd_mode;
        Ok(())
    }

    /// Global `VT_GETSTATE` for the single emulated console.
    pub fn global_stat() -> VtStat {
        VtStat {
            v_active: ACTIVE_VT,
            v_signal: 0,
            // Bit 0: `/dev/tty0` is always open. Bit 1: vt 1 is in use.
            v_state: 0b11,
        }
    }

    /// `VT_WAITACTIVE`: the single console is always active.
    pub fn wait_active(console: usize) -> Result<(), KError> {
        if console == 0 || console > MAX_NR_CONSOLES {
            return Err(KError::NoSuchDevice);
        }
        if console != ACTIVE_VT as usize {
            // No other vt will ever become active; fail instead of sleeping.
            return Err(KError::NoSuchDevice);
        }
        Ok(())
    }

    /// `VT_ACTIVATE` for the single-console compatibility layer.
    pub fn activate(console: usize) -> Result<(), KError> {
        if console == 0 || console > MAX_NR_CONSOLES {
            return Err(KError::NoSuchDevice);
        }
        if console != ACTIVE_VT as usize {
            return Err(KError::NoSuchDevice);
        }
        Ok(())
    }

    /// `VT_RELDISP` acknowledgement. There is never a pending switch.
    pub fn release_display(&mut self, swtch: usize) -> Result<(), KError> {
        if self.mode.mode != VT_PROCESS {
            return Err(KError::InvalidInput);
        }
        if self.newvt < 0 {
            // Linux: only `VT_ACKACQ` is valid when no switch is pending.
            return if swtch == VT_ACKACQ {
                Ok(())
            } else {
                Err(KError::InvalidInput)
            };
        }
        if swtch == 0 {
            self.newvt = -1;
            return Ok(());
        }
        self.newvt = -1;
        Ok(())
    }
}

#[cfg(unittest)]
mod tests {
    use kerrno::KError;
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn vt_mode_layout_matches_linux() {
        assert_eq!(core::mem::size_of::<VtMode>(), 8);
        assert_eq!(core::mem::size_of::<VtStat>(), 6);
    }

    #[def_test]
    fn set_mode_rejects_unknown_and_clears_frsig() {
        let mut state = VtState::default();
        let mode = VtMode {
            mode: 3,
            ..VtMode::default()
        };
        assert_eq!(state.set_mode(mode), Err(KError::InvalidInput));

        let mode = VtMode {
            mode: VT_PROCESS,
            frsig: 9,
            ..VtMode::default()
        };
        state.set_mode(mode).unwrap();
        assert_eq!(state.mode().mode, VT_PROCESS);
        assert_eq!(state.mode().frsig, 0);
    }

    #[def_test]
    fn wait_active_accepts_console_one_only() {
        assert_eq!(VtState::wait_active(1), Ok(()));
        assert_eq!(VtState::wait_active(0), Err(KError::NoSuchDevice));
        assert_eq!(VtState::wait_active(2), Err(KError::NoSuchDevice));
    }

    #[def_test]
    fn reldisp_ackacq_succeeds_without_pending_switch() {
        let mut state = VtState::default();
        state
            .set_mode(VtMode {
                mode: VT_PROCESS,
                ..VtMode::default()
            })
            .unwrap();
        assert_eq!(state.release_display(VT_ACKACQ), Ok(()));
        assert_eq!(state.release_display(1), Err(KError::InvalidInput));
    }

    #[def_test]
    fn kdsetmode_maps_obsolete_text_modes() {
        let mut state = VtState::default();
        state.set_kd_mode(KD_GRAPHICS as usize).unwrap();
        assert_eq!(state.kd_mode(), KD_GRAPHICS);
        state.set_kd_mode(KD_TEXT1 as usize).unwrap();
        assert_eq!(state.kd_mode(), KD_TEXT);
        assert_eq!(state.set_kd_mode(99), Err(KError::InvalidInput));
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kerrno::KError;
use khal::mem::v2p;
use kvfs::{
    DeviceFileOps, DeviceId, DirMapping, MmapMapper, NodeFlags, SimpleFs, VfsError, VfsFile,
    VfsFileBuilder, VfsInode, VfsResult,
};
use memaddr::{PhysAddrRange, VirtAddr};
use osvm::VirtMutPtr;

use crate::{DeviceFile, add_device_entry};

// Types from https://github.com/Tangzh33/asterinas

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct FrameBufferBitfield {
    /// The beginning of bitfield.
    offset: u32,
    /// The length of bitfield.
    length: u32,
    /// Most significant bit is right(!= 0).
    msb_right: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VarScreenInfo {
    pub xres: u32, // Visible resolution
    pub yres: u32,
    pub xres_virtual: u32, // Virtual resolution
    pub yres_virtual: u32,
    pub xoffset: u32, // Offset from virtual to visible
    pub yoffset: u32,
    pub bits_per_pixel: u32, // Guess what
    pub grayscale: u32,      // 0 = color, 1 = grayscale, >1 = FOURCC
    // Add other fields as needed
    pub red: FrameBufferBitfield, // Bitfield in framebuffer memory if true color
    pub green: FrameBufferBitfield, // Else only length is significant
    pub blue: FrameBufferBitfield,
    pub transp: FrameBufferBitfield, // Transparency
    pub nonstd: u32,                 // Non-standard pixel format
    pub activate: u32,               // See FB_ACTIVATE_*
    pub height: u32,                 // Height of picture in mm
    pub width: u32,                  // Width of picture in mm
    pub accel_flags: u32,            // (OBSOLETE) see fb_info.flags
    pub pixclock: u32,               // Pixel clock in ps (pico seconds)
    pub left_margin: u32,            // Time from sync to picture
    pub right_margin: u32,           // Time from picture to sync
    pub upper_margin: u32,           // Time from sync to picture
    pub lower_margin: u32,
    pub hsync_len: u32,     // Length of horizontal sync
    pub vsync_len: u32,     // Length of vertical sync
    pub sync: u32,          // See FB_SYNC_*
    pub vmode: u32,         // See FB_VMODE_*
    pub rotate: u32,        // Angle we rotate counter-clockwise
    pub colorspace: u32,    // Colorspace for FOURCC-based modes
    pub reserved: [u32; 4], // Reserved for future compatibility
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct FixScreenInfo {
    pub id: [u8; 16],       // Identification string, e.g., "TT Builtin"
    pub smem_start: u64,    // Start of framebuffer memory (physical address)
    pub smem_len: u32,      // Length of framebuffer memory
    pub type_: u32,         // See FB_TYPE_*
    pub type_aux: u32,      // Interleave for interleaved planes
    pub visual: u32,        // See FB_VISUAL_*
    pub xpanstep: u16,      // Zero if no hardware panning
    pub ypanstep: u16,      // Zero if no hardware panning
    pub ywrapstep: u16,     // Zero if no hardware ywrap
    pub line_length: u32,   // Length of a line in bytes
    pub mmio_start: u64,    // Start of Memory Mapped I/O (physical address)
    pub mmio_len: u32,      // Length of Memory Mapped I/O
    pub accel: u32,         // Indicate to driver which specific chip/card we have
    pub capabilities: u16,  // See FB_CAP_*
    pub reserved: [u16; 2], // Reserved for future compatibility
}

/// Framebuffer device for graphics output, backed by fbdev emulation's shadow
/// buffer (see [`fbdevice`]).
///
/// Userspace may `read`/`write`/`mmap` the shadow. The shadow is presented to
/// virtio-gpu on `write`, `FBIOPAN_DISPLAY`, `fsync`, and by the fbdev mmap
/// refresh task while this node is open and DRM does not own the scanout.
///
/// Access uses raw pointers rather than `&mut [u8]`: mmap clients, `read`, and
/// `write` share the same bytes and cannot satisfy Rust exclusive-borrow rules.
pub struct FrameBuffer {
    base: VirtAddr,
    size: usize,
}
impl FrameBuffer {
    pub fn new() -> Self {
        // fb_available() is checked by the caller (add_root_entries) before
        // constructing us, so the shadow is guaranteed present.
        let base = fbdevice::fb_shadow_vaddr().expect("fbdev shadow missing");
        let size = fbdevice::fb_shadow_size().expect("fbdev shadow missing");
        Self { base, size }
    }

    fn shadow_ptr(&self) -> *mut u8 {
        self.base.as_mut_ptr()
    }
}
impl DeviceFileOps for FrameBuffer {
    fn open(&self, _inode: &VfsInode, _file: &mut VfsFileBuilder) -> VfsResult<()> {
        fbdevice::fb_client_open();
        Ok(())
    }

    fn release(&self, _inode: &VfsInode, _file: &VfsFile) -> VfsResult<()> {
        fbdevice::fb_client_release();
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        // SAFETY: `base..base+size` is the fbdev shadow, mapped for kernel
        // lifetime by `fb_init`. Offset is clamped inside `copy_from_shadow`.
        // Concurrent mmap writers may race the copy, matching legacy fbdev.
        let copied =
            unsafe { fbdevice::copy_from_shadow(self.shadow_ptr(), self.size, buf, offset) };
        Ok(copied)
    }

    fn write(&self, _file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
        // SAFETY: same shadow lifetime as `read`. mmap clients may write the
        // same bytes concurrently.
        let Some(len) =
            (unsafe { fbdevice::copy_to_shadow(self.shadow_ptr(), self.size, buf, offset) })
        else {
            return Err(VfsError::StorageFull);
        };
        // Make the written pixels visible: write() is an explicit fbdev
        // present trigger. Failure is not fatal for the write itself.
        fbdevice::fb_present();
        Ok(len)
    }

    fn fsync(&self, _file: &VfsFile, _data_only: bool) -> VfsResult<()> {
        // Links2 calls fsync after mmap drawing and mouse cursor updates.
        fbdevice::fb_present();
        Ok(())
    }

    fn ioctl(&self, _file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            // FBIOGET_VSCREENINFO
            0x4600 => {
                let info = fbdevice::fb_info();
                // The shadow scanout buffer is always BGRA8888 (32bpp); the
                // line length reported in FSCREENINFO is width * 4 with no
                // row padding.
                let bpp = 32u32;
                (arg as *mut VarScreenInfo).write_vm(VarScreenInfo {
                    xres: info.width,
                    yres: info.height,
                    xres_virtual: info.width,
                    yres_virtual: info.height,
                    xoffset: 0,
                    yoffset: 0,
                    bits_per_pixel: bpp,
                    grayscale: 0,
                    red: FrameBufferBitfield {
                        offset: 16,
                        length: 8,
                        msb_right: 0,
                    },
                    green: FrameBufferBitfield {
                        offset: 8,
                        length: 8,
                        msb_right: 0,
                    },
                    blue: FrameBufferBitfield {
                        offset: 0,
                        length: 8,
                        msb_right: 0,
                    },
                    transp: FrameBufferBitfield {
                        offset: 24,
                        length: 8,
                        msb_right: 0,
                    },
                    nonstd: 0,
                    activate: 0,
                    height: 0,
                    width: 0,
                    accel_flags: 0,
                    pixclock: 10000000 / info.width * 1000 / info.height,
                    left_margin: (info.width / 8) & 0xf8,
                    right_margin: 32,
                    upper_margin: 16,
                    lower_margin: 4,
                    hsync_len: (info.width / 8) & 0xf8,
                    vsync_len: 4,
                    sync: 0,
                    vmode: 0,
                    rotate: 0,
                    colorspace: 0,
                    reserved: [0; 4],
                })?;
                Ok(0)
            }
            // FBIOPUT_VSCREENINFO
            0x4601 => Ok(0),
            // FBIOGET_FSCREENINFO
            0x4602 => {
                let info = fbdevice::fb_info();
                let line_length = info.width.checked_mul(4).unwrap_or(0);
                (arg as *mut FixScreenInfo).write_vm(FixScreenInfo {
                    id: *b"Virtio Framebuf\0",
                    // smem_start is the physical base of the shadow buffer, so
                    // userspace (and the kernel mmap path) describe the same
                    // backing memory the host scanout resource is bound to.
                    smem_start: v2p(self.base).as_usize() as u64,
                    smem_len: self.size as u32,
                    type_: 0,
                    type_aux: 0,
                    visual: 2, // FB_VISUAL_TRUECOLOR
                    xpanstep: 0,
                    ypanstep: 0,
                    ywrapstep: 0,
                    line_length,
                    mmio_start: 0,
                    mmio_len: 0,
                    accel: 0,
                    capabilities: 0,
                    reserved: [0; 2],
                })?;
                Ok(0)
            }
            // FBIOGETCMAP
            0x4604 => Ok(0),
            // FBIOPUTCMAP
            0x4605 => Ok(0),
            // FBIOPAN_DISPLAY: explicit "make the shadow visible" trigger for
            // apps that mmap the framebuffer and draw directly (writes via
            // write() present automatically). The framebuffer is a fixed
            // full-screen buffer, so panning is a no-op pan + refresh.
            0x4606 => {
                fbdevice::fb_present();
                Ok(0)
            }
            // FBIOBLANK
            0x4611 => Err(KError::InvalidInput),
            _ => Err(KError::NotATty),
        }
    }

    fn mmap(&self, _file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        let paddr = fbdevice::fb_shadow_paddr().ok_or(VfsError::NoSuchDevice)?;
        mapper.map_physical(PhysAddrRange::from_start_size(paddr, self.size))
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    if !fbdevice::fb_available() {
        return;
    }
    let fb0 = DeviceFile::new_character(
        fs.clone(),
        DeviceId::new(29, 0),
        Arc::new(FrameBuffer::new()),
    );
    add_device_entry(root, "fb0", fb0);
}

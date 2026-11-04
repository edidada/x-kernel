// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Framebuffer text console.
//!
//! This is the kernel counterpart of Linux `fbcon`: console output is turned
//! into glyphs painted straight into the fbdev shadow buffer, so whatever the
//! boot log and the native TTY print also shows up on the panel.
//!
//! The renderer owns no memory of its own beyond the cursor and parser state:
//! characters are rasterised directly into the shadow, and scrolling moves the
//! pixels of the previous rows up in place. A partial SGR/CSI subset is
//! understood so coloured console output keeps its colours instead of showing
//! raw escape sequences.

use core::sync::atomic::{AtomicBool, Ordering};

use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use memaddr::VirtAddr;

use crate::font::{FIRST_CHAR, GLYPH_HEIGHT, GLYPH_WIDTH, GLYPHS};

/// Default foreground: VGA light grey, matching a Linux text console.
const DEFAULT_FG: u32 = 0x00aa_aaaa;
/// Default background: black.
const DEFAULT_BG: u32 = 0x0000_0000;

/// VGA text-mode palette in `0x00RRGGBB` order.
const PALETTE: [u32; 16] = [
    0x0000_0000,
    0x00aa_0000,
    0x0000_aa00,
    0x00aa_5500,
    0x0000_00aa,
    0x00aa_00aa,
    0x0000_aaaa,
    0x00aa_aaaa,
    0x0055_5555,
    0x00ff_5555,
    0x0055_ff55,
    0x00ff_ff55,
    0x0055_55ff,
    0x00ff_55ff,
    0x0055_ffff,
    0x00ff_ffff,
];

/// Columns the screen is divided into.
const COLS: usize = 135;
/// Rows the screen is divided into.
const ROWS: usize = 120;

/// Escape-sequence parser state.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Parser {
    /// Ordinary characters.
    Ground,
    /// Saw `ESC`.
    Escape,
    /// Inside `ESC [ ...`, collecting parameters.
    Csi,
    /// Inside `ESC ] ...`, skipping an OSC string.
    Osc,
}

/// Cursor and escape-parser state of the console.
struct Console {
    cursor_x: usize,
    cursor_y: usize,
    fg: u32,
    bg: u32,
    parser: Parser,
    /// Digits of the current CSI parameter.
    param: u32,
    /// Whether at least one digit was seen for the current parameter.
    has_digit: bool,
    /// Collected CSI parameters.
    params: [u32; 8],
    params_len: usize,
    /// Whether the next output should start from a clean screen.
    needs_clear: bool,
    /// Artwork printed once when the console takes over the screen.
    banner: &'static str,
}

impl Console {
    const fn new() -> Self {
        Self {
            cursor_x: 0,
            cursor_y: 0,
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            parser: Parser::Ground,
            param: 0,
            has_digit: false,
            params: [0; 8],
            params_len: 0,
            needs_clear: true,
            banner: "",
        }
    }

    fn reset_parser(&mut self) {
        self.parser = Parser::Ground;
        self.param = 0;
        self.has_digit = false;
        self.params_len = 0;
    }
}

static CONSOLE: LazyInit<SpinNoIrq<Console>> = LazyInit::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Enables the framebuffer console and records the boot banner.
///
/// Must run after fbdev published its shadow buffer. The first subsequent
/// output replaces the boot splash with `banner` followed by the console text,
/// so the firmware logo stays up until the console has something real to say.
pub fn init(banner: &'static str) {
    CONSOLE.call_once(|| SpinNoIrq::new(Console::new()));
    CONSOLE.lock().banner = banner;
    ACTIVE.store(true, Ordering::Release);
    paint_splash();
}

/// Paints the boot splash that stays up until the first console output.
///
/// The firmware logo is replaced by a hue sweep so a stale or half-drawn
/// scan-out is obvious before any text exists.
fn paint_splash() {
    let Some(shadow) = crate::fb_shadow_vaddr() else {
        return;
    };
    let info = crate::fb_info();
    let width = info.width as usize;
    let height = info.height as usize;
    if width == 0 || height == 0 {
        return;
    }
    let ptr = shadow.as_usize() as *mut u32;
    for y in 0..height {
        let hue = hue_to_colour(y * 1536 / height);
        for x in 0..width {
            let is_border = x < 4 || y < 4 || x >= width - 4 || y >= height - 4;
            let colour = if is_border { 0x00ff_ffff } else { hue };
            // SAFETY: `ptr` starts at the fbdev shadow buffer and both indices
            // stay inside the `width * height` pixels of that buffer.
            unsafe { store_pixel(ptr, y * width + x, colour) };
        }
    }
    crate::fb_present();
}

/// Maps a colour wheel position in `0..1536` (six 60-degree sectors of 256
/// steps) to an opaque `0x00RRGGBB` pixel.
fn hue_to_colour(wheel: usize) -> u32 {
    let sector = (wheel / 256) % 6;
    let frac = (wheel % 256) as u32;
    let (r, g, b) = match sector {
        0 => (255, frac, 0),
        1 => (255 - frac, 255, 0),
        2 => (0, 255, frac),
        3 => (0, 255 - frac, 255),
        4 => (frac, 0, 255),
        _ => (255, 0, 255 - frac),
    };
    (r << 16) | (g << 8) | b
}

/// Returns whether the console is enabled and its backing buffer exists.
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Acquire) && crate::fb_available()
}

/// Console output sink: renders `bytes` into the framebuffer.
///
/// This runs from the console write path, which may hold spin locks and may run
/// with interrupts disabled, so it must never allocate, log, or block.
pub fn write(bytes: &[u8]) {
    let Some(shadow) = crate::fb_shadow_vaddr() else {
        return;
    };
    if !ACTIVE.load(Ordering::Acquire) || !crate::fb_available() {
        return;
    }
    let info = crate::fb_info();
    if info.width as usize / GLYPH_WIDTH < COLS || info.height as usize / GLYPH_HEIGHT < ROWS {
        return;
    }

    let mut console = CONSOLE.lock();
    let width = info.width as usize;
    // The boot splash stays up until the console has something to say; the
    // first real output replaces it with an empty text screen, like fbcon
    // taking over from a firmware logo.
    let mut drew = console.needs_clear;
    if console.needs_clear {
        console.clear_screen(shadow, width);
        console.needs_clear = false;
        console.draw_banner(shadow, width);
    }
    for &byte in bytes {
        drew |= console.feed(byte, shadow, width);
    }
    drop(console);

    if drew {
        // Presenting cleans the dirty cache lines; the whole scanout is cleaned
        // because fbdev only tracks the buffer, not the touched rectangle.
        crate::fb_present();
    }
}

impl Console {
    /// Consumes one output byte and returns whether anything was drawn.
    fn feed(&mut self, byte: u8, shadow: VirtAddr, width: usize) -> bool {
        match self.parser {
            Parser::Ground => self.feed_ground(byte, shadow, width),
            Parser::Escape => {
                match byte {
                    b'[' => {
                        self.parser = Parser::Csi;
                        self.params = [0; 8];
                        self.params_len = 0;
                        self.param = 0;
                        self.has_digit = false;
                    }
                    b']' => self.parser = Parser::Osc,
                    // Two-character escapes (ESC c, ESC 7, ESC =, ...) are not
                    // needed by the console; drop them.
                    _ => self.parser = Parser::Ground,
                }
                false
            }
            Parser::Csi => {
                match byte {
                    b'0'..=b'9' => {
                        self.param = self
                            .param
                            .saturating_mul(10)
                            .saturating_add(u32::from(byte - b'0'));
                        self.has_digit = true;
                    }
                    b';' => self.push_param(),
                    b'?' | b'!' | b'>' => {}
                    0x40..=0x7e => {
                        self.push_param();
                        self.parser = Parser::Ground;
                        return self.dispatch_csi(byte, shadow, width);
                    }
                    _ => self.reset_parser(),
                }
                false
            }
            Parser::Osc => {
                // Skip until BEL or ST (ESC \); only the BEL form is used by
                // the shells we run.
                if byte == 0x07 {
                    self.parser = Parser::Ground;
                }
                false
            }
        }
    }

    fn push_param(&mut self) {
        if self.params_len < self.params.len() {
            self.params[self.params_len] = self.param;
            self.params_len += 1;
        }
        self.param = 0;
        self.has_digit = false;
    }

    fn feed_ground(&mut self, byte: u8, shadow: VirtAddr, width: usize) -> bool {
        match byte {
            0x1b => {
                self.parser = Parser::Escape;
                false
            }
            b'\n' => {
                self.carriage_return();
                self.line_feed();
                self.draw_cell_glyph(b' ', shadow, width);
                true
            }
            b'\r' => {
                self.carriage_return();
                self.draw_cell_glyph(b' ', shadow, width);
                true
            }
            0x08 => {
                if self.cursor_x > 0 {
                    self.erase_cursor(shadow, width);
                    self.cursor_x -= 1;
                }
                self.draw_cursor(shadow, width);
                true
            }
            b'\t' => {
                let next = (self.cursor_x / 8 + 1) * 8;
                while self.cursor_x < next.min(COLS) {
                    self.put_char(b' ', shadow, width);
                }
                true
            }
            0x07 => false,
            FIRST_CHAR..=0x7e => {
                self.put_char(byte, shadow, width);
                true
            }
            // Non-ASCII output (e.g. UTF-8) has no glyph in the VGA font.
            _ => false,
        }
    }

    fn dispatch_csi(&mut self, final_byte: u8, shadow: VirtAddr, width: usize) -> bool {
        match final_byte {
            b'm' => {
                self.apply_sgr();
                false
            }
            b'J' => {
                let mode = self.params.first().copied().unwrap_or(0);
                if mode == 2 || mode == 3 {
                    self.clear_screen(shadow, width);
                    true
                } else {
                    false
                }
            }
            b'K' => {
                self.erase_line(shadow, width);
                true
            }
            b'A' => {
                let n = self.param_or(1, 1) as usize;
                self.erase_cursor(shadow, width);
                self.cursor_y = self.cursor_y.saturating_sub(n);
                self.draw_cursor(shadow, width);
                true
            }
            b'B' => {
                let n = self.param_or(1, 1) as usize;
                self.erase_cursor(shadow, width);
                self.cursor_y = (self.cursor_y + n).min(ROWS - 1);
                self.draw_cursor(shadow, width);
                true
            }
            b'C' => {
                let n = self.param_or(1, 1) as usize;
                self.erase_cursor(shadow, width);
                self.cursor_x = (self.cursor_x + n).min(COLS - 1);
                self.draw_cursor(shadow, width);
                true
            }
            b'D' => {
                let n = self.param_or(1, 1) as usize;
                self.erase_cursor(shadow, width);
                self.cursor_x = self.cursor_x.saturating_sub(n);
                self.draw_cursor(shadow, width);
                true
            }
            b'H' | b'f' => {
                let row = self.param_or(0, 1).clamp(1, ROWS as u32) as usize - 1;
                let col = self.param_or(1, 1).clamp(1, COLS as u32) as usize - 1;
                self.erase_cursor(shadow, width);
                self.cursor_x = col;
                self.cursor_y = row;
                self.draw_cursor(shadow, width);
                true
            }
            // Cursor position reports and everything else are ignored: the TTY
            // layer already answers device queries on the serial side.
            _ => false,
        }
    }

    fn param_or(&self, index: usize, default: u32) -> u32 {
        match self.params.get(index).copied() {
            Some(0) | None => default,
            Some(value) => value,
        }
    }

    fn apply_sgr(&mut self) {
        if self.params_len == 0 {
            self.fg = DEFAULT_FG;
            self.bg = DEFAULT_BG;
            return;
        }
        for index in 0..self.params_len {
            match self.params[index] {
                0 => {
                    self.fg = DEFAULT_FG;
                    self.bg = DEFAULT_BG;
                }
                1 => self.fg = brighten(self.fg),
                22 => {}
                30..=37 => self.fg = PALETTE[(self.params[index] - 30) as usize],
                90..=97 => self.fg = PALETTE[(self.params[index] - 90 + 8) as usize],
                39 => self.fg = DEFAULT_FG,
                40..=47 => self.bg = PALETTE[(self.params[index] - 40) as usize],
                100..=107 => self.bg = PALETTE[(self.params[index] - 100 + 8) as usize],
                49 => self.bg = DEFAULT_BG,
                _ => {}
            }
        }
    }

    /// Draws the boot banner centred horizontally, then leaves a blank line.
    ///
    /// The banner is rendered through the normal escape-sequence path so it can
    /// carry SGR colouring, and it is centred per line so the artwork does not
    /// depend on the panel width.
    fn draw_banner(&mut self, shadow: VirtAddr, width: usize) {
        let banner = self.banner;
        if banner.is_empty() {
            return;
        }
        let widest = banner.lines().map(|line| line.len()).max().unwrap_or(0);
        let padding = COLS.saturating_sub(widest) / 2;
        // Bright cyan reads well on black and matches the serial banner style.
        let saved_fg = self.fg;
        self.fg = PALETTE[14];
        for line in banner.lines() {
            self.cursor_x = padding;
            for byte in line.bytes() {
                self.feed_ground(byte, shadow, width);
            }
            self.carriage_return();
            self.line_feed();
        }
        self.carriage_return();
        self.line_feed();
        self.fg = saved_fg;
    }

    fn put_char(&mut self, byte: u8, shadow: VirtAddr, width: usize) {
        self.erase_cursor(shadow, width);
        self.draw_glyph(self.cursor_x, self.cursor_y, byte, shadow, width);
        self.cursor_x += 1;
        if self.cursor_x >= COLS {
            self.carriage_return();
            self.line_feed();
        }
        self.draw_cursor(shadow, width);
    }

    fn draw_cell_glyph(&mut self, byte: u8, shadow: VirtAddr, width: usize) {
        self.draw_glyph(self.cursor_x, self.cursor_y, byte, shadow, width);
        self.draw_cursor(shadow, width);
    }

    fn carriage_return(&mut self) {
        self.cursor_x = 0;
    }

    fn line_feed(&mut self) {
        if self.cursor_y + 1 >= ROWS {
            self.scroll_up();
        } else {
            self.cursor_y += 1;
        }
    }

    /// Erases the cell under the cursor before the cursor moves away.
    fn erase_cursor(&mut self, shadow: VirtAddr, width: usize) {
        self.fill_cell(self.cursor_x, self.cursor_y, self.bg, shadow, width);
    }

    /// Draws the caret as a two-pixel underline.
    fn draw_cursor(&mut self, shadow: VirtAddr, width: usize) {
        let ptr = shadow.as_usize() as *mut u32;
        let base_x = self.cursor_x * GLYPH_WIDTH;
        let base_y = self.cursor_y * GLYPH_HEIGHT + GLYPH_HEIGHT - 2;
        for row in 0..2 {
            for col in 0..GLYPH_WIDTH {
                // SAFETY: the cursor cell is inside the console grid, which is
                // bounded by the shadow buffer checked in `write`.
                unsafe { store_pixel(ptr, (base_y + row) * width + base_x + col, self.fg) };
            }
        }
    }

    fn fill_cell(&mut self, x: usize, y: usize, colour: u32, shadow: VirtAddr, width: usize) {
        let ptr = shadow.as_usize() as *mut u32;
        let base_x = x * GLYPH_WIDTH;
        let base_y = y * GLYPH_HEIGHT;
        for row in 0..GLYPH_HEIGHT {
            for col in 0..GLYPH_WIDTH {
                // SAFETY: the cell is inside the console grid, which is bounded
                // by the shadow buffer checked in `write`.
                unsafe { store_pixel(ptr, (base_y + row) * width + base_x + col, colour) };
            }
        }
    }

    fn draw_glyph(&mut self, x: usize, y: usize, byte: u8, shadow: VirtAddr, width: usize) {
        if x >= COLS || y >= ROWS {
            return;
        }
        let index = usize::from(byte.wrapping_sub(FIRST_CHAR));
        let glyph = GLYPHS.get(index).unwrap_or(&GLYPHS[0]);
        let ptr = shadow.as_usize() as *mut u32;
        let base_x = x * GLYPH_WIDTH;
        let base_y = y * GLYPH_HEIGHT;
        let (fg, bg) = (self.fg, self.bg);
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..GLYPH_WIDTH {
                let set = bits & (0x80 >> col) != 0;
                let colour = if set { fg } else { bg };
                // SAFETY: the cell is inside the console grid, which is bounded
                // by the shadow buffer checked in `write`.
                unsafe { store_pixel(ptr, (base_y + row) * width + base_x + col, colour) };
            }
        }
    }

    /// Erases from the cursor to the end of the current line.
    fn erase_line(&mut self, shadow: VirtAddr, width: usize) {
        let (x, y) = (self.cursor_x, self.cursor_y);
        for column in x..COLS {
            self.fill_cell(column, y, self.bg, shadow, width);
        }
        self.draw_cursor(shadow, width);
    }

    /// Clears the whole screen with the current background colour.
    fn clear_screen(&mut self, shadow: VirtAddr, width: usize) {
        let ptr = shadow.as_usize() as *mut u32;
        let height = ROWS * GLYPH_HEIGHT;
        for y in 0..height {
            for x in 0..COLS * GLYPH_WIDTH {
                // SAFETY: the console grid is bounded by the shadow buffer
                // checked in `write`.
                unsafe { store_pixel(ptr, y * width + x, self.bg) };
            }
        }
        self.cursor_x = 0;
        self.cursor_y = 0;
    }

    /// Moves the console content up by one text row.
    fn scroll_up(&mut self) {
        let (Some(shadow), Ok(info)) = (crate::fb_shadow_vaddr(), Ok::<_, ()>(crate::fb_info()))
        else {
            return;
        };
        let width = info.width as usize;
        let height = info.height as usize;
        let ptr = shadow.as_usize() as *mut u32;
        let shift = GLYPH_HEIGHT * width;
        // SAFETY: both ranges lie inside the shadow buffer and `copy` tolerates
        // the overlap between the source and destination rows.
        unsafe {
            core::ptr::copy(
                ptr.wrapping_add(shift),
                ptr,
                width * (height - GLYPH_HEIGHT),
            );
        }
        for row in 0..GLYPH_HEIGHT {
            for col in 0..width {
                // SAFETY: the cleared rows are the last text row of the same
                // shadow buffer.
                unsafe {
                    store_pixel(ptr, (height - GLYPH_HEIGHT + row) * width + col, self.bg);
                }
            }
        }
        self.cursor_y = ROWS - 1;
    }
}

/// Writes one pixel into the framebuffer shadow.
///
/// # Safety
///
/// `ptr` must point at the start of the framebuffer shadow buffer and `index`
/// must stay below that buffer's pixel count. `wrapping_add` is used so the
/// helper performs exactly one unsafe operation, which keeps the lint surface
/// of the drawing loops honest.
#[inline]
unsafe fn store_pixel(ptr: *mut u32, index: usize, colour: u32) {
    // SAFETY: the caller guarantees `index` addresses a pixel inside the
    // buffer, and `write_volatile` is the single store this helper performs.
    unsafe { ptr.wrapping_add(index).write_volatile(colour) };
}

/// Brightens a palette colour for SGR bold, mirroring the VGA attribute bit.
fn brighten(colour: u32) -> u32 {
    let index = PALETTE.iter().position(|entry| *entry == colour);
    match index {
        Some(index) if index < 8 => PALETTE[index + 8],
        Some(index) => PALETTE[index],
        None => colour,
    }
}

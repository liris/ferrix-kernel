//! Text console drawn directly on the UEFI GOP framebuffer.
//!
//! Glyphs come from font8x8 and are scaled 2x, so each cell is 16x16 pixels.
//! The console only touches memory, so it keeps working after ExitBootServices.

use core::cell::UnsafeCell;
use core::fmt;
use font8x8::legacy::BASIC_LEGACY;

const SCALE: usize = 2;
const CELL: usize = 8 * SCALE;
const MARGIN: usize = 16;

#[derive(Clone, Copy)]
pub enum Format {
    Rgb,
    Bgr,
}

#[derive(Clone, Copy)]
pub struct Framebuffer {
    pub base: *mut u32,
    pub width: usize,
    pub height: usize,
    /// Pixels per scanline (may be larger than `width`).
    pub stride: usize,
    pub format: Format,
}

#[derive(Clone, Copy)]
pub struct Color(pub u8, pub u8, pub u8);

pub const FG: Color = Color(0xe6, 0xe6, 0xe6);
pub const BG: Color = Color(0x16, 0x18, 0x1d);
pub const ACCENT: Color = Color(0xe0, 0x7b, 0x39);
pub const DIM: Color = Color(0x8a, 0x90, 0x9c);
pub const OK: Color = Color(0x6c, 0xc6, 0x7a);
pub const ERR: Color = Color(0xf0, 0x5d, 0x5d);

pub struct Console {
    fb: Framebuffer,
    col: usize,
    row: usize,
    cols: usize,
    rows: usize,
    fg: Color,
}

impl Console {
    fn new(fb: Framebuffer) -> Self {
        Self {
            fb,
            col: 0,
            row: 0,
            cols: (fb.width - 2 * MARGIN) / CELL,
            rows: (fb.height - 2 * MARGIN) / CELL,
            fg: FG,
        }
    }

    fn pixel(&self, c: Color) -> u32 {
        let (r, g, b) = (c.0 as u32, c.1 as u32, c.2 as u32);
        match self.fb.format {
            Format::Rgb => r | (g << 8) | (b << 16),
            Format::Bgr => b | (g << 8) | (r << 16),
        }
    }

    fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, c: Color) {
        let px = self.pixel(c);
        for yy in y..(y + h).min(self.fb.height) {
            let line = unsafe { self.fb.base.add(yy * self.fb.stride) };
            for xx in x..(x + w).min(self.fb.width) {
                unsafe { line.add(xx).write_volatile(px) };
            }
        }
    }

    pub fn clear(&mut self) {
        let (w, h) = (self.fb.width, self.fb.height);
        self.fill_rect(0, 0, w, h, BG);
        self.col = 0;
        self.row = 0;
    }

    pub fn set_color(&mut self, c: Color) {
        self.fg = c;
    }

    /// Moves the cursor to the start of `row`, clearing that line.
    pub fn goto_line(&mut self, row: usize) {
        self.row = row.min(self.rows - 1);
        self.col = 0;
        let y = MARGIN + self.row * CELL;
        let w = self.fb.width;
        self.fill_rect(0, y, w, CELL, BG);
    }

    pub fn row(&self) -> usize {
        self.row
    }

    fn draw_glyph(&mut self, ch: char) {
        let idx = if ch.is_ascii() { ch as usize } else { b'?' as usize };
        let glyph = BASIC_LEGACY[idx];
        let (fg, bg) = (self.pixel(self.fg), self.pixel(BG));
        let x0 = MARGIN + self.col * CELL;
        let y0 = MARGIN + self.row * CELL;
        for (gy, bits) in glyph.iter().enumerate() {
            for sy in 0..SCALE {
                let y = y0 + gy * SCALE + sy;
                let line = unsafe { self.fb.base.add(y * self.fb.stride) };
                for gx in 0..8 {
                    let px = if bits & (1 << gx) != 0 { fg } else { bg };
                    for sx in 0..SCALE {
                        unsafe { line.add(x0 + gx * SCALE + sx).write_volatile(px) };
                    }
                }
            }
        }
    }

    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 < self.rows {
            self.row += 1;
        } else {
            self.scroll();
        }
    }

    fn scroll(&mut self) {
        let stride = self.fb.stride;
        let top = MARGIN;
        let text_h = self.rows * CELL;
        unsafe {
            core::ptr::copy(
                self.fb.base.add((top + CELL) * stride),
                self.fb.base.add(top * stride),
                (text_h - CELL) * stride,
            );
        }
        let w = self.fb.width;
        self.fill_rect(0, top + text_h - CELL, w, CELL, BG);
    }

    pub fn put_char(&mut self, ch: char) {
        match ch {
            '\n' => self.newline(),
            '\r' => self.col = 0,
            _ => {
                if self.col >= self.cols {
                    self.newline();
                }
                self.draw_glyph(ch);
                self.col += 1;
            }
        }
    }
}

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for ch in s.chars() {
            self.put_char(ch);
        }
        Ok(())
    }
}

/// Global console. ferrix-kernel runs on a single core with interrupts off,
/// so unsynchronized access is sound for now; revisit at M3 (interrupts).
struct Global(UnsafeCell<Option<Console>>);
unsafe impl Sync for Global {}

static CONSOLE: Global = Global(UnsafeCell::new(None));

pub fn init(fb: Framebuffer) {
    unsafe { *CONSOLE.0.get() = Some(Console::new(fb)) };
}

/// Runs `f` with the console, if one has been initialized.
pub fn with<R>(f: impl FnOnce(&mut Console) -> R) -> Option<R> {
    unsafe { (*CONSOLE.0.get()).as_mut().map(f) }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    with(|c| fmt::Write::write_fmt(c, args).ok());
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::console::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}

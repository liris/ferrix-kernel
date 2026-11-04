//! Volatile access to memory shared with hardware: MMIO registers and DMA
//! buffers.
//!
//! A `Region` is made once, unsafely, from a range that is device registers
//! or RAM handed to a device. After that every access is bounds and alignment
//! checked, so drivers read and write registers without `unsafe`. RAM is
//! identity mapped, so a region's address is also what the device is given.

use core::ptr::{read_volatile, write_bytes, write_volatile};

#[derive(Clone, Copy)]
pub struct Region {
    base: usize,
    len: usize,
}

impl Region {
    /// # Safety
    ///
    /// `base..base + len` must be mapped and be either device registers or memory that
    /// only this region (and its copies) and the device use, with no Rust references into
    /// it while the region is in use.
    pub const unsafe fn new(base: usize, len: usize) -> Self {
        Self { base, len }
    }

    /// Takes `count` zeroed pages for a device to use.
    pub fn dma(count: usize) -> Self {
        let len = count * crate::mm::PAGE_SIZE;
        let base = crate::mm::pages(|pa| pa.alloc(count))
            .unwrap_or_else(|| panic!("out of memory for {count} DMA pages"));
        // SAFETY: pages just allocated, identity mapped, and owned by nobody else.
        let r = unsafe { Self::new(base, len) };
        r.zero();
        r
    }

    /// Address of the first byte, for handing to a device.
    pub fn addr(&self) -> u64 {
        self.base as u64
    }

    /// The `len` bytes at `off`.
    ///
    /// # Panics
    ///
    /// If they aren't inside this region.
    pub fn sub(&self, off: usize, len: usize) -> Self {
        assert!(off + len <= self.len, "{off:#x}+{len:#x} outside a {:#x}-byte region", self.len);
        Self { base: self.base + off, len }
    }

    fn check(&self, off: usize, size: usize) -> usize {
        assert!(
            off.is_multiple_of(size) && off + size <= self.len,
            "{size}-byte access at {off:#x} in a {:#x}-byte region",
            self.len
        );
        self.base + off
    }

    pub fn read32(&self, off: usize) -> u32 {
        let at = self.check(off, 4);
        // SAFETY: inside the region (checked) and aligned; see `new`.
        unsafe { read_volatile(at as *const u32) }
    }

    pub fn write32(&self, off: usize, v: u32) {
        let at = self.check(off, 4);
        // SAFETY: as in `read32`.
        unsafe { write_volatile(at as *mut u32, v) }
    }

    pub fn read64(&self, off: usize) -> u64 {
        let at = self.check(off, 8);
        // SAFETY: as in `read32`.
        unsafe { read_volatile(at as *const u64) }
    }

    pub fn write64(&self, off: usize, v: u64) {
        let at = self.check(off, 8);
        // SAFETY: as in `read32`.
        unsafe { write_volatile(at as *mut u64, v) }
    }

    pub fn write8(&self, off: usize, v: u8) {
        let at = self.check(off, 1);
        // SAFETY: as in `read32`.
        unsafe { write_volatile(at as *mut u8, v) }
    }

    /// Zeroes the whole region (plain stores: for memory, not registers).
    pub fn zero(&self) {
        // SAFETY: the whole region; see `new`.
        unsafe { write_bytes(self.base as *mut u8, 0, self.len) }
    }

    /// `len` bytes at `off` as a slice, cut short at the end of the region.
    ///
    /// # Safety
    ///
    /// Memory, not registers, and the device must not write to it while the slice lives.
    pub unsafe fn bytes(&self, off: usize, len: usize) -> &[u8] {
        let len = len.min(self.len.saturating_sub(off));
        // SAFETY: inside the region, and nothing writes to it meanwhile (caller).
        unsafe { core::slice::from_raw_parts((self.base + off) as *const u8, len) }
    }
}

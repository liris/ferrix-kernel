//! Our own stage 1 translation tables for EL1, replacing the firmware's.
//!
//! Still an identity map (VA = PA), but now only what the kernel needs is
//! mapped, with attributes chosen per region: RAM is cacheable and
//! non-executable except for code regions, the GIC and MMIO are device
//! memory, the framebuffer is non-cacheable. Page 0 is never mapped, so a
//! null dereference faults.
//!
//! 4 KiB granule, 48-bit VA (T0SZ = 16), walks start at level 0. Ranges are
//! mapped with the largest block that fits (1 GiB, 2 MiB) and 4 KiB pages
//! elsewhere. When two requests overlap, the first one wins, so the more
//! specific mappings (GIC, framebuffer) are made before plain RAM.

use crate::mm::{self, PAGE_SIZE};
use core::arch::asm;
use core::ptr;

const ENTRIES: usize = 512;

const DESC_VALID: u64 = 1 << 0;
/// Bit 1: "table" at levels 0-2, "page" at level 3. Clear at 0-2 = block.
const DESC_TABLE_OR_PAGE: u64 = 1 << 1;
const DESC_AF: u64 = 1 << 10;
const DESC_SH_INNER: u64 = 3 << 8;
const DESC_PXN: u64 = 1 << 53;
const DESC_UXN: u64 = 1 << 54;
const DESC_ADDR: u64 = 0x0000_ffff_ffff_f000;

// MAIR_EL1 slots, referenced by AttrIndx (descriptor bits 4:2).
const MAIR_NORMAL_WB: u64 = 0xff;
const MAIR_DEVICE_NGNRE: u64 = 0x04;
const MAIR_NORMAL_NC: u64 = 0x44;
const MAIR: u64 = MAIR_NORMAL_WB | MAIR_DEVICE_NGNRE << 8 | MAIR_NORMAL_NC << 16;
const ATTR_NORMAL: u64 = 0 << 2;
const ATTR_DEVICE: u64 = 1 << 2;
const ATTR_NC: u64 = 2 << 2;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Cacheable RAM, read/write, never executable.
    Data,
    /// Cacheable RAM, read/write/execute.
    Code,
    /// Device-nGnRE MMIO, never executable.
    Device,
    /// Normal non-cacheable memory (framebuffer).
    Uncached,
}

impl Kind {
    fn attrs(self) -> u64 {
        match self {
            Kind::Data => ATTR_NORMAL | DESC_SH_INNER | DESC_AF | DESC_PXN | DESC_UXN,
            Kind::Code => ATTR_NORMAL | DESC_SH_INNER | DESC_AF | DESC_UXN,
            Kind::Device => ATTR_DEVICE | DESC_AF | DESC_PXN | DESC_UXN,
            Kind::Uncached => ATTR_NC | DESC_SH_INNER | DESC_AF | DESC_PXN | DESC_UXN,
        }
    }
}

#[derive(Default, Clone, Copy)]
pub struct Stats {
    pub tables: usize,
    pub blocks: usize,
    pub pages: usize,
    /// Bytes mapped per `Kind`: data, code, device, uncached.
    pub bytes: [u64; 4],
}

pub struct Mapper {
    root: *mut u64,
    stats: Stats,
}

fn new_table(stats: &mut Stats) -> *mut u64 {
    let addr = mm::pages(|pa| pa.alloc(1)).expect("out of memory for page tables");
    unsafe { ptr::write_bytes(addr as *mut u8, 0, PAGE_SIZE) };
    stats.tables += 1;
    addr as *mut u64
}

impl Mapper {
    pub fn new() -> Self {
        let mut stats = Stats::default();
        let root = new_table(&mut stats);
        Self { root, stats }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Identity-maps `[start, start + len)`, widened to whole pages.
    /// Parts that are already mapped keep their existing mapping.
    pub fn map(&mut self, start: u64, len: u64, kind: Kind) {
        let page = PAGE_SIZE as u64;
        // Page 0 stays unmapped as a null-pointer guard.
        let end = (start + len).next_multiple_of(page);
        let start = (start & !(page - 1)).max(page);
        if start >= end {
            return;
        }
        let mapped = Self::map_level(&mut self.stats, self.root, 0, start, end, kind.attrs());
        self.stats.bytes[kind as usize] += mapped;
    }

    /// Maps `[start, end)` below `table`; returns the bytes newly mapped.
    fn map_level(stats: &mut Stats, table: *mut u64, level: usize, start: u64, end: u64, attrs: u64) -> u64 {
        let shift = 39 - 9 * level;
        let size = 1u64 << shift;
        let mut mapped = 0;
        let mut addr = start;
        while addr < end {
            let index = (addr >> shift) as usize % ENTRIES;
            let chunk_end = ((addr & !(size - 1)) + size).min(end);
            let entry = unsafe { &mut *table.add(index) };

            if *entry & DESC_VALID == 0 {
                if level == 3 {
                    *entry = addr | attrs | DESC_TABLE_OR_PAGE | DESC_VALID;
                    stats.pages += 1;
                    mapped += size;
                } else if level >= 1 && addr % size == 0 && chunk_end - addr == size {
                    *entry = addr | attrs | DESC_VALID;
                    stats.blocks += 1;
                    mapped += size;
                } else {
                    let next = new_table(stats);
                    *entry = next as u64 | DESC_TABLE_OR_PAGE | DESC_VALID;
                    mapped += Self::map_level(stats, next, level + 1, addr, chunk_end, attrs);
                }
            } else if level < 3 && *entry & DESC_TABLE_OR_PAGE != 0 {
                let next = (*entry & DESC_ADDR) as *mut u64;
                mapped += Self::map_level(stats, next, level + 1, addr, chunk_end, attrs);
            }
            // Otherwise a block or page already covers it: first mapping wins.
            addr = chunk_end;
        }
        mapped
    }

    /// Switches TTBR0_EL1 to these tables. Interrupts must be masked.
    ///
    /// # Safety
    /// Everything the kernel touches from now on (code, stack, heap, page
    /// tables, framebuffer, MMIO) must be mapped.
    pub unsafe fn activate(&self) {
        let pa_range = read_sysreg_mmfr0() & 0xf;
        // IPS uses the same encoding as PARange; our descriptors hold 48 bits.
        let ips = pa_range.min(5);
        let tcr = 16                // T0SZ: 48-bit VA
            | 1 << 8                // IRGN0: walks are write-back cacheable
            | 1 << 10               // ORGN0
            | 3 << 12               // SH0: inner shareable
            | 0 << 14               // TG0: 4 KiB
            | 16 << 16              // T1SZ (unused)
            | 1 << 23               // EPD1: no walks through TTBR1
            | 2 << 30               // TG1: 4 KiB
            | ips << 32;
        unsafe {
            asm!(
                "dsb ishst",           // table writes visible to the walker
                "msr mair_el1, {mair}",
                "msr tcr_el1, {tcr}",
                "isb",
                "msr ttbr0_el1, {ttbr}",
                "isb",
                "tlbi vmalle1",        // drop the firmware's translations
                "dsb ish",
                "isb",
                mair = in(reg) MAIR,
                tcr = in(reg) tcr,
                ttbr = in(reg) self.root as u64,
                options(nostack),
            );
        }
    }
}

fn read_sysreg_mmfr0() -> u64 {
    let v: u64;
    unsafe { asm!("mrs {}, id_aa64mmfr0_el1", out(reg) v, options(nomem, nostack)) };
    v
}

pub fn ttbr0() -> u64 {
    let v: u64;
    unsafe { asm!("mrs {}, ttbr0_el1", out(reg) v, options(nomem, nostack)) };
    v
}

/// Asks the MMU to translate `va` for an EL1 read, without faulting.
pub fn translate(va: u64) -> Option<u64> {
    let par: u64;
    unsafe {
        asm!(
            "at s1e1r, {va}",
            "isb",
            "mrs {par}, par_el1",
            va = in(reg) va,
            par = out(reg) par,
            options(nostack),
        )
    };
    if par & 1 != 0 { None } else { Some(par & DESC_ADDR | va & 0xfff) }
}

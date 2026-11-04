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
//!
//! Once active, the tables stay owned by this module, and `map_live` can add
//! regions found later (PCIe config space and BARs). Only invalid entries are
//! filled in then, so no TLB entry can be stale and no invalidation is needed.
//!
//! User programs (M7) each get an `AddressSpace`: a root table of their own
//! whose entries are copies of the kernel's, so everything below the root is
//! shared, plus one entry of their own for `USER_BASE..USER_END`. Kernel
//! mappings are EL1-only and global; user mappings are not global and are
//! tagged with the space's ASID, so switching TTBR0 between tasks needs no
//! TLB invalidation.

use crate::global::Global;
use crate::mm::{self, PAGE_SIZE};
use core::arch::asm;
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

const ENTRIES: usize = 512;

const DESC_VALID: u64 = 1 << 0;
/// Bit 1: "table" at levels 0-2, "page" at level 3. Clear at 0-2 = block.
const DESC_TABLE_OR_PAGE: u64 = 1 << 1;
/// AP[1]: EL0 may access it too.
const DESC_AP_EL0: u64 = 1 << 6;
/// AP[2]: read-only (at every level that may access it).
const DESC_AP_RO: u64 = 1 << 7;
const DESC_AF: u64 = 1 << 10;
/// Not global: the TLB entry belongs to one ASID.
const DESC_NG: u64 = 1 << 11;
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
    /// User program code: EL0 may read and execute it, nobody may write.
    UserCode,
    /// User read-only data.
    UserRo,
    /// User data and stack: EL0 may read and write, nobody may execute.
    UserData,
}

const KINDS: usize = 7;
const USER: u64 = ATTR_NORMAL | DESC_SH_INNER | DESC_AF | DESC_AP_EL0 | DESC_NG | DESC_PXN;

impl Kind {
    fn is_user(self) -> bool {
        matches!(self, Kind::UserCode | Kind::UserRo | Kind::UserData)
    }

    fn attrs(self) -> u64 {
        match self {
            Kind::Data => ATTR_NORMAL | DESC_SH_INNER | DESC_AF | DESC_PXN | DESC_UXN,
            Kind::Code => ATTR_NORMAL | DESC_SH_INNER | DESC_AF | DESC_UXN,
            Kind::Device => ATTR_DEVICE | DESC_AF | DESC_PXN | DESC_UXN,
            Kind::Uncached => ATTR_NC | DESC_SH_INNER | DESC_AF | DESC_PXN | DESC_UXN,
            Kind::UserCode => USER | DESC_AP_RO,
            Kind::UserRo => USER | DESC_AP_RO | DESC_UXN,
            Kind::UserData => USER | DESC_UXN,
        }
    }
}

#[derive(Default, Clone, Copy)]
pub struct Stats {
    pub tables: usize,
    pub blocks: usize,
    pub pages: usize,
    /// Bytes mapped per `Kind`, indexed by `Kind as usize`.
    pub bytes: [u64; KINDS],
}

pub struct Mapper {
    root: *mut u64,
    stats: Stats,
}

// SAFETY: the tables below `root` belong to the `Mapper` alone.
unsafe impl Send for Mapper {}

fn new_table(stats: &mut Stats) -> *mut u64 {
    let addr = mm::pages(|pa| pa.alloc(1)).expect("out of memory for page tables");
    // SAFETY: a page just allocated, identity mapped and owned by nobody else.
    unsafe { ptr::write_bytes(addr as *mut u8, 0, PAGE_SIZE) };
    stats.tables += 1;
    addr as *mut u64
}

/// Maps virtual `[start, end)` below `table` to physical `start + delta`
/// onwards; returns the bytes newly mapped.
///
/// # Safety
///
/// `table` must be a level `level` table from `new_table`, reachable only through tables
/// this module owns. If the tables are live, the new mappings must not let anything
/// reach memory it doesn't own.
unsafe fn map_level(
    stats: &mut Stats,
    table: *mut u64,
    level: usize,
    start: u64,
    end: u64,
    delta: u64,
    attrs: u64,
) -> u64 {
    let shift = 39 - 9 * level;
    let size = 1u64 << shift;
    let mut mapped = 0;
    let mut addr = start;
    while addr < end {
        let index = (addr >> shift) as usize % ENTRIES;
        let chunk_end = ((addr & !(size - 1)) + size).min(end);
        // SAFETY: `table` is a whole table page (caller) and `index < ENTRIES`.
        let entry = unsafe { &mut *table.add(index) };

        if *entry & DESC_VALID == 0 {
            if level == 3 {
                *entry = addr.wrapping_add(delta) | attrs | DESC_TABLE_OR_PAGE | DESC_VALID;
                stats.pages += 1;
                mapped += size;
            } else if level >= 1
                && addr.is_multiple_of(size)
                && delta.is_multiple_of(size)
                && chunk_end - addr == size
            {
                *entry = addr.wrapping_add(delta) | attrs | DESC_VALID;
                stats.blocks += 1;
                mapped += size;
            } else {
                let next = new_table(stats);
                *entry = next as u64 | DESC_TABLE_OR_PAGE | DESC_VALID;
                // SAFETY: `next` is a fresh table, now linked below `table`.
                mapped +=
                    unsafe { map_level(stats, next, level + 1, addr, chunk_end, delta, attrs) };
            }
        } else if level < 3 && *entry & DESC_TABLE_OR_PAGE != 0 {
            let next = (*entry & DESC_ADDR) as *mut u64;
            // SAFETY: every table entry below our root was made by `new_table`.
            mapped += unsafe { map_level(stats, next, level + 1, addr, chunk_end, delta, attrs) };
        }
        // Otherwise a block or page already covers it: first mapping wins.
        addr = chunk_end;
    }
    mapped
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
        assert!(!kind.is_user(), "identity-mapping kernel memory for EL0");
        let page = PAGE_SIZE as u64;
        // Page 0 stays unmapped as a null-pointer guard.
        let end = (start + len).next_multiple_of(page);
        let start = (start & !(page - 1)).max(page);
        if start >= end {
            return;
        }
        // SAFETY: `root` is ours. Identity mappings for EL1 only (checked above) can't
        // give anything access it didn't have, and existing entries are never replaced.
        let mapped =
            unsafe { map_level(&mut self.stats, self.root, 0, start, end, 0, kind.attrs()) };
        self.stats.bytes[kind as usize] += mapped;
    }

    /// Switches TTBR0_EL1 to these tables and keeps them for `map_live`.
    /// Interrupts must be masked.
    ///
    /// # Safety
    /// Everything the kernel touches from now on (code, stack, heap, page
    /// tables, framebuffer, MMIO) must be mapped.
    pub unsafe fn activate(self) {
        let pa_range = read_sysreg_mmfr0() & 0xf;
        // IPS uses the same encoding as PARange; our descriptors hold 48 bits.
        let ips = pa_range.min(5);
        #[expect(clippy::identity_op, reason = "zero-valued fields are spelled out")]
        let tcr = 16                // T0SZ: 48-bit VA
            | 1 << 8                // IRGN0: walks are write-back cacheable
            | 1 << 10               // ORGN0
            | 3 << 12               // SH0: inner shareable
            | 0 << 14               // TG0: 4 KiB
            | 16 << 16              // T1SZ (unused)
            | 1 << 23               // EPD1: no walks through TTBR1
            | 2 << 30               // TG1: 4 KiB
            | ips << 32;
        // SAFETY: the tables are complete (caller), and the TLB is flushed after the switch.
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
        ACTIVE.set(self);
    }
}

static ACTIVE: Global<Mapper> = Global::new("page tables");

/// Maps another region into the active tables. Like `Mapper::map`, parts
/// that are already mapped keep their mapping.
pub fn map_live(start: u64, len: u64, kind: Kind) {
    ACTIVE.with(|m| {
        m.map(start, len, kind);
        // The new entries must be visible to the walker before any access.
        // SAFETY: barriers only.
        unsafe { asm!("dsb ishst", "isb", options(nostack)) };
    });
}

/// User address range: level 0 entry 128 (64 TiB, 512 GiB long), far above
/// anything the kernel's identity map reaches. User programs link here.
pub const USER_BASE: u64 = 0x4000_0000_0000;
pub const USER_END: u64 = USER_BASE + (1 << 39);

/// ASID 0 is the kernel's; user spaces count up from 1, never reused.
static NEXT_ASID: AtomicU64 = AtomicU64::new(1);

/// A user program's address space: the kernel's mappings (EL1-only) plus its
/// own pages in the user range.
pub struct AddressSpace {
    root: *mut u64,
    asid: u64,
}

impl AddressSpace {
    /// Shares the kernel's tables as they are now. The kernel must not map
    /// new level 0 ranges afterwards (all of its mappings sit in entry 0).
    pub fn new() -> Self {
        let kernel = kernel_ttbr() as *const u64;
        let root = new_table(&mut Stats::default());
        let user = (USER_BASE >> 39) as usize;
        // SAFETY: both are whole table pages; `root` is fresh, so they don't overlap.
        unsafe {
            ptr::copy_nonoverlapping(kernel, root, ENTRIES);
            assert!(*root.add(user) & DESC_VALID == 0, "kernel maps the user range");
        }
        let asid = NEXT_ASID.fetch_add(1, Ordering::Relaxed);
        assert!(asid < 256, "out of ASIDs");
        Self { root, asid }
    }

    /// Maps user pages `[va, va + len)` to physical `pa` onwards.
    ///
    /// # Safety
    ///
    /// `[pa, pa + len)` must belong to this space alone: EL0 can read or write it, so the
    /// kernel must not use it for anything else.
    pub unsafe fn map(&mut self, va: u64, pa: u64, len: u64, kind: Kind) {
        let page = PAGE_SIZE as u64;
        assert!(va.is_multiple_of(page) && pa.is_multiple_of(page), "unaligned user mapping");
        assert!(va >= USER_BASE && va + len <= USER_END, "{:#x} is outside the user range", va);
        assert!(kind.is_user(), "kernel mapping in a user space");
        let end = (va + len).next_multiple_of(page);
        let delta = pa.wrapping_sub(va);
        // SAFETY: `root` is ours, the range is inside the user entry (checked above), which
        // no kernel table reaches, and the caller hands over the physical pages.
        unsafe { map_level(&mut Stats::default(), self.root, 0, va, end, delta, kind.attrs()) };
    }

    /// TTBR0_EL1 value selecting this space.
    pub fn ttbr(&self) -> u64 {
        self.root as u64 | self.asid << 48
    }
}

/// TTBR0_EL1 value for the kernel's own tables (ASID 0).
pub fn kernel_ttbr() -> u64 {
    ACTIVE.with(|m| m.root as u64)
}

/// Switches to another address space. Called on task switches.
///
/// # Safety
///
/// `ttbr` must come from `kernel_ttbr` or `AddressSpace::ttbr` of a space that is still
/// alive, so the kernel stays mapped after the switch.
pub unsafe fn set_ttbr0(ttbr: u64) {
    // SAFETY: the caller guarantees the tables map the kernel.
    unsafe { asm!("msr ttbr0_el1, {}", "isb", in(reg) ttbr, options(nostack)) };
}

/// Whether EL0 may read the byte at `va` in the current address space.
pub fn user_readable(va: u64) -> bool {
    let par: u64;
    // SAFETY: `at` only reports a translation (in PAR_EL1); it never faults.
    unsafe {
        asm!(
            "at s1e0r, {va}",
            "isb",
            "mrs {par}, par_el1",
            va = in(reg) va,
            par = out(reg) par,
            options(nostack),
        )
    };
    par & 1 == 0
}

fn read_sysreg_mmfr0() -> u64 {
    let v: u64;
    // SAFETY: reading an ID register has no side effects.
    unsafe { asm!("mrs {}, id_aa64mmfr0_el1", out(reg) v, options(nomem, nostack)) };
    v
}

pub fn ttbr0() -> u64 {
    let v: u64;
    // SAFETY: reading a system register has no side effects.
    unsafe { asm!("mrs {}, ttbr0_el1", out(reg) v, options(nomem, nostack)) };
    v
}

/// Asks the MMU to translate `va` for an EL1 read, without faulting.
pub fn translate(va: u64) -> Option<u64> {
    let par: u64;
    // SAFETY: as in `user_readable`.
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

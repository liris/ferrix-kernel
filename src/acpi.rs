//! Minimal ACPI walk: RSDP -> XSDT -> MADT to find the GIC, and -> MCFG to
//! find PCIe configuration space.
//!
//! The firmware leaves ACPI tables identity mapped, so physical addresses are
//! dereferenced directly. Tables are read with unaligned loads.

use core::ptr::read_unaligned;

/// Address of the firmware's ACPI 2.0 RSDP.
#[derive(Clone, Copy)]
pub struct Rsdp(usize);

impl Rsdp {
    /// # Safety
    ///
    /// `addr` must be the RSDP the firmware published, and the tables it leads to must stay
    /// mapped and unmodified.
    pub unsafe fn new(addr: usize) -> Self {
        Self(addr)
    }

    pub fn addr(self) -> usize {
        self.0
    }
}

pub struct GicInfo {
    /// GIC version from the distributor entry (0 = "probe the hardware").
    pub version: u8,
    pub gicd: Option<u64>,
    /// First redistributor discovery range: (base, length).
    pub gicr: Option<(u64, u32)>,
    pub cpus: usize,
    pub its: usize,
}

// Safety for these readers: `base + off` must be readable for the size read. Within the
// tables reached from an `Rsdp`, offsets inside a table's length are.

unsafe fn u8_at(base: usize, off: usize) -> u8 {
    // SAFETY: see above; `read_unaligned` has no alignment requirement.
    unsafe { read_unaligned((base + off) as *const u8) }
}

unsafe fn u32_at(base: usize, off: usize) -> u32 {
    // SAFETY: as for `u8_at`.
    unsafe { read_unaligned((base + off) as *const u32) }
}

unsafe fn u64_at(base: usize, off: usize) -> u64 {
    // SAFETY: as for `u8_at`.
    unsafe { read_unaligned((base + off) as *const u64) }
}

unsafe fn sig_is(base: usize, sig: &[u8]) -> bool {
    // SAFETY: as for `u8_at`, for `sig.len()` bytes.
    unsafe { core::slice::from_raw_parts(base as *const u8, sig.len()) == sig }
}

/// Header shared by all system description tables.
const SDT_HEADER_LEN: usize = 36;

fn find_table(rsdp: Rsdp, sig: &[u8; 4]) -> Result<usize, &'static str> {
    let rsdp = rsdp.0;
    // SAFETY: `Rsdp` guarantees the RSDP, and the XSDT and tables it points to, are mapped.
    // Offsets stay inside the 36-byte RSDP, the XSDT's length, and each table's header.
    unsafe {
        if !sig_is(rsdp, b"RSD PTR ") {
            return Err("bad RSDP signature");
        }
        if u8_at(rsdp, 15) < 2 {
            return Err("ACPI 1.0 RSDP (no XSDT)");
        }
        let xsdt = u64_at(rsdp, 24) as usize;
        if !sig_is(xsdt, b"XSDT") {
            return Err("bad XSDT signature");
        }
        let entries = (u32_at(xsdt, 4) as usize - SDT_HEADER_LEN) / 8;
        (0..entries)
            .map(|i| u64_at(xsdt, SDT_HEADER_LEN + i * 8) as usize)
            .find(|&t| sig_is(t, sig))
            .ok_or("table not found")
    }
}

pub fn find_gic(rsdp: Rsdp) -> Result<GicInfo, &'static str> {
    let madt = find_table(rsdp, b"APIC")?;
    let mut info = GicInfo { version: 0, gicd: None, gicr: None, cpus: 0, its: 0 };
    // SAFETY: `madt` came from `find_table`, and every read is inside its length: the loop
    // stops when a structure header would cross it.
    unsafe {
        let len = u32_at(madt, 4) as usize;
        // Interrupt controller structures start after the 8-byte MADT body.
        let mut off = SDT_HEADER_LEN + 8;
        while off + 2 <= len {
            let (ty, size) = (u8_at(madt, off), u8_at(madt, off + 1) as usize);
            if size == 0 {
                break;
            }
            match ty {
                0x0b => info.cpus += 1,
                0x0c => {
                    info.gicd = Some(u64_at(madt, off + 8));
                    info.version = u8_at(madt, off + 20);
                }
                0x0e if info.gicr.is_none() => {
                    info.gicr = Some((u64_at(madt, off + 4), u32_at(madt, off + 12)));
                }
                0x0f => info.its += 1,
                _ => {}
            }
            off += size;
        }
    }
    Ok(info)
}

/// PCIe enhanced configuration space (ECAM) of one segment.
pub struct Ecam {
    pub base: u64,
    pub segment: u16,
    pub bus_start: u8,
    pub bus_end: u8,
}

/// The first ECAM window listed in the MCFG.
pub fn find_ecam(rsdp: Rsdp) -> Result<Ecam, &'static str> {
    let mcfg = find_table(rsdp, b"MCFG")?;
    // SAFETY: `mcfg` came from `find_table`, and the length check keeps the reads inside it.
    unsafe {
        // Allocation entries start after 8 reserved bytes, 16 bytes each.
        let first = SDT_HEADER_LEN + 8;
        if (u32_at(mcfg, 4) as usize) < first + 16 {
            return Err("MCFG has no entries");
        }
        Ok(Ecam {
            base: u64_at(mcfg, first),
            segment: read_unaligned((mcfg + first + 8) as *const u16),
            bus_start: u8_at(mcfg, first + 10),
            bus_end: u8_at(mcfg, first + 11),
        })
    }
}

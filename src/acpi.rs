//! Minimal ACPI walk: RSDP -> XSDT -> MADT, to find the GIC.
//!
//! The firmware leaves ACPI tables identity mapped, so physical addresses are
//! dereferenced directly. Tables are read with unaligned loads.

use core::ptr::read_unaligned;

pub struct GicInfo {
    /// GIC version from the distributor entry (0 = "probe the hardware").
    pub version: u8,
    pub gicd: Option<u64>,
    /// First redistributor discovery range: (base, length).
    pub gicr: Option<(u64, u32)>,
    pub cpus: usize,
    pub its: usize,
}

unsafe fn u8_at(base: usize, off: usize) -> u8 {
    unsafe { read_unaligned((base + off) as *const u8) }
}

unsafe fn u32_at(base: usize, off: usize) -> u32 {
    unsafe { read_unaligned((base + off) as *const u32) }
}

unsafe fn u64_at(base: usize, off: usize) -> u64 {
    unsafe { read_unaligned((base + off) as *const u64) }
}

unsafe fn sig_is(base: usize, sig: &[u8]) -> bool {
    unsafe { core::slice::from_raw_parts(base as *const u8, sig.len()) == sig }
}

/// Header shared by all system description tables.
const SDT_HEADER_LEN: usize = 36;

fn find_table(rsdp: usize, sig: &[u8; 4]) -> Result<usize, &'static str> {
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

pub fn find_gic(rsdp: usize) -> Result<GicInfo, &'static str> {
    let madt = find_table(rsdp, b"APIC")?;
    let mut info = GicInfo { version: 0, gicd: None, gicr: None, cpus: 0, its: 0 };
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

//! PCIe configuration space through ECAM, just enough to find a device.
//!
//! ECAM gives every function a 4 KiB window at
//! `base + (bus << 20 | device << 15 | function << 12)`. The firmware has
//! already numbered the buses behind bridges and assigned BARs (it used the
//! devices itself), so a flat scan over the MCFG's bus range finds functions
//! behind root ports too, and BARs are used as the firmware left them.
//!
//! Config space and BARs are mapped as device memory on first use.

use crate::dev::acpi::Ecam;
use crate::mm::mmu;
use crate::mmio::Region;

/// Configuration space per function.
const CONFIG_SIZE: usize = 0x1000;

const VENDOR_ID: usize = 0x00;
const COMMAND: usize = 0x04;
const CLASS_REVISION: usize = 0x08;
const HEADER_TYPE: usize = 0x0e;
const BAR0: usize = 0x10;

const COMMAND_MEMORY: u16 = 1 << 1;
const COMMAND_BUS_MASTER: u16 = 1 << 2;
const HEADER_MULTI_FUNCTION: u8 = 0x80;
const BAR_IO: u32 = 1 << 0;
const BAR_TYPE_64: u32 = 0b10 << 1;

/// One function's configuration space.
pub struct Function {
    cfg: Region,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl Function {
    fn read32(&self, off: usize) -> u32 {
        self.cfg.read32(off)
    }

    fn write32(&self, off: usize, v: u32) {
        self.cfg.write32(off, v)
    }

    fn read16(&self, off: usize) -> u16 {
        (self.read32(off & !3) >> ((off & 2) * 8)) as u16
    }

    fn write16(&self, off: usize, v: u16) {
        let shift = (off & 2) * 8;
        let old = self.read32(off & !3) & !(0xffff << shift);
        self.write32(off & !3, old | (v as u32) << shift);
    }

    /// (vendor, device) IDs.
    pub fn ids(&self) -> (u16, u16) {
        let v = self.read32(VENDOR_ID);
        (v as u16, (v >> 16) as u16)
    }

    /// Lets the device decode its memory BARs and master DMA.
    pub fn enable_memory_and_dma(&self) {
        self.write16(COMMAND, self.read16(COMMAND) | COMMAND_MEMORY | COMMAND_BUS_MASTER);
    }

    /// Address and size of memory BAR `n` (which may span two slots), mapped
    /// as device memory. `None` if it is an I/O BAR or was never assigned.
    pub fn map_bar(&self, n: usize) -> Option<(u64, u64)> {
        let off = BAR0 + n * 4;
        let lo = self.read32(off);
        if lo & BAR_IO != 0 {
            return None;
        }
        let is64 = lo & BAR_TYPE_64 != 0;
        let hi = if is64 { self.read32(off + 4) } else { 0 };
        let base = (hi as u64) << 32 | (lo & !0xf) as u64;
        if base == 0 {
            return None;
        }

        // Size: write all ones and see which address bits stick. Decoding is
        // switched off meanwhile so the probe value is never a live address.
        let cmd = self.read16(COMMAND);
        self.write16(COMMAND, cmd & !COMMAND_MEMORY);
        self.write32(off, !0);
        let mut mask = (self.read32(off) & !0xf) as u64;
        self.write32(off, lo);
        if is64 {
            self.write32(off + 4, !0);
            mask |= (self.read32(off + 4) as u64) << 32;
            self.write32(off + 4, hi);
        } else {
            mask |= 0xffff_ffff << 32;
        }
        self.write16(COMMAND, cmd);

        let size = !mask + 1;
        mmu::map_live(base, size, mmu::Kind::Device);
        Some((base, size))
    }
}

/// Finds the first function whose class code (class, subclass, prog-if)
/// matches.
///
/// # Safety
///
/// `ecam` must come from `acpi::find_ecam`.
pub unsafe fn find_class(ecam: &Ecam, class: [u8; 3]) -> Option<Function> {
    // The MCFG base is where bus 0 would be, even if the range starts later.
    let (first, last) = (ecam.bus_start as u64, ecam.bus_end as u64);
    mmu::map_live(ecam.base + (first << 20), (last - first + 1) << 20, mmu::Kind::Device);

    let want = (class[0] as u32) << 24 | (class[1] as u32) << 16 | (class[2] as u32) << 8;
    for bus in ecam.bus_start..=ecam.bus_end {
        for device in 0..32u8 {
            for function in 0..8u8 {
                let cfg = ecam.base as usize
                    + ((bus as usize) << 20 | (device as usize) << 15 | (function as usize) << 12);
                // SAFETY: this function's window in the ECAM range (caller), mapped above.
                let cfg = unsafe { Region::new(cfg, CONFIG_SIZE) };
                let f = Function { cfg, bus, device, function };
                if f.read32(VENDOR_ID) as u16 == 0xffff {
                    // No device at function 0 means none at all.
                    if function == 0 {
                        break;
                    }
                    continue;
                }
                if f.read32(CLASS_REVISION) & !0xff == want {
                    return Some(f);
                }
                let header = (f.read32(HEADER_TYPE & !3) >> 16) as u8;
                if function == 0 && header & HEADER_MULTI_FUNCTION == 0 {
                    break;
                }
            }
        }
    }
    None
}

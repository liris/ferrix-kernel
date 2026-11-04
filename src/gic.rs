//! GICv3 driver: distributor, this CPU's redistributor, and the system
//! register CPU interface. Only PPIs (per-CPU interrupts such as the timer)
//! are supported for now.
//!
//! Addresses come from the ACPI MADT. The GIC's MMIO is identity mapped as
//! device memory (by the firmware, then by `mmu`), so registers are accessed
//! directly.

use crate::acpi::GicInfo;
use crate::global::Global;
use crate::mmio::Region;
use core::arch::asm;

/// Size of a GICv3 distributor's register frame.
pub const GICD_SIZE: u64 = 0x1_0000;

// Distributor registers.
const GICD_CTLR: usize = 0x0000;
const GICD_PIDR2: usize = 0xffe8;
const GICD_CTLR_RWP: u32 = 1 << 31;
const GICD_CTLR_ARE: u32 = 1 << 4;
const GICD_CTLR_ENABLE_G1: u32 = 1 << 1;

// Redistributor: RD_base frame.
const GICR_CTLR: usize = 0x0000;
const GICR_TYPER: usize = 0x0008;
const GICR_WAKER: usize = 0x0014;
const GICR_CTLR_RWP: u32 = 1 << 3;
const GICR_TYPER_VLPIS: u64 = 1 << 1;
const GICR_TYPER_LAST: u64 = 1 << 4;
const GICR_WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
const GICR_WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;

// Redistributor: SGI_base frame (64 KiB after RD_base).
const SGI_FRAME: usize = 0x1_0000;
/// RD_base and SGI_base; GICv4 adds two frames for virtual LPIs.
const GICR_FRAMES: usize = 0x2_0000;
const GICR_FRAMES_VLPI: usize = 0x4_0000;
const GICR_IGROUPR0: usize = 0x0080;
const GICR_ISENABLER0: usize = 0x0100;
const GICR_IPRIORITYR: usize = 0x0400;

/// INTID returned by an acknowledge when nothing is pending.
pub const SPURIOUS: u32 = 1023;

/// This CPU's redistributor (both frames), once `init` has run.
static RD: Global<Region> = Global::new("GIC redistributor");

fn wait_clear(r: &Region, off: usize, bit: u32) {
    while r.read32(off) & bit != 0 {
        core::hint::spin_loop();
    }
}

/// MPIDR affinity packed the way GICR_TYPER reports it (Aff3.Aff2.Aff1.Aff0).
fn my_affinity() -> u64 {
    let mpidr: u64;
    // SAFETY: reading a system register has no side effects.
    unsafe { asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nomem, nostack)) };
    (mpidr & 0xff_ffff) | ((mpidr >> 32 & 0xff) << 24)
}

/// Finds the redistributor whose affinity matches this CPU.
fn find_redistributor(gicr: &Region, len: usize) -> Option<Region> {
    let me = my_affinity();
    let mut off = 0;
    while off < len {
        let typer = gicr.read64(off + GICR_TYPER);
        if typer >> 32 == me {
            return Some(gicr.sub(off, GICR_FRAMES));
        }
        if typer & GICR_TYPER_LAST != 0 {
            break;
        }
        off += if typer & GICR_TYPER_VLPIS != 0 { GICR_FRAMES_VLPI } else { GICR_FRAMES };
    }
    None
}

/// Brings up the distributor, this CPU's redistributor and CPU interface.
/// Returns the redistributor address on success.
///
/// # Safety
///
/// `info` must come from `acpi::find_gic`, and its GICD and GICR ranges must be mapped
/// as device memory.
pub unsafe fn init(info: &GicInfo) -> Result<u64, &'static str> {
    let gicd = info.gicd.ok_or("no GIC distributor in MADT")?;
    // SAFETY: the distributor's frame, from the MADT and mapped (caller).
    let gicd = unsafe { Region::new(gicd as usize, GICD_SIZE as usize) };
    let version = match info.version {
        0 => ((gicd.read32(GICD_PIDR2) >> 4) & 0xf) as u8,
        v => v,
    };
    if version < 3 {
        return Err("GICv2 is not supported (run QEMU with gic-version=3)");
    }
    let (gicr, len) = info.gicr.ok_or("no GICR range in MADT")?;
    // SAFETY: the redistributor range, from the MADT and mapped (caller).
    let gicr = unsafe { Region::new(gicr as usize, len as usize) };
    let rd = find_redistributor(&gicr, len as usize).ok_or("no redistributor for this CPU")?;

    // Distributor: affinity routing on, group 1 enabled.
    gicd.write32(GICD_CTLR, gicd.read32(GICD_CTLR) | GICD_CTLR_ARE | GICD_CTLR_ENABLE_G1);
    wait_clear(&gicd, GICD_CTLR, GICD_CTLR_RWP);

    // Redistributor: wake it up.
    rd.write32(GICR_WAKER, rd.read32(GICR_WAKER) & !GICR_WAKER_PROCESSOR_SLEEP);
    wait_clear(&rd, GICR_WAKER, GICR_WAKER_CHILDREN_ASLEEP);

    // CPU interface: system registers on, all priorities let through.
    // SAFETY: GIC CPU interface registers; enabling them only lets IRQs be signalled,
    // and IRQs stay masked at the CPU until `arch::enable_irqs`.
    unsafe {
        // ICC_SRE_EL1.SRE
        let mut sre: u64;
        asm!("mrs {}, S3_0_C12_C12_5", out(reg) sre, options(nomem, nostack));
        if sre & 1 == 0 {
            asm!("msr S3_0_C12_C12_5, {}", "isb", in(reg) sre | 1, options(nostack));
            asm!("mrs {}, S3_0_C12_C12_5", out(reg) sre, options(nomem, nostack));
            if sre & 1 == 0 {
                return Err("GIC system register interface unavailable");
            }
        }
        asm!(
            "msr S3_0_C4_C6_0, {pmr}",    // ICC_PMR_EL1: unmask all priorities
            "msr S3_0_C12_C12_3, xzr",   // ICC_BPR1_EL1: no preemption grouping
            "msr S3_0_C12_C12_7, {one}", // ICC_IGRPEN1_EL1: enable group 1
            "isb",
            pmr = in(reg) 0xffu64,
            one = in(reg) 1u64,
            options(nostack),
        );
    }

    RD.set(rd);
    Ok(rd.addr())
}

/// Enables a private peripheral interrupt (INTID 16..31) as group 1.
pub fn enable_ppi(intid: u32, priority: u8) {
    assert!((16..32).contains(&intid), "INTID {} is not a PPI", intid);
    let rd = RD.with(|r| *r);
    let sgi = rd.sub(SGI_FRAME, SGI_FRAME);
    let bit = 1 << intid;
    sgi.write32(GICR_IGROUPR0, sgi.read32(GICR_IGROUPR0) | bit);
    sgi.write8(GICR_IPRIORITYR + intid as usize, priority);
    sgi.write32(GICR_ISENABLER0, bit);
    wait_clear(&rd, GICR_CTLR, GICR_CTLR_RWP);
}

/// Acknowledges the highest priority pending group 1 interrupt.
pub fn acknowledge() -> u32 {
    let intid: u64;
    // ICC_IAR1_EL1
    // SAFETY: acknowledging only changes the GIC's view of which IRQ is active.
    unsafe { asm!("mrs {}, S3_0_C12_C12_0", out(reg) intid, options(nomem, nostack)) };
    intid as u32 & 0xff_ffff
}

pub fn end_of_interrupt(intid: u32) {
    // ICC_EOIR1_EL1
    // SAFETY: ends the IRQ that `acknowledge` returned.
    unsafe { asm!("msr S3_0_C12_C12_1, {}", in(reg) intid as u64, options(nomem, nostack)) };
}

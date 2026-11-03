//! GICv3 driver: distributor, this CPU's redistributor, and the system
//! register CPU interface. Only PPIs (per-CPU interrupts such as the timer)
//! are supported for now.
//!
//! Addresses come from the ACPI MADT. The GIC's MMIO is identity mapped as
//! device memory (by the firmware, then by `mmu`), so registers are accessed
//! directly.

use crate::acpi::GicInfo;
use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicUsize, Ordering};

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
const GICR_IGROUPR0: usize = 0x0080;
const GICR_ISENABLER0: usize = 0x0100;
const GICR_IPRIORITYR: usize = 0x0400;

/// INTID returned by an acknowledge when nothing is pending.
pub const SPURIOUS: u32 = 1023;

static SGI: AtomicUsize = AtomicUsize::new(0);

fn read32(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

fn write32(addr: usize, v: u32) {
    unsafe { write_volatile(addr as *mut u32, v) }
}

fn read64(addr: usize) -> u64 {
    unsafe { read_volatile(addr as *const u64) }
}

fn wait_clear(addr: usize, bit: u32) {
    while read32(addr) & bit != 0 {
        core::hint::spin_loop();
    }
}

/// MPIDR affinity packed the way GICR_TYPER reports it (Aff3.Aff2.Aff1.Aff0).
fn my_affinity() -> u64 {
    let mpidr: u64;
    unsafe { asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nomem, nostack)) };
    (mpidr & 0xff_ffff) | ((mpidr >> 32 & 0xff) << 24)
}

/// Finds the redistributor whose affinity matches this CPU.
fn find_redistributor(base: usize, len: usize) -> Option<usize> {
    let me = my_affinity();
    let mut rd = base;
    while rd < base + len {
        let typer = read64(rd + GICR_TYPER);
        if typer >> 32 == me {
            return Some(rd);
        }
        if typer & GICR_TYPER_LAST != 0 {
            break;
        }
        // GICv4 redistributors have two extra frames for virtual LPIs.
        rd += if typer & GICR_TYPER_VLPIS != 0 { 0x4_0000 } else { 0x2_0000 };
    }
    None
}

/// GIC architecture version from the distributor's ID register.
fn probe_version(gicd: usize) -> u8 {
    ((read32(gicd + GICD_PIDR2) >> 4) & 0xf) as u8
}

/// Brings up the distributor, this CPU's redistributor and CPU interface.
/// Returns the redistributor address on success.
pub fn init(info: &GicInfo) -> Result<usize, &'static str> {
    let gicd = info.gicd.ok_or("no GIC distributor in MADT")? as usize;
    let version = match info.version {
        0 => probe_version(gicd),
        v => v,
    };
    if version < 3 {
        return Err("GICv2 is not supported (run QEMU with gic-version=3)");
    }
    let (gicr, len) = info.gicr.ok_or("no GICR range in MADT")?;
    let rd = find_redistributor(gicr as usize, len as usize).ok_or("no redistributor for this CPU")?;

    // Distributor: affinity routing on, group 1 enabled.
    write32(gicd + GICD_CTLR, read32(gicd + GICD_CTLR) | GICD_CTLR_ARE | GICD_CTLR_ENABLE_G1);
    wait_clear(gicd + GICD_CTLR, GICD_CTLR_RWP);

    // Redistributor: wake it up.
    write32(rd + GICR_WAKER, read32(rd + GICR_WAKER) & !GICR_WAKER_PROCESSOR_SLEEP);
    wait_clear(rd + GICR_WAKER, GICR_WAKER_CHILDREN_ASLEEP);

    // CPU interface: system registers on, all priorities let through.
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

    SGI.store(rd + SGI_FRAME, Ordering::Relaxed);
    Ok(rd)
}

/// Enables a private peripheral interrupt (INTID 16..31) as group 1.
pub fn enable_ppi(intid: u32, priority: u8) {
    assert!((16..32).contains(&intid), "INTID {} is not a PPI", intid);
    let sgi = SGI.load(Ordering::Relaxed);
    assert!(sgi != 0, "GIC not initialized");
    let bit = 1 << intid;
    write32(sgi + GICR_IGROUPR0, read32(sgi + GICR_IGROUPR0) | bit);
    unsafe { write_volatile((sgi + GICR_IPRIORITYR + intid as usize) as *mut u8, priority) };
    write32(sgi + GICR_ISENABLER0, bit);
    wait_clear(sgi - SGI_FRAME + GICR_CTLR, GICR_CTLR_RWP);
}

/// Acknowledges the highest priority pending group 1 interrupt.
pub fn acknowledge() -> u32 {
    let intid: u64;
    // ICC_IAR1_EL1
    unsafe { asm!("mrs {}, S3_0_C12_C12_0", out(reg) intid, options(nomem, nostack)) };
    intid as u32 & 0xff_ffff
}

pub fn end_of_interrupt(intid: u32) {
    // ICC_EOIR1_EL1
    unsafe { asm!("msr S3_0_C12_C12_1, {}", in(reg) intid as u64, options(nomem, nostack)) };
}

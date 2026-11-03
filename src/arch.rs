//! AArch64 system register access.

use core::arch::asm;

macro_rules! read_sysreg {
    ($name:literal) => {{
        let v: u64;
        unsafe { asm!(concat!("mrs {}, ", $name), out(reg) v, options(nomem, nostack)) };
        v
    }};
}

/// Exception level the kernel is running at (1 or 2).
pub fn current_el() -> u64 {
    (read_sysreg!("CurrentEL") >> 2) & 0b11
}

pub fn midr() -> u64 {
    read_sysreg!("MIDR_EL1")
}

/// Generic timer frequency in Hz.
pub fn timer_freq() -> u64 {
    read_sysreg!("CNTFRQ_EL0")
}

/// Generic timer counter value.
pub fn timer_count() -> u64 {
    unsafe { asm!("isb", options(nomem, nostack)) };
    read_sysreg!("CNTPCT_EL0")
}

/// Arms the EL1 virtual timer to fire after `ticks` counter ticks.
/// Its interrupt is PPI 27 on GICv3 systems.
pub fn virtual_timer_arm(ticks: u64) {
    unsafe {
        asm!(
            "msr cntv_tval_el0, {t}",
            "msr cntv_ctl_el0, {on}", // ENABLE=1, IMASK=0
            "isb",
            t = in(reg) ticks,
            on = in(reg) 1u64,
            options(nomem, nostack),
        )
    };
}

/// Unmasks IRQs at the CPU (PSTATE.I = 0).
pub fn enable_irqs() {
    unsafe { asm!("msr daifclr, #2", options(nomem, nostack)) };
}

/// Runs `f` with IRQs masked, then restores the previous mask. On one CPU
/// this is the lock around state shared with the IRQ handler and other tasks.
pub fn without_irqs<R>(f: impl FnOnce() -> R) -> R {
    let daif = read_sysreg!("DAIF");
    // Not `nomem`: memory accesses in `f` must stay inside the masked region.
    unsafe { asm!("msr daifset, #2", options(nostack)) };
    let r = f();
    unsafe { asm!("msr daif, {}", in(reg) daif, options(nostack)) };
    r
}

/// Masks IRQs and FIQs for good (panic).
pub fn disable_irqs() {
    unsafe { asm!("msr daifset, #3", options(nostack)) };
}

/// Sleeps until an interrupt (or other event) arrives.
pub fn wait_for_interrupt() {
    unsafe { asm!("wfi", options(nomem, nostack)) };
}

pub fn implementer_name(midr: u64) -> &'static str {
    match (midr >> 24) & 0xff {
        0x41 => "Arm",
        0x61 => "Apple",
        0x51 => "Qualcomm",
        0x00 => "(software)",
        _ => "unknown",
    }
}

pub fn halt() -> ! {
    loop {
        unsafe { asm!("wfe", options(nomem, nostack)) };
    }
}

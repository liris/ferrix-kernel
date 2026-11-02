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

//! EL1 exception vectors.
//!
//! Every vector entry saves x0/x1, records which of the 16 vectors fired and
//! jumps to a common path that saves the rest of the general registers plus
//! ELR/SPSR/ESR/FAR into a `TrapFrame` on the current stack, then calls
//! `handle_exception`. On return the (possibly updated) ELR/SPSR are restored
//! and execution resumes with `eret`.
//!
//! FP/SIMD registers are not saved. That is fine for synchronous exceptions
//! raised from `asm!` blocks declared with `clobber_abi("C")`, but must be
//! revisited before asynchronous interrupts are enabled (M3).

use core::arch::{asm, global_asm};

#[repr(C)]
pub struct TrapFrame {
    pub x: [u64; 31],
    pub elr: u64,
    pub spsr: u64,
    pub esr: u64,
    pub far: u64,
    _pad: u64,
}

const _: () = assert!(core::mem::size_of::<TrapFrame>() == 288);

global_asm!(
    r#"
.macro FERRIX_VECTOR kind
    .balign 0x80
    sub sp, sp, #288
    stp x0, x1, [sp, #0]
    mov x1, #\kind
    b ferrix_exception_common
.endm

    .text
    .balign 2048
    .global ferrix_vectors
ferrix_vectors:
    FERRIX_VECTOR 0
    FERRIX_VECTOR 1
    FERRIX_VECTOR 2
    FERRIX_VECTOR 3
    FERRIX_VECTOR 4
    FERRIX_VECTOR 5
    FERRIX_VECTOR 6
    FERRIX_VECTOR 7
    FERRIX_VECTOR 8
    FERRIX_VECTOR 9
    FERRIX_VECTOR 10
    FERRIX_VECTOR 11
    FERRIX_VECTOR 12
    FERRIX_VECTOR 13
    FERRIX_VECTOR 14
    FERRIX_VECTOR 15

ferrix_exception_common:
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x19, [sp, #144]
    stp x20, x21, [sp, #160]
    stp x22, x23, [sp, #176]
    stp x24, x25, [sp, #192]
    stp x26, x27, [sp, #208]
    stp x28, x29, [sp, #224]
    str x30, [sp, #240]
    mrs x2, elr_el1
    mrs x3, spsr_el1
    stp x2, x3, [sp, #248]
    mrs x2, esr_el1
    mrs x3, far_el1
    stp x2, x3, [sp, #264]

    mov x0, sp
    bl {handler}

    ldp x2, x3, [sp, #248]
    msr elr_el1, x2
    msr spsr_el1, x3
    ldp x0, x1, [sp, #0]
    ldp x2, x3, [sp, #16]
    ldp x4, x5, [sp, #32]
    ldp x6, x7, [sp, #48]
    ldp x8, x9, [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x14, x15, [sp, #112]
    ldp x16, x17, [sp, #128]
    ldp x18, x19, [sp, #144]
    ldp x20, x21, [sp, #160]
    ldp x22, x23, [sp, #176]
    ldp x24, x25, [sp, #192]
    ldp x26, x27, [sp, #208]
    ldp x28, x29, [sp, #224]
    ldr x30, [sp, #240]
    add sp, sp, #288
    eret
"#,
    handler = sym handle_exception,
);

unsafe extern "C" {
    static ferrix_vectors: [u8; 0];
}

/// Masks IRQ/FIQ and points VBAR_EL1 at our vector table. Returns its address.
pub fn init() -> u64 {
    let vbar = &raw const ferrix_vectors as u64;
    assert!(vbar & 0x7ff == 0, "vector table must be 2 KiB aligned");
    unsafe {
        asm!("msr daifset, #3", options(nomem, nostack));
        asm!("msr vbar_el1, {}", "isb", in(reg) vbar, options(nostack));
    }
    vbar
}

const KIND_NAMES: [&str; 4] = ["sync", "irq", "fiq", "serror"];
const ORIGIN_NAMES: [&str; 4] = ["EL1/SP0", "EL1/SPx", "EL0/A64", "EL0/A32"];

const EC_SVC64: u64 = 0x15;
const EC_BRK64: u64 = 0x3c;

fn ec_name(ec: u64) -> &'static str {
    match ec {
        0x00 => "unknown reason",
        0x01 => "WFI/WFE trapped",
        0x07 => "FP/SIMD access trapped",
        0x0e => "illegal execution state",
        0x15 => "SVC (AArch64)",
        0x18 => "MSR/MRS trapped",
        0x20 => "instruction abort (lower EL)",
        0x21 => "instruction abort (same EL)",
        0x22 => "PC alignment fault",
        0x24 => "data abort (lower EL)",
        0x25 => "data abort (same EL)",
        0x26 => "SP alignment fault",
        0x2c => "FP exception",
        0x2f => "SError",
        0x30 | 0x31 => "breakpoint",
        0x32 | 0x33 => "software step",
        0x34 | 0x35 => "watchpoint",
        0x3c => "BRK (AArch64)",
        _ => "other",
    }
}

extern "C" fn handle_exception(frame: &mut TrapFrame, kind: u64) {
    let ec = (frame.esr >> 26) & 0x3f;
    let iss = frame.esr & 0x1ff_ffff;
    let is_sync = kind % 4 == 0;

    if is_sync && ec == EC_BRK64 {
        crate::println!("[exc ] brk #{:#x} at {:#x}, resuming", iss & 0xffff, frame.elr);
        // ELR points at the BRK itself; skip it.
        frame.elr += 4;
        return;
    }
    if is_sync && ec == EC_SVC64 {
        crate::println!("[exc ] svc #{:#x} (x0={:#x}), returning", iss & 0xffff, frame.x[0]);
        // ELR already points past the SVC. Report a result in x0.
        frame.x[0] = frame.x[0].wrapping_add(1);
        return;
    }

    panic!(
        "unhandled {} exception from {}\n  {} (EC={:#04x})\n  ESR={:#010x} FAR={:#x}\n  ELR={:#x} SPSR={:#x}",
        KIND_NAMES[(kind % 4) as usize],
        ORIGIN_NAMES[(kind / 4) as usize],
        ec_name(ec),
        ec,
        frame.esr,
        frame.far,
        frame.elr,
        frame.spsr
    );
}

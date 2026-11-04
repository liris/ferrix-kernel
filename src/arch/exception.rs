//! EL1 exception vectors.
//!
//! Every vector entry saves x0/x1, records which of the 16 vectors fired and
//! jumps to a common path that saves the rest of the general registers plus
//! ELR/SPSR/ESR/FAR into a `TrapFrame` on the current stack, then calls
//! `handle_exception`. The handler returns the frame to resume: normally the
//! same one, or another task's saved frame (on its own stack) to switch tasks.
//! The exit path moves sp there, restores everything and `eret`s.
//!
//! FP/SIMD registers (q0-q31, FPCR, FPSR) are saved too: IRQs can arrive in
//! the middle of any code, and compiled Rust uses SIMD registers freely.
//!
//! Exceptions from EL0 (M7) arrive on the user task's kernel stack (SP_EL1)
//! and save SP_EL0 in the frame as well. An `svc` from EL0 is a system call;
//! any other synchronous exception from EL0 is the program's fault and ends
//! that task only.

use core::arch::{asm, global_asm};

#[repr(C)]
pub struct TrapFrame {
    pub x: [u64; 31],
    pub elr: u64,
    pub spsr: u64,
    pub esr: u64,
    pub far: u64,
    /// The EL0 stack pointer of a user task (unused by kernel tasks).
    pub sp_el0: u64,
    pub q: [u128; 32],
    pub fpcr: u64,
    pub fpsr: u64,
}

const _: () = assert!(core::mem::size_of::<TrapFrame>() == 816);

global_asm!(
    r#"
.macro FERRIX_VECTOR kind
    .balign 0x80
    sub sp, sp, #816
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
    mrs x2, sp_el0
    str x2, [sp, #280]
    stp q0, q1, [sp, #288]
    stp q2, q3, [sp, #320]
    stp q4, q5, [sp, #352]
    stp q6, q7, [sp, #384]
    stp q8, q9, [sp, #416]
    stp q10, q11, [sp, #448]
    stp q12, q13, [sp, #480]
    stp q14, q15, [sp, #512]
    stp q16, q17, [sp, #544]
    stp q18, q19, [sp, #576]
    stp q20, q21, [sp, #608]
    stp q22, q23, [sp, #640]
    stp q24, q25, [sp, #672]
    stp q26, q27, [sp, #704]
    stp q28, q29, [sp, #736]
    stp q30, q31, [sp, #768]
    mrs x2, fpcr
    mrs x3, fpsr
    add x4, sp, #800
    stp x2, x3, [x4]

    mov x0, sp
    bl {handler}
    mov sp, x0

    ldp x2, x3, [sp, #248]
    msr elr_el1, x2
    msr spsr_el1, x3
    ldr x2, [sp, #280]
    msr sp_el0, x2
    add x4, sp, #800
    ldp x2, x3, [x4]
    msr fpcr, x2
    msr fpsr, x3
    ldp q0, q1, [sp, #288]
    ldp q2, q3, [sp, #320]
    ldp q4, q5, [sp, #352]
    ldp q6, q7, [sp, #384]
    ldp q8, q9, [sp, #416]
    ldp q10, q11, [sp, #448]
    ldp q12, q13, [sp, #480]
    ldp q14, q15, [sp, #512]
    ldp q16, q17, [sp, #544]
    ldp q18, q19, [sp, #576]
    ldp q20, q21, [sp, #608]
    ldp q22, q23, [sp, #640]
    ldp q24, q25, [sp, #672]
    ldp q26, q27, [sp, #704]
    ldp q28, q29, [sp, #736]
    ldp q30, q31, [sp, #768]
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
    add sp, sp, #816
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
    // SAFETY: the vector table is ours, aligned (checked), and in mapped code; with IRQs
    // and FIQs masked first, only synchronous exceptions can reach it.
    unsafe {
        asm!("msr daifset, #3", options(nomem, nostack));
        asm!("msr vbar_el1, {}", "isb", in(reg) vbar, options(nostack));
    }
    vbar
}

const KIND_NAMES: [&str; 4] = ["sync", "irq", "fiq", "serror"];
const ORIGIN_NAMES: [&str; 4] = ["EL1/SP0", "EL1/SPx", "EL0/A64", "EL0/A32"];

/// Vector numbers of exceptions from AArch64 EL0.
const KIND_EL0_SYNC: u64 = 8;
const KIND_EL0_IRQ: u64 = 9;

const EC_SVC64: u64 = 0x15;
const EC_BRK64: u64 = 0x3c;

pub fn ec_name(ec: u64) -> &'static str {
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

extern "C" fn handle_exception(frame: &mut TrapFrame, kind: u64) -> *mut TrapFrame {
    let ec = (frame.esr >> 26) & 0x3f;
    let iss = frame.esr & 0x1ff_ffff;
    let is_sync = kind.is_multiple_of(4);

    // IRQ taken in the kernel (EL1, either stack pointer) or in a user task.
    if kind == 1 || kind == 5 || kind == KIND_EL0_IRQ {
        let ticked = crate::dev::irq::handle();
        return if ticked { crate::proc::task::switch(frame, true) } else { frame };
    }

    if kind == KIND_EL0_SYNC {
        return if ec == EC_SVC64 {
            crate::proc::user::syscall(frame)
        } else {
            crate::proc::user::fault(frame, ec)
        };
    }

    if is_sync && ec == EC_BRK64 {
        crate::println!("[exc ] brk #{:#x} at {:#x}, resuming", iss & 0xffff, frame.elr);
        // ELR points at the BRK itself; skip it.
        frame.elr += 4;
        return frame;
    }
    if is_sync && ec == EC_SVC64 && iss & 0xffff == crate::proc::task::SVC_YIELD {
        return crate::proc::task::switch(frame, false);
    }
    if is_sync && ec == EC_SVC64 {
        crate::println!("[exc ] svc #{:#x} (x0={:#x}), returning", iss & 0xffff, frame.x[0]);
        // ELR already points past the SVC. Report a result in x0.
        frame.x[0] = frame.x[0].wrapping_add(1);
        return frame;
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

//! User programs (M7): loading, system calls and faults.
//!
//! The user program is an ELF built from `user/` (see `build.rs`) and
//! embedded in the kernel. Each instance gets an address space of its own,
//! with the ELF's segments copied into fresh pages and a stack below
//! `USER_END`, and runs as a task at EL0. It reaches the kernel only
//! through `svc #0`, with the call number in x8:
//!
//! | x8 | Call | Arguments | Result in x0 |
//! | --- | --- | --- | --- |
//! | 0 | exit | x0 = code | (does not return) |
//! | 1 | write | x0 = pointer, x1 = length: replaces the program's line on screen | 0, or `u64::MAX` for a bad pointer |
//! | 2 | sleep | x0 = timer ticks | 0 |
//!
//! Pointers from user space are checked with the MMU (`at s1e0r`) before
//! the kernel reads through them, and the reads themselves are unprivileged
//! (`ldtrb`), so PAN can stay on. Any other exception from EL0 kills just
//! that task, and its line on screen says why.

use crate::exception::{self, TrapFrame};
use crate::mmu::{self, AddressSpace, Kind};
use crate::{arch, mm, task};
use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;
use core::cell::UnsafeCell;

static ELF: &[u8] = include_bytes!(env!("USER_ELF"));

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_SLEEP: u64 = 2;

// Roles the user program understands (x0 at entry).
const ROLE_COUNT: u64 = 0;
const ROLE_SPIN: u64 = 1;
const ROLE_BAD: u64 = 2;

/// 16 KiB of user stack, ending at the top of the user range.
const STACK_PAGES: usize = 4;
const STACK_TOP: u64 = mmu::USER_END;
/// Longest line a program may write; with its name it fits 48 columns.
const LINE_MAX: usize = 44;

const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const EM_AARCH64: u16 = 0xb7;

/// What the `bad` program tries to read: kernel memory, mapped but EL1-only.
static KERNEL_SECRET: u64 = 0x5ec2e7;

struct Program {
    task: usize,
    name: &'static str,
    line: String,
}

struct Programs(UnsafeCell<Vec<Program>>);
unsafe impl Sync for Programs {}

static PROGRAMS: Programs = Programs(UnsafeCell::new(Vec::new()));

fn with<R>(f: impl FnOnce(&mut Vec<Program>) -> R) -> R {
    arch::without_irqs(|| f(unsafe { &mut *PROGRAMS.0.get() }))
}

fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

fn alloc_zeroed(pages: usize) -> u64 {
    let pa = mm::pages(|p| p.alloc(pages)).expect("out of memory for a user program");
    unsafe { core::ptr::write_bytes(pa as *mut u8, 0, pages * mm::PAGE_SIZE) };
    pa as u64
}

/// Builds an address space holding the ELF's loadable segments and a
/// stack. Returns it with the entry point.
fn load(elf: &[u8]) -> Result<(AddressSpace, u64), &'static str> {
    if elf.len() < 64 || &elf[..4] != b"\x7fELF" || elf[4] != 2 || elf[5] != 1 {
        return Err("not a 64-bit little-endian ELF");
    }
    if u16_at(elf, 18) != EM_AARCH64 {
        return Err("not an AArch64 ELF");
    }
    let (entry, phoff) = (u64_at(elf, 24), u64_at(elf, 32) as usize);
    let (phentsize, phnum) = (u16_at(elf, 54) as usize, u16_at(elf, 56) as usize);

    let mut space = AddressSpace::new();
    let page = mm::PAGE_SIZE as u64;
    for i in 0..phnum {
        let ph = elf.get(phoff + i * phentsize..phoff + (i + 1) * phentsize).ok_or("truncated ELF")?;
        let (ty, flags) = (u32_at(ph, 0), u32_at(ph, 4));
        let (offset, vaddr) = (u64_at(ph, 8) as usize, u64_at(ph, 16));
        let (filesz, memsz) = (u64_at(ph, 32) as usize, u64_at(ph, 40));
        if ty != PT_LOAD || memsz == 0 {
            continue;
        }
        let data = elf.get(offset..offset + filesz).ok_or("truncated ELF")?;
        let start = vaddr & !(page - 1);
        let end = (vaddr + memsz).next_multiple_of(page);
        let pa = alloc_zeroed(((end - start) / page) as usize);
        let dst = pa + (vaddr - start);
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), dst as *mut u8, filesz) };
        let kind = if flags & PF_X != 0 {
            // Written through the data cache, about to be fetched as code.
            arch::sync_icache(dst as usize, filesz);
            Kind::UserCode
        } else if flags & PF_W != 0 {
            Kind::UserData
        } else {
            Kind::UserRo
        };
        space.map(start, pa, end - start, kind);
    }

    let stack_bytes = (STACK_PAGES * mm::PAGE_SIZE) as u64;
    space.map(STACK_TOP - stack_bytes, alloc_zeroed(STACK_PAGES), stack_bytes, Kind::UserData);
    Ok((space, entry))
}

/// Starts the three demo programs. Returns how many are running.
pub fn start() -> Result<usize, &'static str> {
    arch::allow_fp_at_el0();
    let secret = &KERNEL_SECRET as *const u64 as u64;
    for (name, role, arg) in [("u1", ROLE_COUNT, 0), ("u2", ROLE_SPIN, 0), ("u3", ROLE_BAD, secret)] {
        let (space, entry) = load(ELF)?;
        // The address space lives as long as the task: it is never freed.
        let ttbr = space.ttbr();
        let id = task::spawn_user(name, ttbr, entry, STACK_TOP, [role, arg]);
        with(|p| p.push(Program { task: id, name, line: String::from("starting") }));
    }
    Ok(with(|p| p.len()))
}

pub fn elf_size() -> usize {
    ELF.len()
}

/// Each program's name and current line.
pub fn lines() -> Vec<(&'static str, String)> {
    with(|p| p.iter().map(|p| (p.name, p.line.clone())).collect())
}

fn set_line(line: String) {
    let me = task::current();
    with(|p| {
        if let Some(p) = p.iter_mut().find(|p| p.task == me) {
            p.line = line;
        }
    });
}

/// Copies at most `LINE_MAX` bytes of user memory, if EL0 itself could
/// read every one of them.
fn read_user(ptr: u64, len: u64) -> Option<String> {
    let len = len.min(LINE_MAX as u64);
    let end = ptr.checked_add(len)?;
    if ptr < mmu::USER_BASE || end > mmu::USER_END {
        return None;
    }
    let page = mm::PAGE_SIZE as u64;
    let mut at = ptr & !(page - 1);
    while at < end {
        if !mmu::user_readable(at) {
            return None;
        }
        at += page;
    }
    let bytes: Vec<u8> = (ptr..end).map(arch::read_user_byte).collect();
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// `svc #0` from EL0. Returns the frame to resume.
pub fn syscall(frame: *mut TrapFrame) -> *mut TrapFrame {
    let f = unsafe { &mut *frame };
    let (a, b) = (f.x[0], f.x[1]);
    match f.x[8] {
        SYS_EXIT => {
            set_line(format!("exited ({})", a));
            task::exit_from_trap(frame)
        }
        SYS_WRITE => {
            f.x[0] = match read_user(a, b) {
                Some(line) => {
                    set_line(line);
                    0
                }
                None => u64::MAX,
            };
            frame
        }
        SYS_SLEEP => {
            f.x[0] = 0;
            task::sleep_from_trap(frame, a)
        }
        _ => {
            f.x[0] = u64::MAX;
            frame
        }
    }
}

/// Fault status code (ISS bits 5:0) of an abort, in words.
fn fault_status(iss: u64) -> &'static str {
    match iss & 0x3f {
        0x04..=0x07 => "unmapped",
        0x09..=0x0b => "access flag",
        0x0d..=0x0f => "no permission",
        _ => "fault",
    }
}

const EC_INSTRUCTION_ABORT_EL0: u64 = 0x20;
const EC_DATA_ABORT_EL0: u64 = 0x24;
/// Data abort ISS: the access was a write.
const ISS_WNR: u64 = 1 << 6;

/// Any other synchronous exception from EL0: the program is killed.
pub fn fault(frame: *mut TrapFrame, ec: u64) -> *mut TrapFrame {
    let f = unsafe { &*frame };
    let iss = f.esr & 0x1ff_ffff;
    let what = match ec {
        EC_DATA_ABORT_EL0 => {
            let access = if iss & ISS_WNR != 0 { "write" } else { "read" };
            format!("{} {:#x} {}", access, f.far, fault_status(iss))
        }
        EC_INSTRUCTION_ABORT_EL0 => format!("exec {:#x} {}", f.far, fault_status(iss)),
        _ => exception::ec_name(ec).to_owned(),
    };
    set_line(format!("killed: {}", what));
    task::exit_from_trap(frame)
}

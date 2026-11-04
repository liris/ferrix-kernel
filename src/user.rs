//! User programs (M7-M9): loading, system calls and faults.
//!
//! The user program is an ELF built from `user/` (see `build.rs`) and
//! embedded in the kernel. Each instance gets an address space of its own,
//! with the ELF's segments copied into fresh pages and a stack below
//! `USER_END`, and runs as a task at EL0 holding a table of capabilities
//! (`cap`). It reaches the kernel only through `svc #0`, with the call
//! number in x8. Calls that touch a kernel object name it by a capability
//! slot in x0; `exit` and `sleep` only affect the caller and need none.
//!
//! | x8 | Call | Arguments | Result |
//! | --- | --- | --- | --- |
//! | 0 | exit | x0 = code | (does not return) |
//! | 1 | write | x0 = `Screen` cap, x1 = pointer, x2 = length, x3 = row in its range: replaces that line | x0 = status |
//! | 2 | sleep | x0 = timer ticks | x0 = 0 |
//! | 3 | send | x0 = `Send` cap, x1 = value | x0 = status |
//! | 4 | recv | x0 = `Recv` cap | x0 = status, x1 = value |
//! | 5 | wait | x0 = `Recv` cap: like recv, but blocks while the channel is empty | x0 = status, x1 = value |
//!
//! Status: 0 done, 1 no such capability (or the wrong kind, or a row outside
//! the range), 2 bad pointer, 3 channel empty, 4 channel full.
//!
//! A `wait` on an empty channel blocks the task with ELR moved back onto
//! the `svc`, so once a `send` wakes it the call simply runs again. Typed
//! keys go into a channel of their own (`send_key`), whose receiving end
//! only the circuit builder holds.
//!
//! Pointers from user space are checked with the MMU (`at s1e0r`) before
//! the kernel reads through them, and the reads themselves are unprivileged
//! (`ldtrb`), so PAN can stay on. Any other exception from EL0 kills just
//! that task, and its line on screen says why.

use crate::cap::{Cap, CapTable, Channels};
use crate::exception::{self, TrapFrame};
use crate::global::Global;
use crate::mmu::{self, AddressSpace, Kind};
use crate::{arch, mm, task};
use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

static ELF: &[u8] = include_bytes!(env!("USER_ELF"));

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_SLEEP: u64 = 2;
const SYS_SEND: u64 = 3;
const SYS_RECV: u64 = 4;
const SYS_WAIT: u64 = 5;

const OK: u64 = 0;
const ERR_CAP: u64 = 1;
const ERR_POINTER: u64 = 2;
const ERR_EMPTY: u64 = 3;
const ERR_FULL: u64 = 4;

// Roles the user program understands (x0 at entry).
const ROLE_COUNT: u64 = 0;
const ROLE_SPIN: u64 = 1;
const ROLE_SINK: u64 = 2;
const ROLE_FORGE: u64 = 3;
const ROLE_BAD: u64 = 4;
const ROLE_BUILDER: u64 = 5;

const CHANNEL_DEPTH: usize = 8;
/// Keys typed faster than the builder reads them wait here.
const KEY_DEPTH: usize = 32;
/// The builder's screen: 8 lines of output and 1 input line.
const BUILDER_ROWS: usize = 9;

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
    caps: CapTable,
    /// Its first screen line, where the kernel notes how it ended.
    first: usize,
}

/// Everything user programs can reach through capabilities.
#[derive(Default)]
struct State {
    programs: Vec<Program>,
    lines: Vec<String>,
    /// Bumped on every change to `lines`, so they are redrawn only then.
    generation: u64,
    channels: Channels,
    keys: Option<usize>,
}

static STATE: Global<State> = Global::new("user state");

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with_default(f)
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
    // SAFETY: pages just allocated, identity mapped and owned by nobody else.
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
        let ph =
            elf.get(phoff + i * phentsize..phoff + (i + 1) * phentsize).ok_or("truncated ELF")?;
        let (ty, flags) = (u32_at(ph, 0), u32_at(ph, 4));
        let (offset, vaddr) = (u64_at(ph, 8) as usize, u64_at(ph, 16));
        let (filesz, memsz) = (u64_at(ph, 32) as usize, u64_at(ph, 40));
        if ty != PT_LOAD || memsz == 0 {
            continue;
        }
        if filesz as u64 > memsz {
            return Err("segment larger in the file than in memory");
        }
        let data = elf.get(offset..offset + filesz).ok_or("truncated ELF")?;
        let start = vaddr & !(page - 1);
        let end = (vaddr + memsz).next_multiple_of(page);
        let pa = alloc_zeroed(((end - start) / page) as usize);
        let dst = pa + (vaddr - start);
        // SAFETY: `dst..dst + filesz` lies inside the pages just allocated, since
        // `filesz <= memsz`; they can't overlap the ELF, which the allocator never owned.
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
        // SAFETY: pages just allocated for this segment alone.
        unsafe { space.map(start, pa, end - start, kind) };
    }

    let stack_bytes = (STACK_PAGES * mm::PAGE_SIZE) as u64;
    let stack = alloc_zeroed(STACK_PAGES);
    // SAFETY: pages just allocated for this stack alone.
    unsafe { space.map(STACK_TOP - stack_bytes, stack, stack_bytes, Kind::UserData) };
    Ok((space, entry))
}

/// Starts the demo programs, each with its own lines on screen and only the
/// capabilities listed for it. Returns (programs, capabilities handed out).
pub fn start() -> Result<(usize, usize), &'static str> {
    arch::allow_fp_at_el0();
    let secret = &KERNEL_SECRET as *const u64 as u64;
    let (chan, keys) = with(|s| {
        let keys = s.channels.create(KEY_DEPTH);
        s.keys = Some(keys);
        (s.channels.create(CHANNEL_DEPTH), keys)
    });
    // (name, role, argument, rows on screen, capabilities after its screen)
    let programs: [(&str, u64, u64, usize, &[Cap]); 6] = [
        ("u1", ROLE_COUNT, 0, 1, &[Cap::Send(chan)]),
        ("u2", ROLE_SPIN, 0, 1, &[]),
        ("u3", ROLE_SINK, 0, 1, &[Cap::Recv(chan)]),
        ("u4", ROLE_FORGE, 0, 1, &[]),
        ("u5", ROLE_BAD, secret, 1, &[]),
        ("u6", ROLE_BUILDER, 0, BUILDER_ROWS, &[Cap::Recv(keys)]),
    ];
    for (name, role, arg, rows, extra) in programs {
        let (space, entry) = load(ELF)?;
        // Registered before the task can run, so its first call finds it.
        with(|s| {
            let first = s.lines.len();
            s.lines.resize(first + rows, String::new());
            s.lines[first] = String::from("starting");
            let mut caps = Vec::from([Cap::Screen { first, rows }]);
            caps.extend_from_slice(extra);
            s.programs.push(Program { task: usize::MAX, name, caps: CapTable::new(&caps), first });
        });
        // The address space lives as long as the task: it is never freed.
        let id = task::spawn_user(name, space.ttbr(), entry, STACK_TOP, [role, arg]);
        with(|s| s.programs.last_mut().unwrap().task = id);
    }
    Ok(with(|s| (s.programs.len(), s.programs.iter().map(|p| p.caps.len()).sum())))
}

pub fn elf_size() -> usize {
    ELF.len()
}

/// All user screen lines, each with the name of the program it starts (on
/// a program's first line only).
pub fn lines() -> Vec<(Option<&'static str>, String)> {
    with(|s| {
        s.lines
            .iter()
            .enumerate()
            .map(|(i, l)| (s.programs.iter().find(|p| p.first == i).map(|p| p.name), l.clone()))
            .collect()
    })
}

/// Changes whenever any user line does.
pub fn generation() -> u64 {
    with(|s| s.generation)
}

/// Hands a typed key to whoever holds the keyboard channel. Returns false
/// if it was dropped (channel full, or no user programs).
pub fn send_key(key: char) -> bool {
    let Some(keys) = with(|s| s.keys) else {
        return false;
    };
    let sent = with(|s| s.channels.send(keys, key as u64));
    task::wake(keys as u64);
    sent
}

fn set_line(s: &mut State, line: usize, text: String) {
    s.lines[line] = text;
    s.generation += 1;
}

/// Index of the running program, and the capability in its slot `n`.
fn lookup(s: &State, n: u64) -> (Option<usize>, Option<Cap>) {
    let me = task::current();
    let i = s.programs.iter().position(|p| p.task == me);
    (i, i.and_then(|i| s.programs[i].caps.get(n)))
}

/// The kernel's own note on the running program's first line (exit, fault).
fn note(text: String) {
    with(|s| {
        if let (Some(i), _) = lookup(s, 0) {
            let first = s.programs[i].first;
            set_line(s, first, text);
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
pub fn syscall(f: &mut TrapFrame) -> *mut TrapFrame {
    let (a, b, c, d) = (f.x[0], f.x[1], f.x[2], f.x[3]);
    match f.x[8] {
        SYS_EXIT => {
            note(format!("exited ({})", a));
            return task::exit_from_trap(f);
        }
        SYS_SLEEP => {
            f.x[0] = OK;
            return task::sleep_from_trap(f, a);
        }
        SYS_WRITE => {
            f.x[0] = match with(|s| lookup(s, a).1) {
                Some(Cap::Screen { first, rows }) if (d as usize) < rows => match read_user(b, c) {
                    Some(text) => {
                        with(|s| set_line(s, first + d as usize, text));
                        OK
                    }
                    None => ERR_POINTER,
                },
                _ => ERR_CAP,
            };
        }
        SYS_SEND => {
            f.x[0] = match with(|s| lookup(s, a).1) {
                Some(Cap::Send(chan)) => {
                    let sent = with(|s| s.channels.send(chan, b));
                    task::wake(chan as u64);
                    if sent { OK } else { ERR_FULL }
                }
                _ => ERR_CAP,
            };
        }
        SYS_RECV | SYS_WAIT => {
            let got = with(|s| match lookup(s, a).1 {
                Some(Cap::Recv(chan)) => Ok((chan, s.channels.recv(chan))),
                _ => Err(()),
            });
            match got {
                Ok((_, Some(value))) => (f.x[0], f.x[1]) = (OK, value),
                Ok((chan, None)) if f.x[8] == SYS_WAIT => {
                    // Back onto the `svc`: it runs again once woken.
                    f.elr -= 4;
                    return task::block_from_trap(f, chan as u64);
                }
                Ok((_, None)) => (f.x[0], f.x[1]) = (ERR_EMPTY, 0),
                Err(()) => (f.x[0], f.x[1]) = (ERR_CAP, 0),
            }
        }
        _ => f.x[0] = ERR_CAP,
    }
    f
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
pub fn fault(f: &mut TrapFrame, ec: u64) -> *mut TrapFrame {
    let iss = f.esr & 0x1ff_ffff;
    let what = match ec {
        EC_DATA_ABORT_EL0 => {
            let access = if iss & ISS_WNR != 0 { "write" } else { "read" };
            format!("{} {:#x} {}", access, f.far, fault_status(iss))
        }
        EC_INSTRUCTION_ABORT_EL0 => format!("exec {:#x} {}", f.far, fault_status(iss)),
        _ => exception::ec_name(ec).to_owned(),
    };
    note(format!("killed: {}", what));
    task::exit_from_trap(f)
}

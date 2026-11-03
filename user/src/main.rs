//! The first ferrix user program, run at EL0 in its own address space.
//!
//! The kernel loads this one ELF several times, each copy in a separate
//! address space, and passes a role in x0 (and an argument in x1):
//!
//! - `count`: counts once per 100 ms, shows the counter and its address,
//!   and sends each value on the channel in capability slot 1.
//! - `spin`: counts as fast as it can and never yields, so it only stops
//!   because the timer preempts it, even at EL0. Its counter has the same
//!   address as `count`'s, in a different address space.
//! - `sink`: receives `count`'s values from the channel in slot 1.
//! - `forge`: holds only its own line, and keeps trying to write with other
//!   slot numbers. Every attempt must be refused.
//! - `bad`: tries to read kernel memory at the address in x1, which must
//!   kill it and nothing else.
//! - `builder`: the circuit builder (`builder`), fed by the keyboard
//!   channel in slot 1.
//!
//! Every program gets its own screen line as capability slot 0 and shows
//! everything through it.

#![no_std]
#![no_main]

extern crate alloc;

mod builder;
mod heap;

use core::arch::asm;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU64, Ordering};

// System call numbers (x8) and status codes, shared with the kernel's
// `user` module.
const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_SLEEP: u64 = 2;
const SYS_SEND: u64 = 3;
const SYS_RECV: u64 = 4;
const SYS_WAIT: u64 = 5;

const OK: u64 = 0;
const ERR_CAP: u64 = 1;

const ROLE_COUNT: u64 = 0;
const ROLE_SPIN: u64 = 1;
const ROLE_SINK: u64 = 2;
const ROLE_FORGE: u64 = 3;
const ROLE_BAD: u64 = 4;
const ROLE_BUILDER: u64 = 5;

/// Capability slots handed out by the kernel.
const SCREEN: u64 = 0;
const CHANNEL: u64 = 1;

/// Returns (x0, x1) after the call.
fn syscall(number: u64, a: u64, b: u64, c: u64) -> (u64, u64) {
    let (r0, r1);
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") a => r0,
            inlateout("x1") b => r1,
            in("x2") c,
            in("x8") number,
            options(nostack),
        )
    };
    (r0, r1)
}

fn exit(code: u64) -> ! {
    syscall(SYS_EXIT, code, 0, 0);
    unreachable!()
}

/// Sleeps for `ticks` timer ticks (10 ms each).
fn sleep(ticks: u64) {
    syscall(SYS_SLEEP, ticks, 0, 0);
}

fn send(cap: u64, value: u64) -> u64 {
    syscall(SYS_SEND, cap, value, 0).0
}

fn recv(cap: u64) -> Option<u64> {
    match syscall(SYS_RECV, cap, 0, 0) {
        (OK, value) => Some(value),
        _ => None,
    }
}

/// Blocks until the channel in slot `cap` has a message. `None` if `cap`
/// is not a receiving end.
fn wait(cap: u64) -> Option<u64> {
    match syscall(SYS_WAIT, cap, 0, 0) {
        (OK, value) => Some(value),
        _ => None,
    }
}

/// One screen line of text, formatted on the stack; longer text is cut.
struct Line {
    buf: [u8; Line::CAPACITY],
    len: usize,
}

impl Line {
    /// What the kernel shows of a line.
    const CAPACITY: usize = 44;

    fn new() -> Self {
        Self { buf: [0; Self::CAPACITY], len: 0 }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn pop(&mut self) {
        self.len = self.len.saturating_sub(1);
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Only whole characters are ever stored, so this cannot fail.
    fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_bytes()).unwrap_or("")
    }
}

impl Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut n = s.len().min(Self::CAPACITY - self.len);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// Replaces row `row` of the screen lines in slot `cap`; returns the status.
fn write_row(cap: u64, row: u64, text: &[u8]) -> u64 {
    let r0: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") cap => r0,
            inlateout("x1") text.as_ptr() as u64 => _,
            in("x2") text.len() as u64,
            in("x3") row,
            in("x8") SYS_WRITE,
            options(nostack),
        )
    };
    r0
}

/// Writes the first line through the screen capability in slot `cap`;
/// returns the status.
fn write(cap: u64, args: fmt::Arguments) -> u64 {
    let mut line = Line::new();
    let _ = line.write_fmt(args);
    write_row(cap, 0, line.as_bytes())
}

/// Replaces this program's own line on screen.
fn show(args: fmt::Arguments) {
    write(SCREEN, args);
}

/// The same virtual address in every copy, but each copy's own memory.
static COUNTER: AtomicU64 = AtomicU64::new(0);

fn count() -> ! {
    loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
        show(format_args!("count n={} &n={:#x}", n, &COUNTER as *const _ as usize));
        // If the sink falls behind, the value is simply dropped.
        send(CHANNEL, n);
        sleep(10);
    }
}

fn spin() -> ! {
    loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
        if n % (1 << 20) == 0 {
            show(format_args!("spin  n={}M &n={:#x}", n >> 20, &COUNTER as *const _ as usize));
        }
    }
}

/// Sleeps in `wait` until a value arrives, then takes whatever else is
/// queued without blocking.
fn sink() -> ! {
    let mut got = 0u64;
    loop {
        let Some(mut last) = wait(CHANNEL) else {
            exit(4);
        };
        got += 1;
        while let Some(n) = recv(CHANNEL) {
            got += 1;
            last = n;
        }
        show(format_args!("sink  {} msgs, last n={}", got, last));
    }
}

/// Tries to write with every slot it was not given.
fn forge() -> ! {
    let mut denied = 0u64;
    let mut tried = 0u64;
    loop {
        for slot in 1..=8 {
            tried += 1;
            if write(slot, format_args!("forged by u4")) == ERR_CAP {
                denied += 1;
            }
        }
        show(format_args!("forge {}/{} other slots denied", denied, tried));
        sleep(100);
    }
}

fn bad(kernel_addr: u64) -> ! {
    show(format_args!("bad   will read {:#x}", kernel_addr));
    sleep(300);
    let v = unsafe { (kernel_addr as *const u64).read_volatile() };
    // Not reached: the read above faults.
    show(format_args!("bad   read {:#x} from the kernel!", v));
    exit(1)
}

#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.start")]
pub extern "C" fn _start(role: u64, arg: u64) -> ! {
    match role {
        ROLE_COUNT => count(),
        ROLE_SPIN => spin(),
        ROLE_SINK => sink(),
        ROLE_FORGE => forge(),
        ROLE_BAD => bad(arg),
        ROLE_BUILDER => builder::run(),
        _ => exit(2),
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    exit(3)
}

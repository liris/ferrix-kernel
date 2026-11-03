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
//!
//! Every program gets its own screen line as capability slot 0 and shows
//! everything through it.

#![no_std]
#![no_main]

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

const OK: u64 = 0;
const ERR_CAP: u64 = 1;

const ROLE_COUNT: u64 = 0;
const ROLE_SPIN: u64 = 1;
const ROLE_SINK: u64 = 2;
const ROLE_FORGE: u64 = 3;
const ROLE_BAD: u64 = 4;

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

/// One line of text, formatted on the stack.
struct Line {
    buf: [u8; 48],
    len: usize,
}

impl Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// Writes a line through the screen capability in slot `cap`; returns the
/// status.
fn write(cap: u64, args: fmt::Arguments) -> u64 {
    let mut line = Line { buf: [0; 48], len: 0 };
    let _ = line.write_fmt(args);
    syscall(SYS_WRITE, cap, line.buf.as_ptr() as u64, line.len as u64).0
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

fn sink() -> ! {
    let mut got = 0u64;
    loop {
        while let Some(n) = recv(CHANNEL) {
            got += 1;
            show(format_args!("sink  {} msgs, last n={}", got, n));
        }
        sleep(5);
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
        _ => exit(2),
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    exit(3)
}

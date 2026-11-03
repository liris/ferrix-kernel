//! The first ferrix user program, run at EL0 in its own address space.
//!
//! The kernel loads this one ELF several times, each copy in a separate
//! address space, and passes a role in x0 (and an argument in x1):
//!
//! - `count`: counts once per 100 ms and reports the counter's address.
//!   Two copies show the same address with different values.
//! - `spin`: counts as fast as it can and never yields, so it only stops
//!   because the timer preempts it, even at EL0.
//! - `bad`: tries to read kernel memory at the address in x1, which must
//!   kill it and nothing else.
//!
//! Everything it shows on screen goes through `write`, one line at a time.

#![no_std]
#![no_main]

use core::arch::asm;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU64, Ordering};

// System call numbers (x8), shared with the kernel's `user` module.
const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_SLEEP: u64 = 2;

const ROLE_COUNT: u64 = 0;
const ROLE_SPIN: u64 = 1;
const ROLE_BAD: u64 = 2;

fn syscall(number: u64, a: u64, b: u64) -> u64 {
    let ret;
    unsafe { asm!("svc #0", inlateout("x0") a => ret, in("x1") b, in("x8") number, options(nostack)) };
    ret
}

fn exit(code: u64) -> ! {
    syscall(SYS_EXIT, code, 0);
    unreachable!()
}

/// Sleeps for `ticks` timer ticks (10 ms each).
fn sleep(ticks: u64) {
    syscall(SYS_SLEEP, ticks, 0);
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

/// Replaces this program's line on screen.
fn show(args: fmt::Arguments) {
    let mut line = Line { buf: [0; 48], len: 0 };
    let _ = line.write_fmt(args);
    syscall(SYS_WRITE, line.buf.as_ptr() as u64, line.len as u64);
}

/// The same virtual address in every copy, but each copy's own memory.
static COUNTER: AtomicU64 = AtomicU64::new(0);

fn count() -> ! {
    loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
        show(format_args!("count n={} &n={:#x}", n, &COUNTER as *const _ as usize));
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
        ROLE_BAD => bad(arg),
        _ => exit(2),
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    exit(3)
}

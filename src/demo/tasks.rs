//! Kernel worker tasks that never yield, so they only stop running because
//! the timer preempts them, and each checks that a switch lost nothing.

use crate::dev::irq;
use crate::ok;
use crate::println;
use crate::proc::task;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use nand::Adder4;

// Progress of the worker tasks, read by `screen` for display.
pub static SPIN_COUNT: AtomicU64 = AtomicU64::new(0);
pub static NAND_ROUNDS: AtomicU64 = AtomicU64::new(0);
pub static FPU_ROUNDS: AtomicU64 = AtomicU64::new(0);

/// Counts as fast as it can and never yields: it only stops running because
/// the timer preempts it.
fn spin_task() {
    loop {
        SPIN_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

/// Checks the NAND 4-bit adder on all 512 inputs, over and over. A register
/// lost across a switch would show up as a wrong sum.
fn nand_task() {
    let mut adder = Adder4::new();
    loop {
        adder.check_all();
        NAND_ROUNDS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Repeats a floating point sum and demands a bit-identical result each
/// time, so FP/SIMD registers must survive every switch.
fn fpu_task() {
    let sum = || (1..=1000).map(|k| 1.0 / (k * k) as f64).sum::<f64>();
    let first = core::hint::black_box(sum());
    loop {
        let again = core::hint::black_box(sum());
        if again.to_bits() != first.to_bits() {
            panic!("fpu task: {} != {}", again, first);
        }
        FPU_ROUNDS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Starts the worker tasks, then checks that `main` gets the CPU back (the
/// workers never yield) and that every worker has made progress.
pub fn start() {
    task::init();
    task::spawn("spin", spin_task);
    task::spawn("nand", nand_task);
    task::spawn("fpu", fpu_task);
    task::sleep(irq::TICK_HZ / 5);
    let (tasks, switches) = task::snapshot();
    let progress = [&SPIN_COUNT, &NAND_ROUNDS, &FPU_ROUNDS].map(|c| c.load(Ordering::Relaxed));
    if progress.contains(&0) {
        panic!("a worker task never ran: {:?}", progress);
    }
    let names: Vec<&str> = tasks.iter().map(|t| t.name).collect();
    println!("[task] {} after {} switches", names.join(" "), switches);
    ok(format_args!("M4: {} tasks, round-robin, {} ms slices", tasks.len(), 1000 / irq::TICK_HZ));
}

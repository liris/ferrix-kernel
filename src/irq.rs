//! Interrupt dispatch and the periodic timer tick.
//!
//! The IRQ handler re-arms the timer and bumps `TICKS`; the exception path
//! then lets the scheduler switch tasks. Drawing and NAND evaluation stay in
//! tasks, which share the console and heap by masking IRQs around them.

use crate::{arch, gic};
use core::sync::atomic::{AtomicU64, Ordering};

/// EL1 virtual timer PPI.
pub const VIRTUAL_TIMER_INTID: u32 = 27;
/// Also the scheduler's time slice: 10 ms.
pub const TICK_HZ: u64 = 100;

static TICKS: AtomicU64 = AtomicU64::new(0);
static RELOAD: AtomicU64 = AtomicU64::new(0);

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Starts the periodic tick. The GIC must already be initialized.
pub fn start_timer(timer_freq: u64) {
    let reload = timer_freq / TICK_HZ;
    RELOAD.store(reload, Ordering::Relaxed);
    gic::enable_ppi(VIRTUAL_TIMER_INTID, 0x80);
    arch::virtual_timer_arm(reload);
    arch::enable_irqs();
}

/// Called from the exception vector for IRQs taken at EL1. Returns whether
/// the timer ticked.
pub fn handle() -> bool {
    let intid = gic::acknowledge();
    match intid {
        gic::SPURIOUS => return false,
        VIRTUAL_TIMER_INTID => {
            arch::virtual_timer_arm(RELOAD.load(Ordering::Relaxed));
            TICKS.fetch_add(1, Ordering::Relaxed);
        }
        _ => panic!("unexpected interrupt INTID {}", intid),
    }
    gic::end_of_interrupt(intid);
    true
}

//! Interrupt dispatch and the periodic timer tick.
//!
//! The IRQ handler only re-arms the timer and bumps `TICKS`. Everything that
//! touches the console or the heap stays in the main loop, which sleeps with
//! `wfi` and reacts to the tick count, so no locks are needed yet.

use crate::{arch, gic};
use core::sync::atomic::{AtomicU64, Ordering};

/// EL1 virtual timer PPI.
pub const VIRTUAL_TIMER_INTID: u32 = 27;
pub const TICK_HZ: u64 = 10;

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

/// Called from the exception vector for IRQs taken at EL1.
pub fn handle() {
    let intid = gic::acknowledge();
    match intid {
        gic::SPURIOUS => return,
        VIRTUAL_TIMER_INTID => {
            arch::virtual_timer_arm(RELOAD.load(Ordering::Relaxed));
            TICKS.fetch_add(1, Ordering::Relaxed);
        }
        _ => panic!("unexpected interrupt INTID {}", intid),
    }
    gic::end_of_interrupt(intid);
}

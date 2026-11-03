//! Keyboard input: HID boot protocol reports turned into characters, and the
//! queue that hands them to whoever reads input.
//!
//! A boot report is 8 bytes: modifier bits, a reserved byte, then the usage
//! codes of up to six keys held down. A key counts as typed when it appears
//! in a report without having been in the previous one; a key held down is
//! reported again and again (idle repeat) but typed only once. The layout is
//! US. Pure logic apart from the queue's IRQ masking.

use alloc::collections::VecDeque;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

const SHIFT_BITS: u8 = 0x22; // left and right shift
/// Usage in every key slot when more keys are down than a report can hold.
const ROLLOVER: u8 = 0x01;
/// Characters beyond this are dropped until someone reads.
const QUEUE_MAX: usize = 64;

#[derive(Default)]
pub struct BootKeyboard {
    held: [u8; 6],
}

impl BootKeyboard {
    /// Queues the characters for keys newly pressed in `report`.
    pub fn report(&mut self, report: &[u8]) {
        let Some(keys) = report.get(2..8) else {
            return;
        };
        if keys[0] == ROLLOVER {
            return;
        }
        let shift = report[0] & SHIFT_BITS != 0;
        for &k in keys {
            if k != 0 && !self.held.contains(&k)
                && let Some(c) = to_char(k, shift)
            {
                push(c);
            }
        }
        self.held.copy_from_slice(keys);
    }
}

/// US layout. Enter is `'\n'`, Backspace `'\x08'`, Tab `'\t'`.
fn to_char(usage: u8, shift: bool) -> Option<char> {
    const DIGITS: &[u8; 10] = b"1234567890";
    const DIGITS_SHIFTED: &[u8; 10] = b"!@#$%^&*()";
    const PUNCT: &[u8; 12] = b"-=[]\\#;'`,./";
    const PUNCT_SHIFTED: &[u8; 12] = b"_+{}|~:\"~<>?";
    let c = match usage {
        0x04..=0x1d => {
            let c = b'a' + usage - 0x04;
            if shift { c.to_ascii_uppercase() } else { c }
        }
        0x1e..=0x27 => (if shift { DIGITS_SHIFTED } else { DIGITS })[(usage - 0x1e) as usize],
        0x28 => b'\n',
        0x2a => 0x08,
        0x2b => b'\t',
        0x2c => b' ',
        0x2d..=0x38 => (if shift { PUNCT_SHIFTED } else { PUNCT })[(usage - 0x2d) as usize],
        _ => return None,
    };
    Some(c as char)
}

struct Queue(UnsafeCell<VecDeque<char>>);
unsafe impl Sync for Queue {}

static QUEUE: Queue = Queue(UnsafeCell::new(VecDeque::new()));

fn push(c: char) {
    crate::arch::without_irqs(|| {
        let q = unsafe { &mut *QUEUE.0.get() };
        if q.len() < QUEUE_MAX {
            q.push_back(c);
        }
    });
}

/// The next typed character, if any.
pub fn read() -> Option<char> {
    crate::arch::without_irqs(|| unsafe { (*QUEUE.0.get()).pop_front() })
}

static ERRORS: AtomicU64 = AtomicU64::new(0);

/// Notes a report that could not be read.
pub fn count_error() {
    ERRORS.fetch_add(1, Ordering::Relaxed);
}

/// Reports lost to transfer errors so far.
pub fn errors() -> u64 {
    ERRORS.load(Ordering::Relaxed)
}

//! Capabilities (M8): what a user program may do, as a table of its own.
//!
//! Every user task has a `CapTable`. A slot holds a capability: a reference
//! to one kernel object together with what the holder may do with it. User
//! code names a capability only by its slot number, and the kernel looks the
//! number up in the caller's own table, so a number means nothing in another
//! task and cannot be forged: a program can use exactly the capabilities it
//! was given. Tables are filled when a program is started.
//!
//! Objects so far: a line on the screen (write), and the two ends of a
//! channel, a small queue of 64-bit messages (send, or receive). Pure logic,
//! no hardware access.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cap {
    /// May replace the text of screen line `n`.
    Screen(usize),
    /// May send on channel `n`.
    Send(usize),
    /// May receive from channel `n`.
    Recv(usize),
}

#[derive(Default)]
pub struct CapTable {
    slots: Vec<Option<Cap>>,
}

impl CapTable {
    pub fn new(caps: &[Cap]) -> Self {
        Self { slots: caps.iter().copied().map(Some).collect() }
    }

    /// The capability in slot `n`, if there is one.
    pub fn get(&self, n: u64) -> Option<Cap> {
        self.slots.get(usize::try_from(n).ok()?).copied().flatten()
    }

    pub fn len(&self) -> usize {
        self.slots.iter().flatten().count()
    }
}

/// Messages a channel holds before `send` reports it full.
const CHANNEL_DEPTH: usize = 8;

/// All channels, indexed by the number in `Cap::Send` / `Cap::Recv`.
#[derive(Default)]
pub struct Channels {
    queues: Vec<VecDeque<u64>>,
}

impl Channels {
    /// Creates a channel and returns its number.
    pub fn create(&mut self) -> usize {
        self.queues.push(VecDeque::with_capacity(CHANNEL_DEPTH));
        self.queues.len() - 1
    }

    /// Queues `value`; false if the channel is full.
    pub fn send(&mut self, n: usize, value: u64) -> bool {
        let q = &mut self.queues[n];
        if q.len() == CHANNEL_DEPTH {
            return false;
        }
        q.push_back(value);
        true
    }

    /// The oldest queued message, if any.
    pub fn recv(&mut self, n: usize) -> Option<u64> {
        self.queues[n].pop_front()
    }
}

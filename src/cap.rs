//! Capabilities (M8): what a user program may do, as a table of its own.
//!
//! Every user task has a `CapTable`. A slot holds a capability: a reference
//! to one kernel object together with what the holder may do with it. User
//! code names a capability only by its slot number, and the kernel looks the
//! number up in the caller's own table, so a number means nothing in another
//! task and cannot be forged: a program can use exactly the capabilities it
//! was given. Tables are filled when a program is started.
//!
//! Objects so far: a range of lines on the screen (write), and the two ends
//! of a channel, a small queue of 64-bit messages (send, or receive). The
//! kernel feeds the keyboard into a channel too, so typed keys reach only
//! the program holding its receiving end. Pure logic, no hardware access.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cap {
    /// May replace the text of the `rows` screen lines from `first` on.
    Screen { first: usize, rows: usize },
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

struct Queue {
    messages: VecDeque<u64>,
    depth: usize,
}

/// All channels, indexed by the number in `Cap::Send` / `Cap::Recv`.
#[derive(Default)]
pub struct Channels {
    queues: Vec<Queue>,
}

impl Channels {
    /// Creates a channel holding up to `depth` messages; returns its number.
    pub fn create(&mut self, depth: usize) -> usize {
        self.queues.push(Queue { messages: VecDeque::with_capacity(depth), depth });
        self.queues.len() - 1
    }

    /// Queues `value`; false if the channel is full.
    pub fn send(&mut self, n: usize, value: u64) -> bool {
        let q = &mut self.queues[n];
        if q.messages.len() == q.depth {
            return false;
        }
        q.messages.push_back(value);
        true
    }

    /// The oldest queued message, if any.
    pub fn recv(&mut self, n: usize) -> Option<u64> {
        self.queues[n].messages.pop_front()
    }
}

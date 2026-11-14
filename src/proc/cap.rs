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
//!
//! A message is a value or a capability (M10b). Sending a capability moves
//! it: it leaves the sender's table when it is queued, and lands in the
//! first free slot of the receiver's table when it is received, so a
//! program can pass on what it holds but never keep a copy.

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

/// What a channel carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Msg {
    Value(u64),
    Cap(Cap),
}

/// Most capabilities one program can hold.
pub const SLOTS: usize = 16;

#[derive(Default)]
pub struct CapTable {
    slots: Vec<Option<Cap>>,
}

impl CapTable {
    /// # Panics
    ///
    /// If `caps` has more than `SLOTS` entries.
    pub fn new(caps: &[Cap]) -> Self {
        assert!(caps.len() <= SLOTS, "more than {} capabilities", SLOTS);
        Self { slots: caps.iter().copied().map(Some).collect() }
    }

    /// The capability in slot `n`, if there is one.
    pub fn get(&self, n: u64) -> Option<Cap> {
        self.slots.get(usize::try_from(n).ok()?).copied().flatten()
    }

    /// Empties slot `n`, returning what it held.
    pub fn take(&mut self, n: u64) -> Option<Cap> {
        self.slots.get_mut(usize::try_from(n).ok()?)?.take()
    }

    /// Puts `cap` in the first free slot and returns its number; `None` if
    /// all `SLOTS` are taken.
    pub fn insert(&mut self, cap: Cap) -> Option<usize> {
        let n = match self.slots.iter().position(Option::is_none) {
            Some(n) => n,
            None if self.slots.len() < SLOTS => {
                self.slots.push(None);
                self.slots.len() - 1
            }
            None => return None,
        };
        self.slots[n] = Some(cap);
        Some(n)
    }

    pub fn len(&self) -> usize {
        self.slots.iter().flatten().count()
    }
}

struct Queue {
    messages: VecDeque<Msg>,
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

    /// Queues `msg`; false if the channel is full.
    pub fn send(&mut self, n: usize, msg: Msg) -> bool {
        let q = &mut self.queues[n];
        if q.messages.len() == q.depth {
            return false;
        }
        q.messages.push_back(msg);
        true
    }

    /// The oldest queued message, left in the queue.
    pub fn peek(&self, n: usize) -> Option<Msg> {
        self.queues[n].messages.front().copied()
    }

    /// Takes the oldest queued message, if any.
    pub fn recv(&mut self, n: usize) -> Option<Msg> {
        self.queues[n].messages.pop_front()
    }

    pub fn count(&self) -> usize {
        self.queues.len()
    }
}

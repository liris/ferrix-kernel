//! Kernel state shared between tasks and the IRQ handler.
//!
//! One CPU, so masking IRQs keeps every other task and the IRQ handler out
//! while `with` runs: that is the lock. A busy flag catches a call from inside
//! `f` back into the same `Global`, which would otherwise hand out a second
//! `&mut`. Panics abort, so a flag left set by a panicking `f` never matters
//! except to the panic handler, which uses `reclaim`.

use core::cell::{Cell, UnsafeCell};

pub struct Global<T> {
    name: &'static str,
    value: UnsafeCell<Option<T>>,
    busy: Cell<bool>,
}

// SAFETY: one CPU, and `value` is only reached through `slot`, with IRQs masked and `busy`
// clear, so there is never more than one reference to it. `T: Send` because the tasks that
// take turns using it are, in effect, threads.
unsafe impl<T: Send> Sync for Global<T> {}

impl<T> Global<T> {
    /// An empty `Global`; `name` appears in panic messages.
    pub const fn new(name: &'static str) -> Self {
        Self { name, value: UnsafeCell::new(None), busy: Cell::new(false) }
    }

    /// Stores `value`, replacing any previous one.
    pub fn set(&self, value: T) {
        self.slot(|s| *s = Some(value)).unwrap_or_else(|| panic!("{} set while in use", self.name));
    }

    /// Takes the value out, leaving the `Global` empty.
    pub fn take(&self) -> Option<T> {
        self.slot(Option::take).flatten()
    }

    /// Runs `f` with the value.
    ///
    /// # Panics
    ///
    /// If the value hasn't been set, or when called from inside another `with` on the
    /// same `Global`.
    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        match self.slot(|s| s.as_mut().map(f)) {
            Some(Some(r)) => r,
            Some(None) => panic!("{} used before it was set", self.name),
            None => panic!("{} re-entered", self.name),
        }
    }

    /// Like `with`, but `None` instead of panicking.
    pub fn try_with<R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        self.slot(|s| s.as_mut().map(f)).flatten()
    }

    /// Like `with`, but starts from `T::default()` if the value isn't set yet.
    pub fn with_default<R>(&self, f: impl FnOnce(&mut T) -> R) -> R
    where
        T: Default,
    {
        self.slot(|s| f(s.get_or_insert_with(T::default)))
            .unwrap_or_else(|| panic!("{} re-entered", self.name))
    }

    /// Clears the busy flag left by a `with` that panicked.
    ///
    /// # Safety
    ///
    /// The code that set the flag must never run again: IRQs are off for good and the
    /// panicking task never resumes.
    pub unsafe fn reclaim(&self) {
        self.busy.set(false);
    }

    /// Runs `f` with the slot, or returns `None` if it's already in use.
    fn slot<R>(&self, f: impl FnOnce(&mut Option<T>) -> R) -> Option<R> {
        crate::arch::without_irqs(|| {
            if self.busy.replace(true) {
                return None;
            }
            // SAFETY: IRQs are masked and `busy` was clear, so no other reference exists.
            let r = f(unsafe { &mut *self.value.get() });
            self.busy.set(false);
            Some(r)
        })
    }
}

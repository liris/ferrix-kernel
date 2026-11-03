//! Preemptive round-robin scheduling of kernel tasks (single CPU).
//!
//! Every task runs at EL1 on its own stack. Switches only happen inside an
//! exception: the vector has already pushed a full `TrapFrame` onto the
//! running task's stack, so switching means remembering that frame and
//! handing another task's frame to the exit path, which restores it and
//! `eret`s into that task. The timer IRQ switches on every tick (preemption);
//! `svc #0` switches on request (`yield_now`, `sleep`).
//!
//! The task table is only touched with IRQs masked, which is all the locking
//! one CPU needs.

use crate::exception::TrapFrame;
use crate::{arch, irq, mm};
use alloc::vec::Vec;
use core::arch::asm;
use core::cell::UnsafeCell;
use core::ptr;

/// 64 KiB per task.
const STACK_PAGES: usize = 16;
/// PSTATE of a new task: EL1 on SP_EL1 (EL1h), all exceptions unmasked.
const SPSR_EL1H: u64 = 0b0101;
/// `svc` immediate that asks for a switch.
pub const SVC_YIELD: u64 = 0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    Ready,
    /// Not runnable until the tick count reaches this value.
    Sleeping(u64),
    Dead,
}

struct Task {
    name: &'static str,
    state: State,
    /// Where its registers were saved when it last stopped running.
    frame: *mut TrapFrame,
    /// Timer ticks that landed while this task was running.
    ticks: u64,
}

struct Scheduler {
    tasks: Vec<Task>,
    current: usize,
    idle: usize,
    switches: u64,
}

struct Global(UnsafeCell<Option<Scheduler>>);
unsafe impl Sync for Global {}

static SCHED: Global = Global(UnsafeCell::new(None));

fn with<R>(f: impl FnOnce(&mut Scheduler) -> R) -> R {
    arch::without_irqs(|| f(unsafe { (*SCHED.0.get()).as_mut() }.expect("scheduler not initialized")))
}

/// Turns the running code into task 0, `main`, and adds the idle task.
pub fn init() {
    let main = Task { name: "main", state: State::Ready, frame: ptr::null_mut(), ticks: 0 };
    arch::without_irqs(|| unsafe {
        *SCHED.0.get() = Some(Scheduler { tasks: alloc::vec![main], current: 0, idle: 0, switches: 0 })
    });
    let idle = spawn("idle", idle_task);
    with(|s| s.idle = idle);
}

/// Creates a ready task that runs `entry` on a fresh stack. Returns its id.
pub fn spawn(name: &'static str, entry: fn()) -> usize {
    let stack = mm::pages(|pa| pa.alloc(STACK_PAGES)).expect("out of memory for a task stack");
    let top = stack + STACK_PAGES * mm::PAGE_SIZE;
    // A frame that "returns" into task_start(entry); 816 bytes keeps sp 16-aligned.
    let frame = (top - size_of::<TrapFrame>()) as *mut TrapFrame;
    unsafe {
        ptr::write_bytes(frame, 0, 1);
        (*frame).x[0] = entry as usize as u64;
        (*frame).elr = task_start as extern "C" fn(usize) -> ! as usize as u64;
        (*frame).spsr = SPSR_EL1H;
    }
    with(|s| {
        s.tasks.push(Task { name, state: State::Ready, frame, ticks: 0 });
        s.tasks.len() - 1
    })
}

/// First code of every spawned task; x0 holds the `fn()` from `spawn`.
extern "C" fn task_start(entry: usize) -> ! {
    // SAFETY: `spawn` stored a `fn()` here.
    let entry: fn() = unsafe { core::mem::transmute(entry) };
    entry();
    // Its stack is still in use right here, so it is never freed.
    with(|s| {
        let me = s.current;
        s.tasks[me].state = State::Dead;
    });
    yield_now();
    unreachable!("a dead task was resumed");
}

fn idle_task() {
    loop {
        arch::wait_for_interrupt();
    }
}

/// Gives up the CPU to the next ready task.
pub fn yield_now() {
    unsafe { asm!("svc #0", options(nostack)) };
}

/// Blocks the calling task for at least `ticks` timer ticks.
pub fn sleep(ticks: u64) {
    // Masked so a tick cannot switch away between marking and yielding.
    arch::without_irqs(|| {
        with(|s| {
            let me = s.current;
            s.tasks[me].state = State::Sleeping(irq::ticks() + ticks);
        });
        yield_now();
    });
}

/// Called from the exception path with IRQs masked: saves `frame` as the
/// running task's state and returns the frame of the task to run next.
/// `tick` charges the tick that caused the call to the running task.
pub fn switch(frame: *mut TrapFrame, tick: bool) -> *mut TrapFrame {
    let Some(s) = (unsafe { (*SCHED.0.get()).as_mut() }) else {
        return frame;
    };
    let now = irq::ticks();
    let me = s.current;
    s.tasks[me].frame = frame;
    if tick {
        s.tasks[me].ticks += 1;
    }
    for t in &mut s.tasks {
        if let State::Sleeping(until) = t.state
            && now >= until
        {
            t.state = State::Ready;
        }
    }

    // Round-robin starting after the current task; idle only if nobody else.
    let n = s.tasks.len();
    let next = (1..=n)
        .map(|i| (me + i) % n)
        .find(|&i| i != s.idle && s.tasks[i].state == State::Ready)
        .unwrap_or(s.idle);
    if next != me {
        s.switches += 1;
    }
    s.current = next;
    s.tasks[next].frame
}

pub struct TaskInfo {
    pub name: &'static str,
    pub ticks: u64,
}

/// Per-task tick counts and the total number of switches so far.
pub fn snapshot() -> (Vec<TaskInfo>, u64) {
    with(|s| {
        let tasks = s.tasks.iter().map(|t| TaskInfo { name: t.name, ticks: t.ticks }).collect();
        (tasks, s.switches)
    })
}

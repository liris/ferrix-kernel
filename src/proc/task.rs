//! Preemptive, weighted fair scheduling of tasks (single CPU).
//!
//! Every task runs at EL1 on its own stack. Switches only happen inside an
//! exception: the vector has already pushed a full `TrapFrame` onto the
//! running task's stack, so switching means remembering that frame and
//! handing another task's frame to the exit path, which restores it and
//! `eret`s into that task. The timer IRQ offers a switch on every tick
//! (preemption); `svc #0` switches on request (`yield_now`, `sleep`).
//!
//! Which task (M10d): each has a `Weight`, and a virtual runtime that grows
//! by the time it runs (measured with `CNTVCT` at every switch) divided by
//! its weight. The ready task with the least virtual runtime runs next, so
//! CPU time is shared in proportion to weight and nobody starves (stride
//! scheduling; Linux's CFS works the same way). A task that slept falls
//! behind and so runs first once it wakes, but its virtual runtime is raised
//! to at most `WAKE_CREDIT` behind the least of the others, so a long sleep
//! doesn't buy it the CPU for long. The idle task runs only when nobody else
//! is ready.
//!
//! User tasks (M7) are the same, except that their frame returns to EL0 and
//! each carries its own address space: a switch loads the next task's TTBR0
//! when it differs. Kernel tasks all use the kernel's tables.
//!
//! A task that ends is only marked `Dead`: it may still be running on its
//! own kernel stack at that moment. The `reaper` task later takes it out of
//! the table and drops it, which frees its stack and, for a user task, its
//! address space (`AddressSpace`'s `Drop`). Its slot, and so its id, is then
//! free for the next `spawn`.
//!
//! The task table is only touched with IRQs masked, which is all the locking
//! one CPU needs.

use crate::arch;
use crate::arch::exception::TrapFrame;
use crate::dev::irq;
use crate::global::Global;
use crate::mm::{
    self,
    mmu::{self, AddressSpace},
};
use alloc::vec::Vec;
use core::arch::asm;
use core::ptr;

/// 64 KiB per task.
const STACK_PAGES: usize = 16;
/// PSTATE of a new task: EL1 on SP_EL1 (EL1h), all exceptions unmasked.
const SPSR_EL1H: u64 = 0b0101;
/// PSTATE of a new user task: EL0, all exceptions unmasked.
const SPSR_EL0T: u64 = 0b0000;
/// `svc` immediate that asks for a switch.
pub const SVC_YIELD: u64 = 0;
/// `wake` tag of the reaper; channel numbers, the other tags, are small.
const REAP: u64 = u64::MAX;
/// Virtual runtime is runtime × `VR_SCALE` / weight, kept in integers.
const VR_SCALE: u64 = 8;

/// A task's share of the CPU: twice the weight, twice the time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Weight {
    Low = 1,
    Normal = 2,
    High = 4,
    Highest = 8,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    Ready,
    /// Not runnable until the tick count reaches this value.
    Sleeping(u64),
    /// Not runnable until `wake` is called with this tag.
    Blocked(u64),
    Dead,
}

/// A task's kernel stack, given back to the page allocator when dropped.
struct Stack {
    base: usize,
}

impl Stack {
    fn new() -> Self {
        let base = mm::pages(|pa| pa.alloc(STACK_PAGES)).expect("out of memory for a task stack");
        Self { base }
    }

    fn top(&self) -> usize {
        self.base + STACK_PAGES * mm::PAGE_SIZE
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        // SAFETY: allocated in `new`. A `Stack` belongs to one task and is only dropped
        // with it, by the reaper, once the task is dead and switched away from for good.
        mm::pages(|pa| unsafe { pa.free(self.base, STACK_PAGES) });
    }
}

struct Task {
    name: &'static str,
    state: State,
    /// Where its registers were saved when it last stopped running.
    frame: *mut TrapFrame,
    /// Time it has run, in `CNTVCT` counts.
    runtime: u64,
    /// Runtime scaled by `VR_SCALE` / weight; the least runs next.
    vruntime: u64,
    weight: Weight,
    /// When `wake` made it ready, until it runs.
    woken_at: Option<u64>,
    /// Its address space.
    ttbr: u64,
    /// `None` for `main`, which runs on the boot stack.
    #[expect(dead_code, reason = "only held, to be freed when the reaper drops the task")]
    stack: Option<Stack>,
    /// A user task's address space; `ttbr` selects it.
    #[expect(dead_code, reason = "only held, to be freed when the reaper drops the task")]
    space: Option<AddressSpace>,
}

// SAFETY: `frame` points into the task's own kernel stack, used only by the scheduler and
// the exception path while this task isn't running. `space` is reached only through the
// task, which the scheduler owns.
unsafe impl Send for Task {}

/// Time from `wake` to running, over all wakes so far, in `CNTVCT` counts.
#[derive(Default, Clone, Copy)]
struct WakeStats {
    count: u64,
    total: u64,
    max: u64,
}

struct Scheduler {
    /// Indexed by task id; `None` is a free slot.
    tasks: Vec<Option<Task>>,
    current: usize,
    idle: usize,
    switches: u64,
    reaped: u64,
    /// `CNTVCT` when the current task started running.
    started: u64,
    /// The least virtual runtime of the ready tasks; never goes back.
    min_vruntime: u64,
    /// How far behind `min_vruntime` a waking task may stay: one tick of
    /// running at `Weight::Normal`.
    wake_credit: u64,
    wakes: WakeStats,
}

impl Scheduler {
    /// The live task `id`. Panics on a free slot: ids handed out are live
    /// until the reaper takes them, and nobody keeps one past that.
    fn task(&mut self, id: usize) -> &mut Task {
        self.tasks[id].as_mut().expect("no task in this slot")
    }

    fn is_ready(&self, id: usize) -> bool {
        self.tasks[id].as_ref().is_some_and(|t| t.state == State::Ready)
    }

    /// The least virtual runtime a task becoming ready keeps.
    fn wake_floor(&self) -> u64 {
        self.min_vruntime.saturating_sub(self.wake_credit)
    }
}

fn new_task(name: &'static str, frame: *mut TrapFrame, vruntime: u64) -> Task {
    Task {
        name,
        state: State::Ready,
        frame,
        runtime: 0,
        vruntime,
        weight: Weight::Normal,
        woken_at: None,
        ttbr: mmu::kernel_ttbr(),
        stack: None,
        space: None,
    }
}

static SCHED: Global<Scheduler> = Global::new("scheduler");

fn with<R>(f: impl FnOnce(&mut Scheduler) -> R) -> R {
    SCHED.with(f)
}

/// Turns the running code into task 0, `main`, and adds the idle task and
/// the reaper.
pub fn init() {
    let main = new_task("main", ptr::null_mut(), 0);
    let tick = arch::timer_freq() / irq::TICK_HZ;
    SCHED.set(Scheduler {
        tasks: alloc::vec![Some(main)],
        current: 0,
        idle: 0,
        switches: 0,
        reaped: 0,
        started: arch::timer_count(),
        min_vruntime: 0,
        wake_credit: tick * VR_SCALE / Weight::Normal as u64,
        wakes: WakeStats::default(),
    });
    let idle = spawn("idle", idle_task);
    with(|s| s.idle = idle);
    spawn("reap", reaper_task);
}

/// A fresh kernel stack with a zeroed frame on top.
fn new_stack() -> (Stack, *mut TrapFrame) {
    let stack = Stack::new();
    // 816 bytes keeps sp 16-aligned.
    let frame = (stack.top() - size_of::<TrapFrame>()) as *mut TrapFrame;
    // SAFETY: the top of a stack just allocated for this task; an all-zero frame is valid.
    unsafe { ptr::write_bytes(frame, 0, 1) };
    (stack, frame)
}

/// Puts a ready task in the first free slot; its index is the task's id.
/// It starts level with the least virtual runtime, neither ahead nor behind.
fn add(
    name: &'static str,
    stack: Stack,
    frame: *mut TrapFrame,
    space: Option<AddressSpace>,
) -> usize {
    let ttbr = space.as_ref().map_or_else(mmu::kernel_ttbr, AddressSpace::ttbr);
    with(|s| {
        let task =
            Task { ttbr, stack: Some(stack), space, ..new_task(name, frame, s.min_vruntime) };
        place(s, task)
    })
}

fn place(s: &mut Scheduler, task: Task) -> usize {
    match s.tasks.iter().position(Option::is_none) {
        Some(id) => {
            s.tasks[id] = Some(task);
            id
        }
        None => {
            s.tasks.push(Some(task));
            s.tasks.len() - 1
        }
    }
}

/// Sets the share of the CPU task `id` gets from now on.
pub fn set_weight(id: usize, weight: Weight) {
    with(|s| s.task(id).weight = weight);
}

/// Creates a ready task that runs `entry` on a fresh stack. Returns its id.
pub fn spawn(name: &'static str, entry: fn()) -> usize {
    let (stack, frame) = new_stack();
    // SAFETY: a fresh frame that no task uses yet.
    unsafe {
        // A frame that "returns" into task_start(entry).
        (*frame).x[0] = entry as usize as u64;
        (*frame).elr = task_start as extern "C" fn(usize) -> ! as usize as u64;
        (*frame).spsr = SPSR_EL1H;
    }
    add(name, stack, frame, None)
}

/// Creates a ready user task that starts at `entry` at EL0 in `space`, on
/// the user stack `sp`, with `args` in x0 and x1. Its exceptions are taken
/// on a kernel stack of its own. The task owns `space` from now on, and the
/// reaper frees it after the task ends. Returns its id.
pub fn spawn_user(
    name: &'static str,
    space: AddressSpace,
    entry: u64,
    sp: u64,
    args: [u64; 2],
) -> usize {
    let (stack, frame) = new_stack();
    // SAFETY: a fresh frame that no task uses yet.
    unsafe {
        (*frame).x[0] = args[0];
        (*frame).x[1] = args[1];
        (*frame).elr = entry;
        (*frame).sp_el0 = sp;
        (*frame).spsr = SPSR_EL0T;
    }
    add(name, stack, frame, Some(space))
}

/// Id of the running task.
pub fn current() -> usize {
    with(|s| s.current)
}

/// From an exception: puts the running task to sleep for `ticks` and
/// returns the frame to resume instead.
pub fn sleep_from_trap(frame: *mut TrapFrame, ticks: u64) -> *mut TrapFrame {
    with(|s| {
        let me = s.current;
        s.task(me).state = State::Sleeping(irq::ticks() + ticks);
    });
    switch(frame)
}

/// From an exception: blocks the running task until `wake(tag)` and
/// returns the frame to resume instead.
pub fn block_from_trap(frame: *mut TrapFrame, tag: u64) -> *mut TrapFrame {
    with(|s| {
        let me = s.current;
        s.task(me).state = State::Blocked(tag);
    });
    switch(frame)
}

/// Makes every task blocked on `tag` ready again. It runs before the tasks
/// that kept running meanwhile, from the next switch on.
pub fn wake(tag: u64) {
    let now = arch::timer_count();
    with(|s| {
        let floor = s.wake_floor();
        for t in s.tasks.iter_mut().flatten() {
            if t.state == State::Blocked(tag) {
                t.state = State::Ready;
                t.vruntime = t.vruntime.max(floor);
                t.woken_at = Some(now);
            }
        }
    });
}

/// Marks the running task dead and wakes the reaper, which frees it once
/// it has been switched away from.
fn mark_dead() {
    with(|s| {
        let me = s.current;
        s.task(me).state = State::Dead;
    });
    wake(REAP);
}

/// From an exception: ends the running task for good and returns the frame
/// to resume instead.
pub fn exit_from_trap(frame: *mut TrapFrame) -> *mut TrapFrame {
    mark_dead();
    switch(frame)
}

/// First code of every spawned task; x0 holds the `fn()` from `spawn`.
extern "C" fn task_start(entry: usize) -> ! {
    // SAFETY: `spawn` stored a `fn()` here.
    let entry: fn() = unsafe { core::mem::transmute(entry) };
    entry();
    // Still on its stack here: the reaper frees it only after the switch.
    mark_dead();
    yield_now();
    unreachable!("a dead task was resumed");
}

fn idle_task() {
    loop {
        arch::wait_for_interrupt();
    }
}

/// Takes the dead tasks out of the table, then drops them, which frees
/// their stacks and address spaces. Blocks while there are none.
///
/// A dead task is never current here (the reaper is), and is never picked
/// again, so nothing runs on its stack or in its address space any more.
fn reaper_task() {
    loop {
        // Masked from the check to the block, so a `wake(REAP)` in between
        // isn't lost.
        let dead: Vec<Task> = arch::without_irqs(|| {
            let dead = with(|s| {
                let dead: Vec<Task> = s
                    .tasks
                    .iter_mut()
                    .filter(|t| t.as_ref().is_some_and(|t| t.state == State::Dead))
                    .filter_map(Option::take)
                    .collect();
                s.reaped += dead.len() as u64;
                dead
            });
            if dead.is_empty() {
                with(|s| {
                    let me = s.current;
                    s.task(me).state = State::Blocked(REAP);
                });
                yield_now();
            }
            dead
        });
        // Dropped with IRQs on: freeing an address space walks its tables.
        drop(dead);
    }
}

/// Gives up the CPU to the next ready task.
pub fn yield_now() {
    // SAFETY: the exception path saves and restores every register around the switch.
    unsafe { asm!("svc #0", options(nostack)) };
}

/// Blocks the calling task for at least `ticks` timer ticks.
pub fn sleep(ticks: u64) {
    // Masked so a tick cannot switch away between marking and yielding.
    arch::without_irqs(|| {
        with(|s| {
            let me = s.current;
            s.task(me).state = State::Sleeping(irq::ticks() + ticks);
        });
        yield_now();
    });
}

/// Called from the exception path with IRQs masked: saves `frame` as the
/// running task's state and returns the frame of the task to run next.
pub fn switch(frame: *mut TrapFrame) -> *mut TrapFrame {
    // Ticks before `init` keep running whatever was interrupted.
    SCHED.try_with(|s| pick(s, frame)).unwrap_or(frame)
}

fn pick(s: &mut Scheduler, frame: *mut TrapFrame) -> *mut TrapFrame {
    let now = arch::timer_count();
    let me = s.current;
    let ran = now - s.started;
    s.started = now;
    let t = s.task(me);
    t.frame = frame;
    t.runtime += ran;
    t.vruntime += ran * VR_SCALE / t.weight as u64;

    let tick = irq::ticks();
    let floor = s.wake_floor();
    for t in s.tasks.iter_mut().flatten() {
        if let State::Sleeping(until) = t.state
            && tick >= until
        {
            t.state = State::Ready;
            t.vruntime = t.vruntime.max(floor);
        }
    }

    // The least virtual runtime wins; among equals, the first after the
    // current task (round-robin). Idle only if nobody else is ready.
    let n = s.tasks.len();
    let vruntime = |s: &Scheduler, i: usize| s.tasks[i].as_ref().map_or(u64::MAX, |t| t.vruntime);
    let next = (1..=n)
        .map(|i| (me + i) % n)
        .filter(|&i| i != s.idle && s.is_ready(i))
        .min_by_key(|&i| vruntime(s, i))
        .unwrap_or(s.idle);
    if next != s.idle {
        s.min_vruntime = s.min_vruntime.max(vruntime(s, next));
    }
    if let Some(at) = s.task(next).woken_at.take() {
        let w = &mut s.wakes;
        w.count += 1;
        w.total += now - at;
        w.max = w.max.max(now - at);
    }
    if next != me {
        s.switches += 1;
        let ttbr = s.task(next).ttbr;
        if ttbr != s.task(me).ttbr {
            // SAFETY: task TTBRs come from `kernel_ttbr` or the task's own `AddressSpace`,
            // which lives as long as the task.
            unsafe { mmu::set_ttbr0(ttbr) };
        }
    }
    s.current = next;
    s.task(next).frame
}

pub struct TaskInfo {
    pub name: &'static str,
    /// Time it has run, in `CNTVCT` counts.
    pub runtime: u64,
}

/// Per-task runtime of the live tasks and the total number of switches so
/// far.
pub fn snapshot() -> (Vec<TaskInfo>, u64) {
    with(|s| {
        let tasks = s
            .tasks
            .iter()
            .flatten()
            .map(|t| TaskInfo { name: t.name, runtime: t.runtime })
            .collect();
        (tasks, s.switches)
    })
}

/// Wake-to-run latency so far: (wakes, average µs, maximum µs).
pub fn wake_latency() -> (u64, u64, u64) {
    let w = with(|s| s.wakes);
    let us = |counts: u64| counts * 1_000_000 / arch::timer_freq().max(1);
    (w.count, us(w.total / w.count.max(1)), us(w.max))
}

/// How many tasks the reaper has freed so far.
pub fn reaped() -> u64 {
    with(|s| s.reaped)
}

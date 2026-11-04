//! The status lines at the bottom of the screen, redrawn by the `main` task:
//! uptime, the NAND lab, CPU share per task, the user programs' lines and
//! key help. Typed keys go to the lab or to the circuit builder.

use crate::arch;
use crate::demo::lab::{self, Lab};
use crate::demo::tasks;
use crate::dev::console::{self, DIM, ERR, FG};
use crate::dev::irq;
use crate::dev::usb::keyboard;
use crate::proc::{task, user};
use crate::{print, println};
use alloc::format;
use alloc::string::String;
use core::sync::atomic::Ordering;

/// Redraws `line` in the dim colour, cut to the screen width: wrapping
/// would overwrite the line below.
fn status_line(line: usize, args: core::fmt::Arguments) {
    let text = format!("{}", args);
    console::with(|c| {
        c.goto_line(line);
        c.set_color(DIM);
        let cut = text.char_indices().nth(c.cols()).map_or(text.len(), |(i, _)| i);
        core::fmt::Write::write_str(c, &text[..cut]).ok();
        c.set_color(FG);
    });
}

fn uptime(line: usize, secs: u64) {
    status_line(
        line,
        format_args!("uptime {:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60),
    );
}

/// Without interrupts: spin on the generic timer, redraw once per second.
pub fn polling_loop(freq: u64) -> ! {
    let line = console::with(|c| c.row()).unwrap_or(0);
    let start = arch::timer_count();
    let mut last = u64::MAX;
    loop {
        let secs = (arch::timer_count() - start) / freq.max(1);
        if secs != last {
            last = secs;
            uptime(line, secs);
        }
        core::hint::spin_loop();
    }
}

/// Timer ticks per half clock period of the NAND counter (0.5 s).
const HALF_PERIOD_TICKS: u64 = irq::TICK_HZ / 2;

/// Task lines: CPU share per task (ticks it was running for), then progress.
fn task_status(line: usize) {
    let (tasks, switches) = task::snapshot();
    let total = tasks.iter().map(|t| t.ticks).sum::<u64>().max(1);
    let mut cpu = String::from("cpu%");
    // Tasks under 1% are left out: with user programs not all names fit.
    for t in tasks.iter().filter(|t| t.ticks * 100 / total > 0) {
        cpu += &format!(" {} {}", t.name, t.ticks * 100 / total);
    }
    status_line(line, format_args!("{}", cpu));
    status_line(
        line + 1,
        format_args!(
            "spin {}M  nand {}  fpu {}k  sw {}",
            tasks::SPIN_COUNT.load(Ordering::Relaxed) / 1_000_000,
            tasks::NAND_ROUNDS.load(Ordering::Relaxed),
            tasks::FPU_ROUNDS.load(Ordering::Relaxed) / 1000,
            switches
        ),
    );
}

/// Ticks between redraws of the task lines (10 per second).
const TASK_REDRAW_TICKS: u64 = irq::TICK_HZ / 10;

/// Where typed keys go: the kernel's NAND lab, or the circuit builder.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Lab,
    Builder,
}

/// The key help line for the current focus, or why there is no keyboard;
/// with the count of reports lost to USB errors, if any.
fn help_status(line: usize, keyboard: bool, focus: Focus, builder: bool) {
    if !keyboard {
        return status_line(line, format_args!("keys: (no keyboard)"));
    }
    match (focus, builder) {
        (Focus::Lab, false) => status_line(line, format_args!("{}", lab::HELP)),
        (Focus::Lab, true) => status_line(line, format_args!("{}  tab:u6", lab::HELP)),
        (Focus::Builder, _) => {
            status_line(line, format_args!("keys -> u6 circuit builder  tab:lab"))
        }
    }
    let errors = keyboard::errors();
    if errors > 0 {
        console::with(|c| c.set_color(ERR));
        print!(" err {}", errors);
        console::with(|c| c.set_color(FG));
    }
}

/// The `main` task: sleep a tick at a time; redraw the uptime and task lines,
/// clock the NAND counter from the tick count, and pass typed keys to the
/// NAND lab or the circuit builder (Tab switches).
pub fn interrupt_loop(keyboard: bool, builder: bool) -> ! {
    let mut lab = Lab::new();
    println!("[lab ] counter {} NAND, adder {} NAND", lab.counter_gates(), lab.adder_gates());
    // Reserve a blank spacer, then rows for uptime, counter, adder, two task
    // lines, the user programs' lines and key help. Rows are taken relative
    // to the cursor, which stays valid if the screen scrolls.
    let user_rows = user::lines().len();
    let rows = 6 + user_rows;
    for _ in 0..rows {
        println!();
    }
    let line = console::with(|c| c.row()).unwrap_or(rows - 1) + 1 - rows;
    let help = line + rows - 1;
    let mut seen = u64::MAX;
    let mut drawn = u64::MAX;
    let mut errors = keyboard::errors();
    let mut generation = u64::MAX;
    let mut focus = if builder { Focus::Builder } else { Focus::Lab };
    status_line(line + 1, format_args!("{}", lab.counter_line()));
    status_line(line + 2, format_args!("{}", lab.adder_line()));
    help_status(help, keyboard, focus, builder);
    loop {
        while let Some(ch) = keyboard::read() {
            if ch == '\t' && builder {
                focus = if focus == Focus::Lab { Focus::Builder } else { Focus::Lab };
                help_status(help, keyboard, focus, builder);
                continue;
            }
            if focus == Focus::Builder {
                user::send_key(ch);
                continue;
            }
            let changed = lab.key(ch);
            if changed.counter {
                status_line(line + 1, format_args!("{}", lab.counter_line()));
            }
            if changed.adder {
                status_line(line + 2, format_args!("{}", lab.adder_line()));
            }
        }
        if keyboard::errors() != errors {
            errors = keyboard::errors();
            help_status(help, keyboard, focus, builder);
        }
        if user::generation() != generation {
            generation = user::generation();
            for (i, (name, text)) in user::lines().iter().enumerate() {
                status_line(line + 5 + i, format_args!("{:<2} {}", name.unwrap_or(""), text));
            }
        }

        let t = irq::ticks();
        if t != seen {
            seen = t;
            if t / TASK_REDRAW_TICKS != drawn {
                drawn = t / TASK_REDRAW_TICKS;
                uptime(line, t / irq::TICK_HZ);
                task_status(line + 3);
            }
            if lab.timer_clock((t / HALF_PERIOD_TICKS) % 2 == 1) {
                status_line(line + 1, format_args!("{}", lab.counter_line()));
            }
        }
        task::sleep(1);
    }
}

//! NAND circuits driven from the keyboard (M6).
//!
//! Two circuits sit at the bottom of the screen: the 4-bit counter, clocked
//! by the timer or by hand, and the 4-bit adder, whose inputs are toggled
//! by keys. Every change re-evaluates the NAND gates, and the outputs are
//! checked against plain arithmetic as before: the counter must step by one
//! on each rising edge and hold on each falling edge, and the adder must
//! add. Pure logic: drawing and key input stay in `main`.

use nand::{Adder4, Circuit, Wire};
use alloc::format;
use alloc::string::String;

/// One line of key help; fits in 48 columns.
pub const HELP: &str = "keys 1-4:a 5-8:b c:carry spc:clk m:mode";

pub struct Lab {
    counter: Circuit,
    clk: Wire,
    q: [Wire; 4],
    level: bool,
    value: u64,
    edges: u64,
    /// The clock follows the keyboard instead of the timer.
    manual: bool,
    adder: Adder4,
    a: u64,
    b: u64,
    carry: bool,
    sum: u64,
}

/// Which lines a key changed.
#[derive(Default)]
pub struct Changed {
    pub counter: bool,
    pub adder: bool,
}

impl Lab {
    pub fn new() -> Self {
        let mut c = Circuit::new();
        let (clk, one) = (c.input(), c.input());
        c.set(one, true);
        let q = c.counter::<4>(clk, one);
        c.eval().expect("counter did not settle");
        let value = c.get_bits(&q);
        let mut lab = Self {
            counter: c,
            clk,
            q,
            level: false,
            value,
            edges: 0,
            manual: false,
            adder: Adder4::new(),
            a: 3,
            b: 5,
            carry: false,
            sum: 0,
        };
        lab.add();
        lab
    }

    pub fn counter_gates(&self) -> usize {
        self.counter.gate_count()
    }

    pub fn adder_gates(&self) -> usize {
        self.adder.gate_count()
    }

    /// Follows the timer's clock, unless the clock is in manual mode.
    /// Returns whether the counter line changed.
    pub fn timer_clock(&mut self, level: bool) -> bool {
        if self.manual || level == self.level {
            return false;
        }
        self.set_clock(level);
        true
    }

    fn set_clock(&mut self, level: bool) {
        self.level = level;
        self.counter.set(self.clk, level);
        self.counter.eval().expect("counter did not settle");
        let now = self.counter.get_bits(&self.q);
        if level {
            self.edges += 1;
            if now != (self.value + 1) % 16 {
                panic!("counter went {} -> {} on a rising edge", self.value, now);
            }
        } else if now != self.value {
            panic!("counter changed {} -> {} on a falling edge", self.value, now);
        }
        self.value = now;
    }

    fn add(&mut self) {
        self.sum = self.adder.add(self.a, self.b, self.carry);
        if self.sum != self.a + self.b + self.carry as u64 {
            panic!("4-bit adder: {} + {} + {} gave {}", self.a, self.b, self.carry as u8, self.sum);
        }
    }

    /// Applies one typed key. Keys 1-4 and 5-8 toggle the bits of `a` and
    /// `b` in the order they are shown, most significant first.
    pub fn key(&mut self, key: char) -> Changed {
        let mut changed = Changed::default();
        match key.to_ascii_lowercase() {
            k @ '1'..='4' => {
                self.a ^= 8 >> (k as u8 - b'1');
                changed.adder = true;
            }
            k @ '5'..='8' => {
                self.b ^= 8 >> (k as u8 - b'5');
                changed.adder = true;
            }
            'c' => {
                self.carry = !self.carry;
                changed.adder = true;
            }
            // Stepping by hand takes the clock away from the timer.
            ' ' => {
                self.manual = true;
                self.set_clock(!self.level);
                changed.counter = true;
            }
            'm' => {
                self.manual = !self.manual;
                changed.counter = true;
            }
            _ => {}
        }
        if changed.adder {
            self.add();
        }
        changed
    }

    pub fn counter_line(&self) -> String {
        format!(
            "ctr clk={} q={:04b} ({:2}) edges={} {}",
            self.level as u8,
            self.value,
            self.value,
            self.edges,
            if self.manual { "manual" } else { "auto" }
        )
    }

    pub fn adder_line(&self) -> String {
        format!(
            "add a={:04b} b={:04b} c={} -> {:05b} ({}+{}+{}={})",
            self.a, self.b, self.carry as u8, self.sum, self.a, self.b, self.carry as u8, self.sum
        )
    }
}

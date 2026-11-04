//! A gate-level circuit simulator where the only primitive is NAND.
//!
//! A circuit is a list of NAND gates over numbered wires. Inputs are wires
//! with no driving gate; everything else (NOT, AND, OR, XOR, adders) is built
//! from `nand` calls, so `gate_count` is the real NAND count of a design.
//!
//! `eval` sweeps the gates in creation order until no wire changes. A purely
//! combinational circuit settles in one sweep plus one to confirm; circuits
//! with feedback (latches, M3) take more sweeps, and an oscillating one is
//! reported as unstable. Pure logic: no hardware access, no `unsafe`.
//!
//! A crate of its own because both the kernel (demos, `lab`) and the user
//! space circuit builder (M9) simulate with it.

#![no_std]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

pub type Wire = usize;

struct Nand {
    a: Wire,
    b: Wire,
    out: Wire,
}

#[derive(Default)]
pub struct Circuit {
    gates: Vec<Nand>,
    values: Vec<bool>,
}

/// Returned by `eval` when the wires never settle.
#[derive(Debug)]
pub struct Unstable;

const MAX_SWEEPS: usize = 64;

impl Circuit {
    pub fn new() -> Self {
        Self { gates: Vec::new(), values: Vec::new() }
    }

    fn wire(&mut self) -> Wire {
        self.values.push(false);
        self.values.len() - 1
    }

    pub fn input(&mut self) -> Wire {
        self.wire()
    }

    pub fn nand(&mut self, a: Wire, b: Wire) -> Wire {
        let out = self.wire();
        self.gates.push(Nand { a, b, out });
        out
    }

    pub fn gate_count(&self) -> usize {
        self.gates.len()
    }

    /// The wires no gate drives (inputs, and forward wires never driven)
    /// that `w` depends on, in creation order.
    pub fn sources(&self, w: Wire) -> Vec<Wire> {
        let mut driver = vec![None; self.values.len()];
        for g in &self.gates {
            driver[g.out] = Some((g.a, g.b));
        }
        let mut seen = vec![false; self.values.len()];
        let (mut todo, mut found) = (vec![w], Vec::new());
        while let Some(x) = todo.pop() {
            if core::mem::replace(&mut seen[x], true) {
                continue;
            }
            match driver[x] {
                Some((a, b)) => todo.extend([a, b]),
                None => found.push(x),
            }
        }
        found.sort();
        found
    }

    pub fn set(&mut self, w: Wire, v: bool) {
        self.values[w] = v;
    }

    pub fn get(&self, w: Wire) -> bool {
        self.values[w]
    }

    /// Propagates values until stable. Returns the number of sweeps.
    pub fn eval(&mut self) -> Result<usize, Unstable> {
        for sweep in 1..=MAX_SWEEPS {
            let mut changed = false;
            for g in &self.gates {
                let v = !(self.values[g.a] && self.values[g.b]);
                if self.values[g.out] != v {
                    self.values[g.out] = v;
                    changed = true;
                }
            }
            if !changed {
                return Ok(sweep);
            }
        }
        Err(Unstable)
    }

    // --- Gates built from NAND -------------------------------------------

    /// 1 NAND.
    pub fn not(&mut self, a: Wire) -> Wire {
        self.nand(a, a)
    }

    /// 2 NAND.
    pub fn and(&mut self, a: Wire, b: Wire) -> Wire {
        let n = self.nand(a, b);
        self.not(n)
    }

    /// 3 NAND: a | b = !(!a & !b).
    pub fn or(&mut self, a: Wire, b: Wire) -> Wire {
        let (na, nb) = (self.not(a), self.not(b));
        self.nand(na, nb)
    }

    /// 4 NAND.
    pub fn xor(&mut self, a: Wire, b: Wire) -> Wire {
        let n = self.nand(a, b);
        self.xor_with(a, b, n)
    }

    /// The last 3 gates of XOR, reusing an existing `nand(a, b)`.
    fn xor_with(&mut self, a: Wire, b: Wire, nab: Wire) -> Wire {
        let x = self.nand(a, nab);
        let y = self.nand(b, nab);
        self.nand(x, y)
    }

    // --- Arithmetic -------------------------------------------------------

    /// 5 NAND. Returns (sum, carry); XOR and carry share `nand(a, b)`.
    pub fn half_adder(&mut self, a: Wire, b: Wire) -> (Wire, Wire) {
        let n = self.nand(a, b);
        let sum = self.xor_with(a, b, n);
        let carry = self.not(n);
        (sum, carry)
    }

    /// 9 NAND, the classic layout. Returns (sum, carry_out).
    pub fn full_adder(&mut self, a: Wire, b: Wire, cin: Wire) -> (Wire, Wire) {
        let n1 = self.nand(a, b);
        let s1 = self.xor_with(a, b, n1);
        let n2 = self.nand(s1, cin);
        let sum = self.xor_with(s1, cin, n2);
        // carry = (a & b) | (s1 & cin) = !(n1 & n2)
        let carry = self.nand(n1, n2);
        (sum, carry)
    }

    /// Ripple-carry adder over little-endian bits. Returns (sum, carry_out).
    pub fn adder<const N: usize>(
        &mut self,
        a: [Wire; N],
        b: [Wire; N],
        cin: Wire,
    ) -> ([Wire; N], Wire) {
        let mut carry = cin;
        let sum = core::array::from_fn(|i| {
            let (s, c) = self.full_adder(a[i], b[i], carry);
            carry = c;
            s
        });
        (sum, carry)
    }

    // --- Sequential logic -------------------------------------------------
    //
    // Feedback needs wires that are used before the gate driving them exists:
    // `forward` creates such a wire and `drive` attaches its NAND later.

    pub fn forward(&mut self) -> Wire {
        self.wire()
    }

    pub fn drive(&mut self, out: Wire, a: Wire, b: Wire) {
        self.gates.push(Nand { a, b, out });
    }

    /// SR latch with active-low inputs, driving `q`. 2 NAND.
    fn sr_latch_to(&mut self, q: Wire, s_n: Wire, r_n: Wire) {
        let q_n = self.forward();
        self.drive(q, s_n, q_n);
        self.drive(q_n, r_n, q);
    }

    /// Gated D latch, transparent while `en` is 1, driving `q`. 4 NAND.
    fn d_latch_to(&mut self, q: Wire, d: Wire, en: Wire) {
        let s_n = self.nand(d, en);
        // nand(s_n, en) == nand(!d, en) whenever en is 1, saving a NOT.
        let r_n = self.nand(s_n, en);
        self.sr_latch_to(q, s_n, r_n);
    }

    pub fn d_latch(&mut self, d: Wire, en: Wire) -> Wire {
        let q = self.forward();
        self.d_latch_to(q, d, en);
        q
    }

    /// Rising-edge master-slave D flip-flop driving `q`. 8 NAND; `clk_n`
    /// (= !clk) is shared between flip-flops on the same clock.
    pub fn d_flip_flop_to(&mut self, q: Wire, d: Wire, clk: Wire, clk_n: Wire) {
        let master = self.d_latch(d, clk_n);
        self.d_latch_to(q, master, clk);
    }

    /// Synchronous N-bit binary counter: on every rising edge of `clk` the
    /// flip-flops load `q + 1`, computed by a half-adder chain whose carry-in
    /// is `one` (a wire held at 1). Returns the little-endian outputs.
    pub fn counter<const N: usize>(&mut self, clk: Wire, one: Wire) -> [Wire; N] {
        let clk_n = self.not(clk);
        let q: [Wire; N] = core::array::from_fn(|_| self.forward());
        let mut carry = one;
        let next: [Wire; N] = core::array::from_fn(|i| {
            let (s, c) = self.half_adder(q[i], carry);
            carry = c;
            s
        });
        for i in 0..N {
            self.d_flip_flop_to(q[i], next[i], clk, clk_n);
        }
        q
    }

    // --- Multi-bit helpers ------------------------------------------------

    pub fn inputs<const N: usize>(&mut self) -> [Wire; N] {
        core::array::from_fn(|_| self.input())
    }

    pub fn set_bits(&mut self, wires: &[Wire], value: u64) {
        for (i, &w) in wires.iter().enumerate() {
            self.set(w, value >> i & 1 == 1);
        }
    }

    pub fn get_bits(&self, wires: &[Wire]) -> u64 {
        wires.iter().enumerate().map(|(i, &w)| (self.get(w) as u64) << i).sum()
    }
}

/// A NAND 4-bit ripple-carry adder with its input and output wires.
pub struct Adder4 {
    c: Circuit,
    a: [Wire; 4],
    b: [Wire; 4],
    cin: Wire,
    sum: [Wire; 4],
    cout: Wire,
}

impl Adder4 {
    pub fn new() -> Self {
        let mut c = Circuit::new();
        let (a, b, cin) = (c.inputs::<4>(), c.inputs::<4>(), c.input());
        let (sum, cout) = c.adder(a, b, cin);
        Self { c, a, b, cin, sum, cout }
    }

    pub fn gate_count(&self) -> usize {
        self.c.gate_count()
    }

    /// Returns the 5-bit result of `x + y + carry_in`.
    pub fn add(&mut self, x: u64, y: u64, carry_in: bool) -> u64 {
        self.c.set_bits(&self.a, x);
        self.c.set_bits(&self.b, y);
        self.c.set(self.cin, carry_in);
        self.c.eval().expect("adder did not settle");
        self.c.get_bits(&self.sum) | (self.c.get(self.cout) as u64) << 4
    }

    /// Checks all 512 inputs against real addition; panics on a mismatch.
    pub fn check_all(&mut self) {
        for x in 0..16 {
            for y in 0..16 {
                for carry_in in [false, true] {
                    let r = self.add(x, y, carry_in);
                    if r != x + y + carry_in as u64 {
                        panic!("4-bit adder: {} + {} + {} gave {}", x, y, carry_in as u8, r);
                    }
                }
            }
        }
    }
}

impl Default for Adder4 {
    fn default() -> Self {
        Self::new()
    }
}

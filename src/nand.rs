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

use alloc::vec::Vec;

pub type Wire = usize;

struct Nand {
    a: Wire,
    b: Wire,
    out: Wire,
}

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

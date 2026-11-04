//! NAND circuits printed at boot: truth tables of the basic gates built from
//! NAND, and a 4-bit ripple-carry adder checked on every input.

use crate::ok;
use crate::println;
use alloc::string::String;
use alloc::vec::Vec;
use nand::{Adder4, Circuit, Wire};

/// Prints the truth table of a small NAND circuit and checks every row.
///
/// Inputs are listed MSB first (`ab`), outputs in the order `build` returns
/// them; `expect` maps the input bits to the expected output bits.
fn truth_table(
    name: &str,
    n_inputs: usize,
    build: impl Fn(&mut Circuit, &[Wire]) -> Vec<Wire>,
    expect: impl Fn(&[bool]) -> Vec<bool>,
) {
    let mut c = Circuit::new();
    let ins: Vec<Wire> = (0..n_inputs).map(|_| c.input()).collect();
    let outs = build(&mut c, &ins);
    let bit = |b: bool| if b { '1' } else { '0' };

    let mut rows = Vec::new();
    for combo in 0..1u32 << n_inputs {
        let bits: Vec<bool> = (0..n_inputs).map(|k| combo >> (n_inputs - 1 - k) & 1 == 1).collect();
        for (&w, &v) in ins.iter().zip(&bits) {
            c.set(w, v);
        }
        c.eval().expect("combinational circuit did not settle");
        let got: Vec<bool> = outs.iter().map(|&w| c.get(w)).collect();
        if got != expect(&bits) {
            panic!("{} is wrong for inputs {:?}", name, bits);
        }
        let mut row: String = bits.iter().map(|&b| bit(b)).collect();
        row.push('>');
        row.extend(got.iter().map(|&b| bit(b)));
        rows.push(row);
    }

    for (i, chunk) in rows.chunks(4).enumerate() {
        let label = if i == 0 { name } else { "" };
        let cells = chunk.join(" ");
        if i == 0 {
            println!("[nand] {:<4} {}  {} NAND", label, cells, c.gate_count());
        } else {
            println!("       {:<4} {}", label, cells);
        }
    }
}

/// Prints the truth tables of the basic gates and checks the 4-bit adder.
pub fn run() {
    truth_table("NOT", 1, |c, i| [c.not(i[0])].into(), |b| [!b[0]].into());
    truth_table("AND", 2, |c, i| [c.and(i[0], i[1])].into(), |b| [b[0] & b[1]].into());
    truth_table("OR", 2, |c, i| [c.or(i[0], i[1])].into(), |b| [b[0] | b[1]].into());
    truth_table("XOR", 2, |c, i| [c.xor(i[0], i[1])].into(), |b| [b[0] ^ b[1]].into());
    truth_table(
        "HA",
        2,
        |c, i| {
            let (s, carry) = c.half_adder(i[0], i[1]);
            [s, carry].into()
        },
        |b| [b[0] ^ b[1], b[0] & b[1]].into(),
    );
    truth_table(
        "FA",
        3,
        |c, i| {
            let (s, carry) = c.full_adder(i[0], i[1], i[2]);
            [s, carry].into()
        },
        |b| {
            let n = b.iter().filter(|&&x| x).count();
            [n & 1 == 1, n >= 2].into()
        },
    );

    // 4-bit ripple-carry adder: show a few sums, then check every input.
    let mut adder = Adder4::new();
    for (x, y) in [(3, 5), (7, 8), (9, 9), (15, 15)] {
        let r = adder.add(x, y, false);
        println!("[nand] {:2} + {:2} = {:2}  ({:04b} + {:04b} = {:05b})", x, y, r, x, y, r);
    }
    adder.check_all();
    ok(format_args!("M2.5: 4-bit adder 512/512 ({} NAND)", adder.gate_count()));
}

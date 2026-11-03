//! The circuit builder (M9): type NAND circuits on the keyboard and try them.
//!
//! Keys arrive through the keyboard channel (capability slot 1), with a
//! blocking `wait`, so the builder sleeps until something is typed. Its
//! screen capability covers 9 lines: 8 of output and the input line.
//!
//! The only built-in gate is `nand`; everything else is a part the user
//! defines (`demo` loads the usual ones). A part is kept as text and
//! expanded into fresh NAND gates every time it is used, with its own names,
//! so it cannot see or change the caller's wires. Expressions are prefix
//! calls with fixed arity, so `nand not a b` and `nand (not a) b` mean the
//! same. Feedback is built from a forward wire (`q = wire`) driven later by
//! one NAND (`q <- nand s qn`). Simulation is the kernel's own `nand` crate.

use crate::{heap, Line};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use nand::{Circuit, Wire};

const SCREEN: u64 = crate::SCREEN;
const KEYS: u64 = 1;
const LOG_ROWS: usize = 8;
const INPUT_ROW: u64 = 8;
/// Longest command; with the prompt it fits the line.
const INPUT_MAX: usize = 40;
/// Parts may use parts, but not without end.
const MAX_DEPTH: usize = 16;
/// `table` lists every combination of at most this many inputs.
const TABLE_MAX: usize = 4;

const KEYWORDS: &[&str] = &["in", "def", "set", "show", "table", "parts", "clear", "demo", "help", "nand", "wire"];

const HELP: &[&str] = &[
    "in a b          make inputs a and b",
    "x = nand a b    a wire (parts, parens ok)",
    "def f a b = n = nand a b; nand n n",
    "set a 1   show [x..]   table x",
    "parts   clear   demo (not and or xor mux)",
    "q = wire, q <- nand a b   for feedback",
];

const DEMO: &[&str] = &[
    "def not a = nand a a",
    "def and a b = not (nand a b)",
    "def or a b = nand (not a) (not b)",
    "def xor a b = n = nand a b; nand (nand a n) (nand b n)",
    "def mux s a b = nand (nand a (not s)) (nand b s)",
];

/// The 8 output lines, scrolling up, and the line being typed.
struct Ui {
    log: [Line; LOG_ROWS],
    input: Line,
}

impl Ui {
    fn say(&mut self, args: core::fmt::Arguments) {
        self.log.rotate_left(1);
        let last = &mut self.log[LOG_ROWS - 1];
        last.clear();
        let _ = last.write_fmt(args);
        for (row, line) in self.log.iter().enumerate() {
            crate::write_row(SCREEN, row as u64, line.as_bytes());
        }
    }

    fn draw_input(&self) {
        let mut line = Line::new();
        let _ = write!(line, "> {}_", self.input.as_str());
        crate::write_row(SCREEN, INPUT_ROW, line.as_bytes());
    }
}

/// Why a command failed, and the word it failed on (if any).
struct Error<'a>(&'static str, &'a str);

type Result<'a, T> = core::result::Result<T, Error<'a>>;

struct Part {
    name: String,
    params: Vec<String>,
    body: String,
}

#[derive(Default)]
struct Builder {
    circuit: Circuit,
    /// Top-level names, in definition order.
    names: Vec<(String, Wire)>,
    parts: Vec<Part>,
}

fn tokens(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in line.char_indices() {
        let single = matches!(c, '(' | ')' | ';');
        if c.is_whitespace() || single {
            if let Some(s) = start.take() {
                out.push(&line[s..i]);
            }
            if single {
                out.push(&line[i..i + 1]);
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        out.push(&line[s..]);
    }
    out
}

fn valid_name(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !KEYWORDS.contains(&word)
}

/// Names visible while evaluating: the top level, or one part expansion.
type Env<'a> = Vec<(&'a str, Wire)>;

/// The top-level names as an environment. A free function, so that only
/// `names` is borrowed while the circuit is being built.
fn env_of(names: &[(String, Wire)]) -> Env<'_> {
    names.iter().map(|(n, w)| (n.as_str(), *w)).collect()
}

fn lookup(env: &Env, name: &str) -> Option<Wire> {
    env.iter().rev().find(|(n, _)| *n == name).map(|&(_, w)| w)
}

/// Builds one expression starting at `toks[*pos]` and returns its wire.
fn expr<'a>(
    circuit: &mut Circuit,
    parts: &[Part],
    env: &Env,
    toks: &[&'a str],
    pos: &mut usize,
    depth: usize,
) -> Result<'a, Wire> {
    let Some(&tok) = toks.get(*pos) else {
        return Err(Error("expression ends too soon", ""));
    };
    *pos += 1;
    if tok == "(" {
        let w = expr(circuit, parts, env, toks, pos, depth)?;
        if toks.get(*pos) != Some(&")") {
            return Err(Error("missing )", ""));
        }
        *pos += 1;
        return Ok(w);
    }
    if tok == "nand" {
        let a = expr(circuit, parts, env, toks, pos, depth)?;
        let b = expr(circuit, parts, env, toks, pos, depth)?;
        return Ok(circuit.nand(a, b));
    }
    if let Some(part) = parts.iter().find(|p| p.name == tok) {
        let mut args = Vec::new();
        for _ in &part.params {
            args.push(expr(circuit, parts, env, toks, pos, depth)?);
        }
        return expand(circuit, parts, part, &args, depth + 1).map_err(|e| Error(e.0, tok));
    }
    lookup(env, tok).ok_or(Error("unknown name", tok))
}

/// Builds a whole expression; nothing may follow it.
fn whole<'a>(circuit: &mut Circuit, parts: &[Part], env: &Env, toks: &[&'a str]) -> Result<'a, Wire> {
    let mut pos = 0;
    let w = expr(circuit, parts, env, toks, &mut pos, 0)?;
    match toks.get(pos) {
        None => Ok(w),
        Some(&extra) => Err(Error("unexpected", extra)),
    }
}

/// Instantiates a part: fresh gates for its body, with the parameters bound
/// to `args`. Errors carry no word: the body's text is about to be dropped.
fn expand(circuit: &mut Circuit, parts: &[Part], part: &Part, args: &[Wire], depth: usize) -> Result<'static, Wire> {
    if depth > MAX_DEPTH {
        return Err(Error("parts nest too deep in", ""));
    }
    let mut env: Env = part.params.iter().map(String::as_str).zip(args.iter().copied()).collect();
    let toks = tokens(&part.body);
    let mut result = None;
    for stmt in toks.split(|&t| t == ";") {
        let r = match stmt {
            [name, "=", rest @ ..] => whole(circuit, parts, &env, rest).map(|w| env.push((name, w))),
            _ => whole(circuit, parts, &env, stmt).map(|w| result = Some(w)),
        };
        r.map_err(|e| Error(e.0, ""))?;
    }
    result.ok_or(Error("no output expression in", ""))
}

impl Builder {
    fn wire(&self, name: &str) -> Option<Wire> {
        self.names.iter().find(|(n, _)| n == name).map(|&(_, w)| w)
    }

    fn name_of(&self, w: Wire) -> &str {
        self.names.iter().find(|&&(_, x)| x == w).map_or("?", |(n, _)| n.as_str())
    }

    fn bind<'a>(&mut self, name: &'a str, w: Wire) -> Result<'a, ()> {
        if !valid_name(name) || self.parts.iter().any(|p| p.name == name) {
            return Err(Error("not a usable name", name));
        }
        if self.wire(name).is_some() {
            return Err(Error("already defined", name));
        }
        self.names.push((String::from(name), w));
        Ok(())
    }

    fn define<'a>(&mut self, line: &'a str, toks: &[&'a str]) -> Result<'a, String> {
        let eq = toks.iter().position(|&t| t == "=").ok_or(Error("def needs =", ""))?;
        let (name, params) = match &toks[1..eq] {
            [name, params @ ..] => (*name, params),
            [] => return Err(Error("def needs a name", "")),
        };
        if !valid_name(name) || self.parts.iter().any(|p| p.name == name) || self.wire(name).is_some() {
            return Err(Error("not a usable name", name));
        }
        if let Some(&bad) = params.iter().find(|p| !valid_name(p)) {
            return Err(Error("not a usable name", bad));
        }
        let body = &line[line.find('=').unwrap() + 1..];
        self.parts.push(Part {
            name: String::from(name),
            params: params.iter().map(|&p| String::from(p)).collect(),
            body: String::from(body.trim()),
        });
        let mut msg = String::new();
        let _ = write!(msg, "part {}/{}", name, params.len());
        Ok(msg)
    }

    /// Values of `names` (all when empty), as `a=1 b=0 ...` lines.
    fn show(&mut self, names: &[&str], ui: &mut Ui) {
        if self.circuit.eval().is_err() {
            return ui.say(format_args!("unstable: the wires never settle"));
        }
        if self.names.is_empty() {
            return ui.say(format_args!("no wires yet (try: in a b)"));
        }
        let mut line = Line::new();
        let all: Vec<&str> = self.names.iter().map(|(n, _)| n.as_str()).collect();
        let list: &[&str] = if names.is_empty() { &all } else { names };
        for &n in list {
            let Some(w) = self.wire(n) else {
                return ui.say(format_args!("unknown name {}", n));
            };
            if line.len() + n.len() + 3 > Line::CAPACITY {
                ui.say(format_args!("{}", line.as_str()));
                line.clear();
            }
            let _ = write!(line, "{}={} ", n, self.circuit.get(w) as u8);
        }
        ui.say(format_args!("{}", line.as_str()));
    }

    fn table(&mut self, w: Wire, ui: &mut Ui) {
        let sources = self.circuit.sources(w);
        if sources.len() > TABLE_MAX {
            return ui.say(format_args!("depends on {} inputs, {} max", sources.len(), TABLE_MAX));
        }
        let mut head = Line::new();
        for &s in &sources {
            let _ = write!(head, "{} ", self.name_of(s));
        }
        ui.say(format_args!("{}| {}  ({} NAND in all)", head.as_str(), self.name_of(w), self.circuit.gate_count()));
        let saved: Vec<bool> = sources.iter().map(|&s| self.circuit.get(s)).collect();
        let n = sources.len();
        let mut line = Line::new();
        for combo in 0..1u32 << n {
            for (k, &s) in sources.iter().enumerate() {
                self.circuit.set(s, combo >> (n - 1 - k) & 1 == 1);
            }
            if self.circuit.eval().is_err() {
                return ui.say(format_args!("unstable for input {:b}", combo));
            }
            // Four combinations per line, so rows line up.
            if combo > 0 && combo % 4 == 0 {
                ui.say(format_args!("{}", line.as_str()));
                line.clear();
            }
            for k in 0..n {
                let _ = write!(line, "{}", combo >> (n - 1 - k) & 1);
            }
            let _ = write!(line, ">{} ", self.circuit.get(w) as u8);
        }
        ui.say(format_args!("{}", line.as_str()));
        for (&s, &v) in sources.iter().zip(&saved) {
            self.circuit.set(s, v);
        }
        let _ = self.circuit.eval();
    }

    /// Runs one command line.
    fn run<'a>(&mut self, line: &'a str, ui: &mut Ui) -> Result<'a, ()> {
        let toks = tokens(line);
        let before = self.circuit.gate_count();
        match toks.as_slice() {
            [] => {}
            ["help"] => HELP.iter().for_each(|h| ui.say(format_args!("{}", h))),
            ["demo"] => {
                for d in DEMO {
                    // Parts already defined are refused and kept as they are.
                    let _ = self.define(d, &tokens(d));
                }
                ui.say(format_args!("parts: not and or xor mux"));
            }
            ["clear"] => {
                *self = Builder::default();
                ui.say(format_args!("cleared, heap {} KiB free", heap::free_bytes() >> 10));
            }
            ["parts"] => {
                let mut l = Line::new();
                for p in &self.parts {
                    let _ = write!(l, "{}/{} ", p.name, p.params.len());
                }
                ui.say(format_args!("{}", if self.parts.is_empty() { "no parts (try demo)" } else { l.as_str() }));
            }
            ["def", ..] => {
                let msg = self.define(line, &toks)?;
                ui.say(format_args!("{}", msg));
            }
            ["in", names @ ..] if !names.is_empty() => {
                for &n in names {
                    let w = self.circuit.input();
                    self.bind(n, w)?;
                }
                ui.say(format_args!("{} input(s)", names.len()));
            }
            ["set", name, value @ ("0" | "1")] => {
                let w = self.wire(name).ok_or(Error("unknown name", name))?;
                if !self.circuit.sources(w).contains(&w) {
                    return Err(Error("not an input", name));
                }
                self.circuit.set(w, *value == "1");
                self.show(&[], ui);
            }
            ["show", names @ ..] => self.show(names, ui),
            ["table", name] => {
                let w = self.wire(name).ok_or(Error("unknown name", name))?;
                self.table(w, ui);
            }
            [name, "=", "wire"] => {
                let w = self.circuit.forward();
                self.bind(name, w)?;
                ui.say(format_args!("{} waits for <-", name));
            }
            [name, "<-", "nand", rest @ ..] => {
                let w = self.wire(name).ok_or(Error("unknown name", name))?;
                if !self.circuit.sources(w).contains(&w) {
                    return Err(Error("already driven", name));
                }
                let env = env_of(&self.names);
                let mut pos = 0;
                let a = expr(&mut self.circuit, &self.parts, &env, rest, &mut pos, 0)?;
                let b = expr(&mut self.circuit, &self.parts, &env, rest, &mut pos, 0)?;
                if let Some(&extra) = rest.get(pos) {
                    return Err(Error("unexpected", extra));
                }
                drop(env);
                self.circuit.drive(w, a, b);
                ui.say(format_args!("{} driven, {} NAND added", name, self.circuit.gate_count() - before));
            }
            [name, "=", rest @ ..] => {
                let env = env_of(&self.names);
                let w = whole(&mut self.circuit, &self.parts, &env, rest)?;
                drop(env);
                self.bind(name, w)?;
                let _ = self.circuit.eval();
                ui.say(format_args!(
                    "{}={}, {} NAND added",
                    name,
                    self.circuit.get(w) as u8,
                    self.circuit.gate_count() - before
                ));
            }
            [word, ..] => return Err(Error("unknown command", word)),
        }
        Ok(())
    }
}

pub fn run() -> ! {
    let mut ui = Ui { log: core::array::from_fn(|_| Line::new()), input: Line::new() };
    let mut b = Builder::default();
    ui.say(format_args!("u6 circuit builder: type help, Enter"));
    loop {
        ui.draw_input();
        let Some(key) = crate::wait(KEYS) else {
            ui.say(format_args!("no keyboard capability"));
            crate::exit(5);
        };
        match char::from_u32(key as u32).unwrap_or('?') {
            '\n' => {
                let mut line = Line::new();
                let _ = write!(line, "{}", ui.input.as_str());
                ui.input.clear();
                ui.say(format_args!("> {}", line.as_str()));
                if let Err(Error(msg, word)) = b.run(line.as_str(), &mut ui) {
                    ui.say(format_args!("error: {} {}", msg, word));
                }
            }
            '\x08' => ui.input.pop(),
            c if (' '..='~').contains(&c) && ui.input.len() < INPUT_MAX => {
                let _ = ui.input.write_char(c);
            }
            _ => {}
        }
    }
}

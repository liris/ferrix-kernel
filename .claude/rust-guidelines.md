# Rust guidelines for ferrix-kernel

Source: Microsoft Pragmatic Rust Guidelines, <https://microsoft.github.io/rust-guidelines/>
(agent version: <https://microsoft.github.io/rust-guidelines/agents/all.txt>). IDs below are
theirs, so a review finding can cite them. This file keeps the guidelines that apply to a
single-CPU `no_std` kernel and says how they apply here. Where a guideline is written for
libraries or services and conflicts with kernel reality, the "Here" note wins.

Use it two ways:

- **Writing code:** new and changed code follows every rule below.
- **Reviewing code:** check the diff against the checklist at the end. Report findings with the
  ID, `file:line`, and a concrete failure (inputs/state → wrong result). Existing code that
  predates this file is not a finding unless the diff touches it.

## Safety and soundness

**M-UNSOUND — all code must be sound.** A safe function must not be able to cause UB for any
argument, in any state. Here: a safe `fn` that takes a raw address and dereferences it is
unsound unless it validates the address first (as `mmu::user_readable` does for user pointers).
Otherwise make it `unsafe fn` with a `# Safety` section.

**M-UNSAFE — unsafe needs a reason.** The reasons that exist here are platform ones: MMIO,
system registers, `asm!`, page tables, DMA memory shared with a device, `GlobalAlloc`, and the
`Global` wrappers. Not valid: `transmute` to shorten safe code, `unsafe impl Send/Sync` on
something that isn't actually protected, bypassing lifetimes.

- Every `unsafe { }` block gets a `// SAFETY:` comment saying why the preconditions hold
  (as in `main.rs` around `exit_boot_services`).
- Every `unsafe fn` gets a `# Safety` doc section listing the caller's obligations.
- Keep unsafe blocks small and wrap them in a safe function with a narrow API, so callers
  don't need `unsafe` (e.g. one `read32`/`write32` per register block, not volatile
  pointer arithmetic at call sites).
- `unsafe impl Sync for Global` is sound only because the state is reached through
  `arch::without_irqs` on one CPU. Any new `Global` follows the same pattern, accessed through
  one closure-taking function (like `mm::pages` and `task::with`). Say so in the SAFETY comment.
  Adding a second CPU invalidates all of these; that is a design change, not a local fix.

**M-UNSAFE-IMPLIES-UB.** Mark a function `unsafe` only when misuse can cause UB, not because it
is dangerous in some other way (resetting a controller, killing a task).

## Panics vs. errors

**M-PANIC-IS-STOP / M-PANIC-ON-BUG.** A panic stops the kernel and shows the panic screen. Use
it for kernel bugs and broken invariants: out of memory during boot, scheduler used before
init, a circuit that doesn't settle.

Here, these are **inputs, not bugs**, and must not panic:

- anything a user program passes in a syscall (bad cap slot, bad pointer, bad length) → status
  code;
- a user program faulting → kill that task;
- hardware or firmware behaving unexpectedly (a device that doesn't answer, a missing ACPI
  table, a USB descriptor that doesn't parse) → `Result` / `Option`, report on screen, carry
  on without that device.

Every busy-wait on hardware has a bound (iteration count or timer) and returns an error when
it runs out.

**M-PANIC-MESSAGE.** `expect`/`panic!` messages say what was expected and include the values
that matter (`"xHCI: no event for slot {slot}"`), because the screen is the only debugger.

## Errors

**M-ERRORS-CANONICAL-STRUCTS**, adapted: no `std::error::Error`, backtraces or error crates here.
A small `enum` per module, or `&'static str` for errors that only end up on screen, is fine.
Don't collapse distinct failures the caller would handle differently into one variant or
string.

## Types and APIs

**M-STRONG-TYPES / M-STRONG-TYPES-GUARD.** Prefer a newtype when a raw integer has an invariant
or is easy to mix up: cap slots, ASIDs, slot/endpoint IDs, physical vs. virtual addresses at
module boundaries. A newtype's constructor checks the invariant. Don't retrofit existing code
just for this.

**M-RUST-SHAPED.** Write Rust, not C with Rust syntax: enums with data for states and TRB
kinds, `Option` instead of sentinel values, iterators over index loops where clearer,
`#[repr(C)]` structs over manual offset arithmetic for in-memory layouts shared with hardware.
Register offsets in MMIO blocks stay named constants.

**M-REGULAR-FN.** Logic that doesn't need `self` is a free function in the module, not an
associated function.

**M-SHORT-NAMES / M-WEASEL-WORDS.** Short names that don't repeat the module (`xhci::Ring`, not
`xhci::XhciRing`). No `Manager`, `Service`, `Factory`, `Helper`.

**M-SINGLE-ITEM-PATH.** An item is reachable through one path; no re-export shortcuts.

**M-PUBLIC-DEBUG.** `pub` types in `nand` derive or implement `Debug`. Inside the kernel, add
`Debug` where it helps print state on screen.

## Magic values and documentation

**M-DOCUMENTED-MAGIC.** Every register offset, bit field, mask, address and size is a named
constant with a doc comment, citing the spec where one exists (xHCI section, ARM ARM register
name, ACPI table, USB HID). Values learned from VMware or QEMU behaviour say which and how it
was found.

**M-MODULE-DOCS.** Every module starts with `//!` docs explaining the hardware or mechanism it
drives and the decisions behind it (see `src/xhci.rs`). Keep these current when behaviour
changes.

**M-CANONICAL-DOCS / M-FIRST-DOC-SENTENCE.** Doc comments start with one short summary line
(about 15 words). Add `# Safety` on unsafe functions and `# Panics` where a public function can
panic for a reason the caller controls.

**M-NO-META-DESIGN-DOCUMENTATION.** Comments and docs describe how the code is now, not how it
got there ("previously…", "changed to…"). History lives in commit messages and `DESIGN.md`'s
roadmap.

## Lints and verification

**M-STATIC-VERIFICATION.** `cargo build --release` has zero warnings. New code adds no
`cargo clippy --release` warnings (the existing ones are known debt; fix them when touching that
code). Miri doesn't run on this target; `nand` logic is host-testable instead.

**M-LINT-OVERRIDE-EXPECT.** Silence a lint with `#[expect(lint, reason = "...")]`, never
`#[allow]`.

**M-TAUTOLOGICAL-TESTS.** Tests check against independently known truth (truth tables, exhaustive
input sweeps, spec examples), not against a re-run of the same code. For hardware-facing code,
the test is booting on QEMU and VMware and reading the screen.

## Kernel performance

**M-THROUGHPUT / M-YIELD-POINTS**, adapted: the interrupt and timer paths are the hot path.
No heap allocation, no unbounded loops and no screen redraws beyond what changed inside an IRQ
handler. Long-running kernel tasks yield (`task::sleep` or blocking on a channel) instead of
spinning on a condition another task will set.

**M-MEM-REUSE / M-INITIAL-CAPACITY.** The heap is small and fixed. Allocate at init, reuse
buffers, size collections up front.

## Not applicable here

FFI crate naming and DLL state, proc macros, async, mimalloc, `target-cpu`, anyhow, services
being `Clone`, builders, mockable syscalls, telemetry/logging crates, `M-AVOID-STATICS` (a
kernel needs global state; see the `Global` rule above), `M-OOBE` and `-sys` crates.

## Review checklist

1. Any new `unsafe`: valid reason? `// SAFETY:` present and true? Could a safe wrapper hide it?
   (M-UNSAFE, M-UNSOUND, M-UNSAFE-IMPLIES-UB)
2. Any safe function that can be driven into UB by its arguments or by a user program?
   (M-UNSOUND)
3. User memory touched from EL1 without `user_readable` + `read_user_byte`? (PAN: crashes on
   VMware only.)
4. Global state accessed outside `without_irqs`, or held across a point that can switch tasks?
5. Panic on user, device or firmware input instead of an error? Unbounded hardware wait?
   (M-PANIC-ON-BUG)
6. Unnamed magic numbers or hardcoded device addresses? (M-DOCUMENTED-MAGIC)
7. Allocation or unbounded work in an IRQ path? (M-THROUGHPUT)
8. Names, free functions, Rust-shaped types? (M-SHORT-NAMES, M-REGULAR-FN, M-RUST-SHAPED)
9. Docs: module `//!` still accurate, summary lines short, `# Safety` / `# Panics` present,
   no history in comments. (M-MODULE-DOCS, M-CANONICAL-DOCS, M-NO-META-DESIGN-DOCUMENTATION)
10. Build warnings, new clippy warnings, `#[allow]`? (M-STATIC-VERIFICATION,
    M-LINT-OVERRIDE-EXPECT)
11. On-screen lines over 48 columns?

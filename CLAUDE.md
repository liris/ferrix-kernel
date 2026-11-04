# ferrix-kernel

AArch64 kernel in Rust, booting from UEFI on VMware Fusion (Apple Silicon) and QEMU. End goal:
Tetris, as in Nand2Tetris. `DESIGN.md` has the roadmap and design; `README.md` has commands and
keys. `user/` (EL0 programs) is built by the kernel's `build.rs` and embedded; `nand/` is shared.

## Working with the user

- Talk in Japanese; code, comments, commits and docs are English.
- Propose a short plan before each milestone and wait for the go-ahead.
- One commit per milestone (`M10: ...`), pushed to `main`, with `DESIGN.md`/`README.md` and
  their `*.ja.md` updated in the same commit.

## Verifying

1. `make build`: zero warnings. `cargo clippy --release`: no new warnings.
2. QEMU: `make run-headless`, drive via `build/mon.sock` (`sendkey`, `screendump`). Every
   screen line must fit 48 columns.
3. VMware: the user runs `make vm-run` and sends a screenshot. Done only after VMware.

`nand` tests: `cd nand && cargo test --target aarch64-apple-darwin` (default target is UEFI).

## Things that bite

- VMware has no serial; diagnostics go on screen.
- VMware's CPU has PAN on, QEMU's Cortex-A72 doesn't: read user memory only via
  `mmu::user_readable` + `arch::read_user_byte` (`ldtrb`).
- Never hardcode device addresses; find them through ACPI (MADT, MCFG) and PCI. Regions found
  after the MMU is on go in through `mmu::map_live`.
- Single CPU: `Global` state is sound only because it's touched inside `arch::without_irqs`.
- xHCI: read capability registers as 32-bit dwords; write 64-bit registers low half first.
- A user fault or bad syscall argument kills the task or returns a status, never panics.

## Code style

Follow `.claude/rust-guidelines.md` (Microsoft Pragmatic Rust Guidelines adapted for this
kernel) when writing and reviewing code. Match surrounding code; the tree isn't
`rustfmt`-clean, so don't reformat files wholesale.

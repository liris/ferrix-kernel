# ferrix-kernel design

English | [日本語](DESIGN.ja.md)

2026-11-03

## Overview and goals

ferrix-kernel is a small ARM64 kernel written in Rust that boots in VMware Fusion on an Apple Silicon Mac. It starts as a UEFI application and, once boot services have ended, runs on its own code only.

**Goals**

- Boot from an ISO in a VMware Fusion ARM64 VM and draw to the screen
- After boot, rely on no firmware: own exception handling, memory management and timers
- Write everything in Rust, confining `unsafe` to the layer that touches hardware
- Run the same binary in QEMU for fast local iteration

**Non-goals (for now)**

- Booting on real hardware (the Mac itself)
- x86_64 support
- POSIX compatibility or running existing applications
- Networking, file systems, GUI

## Target environment

The host is arm64, so the guest is fixed to AArch64 + UEFI (VMware Fusion on Apple Silicon cannot run x86 guests).

| Item | Production (VMware) | Testing (QEMU) |
| --- | --- | --- |
| Host | macOS 27.2 / arm64 | Same |
| Hypervisor | VMware Fusion | `qemu-system-aarch64 -M virt` |
| Firmware | VMware's built-in UEFI | edk2 `edk2-aarch64-code.fd` |
| Boot media | ISO (El Torito EFI) | Same ISO |
| Display | UEFI GOP framebuffer | GOP on `ramfb` |
| Serial | Not used (see risks) | PL011 UART (`-serial stdio`) |

Values measured in both environments at M1 (2026-11-03):

| Item | VMware Fusion | QEMU virt |
| --- | --- | --- |
| Display | 1024x768, stride 1024 | 800x600, stride 800 |
| Exception level | EL1 | EL1 |
| MIDR_EL1 | `0x610f0000` (Apple, part 0 = model hidden) | `0x410fd083` (Arm Cortex-A72) |
| Generic Timer | 24 MHz | 62.5 MHz |
| ACPI RSDP | `0x9c060000` | `0x5cb43018` |
| Memory | 32 regions, 512 MiB, 484 MiB usable | 33 regions, 576 MiB, 478 MiB usable |
| GIC (from the MADT, M2) | GICv3, GICD `0x2c010000`, GICR `0x2c100000` (`0x20000` = one CPU), no ITS | GICv2, GICD `0x8000000` (v3 with `-M virt,gic-version=3`) |
| PCIe ECAM (from the MCFG, M5) | `0x40000000`, buses 0-255 | `0x4010000000`, buses 0-255 |
| xHCI (M5) | `15ad:077a` at `01:00.0` (behind a root port), xHCI 1.2, 127 slots, 8 ports | `1b36:000d` at `00:02.0`, xHCI 1.0, 64 slots, 8 ports |
| USB devices on root ports (M5) | 5: `0e0f:0003` HID (pointer), 6: `0e0f:0006` keyboard (full speed), 7/8: `0e0f:0002` hubs | 5: `0627:0001` keyboard (high speed) |

Timer frequency and screen size differ between the environments, which confirms the decision to read both at run time rather than hard-code them.

Toolchain: rustc 1.99, targets `aarch64-unknown-uefi` (kernel) and `aarch64-unknown-none` (user programs), the `uefi` crate 0.41, and xorriso plus mtools to build the ISO.

## Architecture and boot flow

`ExitBootServices` is where control passes from the firmware to ferrix-kernel.

```
┌─ UEFI phase (boot services available) ─────────────────────────────────┐
│ firmware ─────▶ efi_main ────▶ collect info ──────▶ ExitBootServices   │
│ loads           uefi crate     GOP framebuffer      services end       │
│ BOOTAA64.EFI    init           memory map, ACPI           │            │
└───────────────────────────────────────────────────────────┼────────────┘
                     no way back after boot services end    │
┌─ Kernel phase (our code only) ────────────────────────────▼────────────┐
│ console ──▶ exception ──▶ memory ─────▶ GIC + timer ──▶ tasks          │
│ 16x16 text  vectors       pages, heap   interrupts      preemptive     │
│             VBAR_EL1      page tables                   round-robin    │
└────────────────────────────────────────────────────────────────────────┘
```

The upper half gathers what it needs while UEFI services are still available; the lower half initializes everything itself from that information alone.

## Kernel components

Each component works only from what the firmware handed over (framebuffer, memory map, ACPI tables) and hard-codes no device addresses. This is how the differences between VMware and QEMU are absorbed.

| Module | Role | Approach |
| --- | --- | --- |
| `main` | UEFI entry, information gathering, `ExitBootServices` | Uses the `uefi` crate. Collected data goes into a `BootInfo` passed to the kernel |
| `console` | Text on the framebuffer | The 8x8 font8x8 glyphs scaled 2x to 16x16. Implements `core::fmt::Write` and provides `println!` |
| `exception` | Exception vectors (EL1) | A 16-entry vector table in `global_asm!`, installed in `VBAR_EL1`. The frame also saves SP_EL0. From EL0, `svc` is a system call and any other synchronous exception kills the task; in the kernel, a panic screen showing ESR/FAR |
| `mm` | Physical pages and heap | A bitmap allocator of 4 KiB pages over the free regions in the UEFI memory map. `GlobalAlloc` enables `alloc` |
| `arch` | System registers and time | ARM Generic Timer. Uptime is counted with 100 Hz interrupts from the EL1 virtual timer (PPI 27); without GICv3 it falls back to polling |
| `gic` / `irq` | Interrupt controller and dispatch | GICD/GICR come from the ACPI MADT; GICv3 only (QEMU is run with `gic-version=3` to match). The IRQ handler re-arms the timer, counts the tick, then lets the scheduler switch tasks. The exception entry also saves FP/SIMD registers |
| `task` | Preemptive tasks | Each task runs at EL1 on its own 64 KiB stack. The `TrapFrame` the exception entry pushes is the task's saved state: when the handler returns another task's frame, the exit path switches to that stack and `eret`s. Switches happen on the timer interrupt (every 10 ms) and on `svc #0` (`yield_now` / `sleep`), round-robin; an idle task runs `wfi` when nothing is ready. A user task's frame returns to EL0, and the switch loads its TTBR0 (address space + ASID). The console, heap, page allocator and task table are accessed with IRQs masked, which is the lock on one CPU |
| `mmu` | Our own page tables | Identity map with a 4 KiB granule and 48-bit VA. 1 GiB / 2 MiB blocks where possible, 4 KiB pages for the rest. The first mapping of an address wins, so GIC → framebuffer → MMIO → RAM are mapped in that order. Regions found later (PCIe ECAM, BARs) are added to the live tables with `map_live`; it only fills invalid entries, so no TLB invalidation is needed. Each user program gets an `AddressSpace`: a root table copied from the kernel's (sharing every table below it) plus its own level 0 entry for `0x4000_0000_0000`-`0x4080_0000_0000`. User pages are non-global with their ASID, kernel pages global, so switching spaces needs no TLB flush |
| `pci` | PCIe configuration space | ECAM base from the ACPI MCFG. A flat scan over the bus range finds functions behind bridges too, because the firmware already numbered the buses and assigned BARs. Config space and BARs are mapped as device memory on first use |
| `xhci` | USB host controller | Taken over from the firmware (legacy handoff), halted and reset. Command, event and transfer rings in identity-mapped RAM; the event ring is polled, no interrupts. Root port reset, Enable Slot / Address Device / Evaluate Context / Configure Endpoint, control transfers and interrupt IN transfers |
| `usb` / `keyboard` | Devices and the keyboard | Every connected root port is enumerated and its device named from its descriptors. The first HID boot keyboard is switched to the boot protocol; a `kbd` task keeps one 8-byte report request queued and checks it every tick. `keyboard` turns newly pressed keys into characters (US layout) and queues them for `main`, which hands them to `lab`. Hubs are not supported yet |
| `lab` | NAND circuits driven from the keyboard | The 4-bit counter (clocked by the timer, or by hand with the space bar) and the 4-bit adder (keys toggle its input bits) at the bottom of the screen. Every change re-evaluates the gates and checks the outputs against arithmetic. Pure logic; `main` draws and reads keys |
| `user` | User programs | `user/` is a separate `no_std` crate for `aarch64-unknown-none`, linked at `0x4000_0000_0000`; `build.rs` builds it and the kernel embeds the ELF. Each instance's `PT_LOAD` segments are copied into fresh pages (code R+X, read-only data R, data RW) with a 16 KiB stack. System calls (`svc #0`, number in x8): `exit`, `write` (replace the program's line on screen), `sleep`. User pointers are checked with `at s1e0r` and read with `ldtrb`, so PAN can stay on |
| `nand` | Circuit simulator built from NAND gates | A circuit is a `Vec` of gates over numbered wires, and only NAND is evaluated. Depends on `alloc` from M2 and touches no hardware (no `unsafe`) |

Memory stays identity mapped (virtual = physical), but at M3b the kernel switched from UEFI's page tables to its own, mapping only the regions it needs, each with its own attributes.

| Region | Attributes | Executable |
| --- | --- | --- |
| `LOADER_CODE` (the kernel's whole PE image), runtime services code | Normal memory (write-back cacheable) | Yes |
| Other RAM (free, stacks, ACPI, runtime data) | Normal memory (write-back cacheable) | No |
| GICD / GICR, MMIO from the memory map, PCIe ECAM and BARs | Device-nGnRE | No |
| Framebuffer | Normal memory (non-cacheable) | No |
| User code / read-only data / data and stack (per address space, M7) | Normal memory, EL0 accessible, not global | Code: EL0 only |
| Page 0 and everything else | Unmapped (access faults) | — |

Kernel mappings are EL1-only, so EL0 faults on any kernel address even though it is mapped. `LOADER_CODE` also holds the kernel's data sections, so for now that region is both writable and executable. Reading the PE section table to give code and data separate attributes (W^X) is future work.

## Build and deploy

`make iso` builds an ISO ready for VMware, and `make run` boots the same ISO in QEMU.

1. `cargo build --release` produces `ferrix-kernel.efi` (target `aarch64-unknown-uefi` from `.cargo/config.toml`); its `build.rs` first builds the user program in `user/` for `aarch64-unknown-none` and the kernel embeds it
2. mtools builds a FAT image with it at `EFI/BOOT/BOOTAA64.EFI` (the UEFI default boot path)
3. xorriso wraps the FAT image as an El Torito EFI boot entry in `ferrix-kernel.iso`
4. Testing: `make run` (QEMU virt + edk2 + ramfb)
5. Production: `make vm-run` assembles a VMware Fusion VM ("Other 64-bit ARM") from `vmware/ferrix-kernel.vmx` with the ISO in its CD/DVD drive, and cold-boots it

512 MB of memory, 1 vCPU and no disk are enough for the VM.

## Roadmap

A milestone is done when it visibly works on VMware.

- [x] **M0 Boot**: boot from UEFI and show "ferrix-kernel" on the GOP screen (verified on QEMU and VMware)
- [x] **M1 Self-hosted**: after `ExitBootServices`, show the memory map and CPU information on our own console (QEMU: EL1, Cortex-A72, 576 MiB, uptime updating)
- [x] **M2 Exceptions and memory**: catch `brk` in the exception vectors and resume; the page allocator and `Vec` work (verified on QEMU and VMware)
- [x] **M2.5 NAND circuits (combinational)**: build NOT / AND / OR / XOR → half adder → full adder → 4-bit adder from NAND alone, and show each stage's truth table and sums such as `3 + 5 = 8` (verified on QEMU and VMware. NAND counts: NOT 1 / AND 2 / OR 3 / XOR 4 / half adder 5 / full adder 9 / 4-bit adder 36. All 512 inputs are checked at boot)
- [x] **M3 Interrupts**: keep the uptime on screen updated with the GIC and timer interrupts, and move to our own page tables. As a demo, clock an SR latch → D flip-flop → 4-bit counter built from NAND with the timer interrupt and show the value counting up
    - [x] **M3a**: GICv3 + virtual timer interrupt, NAND counter (53 NAND, clock toggles every 0.5 s, +1 checked on every rising edge). Verified on QEMU and VMware
    - [x] **M3b**: move to our own page tables (verified on QEMU and VMware. After the switch, the `at` instruction confirms page 0 is unmapped and kernel code is identity mapped. `make run FEATURES=fault-test` dereferences a null pointer and shows the panic screen)
- [x] **M4 Tasks**: switch between two or more kernel tasks preemptively (verified on QEMU and VMware. Five tasks: main / idle / spin / nand / fpu. The workers are infinite loops that never yield, so main getting the CPU back at all proves preemption. nand re-checks all 512 inputs of the 4-bit adder and fpu demands a bit-identical floating point sum, round after round, confirming that switches preserve registers. The bottom of the screen shows each task's CPU share and progress)

- [x] **M5 Keyboard input**: type on a USB keyboard through our own xHCI driver and see the characters echoed on the bottom line while the demos keep running (verified on QEMU and VMware)
    - [x] **M5a**: find the xHCI controller over PCIe (ACPI MCFG → ECAM) and take it over from the firmware, then reset it
    - [x] **M5b**: enumerate the devices on the root ports and configure the boot protocol keyboard (VMware: keyboard, pointer and two hubs on ports 5-8; QEMU: keyboard on port 5)
    - [x] **M5c**: poll interrupt IN reports from a `kbd` task and turn them into characters; Enter clears the line, Backspace deletes
- [x] **M6 Interactive NAND circuit**: drive NAND circuits from the keyboard (verified on QEMU and VMware). Keys 1-4 / 5-8 / c toggle the 4-bit adder's a / b / carry inputs and show the sum; space steps the counter's clock by hand (manual mode), m switches back to the timer. The counter is still checked on every edge and the adder on every change. Status lines are now cut to the screen width so a long line can no longer wrap onto the next
- [x] **M7 User space**: run programs at EL0, each in its own address space (verified on QEMU and VMware). One embedded ELF is loaded three times: `u1` counts and sleeps, `u2` spins without ever yielding (preempted at EL0), `u3` reads a kernel variable and is killed with a permission fault while everything else keeps running. `u1` and `u2` show the same address for their counter with unrelated values, so the spaces are separate. On VMware, PAN was found to be on (an EL1 read of the user stack faulted), so the kernel reads user memory with `ldtrb`
- [ ] **M8 Capabilities**

## Risks and open questions

The biggest risk is that VMware's ARM virtual hardware is not publicly documented, so working on QEMU does not guarantee working on VMware.

| Risk / open question | Impact | Mitigation |
| --- | --- | --- |
| ~~VMware's GIC type and addresses unknown~~ Resolved: GICv3 | — | Read from the MADT at M2. QEMU also runs with `gic-version=3` to match |
| Unknown whether VMware has a serial port | No debug output besides the screen | The screen console is primary. A `serial0` file output in the `.vmx` produced nothing and raised a "replace or append?" dialog on every boot, so it was removed |
| ~~No keyboard input after `ExitBootServices`~~ Resolved: USB keyboard behind xHCI | — | M5: own xHCI and HID boot keyboard driver, the same on VMware and QEMU |
| Devices behind a USB hub are not seen | A keyboard plugged into a hub would not work | VMware puts its keyboard on a root port, so not needed yet; hub support when something needs it |
| CPU features differ between environments (VMware's Apple CPU has PAN on; QEMU's Cortex-A72 is ARMv8.0) | Code that works on QEMU faults on VMware | Do not depend on optional features; access user memory with unprivileged loads, which work either way |
| Framebuffer pixel format differs between environments | Wrong colours | Switch between RGB and BGR based on GOP's `PixelFormat` |
| Iterating on VMware is slow | Slower development | Test on QEMU day to day; check on VMware at each milestone |

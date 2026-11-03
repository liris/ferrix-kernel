# ferrix-kernel

English | [日本語](README.ja.md)

A tiny AArch64 kernel written in Rust, booting from UEFI on VMware Fusion (Apple Silicon) and QEMU.

It starts as a UEFI application, collects the framebuffer, memory map and ACPI pointer, calls
`ExitBootServices`, and then runs on its own code only: exception vectors, a page allocator and
heap, its own page tables, GICv3 timer interrupts, preemptive round-robin tasks, a USB keyboard
through its own xHCI driver, and user programs at EL0 in their own address spaces that can only use
the capabilities they were given. Alongside, a circuit simulator whose only primitive is NAND
builds adders and a counter, which can be driven from the keyboard.

Excerpt of a boot on VMware Fusion:

```
ferrix-kernel 0.1.0 (aarch64)
[boot] framebuffer 1024x768 stride=1024
[ ok ] ExitBootServices: now running on our own
[cpu ] EL1  MIDR=0x610f0000 (Apple)  part=0x000
[mem ] 31 regions, 512 MiB total, 484 MiB usable
[ ok ] M1: kernel is self-hosted
[ ok ] M2: exceptions and memory
[mmu ] TTBR0 0x9ffdf000 -> 0x80380000 (ours)
[ ok ] M3b: own page tables, null page unmapped
[nand]  3 +  5 =  8  (0011 + 0101 = 01000)
[ ok ] M2.5: 4-bit adder 512/512 (36 NAND)
[ ok ] M3: virtual timer IRQ at 100 Hz
[ ok ] M4: 5 tasks, round-robin, 10 ms slices
[pci ] 01:00.0 xHCI 15ad:077a
[usb ] port 6 full 0e0f:0006 keyboard
[ ok ] M5: USB keyboard ready
[user] 5 programs at EL0, 16 KiB ELF
[ ok ] M7: user space, 5 address spaces
[ ok ] M8: 7 capabilities, 1 channel
[lab ] counter 53 NAND, adder 36 NAND

uptime 00:04:12
ctr clk=0 q=1011 (11) edges=252 auto
add a=0011 b=0101 c=0 -> 01000 (3+5+0=8)
cpu% spin 25 nand 25 fpu 25 u2 24
spin 26309M  nand 1627404  fpu 125926k  sw 43357
u1 count n=2101 &n=0x400000003000
u2 spin  n=19561M &n=0x400000003000
u3 sink  2101 msgs, last n=2101
u4 forge 2024/2024 other slots denied
u5 killed: read 0x9bdd5348 no permission
keys 1-4:a 5-8:b c:carry spc:clk m:mode
```

## Requirements

- Rust with the `aarch64-unknown-uefi` and `aarch64-unknown-none` targets:
  `rustup target add aarch64-unknown-uefi aarch64-unknown-none`
- `brew install qemu xorriso mtools`
- VMware Fusion (optional)

## Usage

| Command | What it does |
| --- | --- |
| `make iso` | Build `build/ferrix-kernel.iso` (El Torito EFI, `EFI/BOOT/BOOTAA64.EFI`) |
| `make run` | Boot the ISO in QEMU (`virt` + edk2 + ramfb) |
| `make vm` | Assemble a VMware Fusion VM in `build/ferrix-kernel.vmwarevm` |
| `make vm-run` | Assemble that VM and cold-boot it with the fresh ISO |
| `make run FEATURES=fault-test` | Boot with a deliberate null dereference to show the panic screen |

## Keys

Once `M5: USB keyboard ready` appears, the NAND circuits at the bottom of the screen follow the
keyboard (US layout):

| Key | Effect |
| --- | --- |
| `1`-`4` / `5`-`8` | Toggle a bit of the adder's input `a` / `b`, most significant first |
| `c` | Toggle the adder's carry in |
| Space | Step the counter's clock by hand (switches to manual mode) |
| `m` | Switch the counter between manual and timer clock |

## Design

See [DESIGN.md](DESIGN.md) for goals, boot flow, components and the roadmap.

Documents are written in English; Japanese translations sit next to them as `*.ja.md`.

## License

MIT. See [LICENSE](LICENSE).

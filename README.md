# ferrix-kernel

A tiny AArch64 kernel written in Rust, booting from UEFI on VMware Fusion (Apple Silicon) and QEMU.

It starts as a UEFI application, collects the framebuffer, memory map and ACPI pointer, calls
`ExitBootServices`, and then runs on its own code only.

```
ferrix-kernel 0.1.0 (aarch64)
[boot] framebuffer 1024x768 stride=1024
[ ok ] ExitBootServices: now running on our own
[cpu ] EL1  MIDR=0x610f0000 (Apple)  part=0x000
[time] generic timer 24000000 Hz
[acpi] RSDP at 0x9c060000
[mem ] 32 regions, 512 MiB total, 484 MiB usable
[ ok ] M1: kernel is self-hosted
uptime 00:12:55
```

## Requirements

- Rust with the `aarch64-unknown-uefi` target: `rustup target add aarch64-unknown-uefi`
- `brew install qemu xorriso mtools`
- VMware Fusion (optional)

## Usage

| Command | What it does |
| --- | --- |
| `make iso` | Build `build/ferrix-kernel.iso` (El Torito EFI, `EFI/BOOT/BOOTAA64.EFI`) |
| `make run` | Boot the ISO in QEMU (`virt` + edk2 + ramfb) |
| `make vm` | Assemble a VMware Fusion VM in `build/ferrix-kernel.vmwarevm` |
| `make vm-run` | Assemble and start that VM |

## Design

See [DESIGN.md](DESIGN.md) for goals, boot flow, components and the roadmap.

## License

MIT. See [LICENSE](LICENSE).

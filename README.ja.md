# ferrix-kernel

[English](README.md) | 日本語

Rust で書いた小さな AArch64 カーネルです。UEFI から起動し、VMware Fusion（Apple Silicon）と QEMU で動きます。

UEFI アプリとして起動してフレームバッファ・メモリマップ・ACPI のポインタを集め、`ExitBootServices` を呼んだあとは自前のコードだけで動きます。例外ベクタ、ページアロケータとヒープ、自前のページテーブル、GICv3 のタイマー割り込み、プリエンプティブなラウンドロビンのタスクを備えています。あわせて、NAND だけを部品とする回路シミュレータで加算器と、タイマーをクロックにしたカウンタを組んでいます。

VMware Fusion での起動画面（抜粋）:

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
[ctr ] 4-bit counter, 53 NAND, timer-clocked

uptime 00:00:19
clk=1  q=0011 ( 3)  edges=20
cpu  main 0% idle 0% spin 33% nand 33% fpu 33%
spin 2700804414  nand 159419  fpu 12738948  sw 2594
```

## 必要なもの

- `aarch64-unknown-uefi` ターゲット入りの Rust: `rustup target add aarch64-unknown-uefi`
- `brew install qemu xorriso mtools`
- VMware Fusion（任意）

## 使い方

| コマンド | 内容 |
| --- | --- |
| `make iso` | `build/ferrix-kernel.iso` を作る（El Torito EFI、`EFI/BOOT/BOOTAA64.EFI`） |
| `make run` | ISO を QEMU で起動する（`virt` + edk2 + ramfb） |
| `make vm` | VMware Fusion の VM を `build/ferrix-kernel.vmwarevm` に組み立てる |
| `make vm-run` | その VM を組み立て、新しい ISO でコールドブートする |
| `make run FEATURES=fault-test` | わざとヌルポインタを参照してパニック画面を出す |

## 設計

目標、起動フロー、構成要素、ロードマップは [DESIGN.ja.md](DESIGN.ja.md)（英語版: [DESIGN.md](DESIGN.md)）にあります。

ドキュメントは英語版が正本で、日本語訳を `*.ja.md` として隣に置いています。

## ライセンス

MIT。[LICENSE](LICENSE) を参照してください。

# ferrix-kernel

[English](README.md) | 日本語

Rust で書いた小さな AArch64 カーネルです。UEFI から起動し、VMware Fusion（Apple Silicon）と QEMU で動きます。

UEFI アプリとして起動してフレームバッファ・メモリマップ・ACPI のポインタを集め、`ExitBootServices` を呼んだあとは自前のコードだけで動きます。例外ベクタ、ページアロケータとヒープ、自前のページテーブル、GICv3 のタイマー割り込み、プリエンプティブなラウンドロビンのタスク、自前の xHCI ドライバによる USB キーボード入力、専用のアドレス空間で EL0 で動き、渡された capability しか使えないユーザープログラムを備えています。あわせて、NAND だけを部品とする回路シミュレータで加算器とカウンタを組み、キーボードから操作できます。

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

## 必要なもの

- `aarch64-unknown-uefi` と `aarch64-unknown-none` ターゲット入りの Rust:
  `rustup target add aarch64-unknown-uefi aarch64-unknown-none`
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

## キー操作

`M5: USB keyboard ready` が出たあとは、画面下部の NAND 回路をキーボード（US 配列）で操作できます。

| キー | 動作 |
| --- | --- |
| `1`-`4` / `5`-`8` | 加算器の入力 `a` / `b` のビットを反転（左が上位ビット） |
| `c` | 加算器のキャリー入力を反転 |
| スペース | カウンタのクロックを手で送る（手動モードになる） |
| `m` | カウンタのクロックを手動とタイマーで切り替える |

## 設計

目標、起動フロー、構成要素、ロードマップは [DESIGN.ja.md](DESIGN.ja.md)（英語版: [DESIGN.md](DESIGN.md)）にあります。

ドキュメントは英語版が正本で、日本語訳を `*.ja.md` として隣に置いています。

## ライセンス

MIT。[LICENSE](LICENSE) を参照してください。

# ferrix-kernel 設計ドキュメント

2026-11-03

## 概要と目標

ferrix-kernel は、Apple Silicon Mac 上の VMware Fusion で起動する、Rust 製の小さな ARM64 カーネルです。UEFI アプリとして起動し、ブートサービス終了後は自前のコードだけで動きます。

**目標**

- VMware Fusion の ARM64 VM で、ISO から起動して画面に出力する
- 起動後はファームウェアに頼らず、例外処理・メモリ管理・タイマーを自前で持つ
- 全コードを Rust で書き、`unsafe` はハードウェアに触れる層に閉じ込める
- 同じバイナリを QEMU でも動かし、手元で素早く検証できるようにする

**非目標（当面）**

- 実機（Mac 本体）での起動
- x86_64 対応
- POSIX 互換や既存アプリの実行
- ネットワーク、ファイルシステム、GUI

## ターゲット環境

ホストが arm64 なので、ゲストは AArch64 + UEFI に固定します（Apple Silicon 版 VMware Fusion は x86 ゲストを動かせません）。

| 項目 | 本番（VMware） | 検証（QEMU） |
| --- | --- | --- |
| ホスト | macOS 27.2 / arm64 | 同左 |
| ハイパーバイザ | VMware Fusion | `qemu-system-aarch64 -M virt` |
| ファームウェア | VMware 内蔵 UEFI | edk2 `edk2-aarch64-code.fd` |
| ブート媒体 | ISO（El Torito EFI） | 同じ ISO |
| 画面 | UEFI GOP フレームバッファ | `ramfb` の GOP |
| シリアル | 未確認（リスク参照） | PL011 UART（`-serial stdio`） |

M1 で両環境から読み取った実測値（2026-11-03）:

| 項目 | VMware Fusion | QEMU virt |
| --- | --- | --- |
| 画面 | 1024x768, stride 1024 | 800x600, stride 800 |
| 例外レベル | EL1 | EL1 |
| MIDR_EL1 | `0x610f0000`（Apple, part 0 = 型番を隠している） | `0x410fd083`（Arm Cortex-A72） |
| Generic Timer | 24 MHz | 62.5 MHz |
| ACPI RSDP | `0x9c060000` | `0x5cb43018` |
| メモリ | 32 領域, 512 MiB, 使用可能 484 MiB | 33 領域, 576 MiB, 使用可能 478 MiB |

タイマー周波数と画面サイズが環境で異なるため、どちらもハードコードせず実行時に読む方針で正しいことを確認しました。

ツールチェーンは rustc 1.99、ターゲット `aarch64-unknown-uefi`、`uefi` crate 0.41、ISO 作成に xorriso と mtools を使います。

## 全体アーキテクチャと起動フロー

ExitBootServices を境に、制御がファームウェアから ferrix-kernel へ移ります。

```
┌─ UEFI フェーズ（ブートサービスあり） ──────────────────────────────────┐
│ ファームウェア ──▶ efi_main ──▶ 情報収集 ──────────▶ ExitBootServices │
│ BOOTAA64.EFI     uefi crate   GOP 画面情報          サービス終了       │
│ を読み込む       で初期化     メモリマップ, ACPI          │            │
└───────────────────────────────────────────────────────────┼────────────┘
                     ブートサービス終了後は戻らない          │
┌─ カーネルフェーズ（自前コードのみ） ───────────────────────▼────────────┐
│ console ──▶ 例外ベクタ ──▶ メモリ管理 ──▶ GIC + タイマー             │
│ 8x16 で描画  VBAR_EL1     ページアロケータ  割り込み駆動 → sched       │
└────────────────────────────────────────────────────────────────────────┘
```

上段では UEFI のサービスが使えるうちに必要な情報を集め、下段ではその情報だけを頼りに自前で初期化します。

## カーネル構成要素

各要素はファームウェアから受け取った情報（フレームバッファ、メモリマップ、ACPI テーブル）だけを頼りに動き、デバイスのアドレスをハードコードしません。VMware と QEMU の差を吸収するためです。

| モジュール | 役割 | 実装方針 |
| --- | --- | --- |
| `boot` | UEFI エントリ、情報収集、`ExitBootServices` | `uefi` crate を使用。収集した内容を `BootInfo` にまとめてカーネルに渡す |
| `console` | フレームバッファ上の文字表示 | 8x8 フォント（font8x8）を 2 倍拡大して 16x16 で描画。`core::fmt::Write` を実装し `println!` を提供 |
| `arch::exception` | 例外ベクタ（EL1） | 16 エントリのベクタを `global_asm!` で定義し `VBAR_EL1` に設定。ESR/FAR を表示するパニック画面 |
| `mm` | 物理ページ管理とヒープ | UEFI メモリマップの空き領域から 4KiB ページのビットマップアロケータ。`GlobalAlloc` で `alloc` を有効化 |
| `arch::timer` | 時間計測 | ARM Generic Timer（`CNTPCT_EL0` / `CNTFRQ_EL0`）。まずポーリング、後で割り込み化 |
| `arch::gic` | 割り込みコントローラ | ACPI MADT から GIC の種類とアドレスを取得。GICv3 を優先 |
| `sched` | タスク | タイマー割り込みが動いてから、ラウンドロビンのプリエンプティブ化 |

メモリ配置は当面、UEFI が用意した恒等マッピング（仮想 = 物理）をそのまま使います。自前ページテーブルへの移行は M3 で行います。

## ビルドとデプロイ

`make iso` 一つで VMware に渡せる ISO を作り、`make run` で同じ ISO を QEMU で起動します。

1. `cargo build --release --target aarch64-unknown-uefi` で `ferrix-kernel.efi` を生成する
2. mtools で FAT イメージを作り、`EFI/BOOT/BOOTAA64.EFI` として配置する（UEFI の既定ブートパス）
3. xorriso で FAT イメージを El Torito EFI ブートエントリにした `ferrix-kernel.iso` を作る
4. 検証: `make run`（QEMU virt + edk2 + ramfb）
5. 本番: VMware Fusion で「その他の 64 ビット ARM」の VM を作り、CD/DVD に `ferrix-kernel.iso` を割り当てて起動する

VM はメモリ 512MB、1 vCPU、ディスクなしで十分です。

## ロードマップ

各マイルストーンは「VMware 上で目に見えて動くこと」を完了条件にします。

- [x] **M0 起動**: UEFI から起動し、GOP 画面に「ferrix-kernel」を表示する（QEMU・VMware とも確認済み）
- [x] **M1 自立**: `ExitBootServices` 後に自前コンソールでメモリマップと CPU 情報を表示する（QEMU: EL1, Cortex-A72, 576 MiB, uptime 更新）
- [ ] **M2 例外とメモリ**: 例外ベクタで `brk` を捕まえて復帰し、ページアロケータと `Vec` が動く
- [ ] **M3 割り込み**: GIC とタイマー割り込みで経過時間を画面に更新し続ける。自前ページテーブルに移行する
- [ ] **M4 タスク**: 2 つ以上のカーネルタスクをプリエンプティブに切り替える

M4 の先（キーボード入力、ユーザー空間 EL0、capability）は M3 完了時に改めて計画します。

## リスクと未決事項

最大のリスクは、VMware ARM の仮想ハードウェア仕様が公開されておらず、QEMU で動いても VMware で動く保証がないことです。

| リスク / 未決事項 | 影響 | 対策 |
| --- | --- | --- |
| VMware の GIC の種類とアドレスが不明 | M3 の割り込みが動かない | ACPI MADT を読んで判定し、M1 で内容を画面に出して確認する |
| VMware にシリアルポートがあるか不明 | 画面以外のデバッグ出力がない | 画面コンソールを主にし、`.vmx` の `serial0` をファイル出力にして試す |
| `ExitBootServices` 後はキーボード入力がなくなる | 対話的なシェルを作れない | M4 までは入力なし。その後 VMware 側の入力デバイス（USB xHCI 等）を調べて判断 |
| フレームバッファのピクセル形式が環境で異なる | 色が崩れる | GOP の `PixelFormat` を見て RGB / BGR を切り替える |
| VMware での反復が遅い | 開発速度が落ちる | 日常の検証は QEMU で行い、VMware はマイルストーンごとに確認する |

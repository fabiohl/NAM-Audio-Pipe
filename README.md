<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# NAM-Audio-Pipe

![License](https://img.shields.io/badge/License-GPL--3.0--or--later-blue.svg) ![Rust](https://img.shields.io/badge/Rust-orange.svg) ![PipeWire](https://img.shields.io/badge/PipeWire-%E2%89%A5%200.3-brightgreen.svg) ![Latency](https://img.shields.io/badge/Latency-Sub--ms-red.svg) ![RT-Safe](https://img.shields.io/badge/RT--Safe-Zero--Alloc-brightgreen.svg) ![SIMD](https://img.shields.io/badge/SIMD-AVX2%20%7C%20AVX--512-blueviolet.svg) ![Recording](https://img.shields.io/badge/Recording-WAV%2032bit%20Float-yellow.svg)

**NAM-Audio-Pipe** is an ultra-low latency, real-time standalone PipeWire host application for [Neural Amp Modeler (NAM)](https://www.neuralampmodeler.com/) simulation on Linux.

It directly embeds [`NeuralAmpModeler-rs`](https://github.com/fabiohl/NeuralAmpModeler-rs) as its core neural DSP engine, inheriting all of its real-time guarantees: **zero heap allocations**, **zero locks**, **zero blocking system calls** on the audio thread, `x86-64-v3` (AVX2/FMA) baseline SIMD vectorization, AVX-512 multiversioning, and exact numerical parity against canonical C++ NAMCore and double-precision f64 reference oracles.

Designed for live performance, automated studio routing, and headless Linux audio setups, NAM-Audio-Pipe processes audio directly over PipeWire graphs and provides built-in lock-free 32-bit float WAV stream recording.

> **❤️‍🔥 NAM-Audio-Pipe is in active development.** Feedback, bug reports, performance metrics, and patchbay workflow suggestions are very welcome!

---

## ⚡ Key Strengths & Architectural Highlights

* **Pure Rust & Native PipeWire Integration:** Built directly on top of `libpipewire-0.3` (via the `pipewire` crate), interfacing with the PipeWire daemon at native quantum sizes (down to 64 samples) with minimal overhead and zero translation layers.
* **Inherited Neural Engine Excellence:** Powered by [`NeuralAmpModeler-rs`](https://github.com/fabiohl/NeuralAmpModeler-rs), supporting WaveNet (A1/A2 standard & lite profiles), LSTM (1-layer and 2-layer topologies), ConvNet, Linear FIR, and partitioned FFT cabinet impulse responses (.wav).
* **Zero-Allocation RT Safety:** The audio callback thread runs with strict real-time determinism — no heap allocations, no mutex locks, and no blocking I/O on the hot path. Parameter updates and state transitions pass through lock-free SPSC channels with GC cascades.
* **Concurrent Asynchronous WAV Recording:** Captures the raw input audio stream directly to disk without causing buffer underruns (xruns) or audio dropouts on the RT thread. Powered by Linux `tokio-uring` (io_uring) with automatic 4 GiB RIFF file segmentation (`_partN.wav`).
* **Adaptive Compute (Auto-Slimming):** Automatically monitors host CPU pressure and safely downgrades multi-profile `.namb` models (`--slim auto`) during CPU spikes to maintain real-time audio playback without glitching.
* **Half-Band Anti-Aliasing Oversampling:** Optional `2x` and `4x` polyphase oversampling centered around the neural inference stage to attenuate non-linear high-frequency foldover in high-gain amp models.

---

## 🥊 Feature Showcase ("Roofshoot")

| Feature / Attribute            | Technical Implementation                                                 | Benefit & Impact                                                   |
|:------------------------------ |:------------------------------------------------------------------------ |:------------------------------------------------------------------ |
| **Inference Engine**           | Core `NeuralAmpModeler-rs` engine (WaveNet A1/A2, LSTM, ConvNet, Linear) | Full model compatibility with exact C++ f32 & f64 reference parity |
| **Audio Subsystem**            | Direct PipeWire Client API (`libpipewire-0.3-dev`)                       | Sub-millisecond buffer sizes and native Linux audio graph routing  |
| **RT Determinism**             | Strict Zero Heap Drop, Zero Locks, Zero Hot-Path Logging                 | Guaranteed audio stability without buffer underruns (xruns)        |
| **SIMD Hardware Acceleration** | Mandatory `x86-64-v3` (AVX2/FMA) baseline + AVX-512 multiversioning      | Ultra-low CPU usage (< 3% of 1.33 ms quantum deadline)             |
| **Cabinet IR Convolution**     | Partitioned FFT & Direct FIR convolution engine (.wav IRs)               | Seamless, zero-latency speaker cabinet simulation                  |
| **WAV Recording**              | High-performance `tokio-uring` ringbuffer (`--record`)                   | Records raw 32-bit float stereo WAV files in real-time to disk     |
| **Oversampling**               | Half-band polyphase FIR filters (`off`, `2x`, `4x`)                      | Eliminates aliasing distortion in high-gain amp models             |
| **Activation Precision**       | `Standard` (exact-grade, default) vs `Fast` (Padé approximations)        | User-selectable trade-off between math precision and latency       |
| **Adaptive Compute**           | Auto-slimming (`--slim auto\|full\|lite`)                                | Prevents audio drops by dynamically throttling model complexity    |
| **PipeWire Naming**            | Native node names (`NAM-Audio-Pipe-input`, `NAM-Audio-Pipe-playback`)    | Clean, distinct graph identity in `qpwgraph` & `Helvum` patchbays  |

---

## 🛠️ System Prerequisites

| Dependency                | Minimum Version                              | Package / Command     |
|:------------------------- |:-------------------------------------------- |:--------------------- |
| **Linux Kernel**          | ≥ 5.10 (≥ 5.1 for io_uring)                  | `uname -r`            |
| **Rust Toolchain**        | ≥ 1.85                                       | `rustc --version`     |
| **PipeWire Daemon**       | ≥ 0.3                                        | `pipewire --version`  |
| **Development Libraries** | `libpipewire-0.3-dev`, `pkg-config`, `cmake` | See apt command below |

### Installation of System Dependencies (Debian / Ubuntu / Pop!_OS)

```bash
sudo apt update && sudo apt install -y build-essential pkg-config libpipewire-0.3-dev cmake
```

---

## 🚀 Building & Running

### 1. Direct Development Execution (`cargo run`)

For rapid development, testing, and debugging:

```bash
cargo run --release -- --model /path/to/amp_model.nam
```

To compile standard release binaries manually:

```bash
cargo build --release
```

The resulting binary will be placed at `target/release/nam-audio-pipe`.

---

### 2. Mega-Optimized Compiler Build (`./utils/build-release.sh`)

For maximum performance in live setups, `NAM-Audio-Pipe` includes a 5-phase optimization pipeline leveraging **Profile-Guided Optimization (PGO)** and **LLVM BOLT** (Binary Optimization and Layout Tool).

```bash
./utils/build-release.sh
```

#### What `build-release.sh` does under the hood

1. **Phase 1 — Environment Verification:** Validates toolchain prerequisites (`rustc`, `cargo`, `python3`, `llvm-profdata`, `llvm-bolt`, and `perf`) and verifies target CPU flags from `.cargo/config.toml`.
2. **Phase 2 — PGO Trace Generation:** Compiles the `pgo_workload` binary with `-Cprofile-generate`, executing synthetic neural DSP workloads to collect realistic hardware branch and execution profile files (`.profraw`), merging them into `merged.profdata`.
3. **Phase 3 — PGO-Optimized Compilation:** Recompiles `nam-audio-pipe` using `-Cprofile-use=merged.profdata` and relocation symbols (`-Clink-arg=-Wl,-q`), allowing LLVM to optimize hot loops, inline critical neural activation functions, and unroll vector SIMD loops based on real execution data.
4. **Phase 4 — LLVM BOLT Machine Code Reordering:** Uses Linux `perf` to record CPU cycle samples during live execution, parses performance counters via `perf2bolt`, and reorders machine code binary instructions via `llvm-bolt` to minimize Instruction Cache (I-Cache) misses and TLB pressure.
5. **Phase 4.5 — Assembly Hotspot Disassembly Report:** Generates an AI-ready assembly hotspot report at `target/dsp_hotpath.asm` for low-level inspection.
6. **Phase 5 — Automated Deployment:** Strips and installs the finalized, hyper-optimized binary directly into `~/.local/bin/nam-audio-pipe`.

---

## 🎛️ Command-Line Interface (CLI) Guide

### CLI Argument Reference

| Option                   | Description                                                                                                                    | Default             |
|:------------------------ |:------------------------------------------------------------------------------------------------------------------------------ |:------------------- |
| `-m, --model <FILE>`     | Path to `.nam` or `.namb` neural model file (supports `~`, `../`)                                                              | *Optional (Bypass)* |
| `-c, --cab <FILE>`       | Path to cabinet impulse response `.wav` file                                                                                   | *Optional (bypass)* |
| `-i, --input-gain <DB>`  | Input gain staging in dB (`-20.0` to `+20.0`)                                                                                  | `0.0`               |
| `-o, --output-gain <DB>` | Output gain staging in dB (`-20.0` to `+20.0`)                                                                                 | `0.0`               |
| `-b, --buffer-size <N>`  | Quantum block size in samples (e.g. `64`, `256`, `512`; `0` for auto)                                                          | `256`               |
| `--oversample <MODE>`    | Half-band oversampling mode (`off`, `2x`, `4x`)                                                                                | `off`               |
| `--activation <MODE>`    | Math precision mode: `standard` (exact) or `fast` (Padé polynomial)                                                            | `standard`          |
| `--slim <MODE>`          | Adaptive compute override: `auto` (CPU-gated), `full`, `lite`                                                                  | `auto`              |
| `--record`               | Enables lock-free 32-bit float WAV recording of processed (neural + cab) audio via `io_uring` (silences trimmed by noise gate) | `false`             |
| `--diagnose`             | Emits technical system diagnostic bundle and exits                                                                             | `false`             |
| `--diagnose-full`        | Emits diagnostic bundle with unredacted raw file paths and exits                                                               | `false`             |
| `-h, --help`             | Displays command-line help screen and exits                                                                                    | —                   |

---

### CLI Usage Examples

#### Basic Amp Simulation

```bash
nam-audio-pipe --model models/BossWN-standard.nam
```

#### Full Rig Setup (Amp Model + Cabinet IR + Gain Staging)

```bash
nam-audio-pipe \
  --model models/BossWN-standard.nam \
  --cab irs/Marshall1960A.wav \
  --input-gain 3.0 \
  --output-gain -2.0
```

#### Ultra-Low Latency Setup (64-sample buffer + Fast Activations)

```bash
nam-audio-pipe \
  --model models/BossWN-standard.nam \
  --buffer-size 64 \
  --activation fast
```

#### High-Gain Anti-Aliasing (4x Oversampling)

```bash
nam-audio-pipe \
  --model models/HighGainDrive.nam \
  --oversample 4x
```

#### Live Stream WAV Recording

```bash
nam-audio-pipe \
  --model models/BossWN-standard.nam \
  --record
```

*Creates timestamped `capture_YYYYMMDD_HHMMSS.wav` files in the current working directory (32-bit float stereo PCM at PipeWire sample rate, automatically splitting into `_partN.wav` if reaching the 4 GiB RIFF size limit).*

> [!NOTE]
> **Universal Noise Gate Behavior & Recording Impact**
> The Noise Gate is active in **all operational modes** of `NAM-Audio-Pipe` (with or without a `.nam` neural model, with or without a cabinet IR). There is no "gate off" mode — this is an intentional architectural decision: the gate is part of the application's core value proposition, ensuring playing pauses do not produce residual background noise at the output.
>
> **Recording with `--record`:** Audio is recorded **post-DSP** (neural model + cabinet IR + gain staging), matching exactly what is monitored on hardware output. Only blocks processed while the noise gate is open (`n_pw > 0`) are enqueued for recording. Silence before, during, and after performance is automatically trimmed in real-time with zero RT thread overhead.

#### Full Production Command

```bash
nam-audio-pipe \
  --model models/BossWN-standard.nam \
  --cab irs/Marshall1960A.wav \
  --input-gain 2.0 \
  --output-gain 3.0 \
  --buffer-size 64 \
  --oversample 4x \
  --activation fast \
  --slim auto \
  --record
```

---

## 🔀 PipeWire Graph Integration & Naming

When started, `NAM-Audio-Pipe` automatically registers the following PipeWire nodes and streams:

| PipeWire Object     | Registered Node / Stream Name | Node Description                  | Topology Role                                        |
|:------------------- |:----------------------------- |:--------------------------------- |:---------------------------------------------------- |
| **Capture Node**    | `NAM-Audio-Pipe-input`        | `NAM-Audio-Pipe Input`            | Virtual Input Sink (`Audio/Sink`, Input)             |
| **Capture Stream**  | `NAM-Audio-Pipe`              | —                                 | Primary audio capture stream                         |
| **Playback Node**   | `NAM-Audio-Pipe-playback`     | `NAM-Audio-Pipe Processed Output` | Output Playback Node (`Stream/Output/Audio`, Output) |
| **Playback Stream** | `NAM-Audio-Pipe-Output`       | —                                 | Processed audio stream routed to speakers/headphones |

* **Graph Grouping:** Both streams are bound to `node.group = "nam-audio-pipe-dsp"` and `node.link-group = "nam-audio-pipe-link-group"`, ensuring they are scheduled synchronously on the same PipeWire driver thread (`nam-audio-pipe-loop`).

---

## 🧪 CI & QA Automation Suite (`./utils/`)

The `./utils/` directory contains standard scripts for maintaining code quality, performance, and continuous integration:

| Script                                             | Purpose & Execution Scope                                                                                                                                                                                                                                                     |
|:-------------------------------------------------- |:----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [`utils/lints.sh`](utils/lints.sh)                 | **Static Analysis Gate:** Runs `cargo fmt`, compilation checks (`cargo check`), strict `cargo clippy` across broad feature combinations (`--all-features`, `--no-default-features`), validates SPDX license headers, and checks anti-patterns (`#[test]` in `tests/common/`). |
| [`utils/tests-quick.sh`](utils/tests-quick.sh)     | **Consolidated QA Suite:** Executes unit tests, integration suites (`recording`, `e2e_cli`), and live PipeWire daemon integration tests (`pw_integration`) in both debug and release modes under low CPU/IO priority. Phase 3 is fail-closed (R-17): `LIVE_PW=RAN`            |
|                                                    | is only emitted with a stable daemon and ≥1 DSP quantum — a `pw-cli` divergence inside the test is a hard failure, never a silent skip. Phase 4 adds the E2E `--record` test (`record_e2e_pipewire_wav_header_matches_bytes`, R-13): it spawns the real host + recording I/O  |
|                                                    | thread, records ≥1 quantum, stops cleanly and asserts the WAV `data` size equals the PCM bytes written; a missing daemon prints an honest `SKIP:` which the script maps to `RECORDING_IO_URING=SKIP` (never `=RAN`).                                                          |
| [`utils/build-release.sh`](utils/build-release.sh) | **Compiler Optimization Pipeline:** Multi-stage release builder using PGO and LLVM BOLT, outputting assembly report `target/dsp_hotpath.asm` and binary `~/.local/bin/nam-audio-pipe`.                                                                                        |

## 📚 Architectural & Technical Documentation

The following technical documents are maintained in the source repository:

| Document                                       | Primary Focus & Topic Coverage                                                                                                                                                  |
|:---------------------------------------------- |:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [`docs/architecture.md`](docs/architecture.md) | PipeWire dual-stream architecture, DspBridge shared memory, SPSC GC cascade, Linux RT tuning (`rt_setup`), audio processing pipeline, and asynchronous `io_uring` WAV recording |
| [`docs/testing.md`](docs/testing.md)           | Test suite layout, verification phases (`tests-quick.sh`), live PipeWire integration tests, `io_uring` recording validation, and compiler optimization pipeline                 |

---

## 🙏 Credits & Acknowledgments

* **Steven Atkinson** — Creator of [Neural Amp Modeler (NAM)](https://github.com/sdatkinson/neural-amp-modeler) for pioneering deep learning guitar amplifier modeling.
* **PipeWire Community** — For creating the next-generation Linux low-latency multimedia graph routing engine.

---

## ⚖️ License & AI Transparency

### AI Transparency Note

The system architecture, real-time safety guarantees, DSP pipeline design, and optimization engineering are intellectual work (and love) of the maintainer (**Fábio Lima**). Implementation was accelerated through pair programming (*Vibe Coding*) using artificial intelligence models (Gemini, Claude, Grok, DeepSeek and others) within Google Antigravity IDE. IA is just a tool that make wonder in wise hands.

### License

This project is licensed under the **GNU General Public License v3.0 or later** (**GPL-3.0-or-later**). See [LICENSE.txt](LICENSE.txt) for full license details.

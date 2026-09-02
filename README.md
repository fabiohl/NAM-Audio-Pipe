<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# NAM-Audio-Pipe

![License](https://img.shields.io/badge/License-GPL--3.0--or--later-blue.svg) ![Rust](https://img.shields.io/badge/Rust-orange.svg) ![PipeWire](https://img.shields.io/badge/PipeWire-%E2%89%A5%200.3-brightgreen.svg) ![Latency](https://img.shields.io/badge/Latency-Sub--ms-red.svg) ![RT-Safe](https://img.shields.io/badge/RT--Safe-Zero--Alloc-brightgreen.svg) ![SIMD](https://img.shields.io/badge/SIMD-AVX2%20x86--64--v3-blueviolet.svg) ![Recording](https://img.shields.io/badge/Recording-WAV%2032bit%20Float-yellow.svg)

**NAM-Audio-Pipe** is an ultra-low latency, real-time standalone PipeWire host application for [Neural Amp Modeler (NAM)](https://www.neuralampmodeler.com/) simulation on Linux.

It directly embeds [`NeuralAmpModeler-rs`](https://github.com/fabiohl/NeuralAmpModeler-rs) as its core neural DSP engine, inheriting all of its real-time guarantees: **zero heap allocations**, **zero locks**, **zero blocking system calls** on the audio thread, `x86-64-v3` (AVX2/FMA) production SIMD, and exact numerical parity against canonical C++ NAMCore and double-precision f64 reference oracles.

Designed for live performance, automated studio routing, and headless Linux audio setups, NAM-Audio-Pipe processes audio directly over PipeWire graphs and provides built-in lock-free 32-bit float WAV stream recording.

> **❤️‍🔥 NAM-Audio-Pipe is in active development.** Feedback, bug reports, performance metrics, and patchbay workflow suggestions are very welcome!

---

## ⚡ Key Strengths & Architectural Highlights

* **Pure Rust & Native PipeWire Integration:** Built directly on top of `libpipewire-0.3` (via the `pipewire` crate), interfacing with the PipeWire daemon at native quantum sizes (down to 64 samples) with minimal overhead and zero translation layers.
* **Inherited Neural Engine Excellence:** Powered by [`NeuralAmpModeler-rs`](https://github.com/fabiohl/NeuralAmpModeler-rs), supporting WaveNet (A1/A2 standard & lite profiles), LSTM (1-layer and 2-layer topologies), ConvNet, Linear FIR, and partitioned FFT cabinet impulse responses (.wav).
* **Zero-Allocation RT Safety:** The audio callback thread runs with strict real-time determinism — no heap allocations, no mutex locks, and no blocking I/O on the hot path. Parameter updates and state transitions pass through lock-free SPSC channels with GC cascades.
* **Zero-Jitter Kernel & OS Tuning:** Actively pins DMA latency to 0 µs (`/dev/cpu_dma_latency`), isolates CPU cores based on live `/proc/interrupts` IRQ profiling, locks RAM pages (`mlockall`), and disables Transparent Huge Pages (THP) to eliminate non-deterministic scheduler interruptions.
* **Concurrent Asynchronous WAV Recording:** Captures the processed (post-DSP) audio stream directly to disk without causing buffer underruns (xruns) or audio dropouts on the RT thread. Powered by Linux `tokio-uring` (io_uring) with automatic 4 GiB RIFF file segmentation (`_partN.wav`) and noise gate silence trimming.
* **Microarchitecture Layout Optimization:** Multi-stage compilation using Profile-Guided Optimization (PGO) and LLVM BOLT to reorganize basic-block code layouts, minimizing Instruction Cache (I-Cache) and TLB pressure during neural inference.
* **Adaptive Compute (Auto-Slimming):** Automatically monitors host CPU pressure and safely downgrades multi-profile `.namb` models (`--slim auto`) during CPU spikes to maintain real-time audio playback without glitching.
* **Half-Band Anti-Aliasing Oversampling:** Optional `2x` and `4x` polyphase oversampling centered around the neural inference stage to attenuate non-linear high-frequency foldover in high-gain amp models.

---

## 🥊 Feature Showcase ("Roofshoot")

| Feature / Attribute            | Technical Implementation                                                 | Benefit & Impact                                                   |
|:------------------------------ |:------------------------------------------------------------------------ |:------------------------------------------------------------------ |
| **Inference Engine**           | Core `NeuralAmpModeler-rs` engine (WaveNet A1/A2, LSTM, ConvNet, Linear) | Full model compatibility with exact C++ f32 & f64 reference parity |
| **Audio Subsystem**            | Direct PipeWire Client API (`libpipewire-0.3-dev`)                       | Sub-millisecond buffer sizes and native Linux audio graph routing  |
| **RT Determinism**             | Strict Zero Heap Drop, Zero Locks, Zero Hot-Path Logging                 | Guaranteed audio stability without buffer underruns (xruns)        |
| **Kernel Latency Tuning**      | Zero DMA latency (`/dev/cpu_dma_latency` 0 µs), `mlockall`, THP disable  | Eliminates CPU C-state wake-up lag and virtual memory page faults  |
| **CPU & IRQ Isolation**        | Dynamic `/proc/interrupts` load profiling + `pthread_setaffinity_np`     | Pins audio loop to the least-interrupted core (noisy neighbor safe)|
| **SIMD Hardware Acceleration** | Engine `x86-64-v3` (AVX2/FMA) production backend                         | Ultra-low CPU usage (< 3% of 1.33 ms quantum on the AVX2 path)     |
| **Binary Optimization**        | Profile-Guided Optimization (PGO) + LLVM BOLT basic block reordering     | Drastically minimizes I-Cache and iTLB misses in DSP loops         |
| **Cabinet IR Convolution**     | Partitioned FFT & Direct FIR convolution engine (.wav IRs)               | Seamless, zero-latency speaker cabinet simulation                  |
| **WAV Recording**              | High-performance `tokio-uring` ringbuffer + 4 GiB auto-split (`--record`)| Records 32-bit float stereo WAV files in real-time with gate trim  |
| **Oversampling**               | Half-band polyphase FIR filters (`off`, `2x`, `4x`)                      | Eliminates aliasing distortion in high-gain amp models             |
| **Activation Precision**       | `Standard` (exact-grade, default) vs `Fast` (Padé approximations)        | User-selectable trade-off between math precision and latency       |
| **Adaptive Compute**           | Auto-slimming (`--slim auto\|full\|lite`)                                | Prevents audio drops by dynamically throttling model complexity    |
| **PipeWire Naming**            | Native node names (`NAM-Audio-Pipe-input`, `NAM-Audio-Pipe-playback`)    | Clean, distinct graph identity in `qpwgraph` & `Helvum` patchbays  |

---

## 🛠️ System Prerequisites

| Dependency                | Minimum Version                              | Package / Command     |
|:------------------------- |:-------------------------------------------- |:--------------------- |
| **Linux Kernel**          | ≥ 5.10 (≥ 5.1 for io_uring)                  | `uname -r`            |
| **Rust Toolchain**        | ≥ 1.98.0 (edition 2024)                      | `rustc --version`     |
| **PipeWire Daemon**       | ≥ 0.3                                        | `pipewire --version`  |
| **Development Libraries** | `libpipewire-0.3-dev`, `pkg-config`, `cmake` | See apt command below |

> **MSRV policy:** `rust-version = "1.98.0"` in `Cargo.toml` is the **public MSRV promise**.
> Development happens on `stable` (pinned by `rust-toolchain.toml`). The MSRV promise is
> verified as an **isolated local check** — never mixed with the dev toolchain:
> `cargo +1.98.0 check --locked` from the repository root.

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

1. **Phase 1 — Environment Verification:** Validates toolchain prerequisites (`rustc`, `cargo`, `python3`, `tar`, `zstd`, `flatpak`, `llvm-profdata`, `llvm-bolt`, and `perf`) and verifies target CPU flags from `.cargo/config.toml`.
2. **Phase 2 — PGO Trace Generation:** Compiles the `pgo_workload` binary with `-Cprofile-generate`, executing synthetic neural DSP workloads across the mandatory topology families (WaveNet A1, WaveNet A2, LSTM) with the deterministic CabSim IR fixture (`tests/fixtures/models/cabsim_ir_pgo.wav`). The workload is fail-closed: any model/IR corruption aborts, and it emits `target/logs/pgo-workload-receipt.json` proving per-topology block counts (≥ 1000 each), oversampling coverage (`Off`/`2x`/`4x`) and the stereo CabSim frame counter. The receipt is formally validated before the `.profraw` files are merged into `merged.profdata`.
3. **Phase 3 — PGO-Optimized Compilation:** Recompiles `nam-audio-pipe` using `-Cprofile-use=merged.profdata` and relocation symbols (`-Clink-arg=-Wl,-q`), allowing LLVM to optimize hot loops, inline critical neural activation functions, and unroll vector SIMD loops based on real execution data.
4. **Phase 4 — LLVM BOLT Machine Code Reordering:** Uses Linux `perf` to record CPU cycle samples during live execution, parses performance counters via `perf2bolt`, and reorders machine code binary instructions via `llvm-bolt` to minimize Instruction Cache (I-Cache) misses and TLB pressure. Readiness is proven by PipeWire node registration plus CPU sample consumption (no blind sleep); `perf.data`/`perf.fdata` are validated for minimum samples and DSP symbol coverage; the ELF Build-ID is strictly matched against the collected traces (no `--ignore-build-id`). BOLT failure/unavailability is recorded explicitly in `target/logs/release-receipt.json` as `PGO-ONLY (BOLT_FAILED/BOLT_UNAVAILABLE)` and is fatal under `--strict-release`.
5. **Phase 4.5 — Assembly Hotspot Disassembly Report:** Generates an AI-ready assembly hotspot report at `target/dsp_hotpath.asm` for low-level inspection.
6. **Phase 5 — Automated Deployment:** Strips and installs the finalized, hyper-optimized binary directly into `~/.local/bin/nam-audio-pipe`.
7. **Phase 6 — Release Packaging (.tar.zst):** Generates a release distribution archive at `~/nam-audio-pipe-vx.y.z-linux-x86_64-v3.tar.zst` containing the optimized binary, documentation, license, and a 1-click installation script.
8. **Phase 7 — Release Packaging (.flatpak):** Builds and exports the standalone Flatpak application bundle (`~/nam-audio-pipe-vx.y.z-linux-x86_64-v3.flatpak`) configured with low-latency PipeWire, PulseAudio, IPC, and desktop metadata.

#### CLI Options

| Option          | Description                                                                                                     |
|:--------------- |:--------------------------------------------------------------------------------------------------------------- |
| `--install`     | Automatically installs the Flatpak application locally (`flatpak install --user`) in addition to `~/.local/bin/`|
| `--no-flatpak`  | Skips Phase 7 (Flatpak bundle creation).                                                                        |
| `--no-tarball`  | Skips Phase 6 (.tar.zst archive creation).                                                                      |
| `--no-pgo`      | Skips Phase 2/3 (Profile-Guided Optimization) and compiles directly with the `dist` release profile.            |
| `--no-bolt`     | Skips Phase 4 (LLVM BOLT post-link optimization).                                                               |
| `--strict-release` | Fails the release whenever the declared optimization cannot be proven (BOLT failure/unavailability is fatal instead of degrading to PGO-ONLY). |
| `-h, --help`    | Displays command-line help screen and exits.                                                                    |

---

### 3. Flatpak Standalone Application Distribution (`.flatpak`)

In addition to traditional native binary deployment (`~/.local/bin/nam-audio-pipe`), `NAM-Audio-Pipe` is distributed as a standalone **Flatpak Application** (`io.github.fabiohl.NAMAudioPipe`), targeting the `org.freedesktop.Platform` runtime (`25.08`).

The Flatpak bundle provides an isolated, reproducible runtime while retaining direct access to host PipeWire low-latency audio graphs, real-time power management QoS, and user model libraries.

#### End-User Installation

Install the `.flatpak` bundle directly into your local user Flatpak repository:

```bash
flatpak install --user --reinstall ~/nam-audio-pipe-v0.5.0-linux-x86_64-v3.flatpak
```

#### Running via Flatpak

Execute `NAM-Audio-Pipe` inside the Flatpak sandbox using `flatpak run`:

```bash
# 1. Run environment diagnostic bundle
flatpak run io.github.fabiohl.NAMAudioPipe --diagnose

# 2. Run amp simulation with model and cabinet IR located in your home directory
flatpak run io.github.fabiohl.NAMAudioPipe \
  --model ~/models/BossWN-standard.nam \
  --cab ~/irs/Marshall1960A.wav \
  --buffer-size 64 \
  --activation fast
```

#### Sandbox Permissions & Low-Latency Audio IPC

The Flatpak package is pre-configured with carefully scoped permissions required for ultra-low latency audio processing:

* `--filesystem=xdg-run/pipewire-0` — Direct client connection to the host PipeWire daemon Unix socket.
* `--share=ipc` — POSIX shared memory and `memfd_create` support, allowing zero-copy audio buffer transport between the sandbox and PipeWire daemon.
* `--device=all` — Grants access to `/dev/snd` and `/dev/cpu_dma_latency` for Linux PM QoS 0 µs C-state latency pinning.
* `--filesystem=home:ro` — Read-only access to user home directory for resolving `.nam`/`.namb` neural models and `.wav` IR files.
* `--socket=wayland` and `--socket=fallback-x11` — Windowing system integration.
* `--socket=pulseaudio` — Fallback audio server connectivity.

#### Developer Workflow (Building & Testing Flatpak Locally)

1. **Automated Pipeline Build & Install:**

   ```bash
   ./utils/build-release.sh --install
   ```

2. **Standalone Manifest Compilation via `flatpak-builder`:**

   ```bash
   # Build the release binary first
   cargo build --release

   # Compile and install the application manifest locally
   flatpak-builder --user --install --force-clean \
     --state-dir=target/flatpak-builder \
     target/flatpak-build \
     packaging/flatpak/io.github.fabiohl.NAMAudioPipe.yml
   ```

#### Uninstallation

To remove the Flatpak application:

```bash
flatpak uninstall --user io.github.fabiohl.NAMAudioPipe
```

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
| `--gate <MODE>`          | Silence gate: `on` (default) trims silence from monitoring and `--record`; `off` passes silence through gracefully             | `on`                |
| `--record`               | Enables lock-free 32-bit float WAV recording of processed (neural + cab) audio via `io_uring` (silences trimmed when `--gate on`) | `false`             |
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
> **Silence Gate Behavior & Recording Impact**
> The Silence Gate is configurable via `--gate on|off` (default: `on`) and operates across all operational modes of `NAM-Audio-Pipe` (with or without a `.nam` neural model, with or without a cabinet IR).
>
> - **Default (`--gate on`):** Eliminates residual background noise (idle hiss and pickup hum) during playing pauses in both real-time monitoring and recording. Audio recorded via `--record` only enqueues blocks processed while the gate is open (`n_pw > 0`), automatically trimming silence in real-time with zero RT thread overhead.
> - **Pass-Through (`--gate off`):** Explicit opt-in that keeps the gate permanently open (`gate_enabled = false`). All audio — including background noise and silent passages — is passed through gracefully to live hardware output and recorded continuously to WAV without trimming.

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

* **Synchronous Driver Loop:** Both streams are bound to `node.group = "nam-audio-pipe-dsp"` and `node.link-group = "nam-audio-pipe-link-group"`, ensuring they are scheduled synchronously on the same PipeWire driver thread (`nam-audio-pipe-loop`) without inter-thread context switching.
* **Auto-Routing & Anti-Loopback Watchdog:** Automatically discovers the default hardware output sink via `pw-metadata` with an asynchronous 500 ms watchdog timeout, filtering out its own virtual input node to prevent feedback loops.

---

## 🧪 CI & QA Automation Suite (`./utils/`)

The `./utils/` directory contains standard scripts for maintaining code quality, performance, and continuous integration:

| Script                                             | Purpose & Execution Scope                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
|:-------------------------------------------------- |:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [`utils/lints.sh`](utils/lints.sh)                 | **Static Analysis Gate:** Runs `cargo fmt`, compilation checks (`cargo check`), strict `cargo clippy` across broad feature combinations (`--all-features`, `--no-default-features`), validates SPDX license headers, and checks anti-patterns (`#[test]` in `tests/common/`).                                                                                                                                                                                                         |
| [`utils/tests-quick.sh`](utils/tests-quick.sh)     | **Consolidated QA Suite:** Executes unit tests, integration suites (`recording`, `e2e_cli`), and live PipeWire daemon integration tests (`pw_integration`) in both debug and release modes under low CPU/IO priority. Phase 3 is fail-closed (R-17: `LIVE_PW=RAN` only with stable daemon and ≥1 DSP quantum). Phase 4 adds E2E `--record` (`record_e2e_pipewire_wav_header_matches_bytes`, R-13), verifying WAV `data` chunk sizes and reporting honest `RECORDING_IO_URING` status. |
| [`utils/build-release.sh`](utils/build-release.sh) | **Compiler Optimization Pipeline:** Multi-stage release builder using PGO and LLVM BOLT, outputting assembly report `target/dsp_hotpath.asm`, binary `~/.local/bin/nam-audio-pipe`, and release archive `~/nam-audio-pipe-vx.y.z-linux-x86_64-v3.tar.zst`.                                                                                                                                                                                                                            |

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

<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# NAM-Audio-Pipe

![License](https://img.shields.io/badge/License-GPL--3.0--or--later-blue.svg) ![Rust](https://img.shields.io/badge/Rust-1.99%2B-orange.svg) ![PipeWire](https://img.shields.io/badge/PipeWire-%E2%89%A5%200.3-brightgreen.svg) ![Latency](https://img.shields.io/badge/Latency-Sub--ms-red.svg) ![RT-Safe](https://img.shields.io/badge/RT--Safe-Zero--Alloc-brightgreen.svg) ![SIMD](https://img.shields.io/badge/SIMD-AVX2%20x86--64--v3-blueviolet.svg) ![Parity](https://img.shields.io/badge/Parity-Dual--Oracle%20%3E136dB%20SNR-success.svg) ![Recording](https://img.shields.io/badge/Recording-io__uring%2032bit%20WAV-yellow.svg)

**NAM-Audio-Pipe** is an ultra-low latency, real-time standalone PipeWire host application for [Neural Amp Modeler (NAM)](https://www.neuralampmodeler.com/) simulation on Linux.

Turn your Linux machine into a world-class, instantaneous guitar amplifier rig. Powered directly by the cutting-edge [`NeuralAmpModeler-rs`](https://github.com/fabiohl/NeuralAmpModeler-rs) engine, NAM-Audio-Pipe connects directly to your native PipeWire audio graph without requiring heavyweight digital audio workstations (DAWs), complex JACK bridging, or Wine emulation layers.

Whether practicing at home, tracking live in the studio, or performing on stage, NAM-Audio-Pipe delivers studio-grade tone with **analog-like responsiveness**, **sub-millisecond buffer sizes**, **zero dropouts**, and **one-touch lock-free WAV recording**.

---

## ⚡ Why NAM-Audio-Pipe? (The Value Proposition)

NAM-Audio-Pipe inherits the relentless engineering, mathematical precision, and raw performance of the [`NeuralAmpModeler-rs`](https://github.com/fabiohl/NeuralAmpModeler-rs) core engine, wrapping it in a standalone Linux application designed specifically for musicians and audio professionals:

### 🎯 1. 100% Bit-Exact Numerical Parity with C++ NAMCore

You get the exact, authentic tone of your favorite amp models. The underlying engine is verified under a continuous dual-oracle regression suite against both the reference C++ NAMCore and a double-precision $f64$ mathematical oracle:

- **WaveNet A1/A2 Models**: Maintains Error-to-Signal Ratio (ESR) $< 2.31 \times 10^{-14}$ ($> 136\text{ dB}$ SNR).
- **ConvNet Models**: Achieves ESR $< 9.33 \times 10^{-16}$ ($> 150\text{ dB}$ SNR).
- **Full Model Coverage**: Loads standard `.nam` (JSON) and lightning-fast pre-transposed `.namb` v2 (binary, $< 1\text{ ms}$ load time) models across all major architectures: WaveNet A1/A2, LSTM (1 & 2 layers), ConvNet, and Linear FIR/FFT.

### 🚀 2. Unmatched CPU Efficiency & Headroom

Hand-written vector SIMD compute kernels (`x86-64-v3` AVX2/FMA) monomorphized at compile-time with zero virtual dispatch:

- **Massive Real-Time Headroom**: On modern hardware (e.g., AMD Ryzen 7 5700U), a WaveNet Standard model processes a 64-sample buffer in just **$41.8\text{ µs}$**—consuming less than **$3.2\%$** of the available $1.33\text{ ms}$ real-time budget. Lighter LSTM profiles execute in as little as **$6.8\text{ µs}$** ($< 0.6\%$ CPU).
- **Zero-Vtable Dispatch**: No function pointer jumps or dynamic memory lookups on the audio path.
- **Microarchitecture Layout Optimization**: Compiled with Profile-Guided Optimization (PGO) and LLVM BOLT instruction reordering to maximize CPU Instruction Cache (I-Cache) and TLB hit rates.

### 🛡️ 3. Strict Hard Real-Time Determinism (The Three Laws of Audio DSP)

Engineered from the ground up for zero-dropout live playing:

- **Zero Heap Allocations**: Scratch memory, model weights, and resamplers are pre-allocated. No `malloc` or `free` ever occurs on the audio thread.
- **Zero Mutex Locks**: State changes, model swaps, and telemetries communicate across bounded lock-free Single-Producer Single-Consumer (SPSC) rings with a 3-tier garbage collection cascade.
- **Zero Blocking I/O**: Filesystem access and logging macros are strictly excluded from the audio callback.
- **Denormal & Subnormal Armor**: Injects symmetric $-220\text{ dBFS}$ dither and enforces hardware FTZ/DAZ to eliminate CPU microcode stalls during digital silence.

### 🔊 4. Studio-Grade Native DSP Suite

No external audio libraries or third-party wrappers needed:

- **Zero-Latency Cabinet Simulation**: Uniformly Partitioned Overlap-Save (UPOLS) FFT convolution engine runs cabinet impulse responses (`.wav`) in **$1.3\text{ µs}$** with bounded, deterministic latency.
- **Polyphase Sinc Resampling**: Native minimum-phase sinc resampler (256 phases × 64 taps, $>105\text{ dB}$ stopband attenuation, zero pre-ringing) seamlessly adapts models trained at 48 kHz to any host sample rate (44.1 kHz, 96 kHz, 192 kHz).
- **Half-Band Anti-Aliasing Oversampling**: Multi-stage half-band polyphase FIR filters (`2x`, `4x`, $>100\text{ dB}$ stopband) attenuate non-linear high-frequency foldover distortion on screaming high-gain leads.
- **Smart Schmitt Silence Gate**: Dual-threshold noise gate with 10 dB Schmitt hysteresis and auto-gain normalization to silence pickup hum without cutting off delicate note decays.

### 💾 5. High-Performance Asynchronous WAV Recording (`--record`)

Capture your tone directly to pristine 32-bit float stereo WAV files without touching DAW software:

- **Zero-Impact `io_uring` Engine**: Audio samples travel through a pre-allocated 64 KiB slot pool transport (~57% lower memory traffic than traditional rings) and write to disk via Linux `tokio-uring`.
- **Crash-Resilient Header Checkpointing**: Automatically updates WAV header chunk lengths and invokes `fsync` every 3 seconds. An unexpected power outage or signal kill loses at most 3 seconds instead of destroying the entire recording.
- **Automatic 4 GiB RIFF File Splitting**: Seamlessly splits extended jam sessions into continuous, gapless `_partN.wav` segments.

### 🐧 6. Deep Linux Real-Time Hardening

Optimizes the operating system kernel for audio automatically on launch:

- Locks application memory into physical RAM (`mlockall`) to prevent disk paging.
- Pins Power Management DMA latency to 0 µs (`/dev/cpu_dma_latency`), eliminating CPU C-state wake-up lag.
- Profiles `/proc/interrupts` in real-time to bind the audio thread to the least-contended CPU core away from hardware device IRQs.
- Disables Transparent Huge Pages (THP) to prevent background kernel memory compaction jitter.

---

## 🎸 Quick Start for Beginners (Plug & Play in 60 Seconds)

Getting started with guitar on Linux has never been easier. You do not need to configure JACK, compile kernel patches, or launch complex software suites.

```text
       Guitar / Bass
             │ (1/4" Instrument Cable)
             ▼
┌─────────────────────────┐
│ USB Audio Interface     │
│ (Focusrite, Behringer,  │
│  PreSonus, MOTU, etc.)  │
└────────────┬────────────┘
             │ (USB)
             ▼
┌─────────────────────────────────────────────────────────────┐
│ Linux Machine (PipeWire Audio Graph)                        │
│                                                             │
│   [Capture: Guitar DI] ──► [NAM-Audio-Pipe] ──► [Speakers]  │
│                                  │                          │
│                                  └──► (WAV Recording)       │
└─────────────────────────────────────────────────────────────┘
```

### Step 1: Install System Dependencies

On Ubuntu, Debian, Pop!_OS, or Linux Mint:

```bash
sudo apt update && sudo apt install -y build-essential pkg-config libpipewire-0.3-dev cmake
```

### Step 2: Grab a Guitar Model (.nam) & Cabinet IR (.wav)

Download free community-created amp models and impulse responses from:

- [ToneHunt.org](https://tonehunt.org/) (Thousands of free captures of vintage & modern amps)
- [Tone3000](https://tone3000.com/)

### Step 3: Run and Play

Plug in your guitar, select your instrument input in your system sound settings, and launch:

```bash
# Basic amp playback:
nam-audio-pipe --model ~/models/MyFavoriteAmp.nam

# Complete rig with amp model and speaker cabinet:
nam-audio-pipe --model ~/models/Plexi50W.nam --cab ~/irs/Greenback4x12.wav
```

You will hear your guitar processed with full neural simulation in real time!

---

## 🥊 Feature Showcase & Technical Matrix

| Capability                       | Technical Implementation                                                       | Practical Musician / Engineering Impact                                                 |
|:-------------------------------- |:------------------------------------------------------------------------------ |:--------------------------------------------------------------------------------------- |
| **Neural Amp Simulation**        | Full `NeuralAmpModeler-rs` engine (WaveNet A1/A2, LSTM, ConvNet, Linear)       | Authentic tube amp tone with exact dynamic feel and pick-attack nuance                  |
| **Native PipeWire Routing**      | Direct Client API (`libpipewire-0.3-dev`) via `pipewire` crate                 | Seamless integration with `qpwgraph`, `Helvum`, and system audio patchbays              |
| **Latency Benchmark**            | Native quantums down to 64 samples ($1.33\text{ ms}$ at 48 kHz)                | Imperceptible latency matching real analog hardware amplifiers                          |
| **Real-Time Safety**             | Strict zero-allocation audio callbacks, zero mutex locks, zero blocking I/O    | Absolute reliability on stage and in the studio—no audio glitches or xruns              |
| **Numerical Accuracy**           | Dual-oracle verified against C++ NAMCore $f32$ & $f64$ mathematical models     | Laboratory-grade fidelity ($>136\text{ dB}$ SNR on standard WaveNet)                    |
| **Low-Latency Cabinet IR**       | Uniform-Partitioned Overlap-Save (UPOLS) FFT convolution engine                | Natural speaker cabinet resonance ($1.3\text{ µs}$ compute overhead for 512-sample IRs) |
| **Multi-Rate Resampling**        | 256-phase Kaiser-windowed polyphase sinc resampler ($>105\text{ dB}$ stopband) | Play 48 kHz models at 44.1 kHz, 96 kHz, or 192 kHz with zero pre-ringing or aliasing    |
| **Anti-Aliasing Oversampling**   | Half-band polyphase FIR filtering (`2x`, `4x`, $>100\text{ dB}$ stopband)      | Eliminates high-frequency digital harshness on extreme high-gain metal captures         |
| **Smart Silence Gate**           | Polymorphic gate (`on`/`off`/custom dBFS) with 10 dB Schmitt hysteresis        | Silences pickup hum and hiss during pauses without cutting off sustain                  |
| **Asynchronous WAV Recording**   | Non-blocking slot pool transport + Linux `tokio-uring` engine                  | Lossless 32-bit float stereo recording straight to disk with zero audio thread burden   |
| **Crash Protection**             | Periodic 3-second header checkpointing & 5-second graceful signal shutdown     | Saves your take even if the host machine experiences a sudden power loss                |
| **Kernel Latency Tuning**        | Real-time PM QoS ($0\text{ µs}$), `mlockall`, THP disable, CPU IRQ isolation   | Prevents CPU sleep states and operating system scheduler interruptions                  |
| **Adaptive Compute**             | Dynamic CPU pressure monitoring (`--slim auto`)                                | Automatically throttles model complexity under system overload to prevent dropouts      |
| **Desktop & Flatpak Deployment** | Standalone Flatpak bundle with AppStream metadata & `.desktop` launcher        | 1-click install and sandboxed execution across all modern Linux distributions           |

---

## 🎛️ Musician's Playbook: Common CLI Recipes

### 1. The Clean Studio Rig (Crystal Clear Tone)

Ideal for clean jazz, funk, or acoustic simulation:

```bash
nam-audio-pipe \
  --model models/TwinReverb-Clean.nam \
  --cab irs/OpenBack2x12.wav \
  --gate -75
```

*Tip: `--gate -75` sets a gentle threshold, preserving delicate fingerpicking tails.*

### 2. High-Gain Metal Lead with 4× Oversampling

Eliminates aliasing foldover on heavy distortion leads:

```bash
nam-audio-pipe \
  --model models/HighGainDrive.nam \
  --cab irs/Vintage4x12.wav \
  --oversample 4x \
  --gate -50
```

*Tip: `--gate -50` silences single-coil hum and high-gain hiss between riffs.*

### 3. Ultra-Low Latency Live Performance (64-Sample Quantum)

For maximum responsiveness during stage monitoring:

```bash
nam-audio-pipe \
  --model models/PlexiLead.nam \
  --cab irs/4x12-Greenback.wav \
  --buffer-size 64 \
  --activation fast
```

*Tip: `--activation fast` selects Padé polynomial activations for maximum CPU speed.*

### 4. Practice & Record Jam Session to Disk

Records your complete processed guitar take directly to a timestamped WAV file:

```bash
nam-audio-pipe \
  --model models/OverdriveAmp.nam \
  --cab irs/Cab1x12.wav \
  --record \
  --record-dir ~/Music/GuitarJams
```

*Files save as `capture_YYYYMMDD_HHMMSS.wav`. When `--gate on` is active, long silent pauses are trimmed automatically.*

### 5. Running in Dry Bypass (Audio Interface / Latency Testing)

Stream dry audio through PipeWire without loading a model:

```bash
nam-audio-pipe --gate off
```

---

## 📦 Installation & Packaging Options

NAM-Audio-Pipe can be run directly via Cargo, compiled into a standalone hyper-optimized binary, or installed as a sandboxed Flatpak application.

### Option A: Standalone Flatpak Application (Recommended for Users)

NAM-Audio-Pipe is packaged as an autonomous desktop Flatpak (`io.github.fabiohl.NAMAudioPipe`), targeting the `org.freedesktop.Platform//25.08` runtime. It provides an isolated, reliable environment with direct low-latency PipeWire IPC access.

#### Install Local Bundle

```bash
flatpak install --user --reinstall ~/nam-audio-pipe-v1.0.0-linux-x86_64-v3.flatpak
```

#### Run via Flatpak

```bash
# 1. Inspect environment diagnostics:
flatpak run io.github.fabiohl.NAMAudioPipe --diagnose

# 2. Run amp simulation reading models from your home directory:
flatpak run io.github.fabiohl.NAMAudioPipe \
  --model ~/models/Amp.nam \
  --cab ~/irs/Cabinet.wav \
  --buffer-size 64
```

The Flatpak bundle exports native desktop launchers and high-resolution icons to your application menu, including desktop actions for **Run in bypass** and **Diagnostics**.

---

### Option B: Pre-Compiled Optimized Native Binary (`utils/build-release.sh`)

For maximum performance in mission-critical live setups, compile using the automated optimization pipeline:

```bash
# Build hyper-optimized PGO + LLVM BOLT binary:
export CARGO_BUILD_JOBS=4
./utils/build-release.sh --install
```

This multi-stage builder executes:

1. **Profile-Guided Optimization (PGO)**: Instruments DSP kernels across quantum matrices (32 to 1024 frames) and model families, generating profile traces to guide inlining and vectorization.
2. **LLVM BOLT**: Rearranges basic blocks in machine code using Linux `perf` sample traces to minimize CPU Instruction Cache (I-Cache) and TLB misses.
3. **Static Hotspot Gates**: Enforces zero `zmm` registers (AVX-512 opt-in policy), zero `libm` calls in RT paths, and zero heap allocations.
4. **Installs directly** to `~/.local/bin/nam-audio-pipe`.

---

### Option C: Direct Cargo Build

For developers working directly on source code:

```bash
# Direct development execution:
export CARGO_BUILD_JOBS=4
cargo run --release -- --model /path/to/amp_model.nam

# Build standard release binary:
cargo build --release
```

The compiled binary will be located at `target/release/nam-audio-pipe`.

---

## 🎛️ Command-Line Interface (CLI) Reference

```text
nam-audio-pipe [OPTIONS]
```

| Flag / Option             | Parameter Domain / Values             | Default                         | Operational Description                                                                        |
|:------------------------- |:------------------------------------- |:------------------------------- |:---------------------------------------------------------------------------------------------- |
| `-m, --model <FILE>`      | Filepath (`.nam`, `.namb`)            | *None (Bypass)*                 | Path to neural amplifier model file. Invalid path fails fast before PipeWire initialization.   |
| `-c, --cab <FILE>`        | Filepath (`.wav`)                     | *None (Bypass)*                 | Path to cabinet impulse response file. Missing file exits with code 2 unless `--cab-optional`. |
| `--cab-optional`          | Flag                                  | `false`                         | Missing or unreadable `--cab` warns and continues without cabinet simulation.                  |
| `-i, --input-gain <DB>`   | Float (`-20.0` to `+20.0` dB)         | `0.0`                           | Input gain adjustment prior to neural inference.                                               |
| `-o, --output-gain <DB>`  | Float (`-20.0` to `+20.0` dB)         | `0.0`                           | Output gain adjustment after cabinet simulation.                                               |
| `-b, --buffer-size <N>`   | `{0} ∪ {2^k \| 16 ≤ 2^k ≤ 8192}`      | `256`                           | PipeWire quantum block size in samples (`0` for auto-negotiation).                             |
| `--cabsim-partition <N>`  | `32`, `64`, `128`, `256` samples      | `128`                           | Cab-sim UPOLS partition size. Lower values reduce algorithmic latency at slight CPU cost.      |
| `--oversample <MODE>`     | `off`, `2x`, `4x`                     | `off`                           | Half-band polyphase anti-aliasing oversampling factor.                                         |
| `--activation <MODE>`     | `standard`, `fast`                    | `standard`                      | Math precision: `standard` (exact Taylor minimax) vs `fast` (Padé polynomial approximation).   |
| `--slim <MODE>`           | `auto`, `full`, `lite`                | `auto`                          | Adaptive compute: automatically swaps submodels under CPU pressure to prevent dropouts.        |
| `--gate <MODE>`           | `on`, `off`, or `[-96.0, -20.0]` dBFS | `on` (`-70 dB`)                 | Silence gate configuration. Numeric thresholds derive close value via 10 dB Schmitt relation.  |
| `--cpu <INDEX>`           | Integer core ID                       | *Dynamic IRQ selection*         | Manually binds the real-time audio thread to a specific CPU core.                              |
| `--record`                | Flag                                  | `false`                         | Enables asynchronous 32-bit float stereo WAV recording via `io_uring`.                         |
| `--record-dir <DIR>`      | Directory path                        | `$XDG_MUSIC_DIR/NAM-Audio-Pipe` | Output directory for saved audio takes (auto-created if missing).                              |
| `--fail-fast`             | Flag                                  | `false`                         | Disables bounded reconnect cycle: exits immediately on PipeWire daemon disconnect.             |
| `--reconnect-timeout <S>` | Seconds (`float`)                     | `11.75` (6 attempts)            | Caps the total reconnect retry backoff budget.                                                 |
| `--diagnose`              | Flag                                  | `false`                         | Emits technical diagnostic bundle and exits.                                                   |
| `--diagnose-full`         | Flag                                  | `false`                         | Emits diagnostic bundle with unredacted raw paths and exits.                                   |
| `--pause`                 | Flag                                  | `false`                         | Waits for Enter before exiting on interactive terminals (used by `.desktop` Diagnostics).      |
| `-h, --help`              | Flag                                  | —                               | Displays command-line help screen and exits.                                                   |

---

### Exit Code Taxonomy

NAM-Audio-Pipe returns typed exit codes to distinguish invocation mistakes from hardware or runtime failures:

| Exit Code | Classification   | Meaning / Trigger Condition                                                      | Remediation Guidance                                      |
|:---------:|:---------------- |:-------------------------------------------------------------------------------- |:--------------------------------------------------------- |
| **0**     | Success          | Normal termination or successful `--help` / `--diagnose`.                        | None.                                                     |
| **1**     | Generic / Forced | Uncategorized failure or hard escalation after termination grace window timeout. | Inspect application error logs.                           |
| **2**     | Usage Error      | Invalid CLI arguments or non-power-of-two buffer size.                           | Review command syntax with `--help`.                      |
| **3**     | Model Load Error | Model file missing, corrupt, or unsupported architecture version.                | Verify `.nam`/`.namb` file integrity.                     |
| **4**     | Cab IR Error     | Cabinet impulse response `.wav` file unreadable or malformed.                    | Check file path or use `--cab-optional`.                  |
| **5**     | Backend Error    | PipeWire daemon unreachable or stream initialization failed.                     | Check PipeWire daemon status (`systemctl --user status`). |
| **6**     | Recording Error  | Recording directory unwritable, `io_uring` failed, or buffer overruns occurred.  | Check disk permissions and available space.               |

---

## 🔀 PipeWire Graph Topology & Auto-Routing

On startup, NAM-Audio-Pipe automatically registers dedicated capture and playback nodes in the PipeWire graph:

| Node Name in Patchbay     | Media Class           | Function in Audio Graph                                                     |
|:------------------------- |:--------------------- |:--------------------------------------------------------------------------- |
| `NAM-Audio-Pipe-input`    | `Audio/Sink` (Input)  | Virtual capture sink receiving guitar DI from hardware inputs or DAW tracks |
| `NAM-Audio-Pipe-playback` | `Stream/Output/Audio` | Playback output streaming processed audio to speakers or headphones         |

- **Zero-Context Switching**: Both nodes are bound to identical scheduling groups (`node.group = "nam-audio-pipe-dsp"`), ensuring execution on the same real-time driver thread (`nam-audio-pipe-loop`).
- **Feedback Protection**: Queries `pw-metadata` with an automatic 500 ms watchdog to discover the default output sink, automatically ignoring self-capture nodes to prevent acoustic feedback loops.
- **Visual Patchbay Support**: Nodes are clearly labelled and easily connected using visual patchbays such as **`qpwgraph`**, **`Helvum`**, or **`Carla`**.

---

## 🧪 Quality Assurance & Verification Suite

NAM-Audio-Pipe maintains enterprise-grade reliability through an automated testing and verification suite located in [`utils/`](utils/):

```bash
# 1. Run static analysis quality gate (fmt, check, clippy, SPDX headers, AppStream XML):
export CARGO_BUILD_JOBS=4
./utils/mem-guard.sh ./utils/lints.sh

# 2. Run consolidated 4-phase agile verification suite:
export CARGO_BUILD_JOBS=4
./utils/tests-quick.sh

# 3. Comprehensive 7-phase long-duration audit suite (Human Operator Only):
# Runs accelerated soak (320k blocks), zero-alloc heap audit, nanosecond RT deadline/jitter
# dispersion, real wall-clock endurance, and PipeWire resilience under private daemons.
export CARGO_BUILD_JOBS=4
./utils/tests-long.sh --strict-pre-release
```

For complete architectural details on testing, verification phases, and real-time compliance criteria, consult [`docs/testing.md`](docs/testing.md).

---

## 📚 Technical Documentation Directory

- [`docs/architecture.md`](docs/architecture.md) — Comprehensive architecture bible: dual-stream PipeWire topology, `DspBridge` shared memory layout, SPSC GC cascade, Linux RT hardening, and asynchronous `io_uring` WAV recording.
- [`docs/testing.md`](docs/testing.md) — Testing architecture, unit/integration inventory, 7-phase long audit suite, and compiler optimization pipeline.
- [`CHANGELOG.md`](CHANGELOG.md) — Project release history, version milestones, and feature additions.

---

## 🙏 Credits & Acknowledgments

- **Steven Atkinson** — Creator of [Neural Amp Modeler (NAM)](https://github.com/sdatkinson/neural-amp-modeler) for revolutionizing open-source deep learning guitar modeling.
- **The PipeWire Project** (Wim Taymans and contributors) — For creating the high-performance Linux multimedia routing subsystem that makes ultra-low latency audio accessible to everyone.
- **ToneHunt Community** — For hosting and curating thousands of exceptional, free neural amp models for guitarists worldwide.

---

## ⚖️ License & AI Transparency

### AI Transparency Note

The software architecture, real-time safety guarantees, DSP pipeline design, and microarchitectural optimizations are intellectual work (and passion) of the maintainer (**Fábio Lima**). Implementation and continuous refinement were accelerated through pair programming (*Vibe Coding*) using cutting-edge artificial intelligence models (Gemini, Claude, Grok, DeepSeek and others) within Google Antigravity IDE. AI is an amplifier of human intent, transforming ambitious vision into robust software.

### License

This project is licensed under the **GNU General Public License v3.0 or later** (**GPL-3.0-or-later**). See [LICENSE.txt](LICENSE.txt) for complete license terms.

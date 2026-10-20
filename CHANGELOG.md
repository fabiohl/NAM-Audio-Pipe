<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0] - 2026-10-20

### Added

- **Native PipeWire Standalone Host:** Direct integration with `libpipewire-0.3` client API, processing audio over PipeWire multimedia graphs at quantum buffer sizes down to 64 samples (1.33 ms).
- **Core Neural Engine Integration:** Embedded [`NeuralAmpModeler-rs`](https://github.com/fabiohl/NeuralAmpModeler-rs) 1.0.0 DSP core with full support for:
  - WaveNet models (A1 and A2 standard and lite profiles).
  - LSTM models (1-layer and 2-layer recurrent network topologies).
  - ConvNet and Linear FIR architectures.
  - Multi-profile `.namb` bundles.
- **Strict Real-Time Safety Guarantees:**
  - Zero heap allocations (`malloc`, `free`, `Box`, `Vec`, `String`, `Arc`) on the audio callback thread.
  - Zero mutexes, `RwLock`s, or blocking system calls on the audio loop.
  - SPSC lock-free ringbuffers and command channels with asynchronous 3-tier Garbage Collection (GC) off-RT.
  - Zero `log::*` invocations on the hot-path, utilizing atomic bitmasks (`RtStatusFlags`) and pre-allocated SPSC log rings.
  - Panic containment via `std::panic::catch_unwind` guards around RT callbacks, preventing process crashes and unfinalized recordings.
- **Cabinet IR UPOLS Convolution:**
  - Partitioned FFT convolution engine for cabinet impulse responses (`.wav`).
  - Configurable partition sizes (`32`, `64`, `128`, `256` samples) via `--cabsim-partition`, allowing custom balance between algorithmic latency and CPU utilization.
  - Optional cabinet loading mode (`--cab-optional`) to warn and continue without cab-sim if an IR is missing or unloadable.
- **Asynchronous 32-Bit Float Stereo WAV Recording:**
  - Powered by Linux `io_uring` (`tokio-uring`) without thread contention or hot-path latency spikes.
  - Automatic 4 GiB RIFF file segmentation (`_partN.wav`).
  - Noise gate silence trimming: only enqueues audio blocks when the silence gate is open.
  - Periodic WAV header sync every 3 seconds for crash durability.
  - Free disk space monitoring and low-space warnings (1 GiB / 5 min thresholds).
  - Clean finalization on termination signals (`SIGINT`, `SIGTERM`) with extended grace window (5 s).
- **Anti-Aliasing Polyphase Oversampling:**
  - Half-band polyphase FIR oversampling (`2x`, `4x`, and `off`) around the non-linear neural inference stage.
- **Adaptive Compute (Auto-Slimming):**
  - Host CPU pressure detection dynamically downgrades multi-profile `.namb` models (`--slim auto`) to prevent buffer underruns during CPU spikes.
- **Production SIMD Vectorization:**
  - Strict `x86-64-v3` architecture baseline (AVX2, FMA, BMI2) achieving < 3% DSP load of a 1.33 ms quantum.
- **Linux Low-Latency OS & Kernel Tuning:**
  - PM QoS DMA latency pinning to 0 µs (`/dev/cpu_dma_latency`).
  - Dynamic `/proc/interrupts` IRQ profiling with automatic audio thread pinning to the least-interrupted CPU core.
  - Memory locking via `mlockall(MCL_CURRENT | MCL_FUTURE)`.
  - Transparent Huge Pages (THP) disablement (`PR_SET_THP_DISABLE`).
- **Resilient Backend Lifecycle:**
  - Bounded automatic reconnection engine handling PipeWire daemon restarts (`systemctl --user restart pipewire`), suspend/resume cycles, and interface unplugging with progressive exponential backoff (6 attempts, 250 ms → 4 s, ~11.75 s budget).
  - Fail-fast mode (`--fail-fast`) and configurable reconnect budget (`--reconnect-timeout`).
- **Command-Line Interface (CLI):**
  - Polymorphic silence gate (`--gate MODE`): `on` (-70 dBFS default with 10 dB Schmitt hysteresis), `off` (pass-through), or explicit dBFS threshold (`[-96, -20] dBFS`).
  - Input staging (`-i, --input-gain`) and output staging (`-o, --output-gain`) from -20.0 to +20.0 dB.
  - Activation precision selection: `standard` (exact) vs `fast` (Padé polynomial approximations).
  - Diagnostic bundles (`--diagnose` and `--diagnose-full`) for technical support and troubleshooting.
  - Classified exit codes (`0` Clean, `1` Forced/generic, `2` Usage, `3` Model, `4` Cab IR, `5` Backend, `6` Recording).
- **Multi-Stage Compiler Optimization Pipeline (`utils/build-release.sh`):**
  - Profile-Guided Optimization (PGO) over multi-topology neural workloads.
  - Machine code basic block reordering via LLVM BOLT.
  - Automated generation of filtered DSP hotspot reports (`target/dsp_hotpath.asm`).
- **Flatpak Distribution (`io.github.fabiohl.NAMAudioPipe`):**
  - Standalone desktop application targeting `org.freedesktop.Platform` 25.08.
  - Scoped low-latency audio permissions (`--socket=pipewire`, `--share=ipc`, `--device=all`, `--filesystem=home:ro`, `--filesystem=xdg-music`).
  - XDG desktop integration and complete hicolor icon theme hierarchy (SVG + 64x64 to 512x512 PNGs).

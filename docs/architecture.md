<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# NAM-Audio-Pipe Architecture: Low-Latency PipeWire Host

This document serves as the technical architecture reference and system specification for **NAM-Audio-Pipe**, a low-latency, real-time standalone audio host for [Neural Amp Modeler (NAM)](https://www.neuralampmodeler.com/) simulation on Linux using native PipeWire multimedia graphs.

NAM-Audio-Pipe embeds [`NeuralAmpModeler-rs`](https://github.com/fabiohl/NeuralAmpModeler-rs) as its core DSP engine, providing complete model execution (WaveNet, LSTM, ConvNet, Linear), cabinet impulse response (IR) convolution, polyphase sample rate conversion, and anti-aliasing oversampling.

> **Core Philosophy**: Source code is the primordial source of truth. This document defines the high-level system topology, threading model, memory layout, real-time safety contracts, SPA latency accounting, and external system integrations that cannot be expressed within code comments alone.

---

## 1. System Topology & Dual-Stream PipeWire Architecture

In PipeWire's multimedia graph, an audio capture sink (`Audio/Sink`) processes incoming signals, but its monitor port delivers audio *before* DSP processing. To route processed sound to hardware playback devices or patchbays without creating feedback loops, NAM-Audio-Pipe implements a **dual-stream architecture** coupled through an internal lock-free shared memory bridge:

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│                           PipeWire Multimedia Graph                         │
└──────┬──────────────────────────────────────────────────────────────▲───────┘
       │ Audio Input (Guitar DI / Mic)                                │ Processed Output
       ▼                                                              │
┌──────────────────────────────┐              ┌───────────────────────────────┐
│     Capture Stream / Node    │              │    Playback Stream / Node     │
│   (NAM-Audio-Pipe-input)     │              │   (NAM-Audio-Pipe-playback)   │
│   - Node: "Audio/Sink"       │              │   - Node: "Stream/Output/Audio"│
│   - Group:                   │              │   - Group:                    │
│     "nam-audio-pipe-dsp"     │              │     "nam-audio-pipe-dsp"      │
│   - Runs DSP on Audio Thread │              │   - Reads from DspBridge      │
└──────────────┬───────────────┘              └───────────────▲───────────────┘
               │                                              │
               │ (capture_dsp_pipeline_streaming)             │ (on_process)
               ▼                                              │
       ┌──────────────────────────────────────────────────────────────┐
       │                   DspBridge (Shared Memory)                  │
       │  - Double-Buffered Scratch Arrays (buf_l, buf_r)             │
       │  - Lock-Free Atomic Read/Write Generation Synchronization    │
       │  - madvise(MADV_DONTFORK | MADV_DONTDUMP)                    │
       │  - #[repr(C, align(128))] Cache-Line Isolation               │
       └──────────────────────────────┬───────────────────────────────┘
                                      │
                                      │ (If --record active)
                                      ▼
                       ┌──────────────────────────────┐
                       │   SPSC Recording Pool        │
                       │    (Descriptor / 64 KiB Slot)│
                       └──────────────┬───────────────┘
                                      │
                                      ▼
                       ┌──────────────────────────────┐
                       │  Disk I/O Worker Thread      │
                       │   - tokio-uring (io_uring)   │
                       │   - Async 32-bit Float WAV   │
                       │   - 4 GiB RIFF Auto-Split    │
                       └──────────────────────────────┘
```

### 1.1 Registered Nodes & Stream Identity

All PipeWire node names, stream descriptions, and thread loop identifiers are defined as typed constants in [`src/standalone/pw_host/identity.rs`](../src/standalone/pw_host/identity.rs):

| PipeWire Entity     | Registered Node Name      | Node Description                  | Media Class / Role                            | Constant in `identity.rs`                         |
|:------------------- |:------------------------- |:--------------------------------- |:--------------------------------------------- |:------------------------------------------------- |
| **Capture Node**    | `NAM-Audio-Pipe-input`    | `NAM-Audio-Pipe Input`            | `Audio/Sink` (Virtual capture sink)           | `PW_CAPTURE_NODE_NAME` / `PW_CAPTURE_NODE_DESC`   |
| **Capture Stream**  | `NAM-Audio-Pipe`          | Primary capture audio stream      | Receives incoming hardware/application audio  | `PW_CAPTURE_STREAM_NAME`                          |
| **Playback Node**   | `NAM-Audio-Pipe-playback` | `NAM-Audio-Pipe Processed Output` | `Stream/Output/Audio` (Audio playback source) | `PW_PLAYBACK_NODE_NAME` / `PW_PLAYBACK_NODE_DESC` |
| **Playback Stream** | `NAM-Audio-Pipe-Output`   | Processed audio output stream     | Emits processed audio to hardware/patchbay    | `PW_PLAYBACK_STREAM_NAME`                         |

### 1.2 Graph Scheduling Synchronization & Control Plane Wakeup

To eliminate inter-thread synchronization jitter, both streams are bound to identical scheduling groups:

- `node.group = "nam-audio-pipe-dsp"` (`PW_NODE_GROUP`)
- `node.link-group = "nam-audio-pipe-link-group"` (`PW_LINK_GROUP`)

This configuration instructs the PipeWire graph scheduler to execute both capture and playback callbacks synchronously on the same real-time driver loop thread (`nam-audio-pipe-loop`, `PW_THREAD_LOOP_NAME`).

Furthermore, coordination between the PipeWire `ThreadLoop` and the main process loop is managed by [`ControlPlaneWakeup`](../src/standalone/pw_host/wakeup.rs). Rather than relying on unconditional sleep intervals, state changes, format renegotiations, and shutdown events trigger instant condition variable notifications with a bounded fallback timeout ($\le 100\text{ ms}$), achieving near-zero latency control plane responsiveness without system call overhead on the audio thread.

---

## 2. Memory Architecture & Lock-Free Bridge (`DspBridge`)

The shared memory bridge ([`src/standalone/pw_host/bridge.rs`](../src/standalone/pw_host/bridge.rs) and `neural_amp_modeler_rs::dsp::pipeline::DspBridge`) coordinates zero-copy, lock-free audio transfer between the capture callback (which executes neural inference) and the playback callback (which delivers processed buffers to hardware output).

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│                          DspBridge Memory Layout                            │
├─────────────────────────────────────────────────────────────────────────────┤
│  #[repr(C, align(128))] buffers: [UnsafeCell<BridgeBuffer>; 2]              │
│    - BridgeBuffer 0: buf_l: [f32; 8192], buf_r: [f32; 8192], n_samples, gen │
│    - BridgeBuffer 1: buf_l: [f32; 8192], buf_r: [f32; 8192], n_samples, gen │
├─────────────────────────────────────────────────────────────────────────────┤
│  Writer-Owned Line (128 bytes):                                             │
│    - active_read_idx: AtomicUsize                                           │
│    - generation:      AtomicU64                                             │
│    - dropped_frames:  AtomicU32                                             │
│    - _pad_writer:     [u8; 108]                                             │
├─────────────────────────────────────────────────────────────────────────────┤
│  Reader-Owned Line (128 bytes):                                             │
│    - consumed_gen:    AtomicU64                                             │
│    - _pad_reader:     [u8; 120]                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 2.1 Double Buffering & Cache Isolation

- **Zero-Copy Double Buffering**: `DspBridge` contains two independent `BridgeBuffer` slots (`MAX_BRIDGE_BUF = 8192` samples). Each slot is wrapped in `UnsafeCell` to provide interior mutability without creating aliasing `&mut DspBridge` references on the real-time hot path, ensuring full compliance with Stacked Borrows and Tree Borrows pointer provenance rules.
- **Cache-Line Isolation (`#[repr(C, align(128))]`)**: Writer and reader atomic control fields are segregated into distinct 128-byte cache lines with explicit byte padding (`_pad_writer: [u8; 108]`, `_pad_reader: [u8; 120]`). This completely eliminates false sharing and cache-line bouncing between CPU cores, including 128-byte line architectures such as Apple Silicon and modern x86 server microarchitectures.
- **Dynamic Page Alignment & Kernel Advisories**: Bridge memory is allocated via `calculate_bridge_layout`, dynamically querying the runtime system page size (`sysconf(_SC_PAGESIZE)`, accommodating 4, 16, and 64 KiB pages). Both base address and allocation length are padded to exact multiples of the page size, ensuring that Linux kernel memory advisories operate cleanly on dedicated memory pages:
  - `madvise(MADV_DONTFORK)`: Eliminates Copy-on-Write duplication overhead if child processes are spawned.
  - `madvise(MADV_DONTDUMP)`: Excludes large scratch buffers from core dumps to conserve disk space and speed crash analysis.

---

## 3. Threading Model & Hard Real-Time Contracts

NAM-Audio-Pipe enforces strict thread segregation across three isolated execution domains:

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│                                Main Thread                                  │
│  - CLI Parsing & Model/IR Loading                                           │
│  - PipeWire Initialization & Shutdown Lifecycle                             │
│  - Off-RT Resampler / CabSim Dynamic Rebuilds                               │
│  - Housekeeping & SPSC Garbage Collection Drain (drain_gc_channels)         │
│  - Telemetry Polling & Control Plane Coordination (ControlPlaneWakeup)      │
└──────┬──────────────────────────────┬───────────────────────────────┬───────┘
       │                              │                               │
       │ SPSC Command Channels        │ Atomic Telemetry              │ SPSC Audio Ring
       ▼                              ▼                               ▼
┌─────────────────────────────┐┌──────────────────────────────┐┌──────────────┐
│  Audio Thread (RT Callback) ││       RtStatusFlags          ││ Recording I/O│
│  - PipeWire thread_loop     ││  - Sample Rate & Latency     ││  - tokio-uring│
│  - Strict RT Safety Contract││  - CPU Cycles per Quantum    ││  - Async Disk│
│  - Neural Inference & DSP   ││  - Gate State & Overloads    ││  - Idle Sleep│
└─────────────────────────────┘└──────────────────────────────┘└──────────────┘
```

### 3.1 Hard Real-Time Safety Contracts (Audio Thread)

The audio processing callback executes under strict real-time determinism:

1. **Zero Heap Allocations**: All scratch buffers, resamplers, layer weights, and convolution engines are pre-allocated during startup or swapped via single-producer single-consumer (SPSC) pointers.
2. **Zero Mutex Locks**: Inter-thread coordination uses wait-free SPSC channels (`rtrb`) and atomic memory primitives.
3. **Zero Blocking I/O**: Filesystem access, standard output printing, and direct logging macros (`log::*`) are prohibited in the audio callback.
4. **Panic Containment**: Callback bodies are wrapped in `catch_unwind(AssertUnwindSafe)`. Unhandled panics latch the fatal atomic flag `RT_STATUS_PANIC_CAPTURED` (bit 31) and mute output, allowing the main control loop to initiate a graceful shutdown and flush pending WAV files instead of aborting the process.

---

### 3.2 Backend State Machine, Fail-Fast SLA & Bounded Reconnect

The PipeWire host lifecycle is governed by a thread-safe backend state machine ([`src/standalone/pw_host/status.rs`](../src/standalone/pw_host/status.rs)):

```mermaid
stateDiagram-v2
    [*] --> Starting
    Starting --> Running: Streams Active
    Starting --> Failed: Connection Error (Exit 5)
    Running --> Degraded: Non-Fatal Anomaly (Format Mismatch)
    Degraded --> Running: Recovered
    Running --> Reconnecting: Daemon Loss / Stream Error
    Degraded --> Reconnecting: Daemon Loss
    Reconnecting --> Running: Reconnected (Within Budget)
    Reconnecting --> Failed: Budget Exhausted (Fail-Fast)
    Failed --> Terminated: Ordered Teardown (WAV Finalized)
    Running --> Terminated: SIGINT / SIGTERM / SIGHUP
```

#### Key Lifecycle Invariants

- **State Representation**: The FSM defines explicit variants: `Starting`, `Running`, `Degraded { reason: String }`, `Reconnecting { attempt: u32, total_attempts: u32, next_backoff: Duration }`, `Failed { stream: &'static str, reason: String }`, and `Terminated`.
- **Sticky Failure**: Once marked `Failed`, the state cannot revert to `Running` except through a formal reconnection handshake (`begin_reconnect`). The main control loop polls this status via an atomic fast-path and `ControlPlaneWakeup`, initiating teardown immediately (fail-fast SLA $< 200\text{ ms}$, with $\le 100\text{ ms}$ fallback timeout).
- **DSP State Preservation**: `CaptureState` (neural models, resamplers, cabinet IRs, gains) and RT-side SPSC channels reside in heap `Box` allocations reached via raw pointers outside stream closures. Re-instantiating PipeWire streams after a daemon restart preserves all internal processing state.
- **Bounded Reconnect Budget** ([`src/standalone/pw_host/reconnect.rs`](../src/standalone/pw_host/reconnect.rs)):
  - Production default: 6 attempts (`PRODUCTION_MAX_ATTEMPTS`) with progressive exponential backoff (250 → 500 → 1000 → 2000 → 4000 → 4000 ms), capped at an 11.75-second ceiling per recovery episode.
  - When `--fail-fast` is passed, reconnection is disabled entirely (`max_attempts = 0`).
  - `--reconnect-timeout <s>` allows operators to tune the total budget.
- **Stability Window Reset (60 s)**: After 60 seconds (`STABLE_RESET`) of uninterrupted healthy streaming, the attempt budget is fully replenished. Transient drops over prolonged sessions never accumulate to trigger an unintended process exit.

---

### 3.3 Termination Signal Handling & Grace Windows

Process termination is unified in [`src/standalone/signals.rs`](../src/standalone/signals.rs):

- **Signal Interception**: Installs an async-signal-safe handler for `SIGINT` (Ctrl+C), `SIGTERM` (systemd/containers), `SIGHUP` (terminal window closure), and `SIGQUIT`.
- **Atomic Grace Window**:
  - The first signal sets the process-global `SHUTDOWN` atomic flag and initiates cooperative teardown.
  - Normal grace window is 500 ms (`DEFAULT_GRACE_WINDOW_MS`); when recording is active, `main()` extends this window to 5,000 ms (`RECORDING_GRACE_WINDOW_MS`) to allow in-flight WAV headers to rewrite and flush cleanly (`fsync`).
  - Signals arriving within the grace window are ignored to avoid interrupting cleanup. Signals arriving *after* the window expires escalate immediately to `_exit(1)`.
- **Broken Pipe Handling**: CLI outputs (`--help`, `--diagnose`) use `cli::write_stdout`, which tolerates `EPIPE` when piped to pagination utilities (`head -n 1`) without panicking.
- **Color Discipline**: ANSI escape sequences are automatically disabled when standard output is redirected to files or pipes, respecting `NO_COLOR` and `CLICOLOR_FORCE`.

---

### 3.4 Runtime Error Taxonomy & Exit Codes

Failures are categorized into distinct exit codes ([`src/standalone/exit.rs`](../src/standalone/exit.rs)) to facilitate script automation:

| Exit Code | Classification   | `ExitKind` Variant    | Trigger Conditions                                                         | User Remediation Hint                               |
|:---------:|:---------------- |:--------------------- |:-------------------------------------------------------------------------- |:--------------------------------------------------- |
| **0**     | Clean Success    | —                     | Normal process termination via signal or `--help`/`--diagnose`.            | None (operation completed successfully).            |
| **1**     | Generic / Forced | `EXIT_GENERIC`        | Uncategorized failure or hard signal escalation after timeout.             | Inspect application error logs.                     |
| **2**     | Argument Error   | `ExitKind::Usage`     | Malformed CLI flags, invalid buffer size, or missing files.                | Run with `-h` or `--help` to review syntax.         |
| **3**     | Model Load Error | `ExitKind::Model`     | Corrupt, missing, or unsupported `.nam`/`.namb` file format.               | Verify model file path and architecture support.    |
| **4**     | Cabinet IR Error | `ExitKind::CabIr`     | Unreadable or malformed cabinet impulse response `.wav` file.              | Check IR file format or pass `--cab-optional`.      |
| **5**     | Backend Error    | `ExitKind::Backend`   | Unreachable PipeWire daemon or stream negotiation failure.                 | Verify PipeWire daemon status (`systemctl --user`). |
| **6**     | Recording Error  | `ExitKind::Recording` | Unwritable directory, `io_uring` failure, or overruns (`SuccessWithLoss`). | Check disk permissions and space availability.      |

---

## 4. Linux Real-Time & Kernel Tuning Layer (`rt_setup`)

To guarantee glitch-free audio processing at quantum sizes down to 64 samples (1.33 ms deadline at 48 kHz), NAM-Audio-Pipe integrates with the engine's canonical `neural_amp_modeler_rs::rt_hardening` infrastructure ([`src/standalone/rt_setup/`](../src/standalone/rt_setup/)):

### 4.1 CPU Core Affinity & Housekeeping Confinement

- **Optimal Core Selection**: Automatically inspects CPU topology (`/sys/devices/system/cpu/`) and streaming `/proc/interrupts` data to identify isolated, non-boot CPU cores with minimal IRQ load.
- **Housekeeping Confinement**: Binds auxiliary threads (main control loop, recording worker, PipeWire driver loop) to housekeeping cores, leaving the dedicated real-time core exclusive to audio processing.
- **IRQ Advisories**: Issues warnings if device interrupt vectors share the selected audio core.

### 4.2 Power Management QoS & Memory Locking

- **PM QoS (`/dev/cpu_dma_latency`)**: Pins CPU DMA latency to 0 µs, preventing power management from transitioning execution cores into deep sleep states (C-states: C1, C6, C8).
- **Memory Paging Lock (`mlockall`)**: Locks the application's virtual address space into physical RAM (`MCL_CURRENT | MCL_FUTURE`), preventing page faults and swap disk I/O.
- **Transparent Huge Pages (THP) Disablement**: Disables THP via `prctl(PR_SET_THP_DISABLE, 1, 0, 0, 0)` to prevent kernel compaction daemon (`khugepaged`) latency spikes.
- **Hardware Sink Auto-Detection**: Queries `pw-metadata` with a 500 ms watchdog timeout to locate the default audio output sink, filtering out loopback nodes to prevent routing feedback.

### 4.3 Host Calibration Guidelines (Kernel Command Line)

For production deployment on dedicated low-latency machines, configure `/etc/default/grub` with core isolation parameters (example for dedicated core 8 and SMT sibling 9 on a 16-thread CPU):

```text
GRUB_CMDLINE_LINUX_DEFAULT="isolcpus=managed_irq,domain,8,9 nohz_full=8,9 rcu_nocbs=8,9 irqaffinity=0-7,10-15 nmi_watchdog=0 skew_tick=1"
```

```bash
# Recommended host preparation commands:
echo 0 | sudo tee /sys/devices/system/cpu/cpu9/online                 # Disable hyperthreaded sibling
echo performance | sudo tee /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor
echo 0 | sudo tee /sys/devices/system/cpu/cpufreq/boost              # Eliminate dynamic boost jitter
ulimit -r 95                                                         # Grant RT FIFO scheduling
ulimit -l unlimited                                                  # Grant physical memory locking
```

### 4.4 Target Architectures & Platform Support (x86_64 & Linux on ARM)

NAM-Audio-Pipe is engineered natively for the Linux multimedia ecosystem, with explicit architectural handling across CPU architectures:

| Platform / Architecture      | Status                            | Continuous Integration Gate                                                          | Real-Time & Hardware Certification                                                                                                                              |
|:---------------------------- |:--------------------------------- |:------------------------------------------------------------------------------------ |:--------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Linux `x86_64`**           | Reference Platform (Tier 1)       | Blocking (`x86_64-linux-lints`, `x86_64-linux-quick`, `msrv`)                        | Full hardware certification, soak tests, RT deadline/jitter auditing, and PGO + LLVM BOLT optimization.                                                         |
| **Linux on ARM (`aarch64`)** | Theoretically Compatible (Tier 2) | Blocking (`linux-arm64-lints` on `ubuntu-26.04-arm`), advisory (`linux-arm64-quick`) | Compiles cleanly and is architecturally compatible. Formal bare-metal RT deadline/jitter certification and audio interface testing have not yet been performed. |

#### Architectural Provisions for Linux on ARM

1. **Target-Scoped Compiler Flags**: In [`.cargo/config.toml`](../.cargo/config.toml), architecture-specific flags (such as `-Ctarget-cpu=x86-64-v3`) are strictly gated under `[target.'cfg(target_arch = "x86_64")']`. Linker hardening flags (`-Wl,-z,now`, `-Wl,--gc-sections`, `-Wl,--as-needed`) apply uniformly to all Linux targets, preventing undefined CPU flags during ARM compilation.
2. **C-ABI Variadic Log Interception (AAPCS64)**: PipeWire's SPA logging functions use C variadics. Under AAPCS64 (`aarch64`), `va_list` arguments are passed by value, whereas SysV (`x86_64`) passes them via register-save area pointers. Pure-Rust logging in [`src/standalone/pw_host/log_redirect.rs`](../src/standalone/pw_host/log_redirect.rs) conditionally binds `SpaVaList` to `pipewire::spa::sys::va_list` on `aarch64` and handles direct `vsnprintf` formatting without external C bridges.
3. **Continuous Integration on Native ARM Runners**: GitHub Actions executes `linux-arm64-lints` on native `ubuntu-26.04-arm` runners as a blocking gate (validating `cargo check`, `cargo clippy`, and code formatting), alongside advisory `linux-arm64-quick` test suites.
4. **Formal Testing Scope**: While the codebase is architecturally sound, compiles cleanly, and is theoretically compatible on Linux ARM64 platforms, formal real-time hardware validation (such as `tests-long.sh` RT-jitter and deadline phases on bare-metal ARM audio interfaces) has not yet been formally conducted.

---

## 5. Lock-Free SPSC Channels & 3-Tier GC Cascade

Communication between the non-RT main thread and the real-time audio thread is coordinated through bounded single-producer single-consumer (`rtrb`) queues:

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│                           SPSC Channel Topology                             │
├─────────────────────────────────────────────────────────────────────────────┤
│  param_channel:     Consumer<ParamPayload>       (Gains, Bypass, Modes)     │
│  resampler_channel: Consumer<ResamplerSwapPayload> (Off-RT Rebuilt Buffers) │
│  cabsim_channel:    Consumer<CabSimSwapPayload>  (Off-RT Rebuilt IRs)       │
│  slimmable_channel: Consumer<Box<SlimModelPair>> (A2 Submodels)             │
│  os_channel:        Consumer<Box<OsEnginePair>>  (Oversampling Engines)     │
│  gc_channel:        Producer<GcItem>             (Replaced Asset Dealloc)   │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 5.1 Three-Tier Garbage Collection Cascade

When models, resamplers, or impulse responses are replaced during playback, dropping complex heap structures directly on the audio thread would trigger allocator locks. Deallocation cascades through three tiers:

```text
RT Thread (Replaced Asset)
       │
       ▼
[Tier 1: SPSC gc_tx (32 slots)] ──────► Drained by Main Thread (periodic housekeeping)
       │ (if full)
       ▼
[Tier 2: RT Parking Lot (16 slots)] ──► Flushed to SPSC on next block;
       │                                Drained during orderly teardown
       │ (if full)
       ▼
[Tier 3: GcOverflowBuffer Atomic Ring] ──► Controlled Leak + Sets RT_STATUS_GC_OVERFLOW
```

1. **Tier 1 (SPSC GC Queue)**: Discarded assets are pushed to `gc_tx` (32 slots) and deallocated by the main thread during housekeeping calls to `drain_gc_channels`.
2. **Tier 2 (RT Parking Lot)**: If the queue is momentarily full, items are stored in a fixed array on the RT thread (`parking_lot: [Option<GcItem>; 16]`) and retried on subsequent blocks.
3. **Tier 3 (GC Overflow Buffer)**: If both tiers are saturated, items are converted into packed 64-bit representations (56-bit pointer + 8-bit type ID) and deposited into an atomic overflow ring (`GcOverflowBuffer`). If packed conversion is unavailable, the memory is safely leaked to guarantee Zero-Heap-Drop, raising the diagnostic flag `RT_STATUS_GC_OVERFLOW`.

---

## 6. DSP Pipeline, Latency Accounting & SPA Semantics

The core processing pipeline executes neural simulation and cabinet filtering across incoming audio blocks in [`capture_dsp_pipeline_streaming`](../src/standalone/pw_host/rt_callback/process.rs):

```text
Audio Input (PipeWire Host Buffer)
       │
       ▼
[Stage 1: Input Stage & Cleanup]
  - Gain LUT scaling + Denormal FTZ/DAZ enforcement
  - Noise Gate FSM (DynamicHysteresis)
  - Closed Gate Path: Causal CabSim IR Tail Drain (reverb preservation)
       │
       ▼
[Stage 2: Neural Inference & Streaming Resampler]
  - Polyphase Resampling (if host_rate != model_rate)
  - Polyphase Half-Band Oversampling (Off / 2× / 4×)
  - Core Neural Network Inference (WaveNet, LSTM, ConvNet, Linear)
  - Downsampling Decimation stages
       │
       ▼
[Stage 3: CabSim Partitioned FFT Convolution]
  - UPOLS Frequency-Domain Filtering (Host sample rate)
  - Partition size decoupled from quantum (32, 64, 128, 256 frames)
       │
       ▼
[Stage 4: Output Gain Stage & Envelope Protection]
  - Output Gain LUT scaling + Gate envelope smoothing
       │
       ▼
[Stage 5: Final Delivery to DspBridge] ──► Audio Output (PipeWire Playback Stream)
```

### 6.1 Static Buffer Limits & Negotiation

To guarantee deterministic memory bounds, all components enforce strict maximum limits:

| Constant / Limit       | Boundary Constraint               | Architectural Enforcement                                                                          |
|:---------------------- |:--------------------------------- |:-------------------------------------------------------------------------------------------------- |
| `MAX_BRIDGE_BUF`       | 8192 samples                      | `DspBridge` width; callbacks reject larger descriptors fail-closed.                                |
| `MAX_RESAMP_BUF`       | 8192 samples                      | Bounds maximum memory allocation for resamplers and oversampling engines.                          |
| CabSim Partition Size  | Clamped to `[16, MAX_RESAMP_BUF]` | CLI `--cabsim-partition` domain `{32, 64, 128, 256}` (default `128`), decoupled from host quantum. |
| `--buffer-size` Domain | `{0} ∪ {2^k \| 16 ≤ 2^k ≤ 8192}`  | Validated at CLI parsing; invalid values reject before PipeWire graph connection.                  |

### 6.2 Adaptive Compute & Slimmable Submodel Swaps

When executing slimmable neural architectures (e.g. A2 models with dynamic layer pruning), real-time load is monitored by `AdaptiveCompute`:

1. **Triggering**: If processing cycles exceed the configured threshold, `try_slimmable_rebuild` raises `RT_STATUS_NEEDS_SLIMMABLE_REBUILD` and stores the target channel in `requested_slimmable_ch`, incrementing `requested_slimmable_generation` with `Ordering::Release`.
2. **Off-RT Reconstruction**: The main thread detects the request, builds the replacement `SlimModelPair` on a background thread without audio interruption, and pushes it to `slimmable_channel`.
3. **Atomic RT Swap**: `drain_slimmable_models` executes `SlimmableSwapHandler` under an engine scheduler budget (1 structural swap per callback), atomically replacing active left and right models (`install_pair`) and routing old models to the GC cascade as `GcItem::SlimModelPair`.

---

### 6.3 Algorithmic Latency Publication & Seqlock Snapshot

Latency compensation in the PipeWire graph requires accurate, lock-free publication of all algorithmic DSP delays:

1. **Oversampling Conversion**: `OversampleEngine::latency_samples` returns native filter latency at model rate. The function `convert_os_latency_to_host(os_native, host_rate, model_rate)` scales this delay to host sample units with integer ceiling:
   $$\text{os\_host} = \left\lceil \frac{\text{os\_native} \times \text{host\_rate}}{\text{model\_rate}} \right\rceil$$
2. **Coherent Seqlock Snapshot**: Algorithmic latencies (CabSim partition, resampler group delay, oversampling filter latency) are updated atomically by the audio thread and synchronized via a wait-free sequence lock (`latency_sequence: AtomicU64` in `StreamStatusFlags`):
   - The audio writer increments `latency_sequence` with `Ordering::Release` before and after updating latency fields.
   - Non-RT watchers in `run.rs` read the snapshot via `read_applied_latencies`, validating consistency before publishing updates to PipeWire via `pw_stream_set_parameter`.

---

### 6.4 PipeWire SPA Latency Semantics (`rate` vs `ns`)

Latency parameters are published to the PipeWire graph using the SPA POD API (`SPA_PARAM_ProcessLatency` and `SPA_PARAM_Latency`) in [`src/standalone/pw_host/latency.rs`](../src/standalone/pw_host/latency.rs):

```rust
builder.add_property(SPA_LATENCY_min_rate, latency_samples);
builder.add_property(SPA_LATENCY_max_rate, latency_samples);
builder.add_property(SPA_LATENCY_min_ns, latency_ns);
builder.add_property(SPA_LATENCY_max_ns, latency_ns);
```

#### Additive Delay Proof (PipeWire 1.6.2 Engine)

Inspection of installed system headers (`spa/param/latency.h`) and PipeWire source code (`src/pipewire/stream.c`, function `pw_stream_get_time_n`) demonstrates how PipeWire calculates total graph delay:

```c
/* PipeWire src/pipewire/stream.c lines 980-990 */
time->delay += (latency.min_rate + latency.max_rate) / 2;
time->delay += ((latency.min_ns + latency.max_ns) / 2) * (int64_t)time->rate.denom
             / ((int64_t)SPA_NSEC_PER_SEC * time->rate.num);
```

- **Additive Behavior**: PipeWire does **not** evaluate `rate` and `ns` as fallback alternatives; it evaluates both terms and sums them into `time->delay`.
- **Double Compensation Risk**: When `latency_samples` is written to `min_rate`/`max_rate` and the exact same duration is converted to nanoseconds and written to `min_ns`/`max_ns`, PipeWire sums the two, doubling the reported delay in graph time calculations.
- **Canonical DSP Emission**: For sample-based synchronous DSP nodes running at graph rate, the canonical configuration is:
  $$\text{min\_rate} = \text{max\_rate} = \text{latency\_samples}, \quad \text{min\_ns} = \text{max\_ns} = 0$$
  The `ns` parameter is reserved for fixed-time asynchronous components (e.g. USB packet intervals or Bluetooth frame scheduling) operating independently of graph sample rates.

#### Starvation Success vs. SPA Validation Failure

The playback callback strictly distinguishes expected pipeline starvation from host protocol violations:

- **Bridge Starvation (Success Path)**: When the capture stream produces no new block during a quantum (due to paused input, active resampler rebuild, or clock drift), playback dequeues the buffer, validates descriptors, and delivers coherent analytical silence with `offset = 0`, `size = silence_bytes`, `stride = 4`. This is a normal operational condition and must never be reported as an SPA contract violation.
- **Malformed SPA Descriptors (Validation Failure)**: A malformed descriptor (null or unaligned pointers, non-cardinal buffer sizes, out-of-bounds windows, asymmetric frame counts, or overlapping left/right channels) represents a host contract violation. The callback sets `RT_STATUS_HOST_CONTRACT_VIOLATION` (`E2304`), zeroes provably writable memory regions, and stamps `size = 0` to prevent downstream nodes from reading uninitialized memory.

#### Operator Verification Protocol

To inspect published graph latency on a live system:

```bash
# Inspect node latency published to PipeWire:
pw-dump | jq '.[] | select(.info.props["node.name"] == "NAM-Audio-Pipe-playback") | .info.params.Latency'

# Monitor real-time graph scheduling delay:
pw-top
```

---

## 7. Asynchronous WAV Recording Subsystem (`src/recording/`)

When launched with `--record`, NAM-Audio-Pipe captures 32-bit float stereo WAV files without impacting real-time determinism:

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│                           Recording Architecture                            │
├─────────────────────────────────────────────────────────────────────────────┤
│  Audio Thread (RT)                                                          │
│    - Checks gate state (pass-through when gate is off)                      │
│    - Audio: try_acquire() slot → fill_planar in place → publish()           │
│      (preallocated pool, 4-byte descriptor; zero allocation on RT)          │
│    - Control: Metadata / StreamStop on a dedicated small control ring       │
├─────────────────────────────────────────────────────────────────────────────┤
│  Recording Worker Thread ("nam-recording-io")                               │
│    - tokio-uring async event loop (Linux io_uring engine)                   │
│    - Consumes descriptors → writes 64 KiB blocks IN PLACE → release()       │
│    - Applies metadata barriers at exact FIFO positions                      │
│    - Reuses internal I/O buffers to eliminate runtime heap allocations      │
│    - Automatically splits files at 4 GiB RIFF limits (_partN.wav)           │
│    - Checkpoints WAV headers every 3 seconds to prevent data loss on crash  │
│    - Finalizes header byte count and issues fsync on clean shutdown         │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 7.1 Preallocated Slot Pool Transport

Audio blocks travel through a preallocated memory pool ([`src/recording/pool.rs`](../src/recording/pool.rs), 256 slots × ~64 KiB ≈ 16.8 MiB) rather than moving data by value across SPSC rings:

- The RT thread acquires a slot index from a free-list ring, writes samples in place (`fill_planar`), and publishes a 4-byte descriptor.
- The I/O worker writes the block directly from memory to disk using `io_uring` and releases the slot index back to the free list.
- **Performance Benefit**: Reduces memory subsystem traffic by ~57% (measured A/B p99 latency gain up to 96% on small quantum sizes).

### 7.2 Recording Reliability Invariants

- **Full Quantum Capacity**: The recording block supports up to `MAX_BLOCK_SIZE = 16384` samples (8192 stereo frames), persisting the largest host quantum without drops.
- **Accurate Overrun Accounting**: When buffer pool starvation occurs, `OVERRUN_COUNT` (blocks) and `OVERRUN_FRAMES_COUNT` (frames) increment atomically. The process exits with code 6 (`SuccessWithLoss`) if loss occurred, preserving the reconciliation invariant: `captured_frames == enqueued_frames + lost_frames`.
- **Periodic Header Checkpoints**: The disk worker rewrites the WAV header and invokes `fsync` every 3 seconds (`CHECKPOINT_INTERVAL_SECS`). An ungraceful system termination loses at most 3 seconds of audio rather than rendering the entire recording unreadable.
- **Proactive Disk Monitoring**: Queries filesystem capacity via `statvfs`, logging remaining recording time and emitting alerts if free space falls below 1 GiB or 5 minutes of audio.

---

## 8. Diagnostic System & PipeWire Log Redirection

Diagnostic capabilities are split between on-demand bundles and low-overhead runtime telemetry:

```bash
# Capture full diagnostic bundle and exit immediately
nam-audio-pipe --diagnose

# Capture diagnostic bundle with full unredacted filesystem paths
nam-audio-pipe --diagnose-full
```

### 8.1 Runtime Telemetry Discipline

- **Bundle-Free Control Loop**: During active streaming, telemetry logs surface as concise `[Exxxx | MNEMONIC]` lines. Retrospective log traces (`Recent Log Trace`) are excluded from normal logs and reserved exclusively for explicit `--diagnose` invocations or crash dumps.
- **Episode Latches**: Anomaly flags (GC overflow, deadline overruns, playback starvation) are latched per episode (`TelemetryLatches`), warning at most once per incident rather than spamming every polling cycle.
- **Consolidated Session Banner**: Key session parameters (model, cabinet IR, quantum, sample rate, latencies, CPU scheduler) are printed once after the first processed audio block.

### 8.2 Error Taxonomy (`NamErrorCode`)

Typed error codes are managed by `neural_amp_modeler_rs::common::diagnostics::NamErrorCode`:

| Code Range | Category              | Canonical Error Identifiers                                                                                                                                                                                                                                                                                                                                                                                                                                   |
|:---------- |:--------------------- |:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `E1xxx`    | Model, IR & CLI Setup | `E1100` FILE_NOT_FOUND, `E1101` FILE_READ_ERROR, `E1102` UNKNOWN_EXTENSION, `E1200` NAM_JSON_PARSE_ERROR, `E1201` NAMB_CRC32_MISMATCH, `E1202` NAMB_INVALID_MAGIC, `E1203` NAMB_UNSUPPORTED_VERSION, `E1204` NAMB_TRUNCATED, `E1214` NAM_JSON_INVALID_SAMPLE_RATE, `E1220` NAM_JSON_INVALID_ENVELOPE, `E1300` UNSUPPORTED_ARCHITECTURE, `E1303` MODEL_BUILD_FAILED, `E1308` SEQUENTIAL_CHANNEL_MISMATCH, `E1400` WAV_INVALID_FORMAT, `E1401` WAV_INVALID_FILE |
| `E2xxx`    | Audio & Real-Time     | `E2001` PROCESSING_OVERLOAD, `E2100` AUDIO_INIT_FAILED, `E2101` STREAM_ERROR, `E2200` RESAMPLER_BUILD_FAILED, `E2201` RESAMPLER_CHANNEL_FULL, `E2202` INVALID_CABSIM_PARTITION_SIZE, `E2300` RT_PRIORITY_DENIED, `E2301` CPU_AFFINITY_FAILED, `E2302` BACKEND_FAILURE, `E2304` HOST_FORMAT_CONTRACT_VIOLATION                                                                                                                                                 |
| `E3xxx`    | SPSC / Lock-Free DSP  | `E3100` PARAM_CHANNEL_FULL, `E3101` GC_OVERFLOW, `E3102` GC_CORRUPTED, `E3103` INVALID_SPSC_CAPACITY                                                                                                                                                                                                                                                                                                                                                          |
| `E4xxx`    | Runtime & CLI         | `E4100` INVALID_GAIN_VALUE, `E4101` UNKNOWN_COMMAND, `E4102` CTRL_C_HANDLER_FAILED, `E4103` IR_LOAD_FAILED                                                                                                                                                                                                                                                                                                                                                    |
| `E5xxx`    | System Resources      | `E5000` OUT_OF_MEMORY, `E5001` UNSUPPORTED_CPU_ARCHITECTURE                                                                                                                                                                                                                                                                                                                                                                                                   |

### 8.3 Pure-Rust PipeWire Log Redirection (Rust 1.99 Variadics)

PipeWire dispatches internal messages through its Simple Plugin API (SPA) logging interface using C-ABI variadic functions (`printf`-style `...` arguments). Leveraging **Rust 1.99** stabilized C variadics, NAM-Audio-Pipe implements a 100% pure-Rust logging interceptor ([`src/standalone/pw_host/log_redirect.rs`](../src/standalone/pw_host/log_redirect.rs)) with **zero external C shims**:

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│                          PipeWire / SPA Core Logging                        │
└──────────────────────────────────────┬──────────────────────────────────────┘
                                       │ Direct C-ABI Dispatch
                                       ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│              Pure-Rust SPA Log Methods Table (log_redirect.rs)              │
│  - nam_pw_log / nam_pw_logt (Variadic '...' signatures)                     │
│  - nam_pw_logv / nam_pw_logtv (Platform va_list signatures)                 │
└──────────────────────────────────────┬──────────────────────────────────────┘
                                       │
                                       ▼ (Fixed 1024-byte Stack Buffer vsnprintf)
┌─────────────────────────────────────────────────────────────────────────────┐
│                      RT-Safe In-Place Formatting Path                       │
│  - Zero heap allocations, zero locks, zero memory allocations               │
└──────────────────────────────────────┬──────────────────────────────────────┘
                                       │
                                       ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                     Routing Gate & Off-RT Queue                             │
│  - Data Thread (rt_log_mark TLS): Enqueues to ~344 KiB SPSC log ring        │
│  - Non-RT Threads: Dispatches directly to Rust log::* facade                │
└─────────────────────────────────────────────────────────────────────────────┘
```

#### Off-RT Log Ring Invariant

If PipeWire internals emit logs from the real-time audio thread, synchronous dispatch to `log::*` would violate real-time safety (logging facade mutex locks and formatting allocations).

To prevent this:

- The audio data thread is identified via a thread-local marker (`rt_log_mark`).
- Log events on the marked thread are written to a preallocated SPSC ring queue (~344 KiB).
- An off-RT drainer thread consumes the queue and replays records through standard logger channels.
- If the ring queue overflows, dropped messages are recorded in `pw_log_dropped` without blocking the audio thread.

#### Platform ABI Variadics (`x86_64` vs `aarch64`)

PipeWire's `spa_log_methods.logv` and `logtv` dispatch platform-specific `va_list` types. In [`src/standalone/pw_host/log_redirect.rs`](../src/standalone/pw_host/log_redirect.rs), the type alias `SpaVaList` is conditioned per target:

- On `x86_64` (System V ABI): Passed as a pointer (`*mut pipewire::spa::sys::__va_list_tag`).
- On `aarch64` (AAPCS64 ABI): Passed by value (`pipewire::spa::sys::va_list`).

This enables identical zero-overhead variadic formatting into the fixed 1024-byte stack buffer across both CPU architectures without external C wrappers.

---

## 9. Flatpak Packaging & Sandbox Architecture

NAM-Audio-Pipe is distributed as a standalone desktop Flatpak application targeting `io.github.fabiohl.NAMAudioPipe` on runtime `org.freedesktop.Platform//26.08`:

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│                           Flatpak Application Sandbox                       │
│                        (io.github.fabiohl.NAMAudioPipe)                     │
├─────────────────────────────────────────────────────────────────────────────┤
│  Binary: /app/bin/nam-audio-pipe                                            │
│  DSP Loop: Hard RT Determinism, SPSC Queues, DspBridge Double Buffering     │
│  Model Loader: Reads from ~/ via --filesystem=home:ro                       │
└──────┬──────────────────────────────┬───────────────────────────────┬───────┘
       │                              │                               │
       │ Socket: pipewire             │ POSIX Shared Memory           │ PM QoS Access
       ▼                              ▼                               ▼
┌─────────────────────────────┐┌──────────────────────────────┐┌───────────────┐
│    PipeWire Host Daemon     ││  Zero-Copy Audio Buffers     ││ Linux Kernel  │
│  - Driver loop scheduling   ││  - memfd_create (--share=ipc)││ - 0 µs C-state│
│  - Node graph auto-routing  ││  - Direct page mapping       ││ - DMA latency │
└─────────────────────────────┘└──────────────────────────────┘└───────────────┘
```

### 9.1 Sandbox Permissions Rationale

- **`--filesystem=xdg-run/pipewire-0` & `--socket=pulseaudio`**: Provides direct Unix domain socket access to the host PipeWire daemon (`$XDG_RUNTIME_DIR/pipewire-0`) for native quantum scheduling down to 64 samples (1.33 ms), and compatibility with PulseAudio audio systems.
- **`--share=ipc`**: Grants access to `memfd_create` for zero-copy SPA buffer sharing between the sandbox and the host daemon.
- **`--device=all`**: Grants access to `/dev/snd/*` devices and `/dev/cpu_dma_latency` for real-time PM QoS configuration.
- **`--filesystem=home:ro`**: Allows loading neural models (`.nam`/`.namb`) and cabinet IRs (`.wav`) from the user's home directory with read-only protection.
- **`--filesystem=xdg-music`**: Allows writing audio captures to `$XDG_MUSIC_DIR/NAM-Audio-Pipe` without filesystem permission errors.

### 9.2 Developer Inspection & Build Commands

```bash
# 1. Build optimized release binary and Flatpak bundle, audit, and install:
./utils/build-release.sh --strict --package
./utils/tests-long.sh --strict --artifact
./utils/build-release.sh --strict --certify --install

# 2. Inspect active sandbox permissions:
flatpak info --show-permissions io.github.fabiohl.NAMAudioPipe

# 3. Validate AppStream metainfo XML and desktop launcher:
appstreamcli validate --no-net --strict packaging/flatpak/io.github.fabiohl.NAMAudioPipe.metainfo.xml
desktop-file-validate packaging/flatpak/io.github.fabiohl.NAMAudioPipe.desktop

# 4. Run containerized diagnostics:
flatpak run io.github.fabiohl.NAMAudioPipe --diagnose
```

---

## 10. References

- [NAM-Audio-Pipe Testing Architecture](testing.md) — Comprehensive test suite layout and verification gates.
- [`NeuralAmpModeler-rs`](https://github.com/fabiohl/NeuralAmpModeler-rs) — Core neural amplifier DSP inference engine.
- [PipeWire Official Documentation](https://docs.pipewire.org/) — Linux low-latency multimedia routing framework.
- [Linux io_uring Documentation](https://kernel.dk/io_uring.pdf) — High-performance asynchronous I/O framework.

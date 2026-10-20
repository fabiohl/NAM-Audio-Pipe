<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# Description of Changes
<!-- Provide a clear, concise summary of what was changed and the technical rationale behind it. -->

## Linked Issues
<!-- e.g. Fixes #123, Closes #456, or Relates to #789 -->
Fixes #

## Subsystem & Scope
<!-- Check all that apply -->
- [ ] **PipeWire Host & Stream Lifecycle** (`src/standalone/pw_host/`, dual-stream setup, reconnection)
- [ ] **Real-Time Audio Loop & DSP Bridge** (`src/standalone/pw_host/rt_callback/`, `src/standalone/pw_host/bridge.rs`, SPSC GC, panic containment)
- [ ] **CLI & Option Parsing** (`src/standalone/cli.rs`, arguments, silence gate parsing, exit codes)
- [ ] **Asynchronous WAV Recording** (`src/recording/`, io_uring worker, slot pool transport, 4 GiB split)
- [ ] **Real-Time System Setup & Telemetry** (`src/standalone/rt_setup/`, CPU affinity, PM QoS, status polling)
- [ ] **Documentation, Packaging & Scripts** (`docs/`, `packaging/`, `utils/`, `README.md`)

---

## Real-Time Audio Safety Checklist (RT-Safe)
<!-- The PipeWire audio callback thread runs at high priority under SCHED_FIFO. RT safety is non-negotiable. -->

- [ ] **Zero Dynamic Heap Allocations in Audio Path**: The audio callback (`process()`, inner DSP stages) makes zero calls to the heap allocator (`malloc`, `Box`, `Vec`, `String`, `Arc::new()`, `format!()`).
- [ ] **Lock-Free Concurrency**: No mutexes, RwLocks, or blocking thread synchronization primitives exist on the audio thread.
- [ ] **Zero Blocking I/O**: No filesystem access, network I/O, or synchronous output (`println!`, `eprintln!`) in the audio callback.
- [ ] **Zero `log::*` on Hot-Path**: No `log::*` macros are executed inside the audio process loop (status signaled via atomic bitmasks `RtStatusFlags`).
- [ ] **Panic Elimination**: No `unwrap()` or `expect()` on the audio hot-path; loops structured for static bounds-check elision.

---

## PipeWire & Linux Low-Latency Integration

- [ ] **Quantum / Buffer Handling**: Correctly handles arbitrary PipeWire quanta (e.g. 32, 64, 128, 256 samples) and sample rate renegotiations.
- [ ] **Real-Time Scheduling & Resource Management**: Correctly utilizes SCHED_FIFO priorities and handles memory locking (`mlockall` / HugeTLB) without leaking resources.

---

## Pre-Submission Verification Suite (Mandatory)
<!-- Run these verification scripts from the repository root before opening or marking PR ready for review: -->

```bash
utils/mem-guard.sh utils/lints.sh        # Static analysis, fmt, clippy (-D warnings), SPDX validation
utils/tests-quick.sh                     # Agile testing, PipeWire mock streams, CLI tests (auto-contained)
```

- [ ] **`utils/lints.sh` Passed**: 100% clean across all feature permutations (`--all-features`, `--no-default-features`).
- [ ] **`utils/tests-quick.sh` Passed**: All unit and integration tests pass cleanly without regressions.
- [ ] **License & SPDX Headers**: All new and modified files include the GPL-3.0-or-later SPDX header and copyright notice:

  ```text
  SPDX-License-Identifier: GPL-3.0-or-later
  Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
  ```

- [ ] **Subproject Self-Containment**: No references or links escape the repository root.
- [ ] **Undocumented Clippy Allows**: Any `#[allow(clippy::...)]` attribute includes an explanatory justification comment on the preceding line.

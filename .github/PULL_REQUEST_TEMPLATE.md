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
<!-- Check all that apply. For documentation, CLI, or packaging PRs, RT/PipeWire checklists below may be marked N/A. -->
- [ ] **PipeWire Host & Stream Lifecycle** (`src/standalone/pw_host/`, dual-stream setup, reconnection)
- [ ] **Real-Time Audio Loop & DSP Bridge** (`src/standalone/pw_host/rt_callback/`, `src/standalone/pw_host/bridge.rs`, SPSC GC, panic containment)
- [ ] **CLI & Option Parsing** (`src/standalone/cli.rs`, arguments, silence gate parsing, exit codes)
- [ ] **Asynchronous WAV Recording** (`src/recording/`, io_uring worker, slot pool transport, 4 GiB split)
- [ ] **Real-Time System Setup & Telemetry** (`src/standalone/rt_setup/`, CPU affinity, PM QoS, status polling)
- [ ] **Documentation, Packaging & Scripts** (`docs/`, `packaging/`, `utils/`, `README.md`)

---

## Real-Time Audio Safety Checklist (RT-Safe)
<!-- Applicable if modifying audio processing paths. Skip or mark N/A for CLI, documentation, or packaging PRs. -->

- [ ] **N/A**: This PR does not touch audio callback code.
- [ ] **Zero Dynamic Heap Allocations in Audio Path**: The audio callback (`process()`, inner DSP stages) makes zero calls to the heap allocator (`malloc`, `Box`, `Vec`, `String`, `Arc::new()`, `format!()`).
- [ ] **Lock-Free Concurrency**: No mutexes, RwLocks, or blocking thread synchronization primitives exist on the audio thread.
- [ ] **Zero Blocking I/O**: No filesystem access, network I/O, or synchronous output (`println!`, `eprintln!`) in the audio callback.
- [ ] **Zero `log::*` on Hot-Path**: No `log::*` macros are executed inside the audio process loop (status signaled via atomic bitmasks `RtStatusFlags`).
- [ ] **Panic Elimination**: No `unwrap()` or `expect()` on the audio hot-path; loops structured for static bounds-check elision.

---

## PipeWire & Linux Low-Latency Integration
<!-- Applicable if modifying PipeWire stream setup or real-time scheduling. Skip or mark N/A otherwise. -->

- [ ] **N/A**: This PR does not touch PipeWire stream setup or scheduling.
- [ ] **Quantum / Buffer Handling**: Correctly handles arbitrary PipeWire quanta (e.g. 32, 64, 128, 256 samples) and sample rate renegotiations.
- [ ] **Real-Time Scheduling & Resource Management**: Correctly utilizes SCHED_FIFO priorities and handles memory locking (`mlockall` / HugeTLB) without leaking resources.

---

## Verification & Quality Gates (Local & GitHub CI)
<!--
Responsible Verification Model:
- Fast Local Feedback: Run cargo check and targeted tests for quick local iteration.
- GitHub CI Automation: The full test matrix (x86_64, Linux ARM64, MSRV 1.99.0, sibling engine integration, and isolated PipeWire daemon tests) runs automatically on every push and PR.
- Friendly Tip for Contributors: If you don't have PipeWire development headers or the sibling engine configured locally, open a DRAFT PR and let GitHub Actions run the full verification matrix for you!
-->

### 1. Recommended Local Checks (Fast Feedback)

```bash
# Fast check during development:
cargo check --all-targets

# Targeted test for changed components (example):
cargo test --lib <module>
```

### 2. Optional Full Local Suite (If environment allows)

```bash
utils/lints.sh        # Static analysis, fmt, clippy (-D warnings), SPDX, AppStream
utils/tests-quick.sh  # Quick discipline against isolated test daemon
```

- [ ] **Targeted Local Checks Passed**: Code compiles cleanly without warnings (`cargo check --all-targets`) and unit tests for modified components pass.
- [ ] **Full Local Suite Executed (Optional)**: Successfully ran `utils/lints.sh` and/or `utils/tests-quick.sh` (or deferring full matrix to GitHub CI via Draft PR).
- [ ] **Automated GitHub CI**: All checks pass on GitHub Actions (`x86_64-linux-lints`, `x86_64-linux-quick`, `linux-arm64-lints`, `linux-arm64-quick`, `msrv`).
- [ ] **License & SPDX Headers**: All new and modified files include the GPL-3.0-or-later SPDX header and copyright notice:

  ```text
  SPDX-License-Identifier: GPL-3.0-or-later
  Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
  ```

- [ ] **Subproject Self-Containment**: No references or links escape the repository root.
- [ ] **Undocumented Clippy Allows**: Any `#[allow(clippy::...)]` attribute includes an explanatory justification comment on the preceding line.

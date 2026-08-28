<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# Automated Test Suite & Verification Architecture — `NAM-Audio-Pipe`

This document details the automated test suite, integration harness, verification scripts, and compiler optimization pipeline for **NAM-Audio-Pipe** ([`../`](../)).

---

## 1. Scope & Crate Features Taxonomy

`NAM-Audio-Pipe` testing relies on feature flags configured in [`Cargo.toml`](../Cargo.toml):

| Feature Flag  | Description                                                                                            | Test Usage Scope                                                |
|:------------- |:------------------------------------------------------------------------------------------------------ |:--------------------------------------------------------------- |
| **`default`** | Standard empty feature set for standalone production builds.                                           | Standard release builds and baseline compilation checks.        |
| **`testing`** | Propagates `NeuralAmpModeler-rs/testing`, enabling internal fixtures, mock controls, and test helpers. | Mandatory flag when executing unit and integration test suites. |

---

## 2. Automated Test Inventory

The test suite is partitioned into unit tests co-located with module sources and integration tests under [`tests/`](../tests/):

### 2.1 Unit Tests (Module-Level)

- **CLI Parsing ([`src/standalone/cli_test.rs`](../src/standalone/cli_test.rs)):** Validates command-line flag combinations, model and cabinet path parsing, gain limits (`-20.0` to `+20.0` dB), oversampling enum mappings, and diagnostic flags (`--diagnose`, `--diagnose-full`). Since G-RB-003 / T6.1 it also covers the `--buffer-size` domain contract (`{0} ∪ {2^k | 4 ≤ k ≤ 13}`): `validate_buffer_size` accepts `0, 16, 32, …, 8192` and rejects `1, 2, 8, 15, 63, 100, 500, 8193, 16384, 65536, u32::MAX` with the typed `BufferSizeError` (below-minimum / above-maximum / not-power-of-two, each with an explanatory `Display` message and `std::error::Error`), plus parse-level acceptance of `-b 0` (auto) and `-b 8192`.
- **Real-Time System Setup & Hardware Sink Detection ([`src/standalone/rt_setup/rt_setup_test.rs`](../src/standalone/rt_setup/rt_setup_test.rs), [`src/standalone/rt_setup/pm_qos.rs`](../src/standalone/rt_setup/pm_qos.rs)):** Validates CPU affinity selection heuristics (`select_optimal_cpu`), interrupt load parsing from `/proc/interrupts` streaming per physical CPU (`parse_interrupts_per_cpu`), PM QoS `/dev/cpu_dma_latency` interaction, and default hardware sink detection via `pw-metadata` with a 500 ms watchdog timeout and self-capture node filtering.
- **Telemetry & Status Polling ([`src/standalone/rt_setup/telemetry_test.rs`](../src/standalone/rt_setup/telemetry_test.rs)):** Validates `PollState` struct lifecycle, hugepage flag synchronization, telemetry throttling, silent and fading transition states, and diagnostic RT status flag clearing during periodic monitoring (`poll_rt_status`).
- **Shared Memory Bridge Allocation ([`src/standalone/pw_host/bridge.rs`](../src/standalone/pw_host/bridge.rs)):** Validates page-aligned (4096-byte) memory layout allocation, `madvise` memory flags, and initial atomic generation counters in `allocate_dsp_bridge`.
- **PipeWire Capture Stream Setup ([`src/standalone/pw_host/capture/setup_test.rs`](../src/standalone/pw_host/capture/setup_test.rs)):** Tests capture property dictionary creation, latency string formatting (`node.latency`), and SPA Pod audio format generation.
- **Process Callback & Gain Staging ([`src/standalone/pw_host/rt_callback/process_test.rs`](../src/standalone/pw_host/rt_callback/process_test.rs)):** Tests the RT audio processing callback, channel extraction, gain multiplier smoothing, noise gate envelope evaluation, recording queue dispatch, and silence trimming.
- **SPSC Resource Swaps & Command Handling ([`src/standalone/pw_host/rt_callback/commands.rs`](../src/standalone/pw_host/rt_callback/commands.rs), [`src/standalone/pw_host/rt_callback/cabsim_swap.rs`](../src/standalone/pw_host/rt_callback/cabsim_swap.rs), [`src/standalone/pw_host/rt_callback/resampler_swap.rs`](../src/standalone/pw_host/rt_callback/resampler_swap.rs)):** Validates zero-alloc dynamic swaps for neural models, cabinet IR convolution engines (`drain_cabsims`), sample rate converters (`drain_resamplers`), and oversampling engines (`drain_os_engines`), asserting correct GC cascades and RT parking lot dirty flag updates.
- **Off-RT Rebuild Handlers ([`src/standalone/pw_host/handlers_test.rs`](../src/standalone/pw_host/handlers_test.rs)):** Validates dynamic resampler reconstruction, cabinet IR convolution updates, slimmable container swaps, and oversampling filter rebuilds triggered by sample rate changes.
- **Backend State Machine & Fail-Fast ([`src/standalone/pw_host/status_test.rs`](../src/standalone/pw_host/status_test.rs), F-RB-010 / T4.4):** Validates `BackendState`/`SharedBackendStatus` lifecycle transitions (Starting → Running → Degraded/Failed → Terminated), the sticky-failure invariant (`mark_running`/`mark_degraded` are no-ops after `mark_failed`), the `observe_stream_state` mapping for capture/playback (`StreamState::Error` and post-streaming `Unconnected` → Failed; initial `Unconnected` → no-op; `Streaming` → Running), and the fail-fast SLA: a control-loop poller mirroring `run.rs` observes a `mark_failed` transition and exits in under 200 ms. F-RB-010 / T4.5 additions: `begin_reconnect` clears the sticky failure and publishes the observable `Reconnecting { attempt, total_attempts, next_backoff }` transition, and a successful reconnection returns the backend to `Running` while a second daemon death is observed as a fresh failure.
- **Bounded Reconnect Policy & Cycle ([`src/standalone/pw_host/reconnect_test.rs`](../src/standalone/pw_host/reconnect_test.rs), F-RB-010 / T4.5):** Validates `ReconnectPolicy` (production defaults: 3 attempts with progressive 250 → 500 → 1000 ms exponential backoff; `fail_fast()` disables reconnection entirely), the strict time ceiling (`total_backoff_budget` = 1750 ms) and the non-blocking guarantee (saturating arithmetic — the schedule can never wrap to zero nor overflow, for any attempt number). `ReconnectCycle` hands out exactly `max_attempts` backoffs and then `None` forever — a daemon that stays inaccessible exhausts the budget and the caller fails fast (T4.4), while a momentary drop is recovered with the carried DSP state intact.
- **Recording Worker Guard & Observable Join ([`src/recording/guard_test.rs`](../src/recording/guard_test.rs)):** Validates the RAII teardown of `nam-recording-io` (F-RB-009 / T3.5): premature-drop cleanup with bounded join, bounded retry on `push_stream_stop`, join-timeout detection, formal inspection of the worker `Result` and panic payloads, and the ordered StreamStop → producer drop → join shutdown.

### 2.2 Integration Test Suites ([`tests/`](../tests/))

- **CLI Black-Box Smoke Tests ([`tests/e2e_cli.rs`](../tests/e2e_cli.rs)):** Executes the compiled binary (`CARGO_BIN_EXE_nam-audio-pipe`) using `std::process::Command` to verify CLI help screens, diagnostic output formatting, and robust error handling for invalid or out-of-range options. G-RB-003 / T6.1 negative acceptances: `-b 100`, `-b 1` and `-b 16384` exit non-zero with an explanatory `Argument error` on stderr *before any PipeWire connection* (validated by asserting the message is absent of PipeWire output).
- **Asynchronous WAV Recording Suite ([`tests/recording.rs`](../tests/recording.rs)):**
  - `disk_writer_loop_creates_valid_wav`: Asserts that `disk_writer_loop` consumes ring buffer audio blocks and produces valid 32-bit float stereo WAV files.
  - `disk_writer_loop_metadata_then_stream_stop_creates_empty_wav`: Validates clean zero-sample WAV creation upon immediate stream stop.
  - `disk_writer_loop_discards_audio_before_metadata`: Verifies uninitialized audio blocks before stream metadata are safely discarded.
  - `disk_writer_loop_fails_fast_on_missing_output_dir` / `disk_writer_loop_fails_fast_on_file_as_output_dir`: Fail-fast startup handshake under unusable output directories (F-RB-009 / T3.3).
  - `record_e2e_pipewire_wav_header_matches_bytes` (R-13): Full end-to-end recording test under live PipeWire, asserting that the finalized WAV `data` chunk header exactly matches the PCM bytes written to disk.
- **Recording Fault-Injection & Byte-by-Byte Harness ([`tests/recording_fault_injection.rs`](../tests/recording_fault_injection.rs), ER-3 / T3.6):**
  - `riff_parser_*`: Hand-rolled independent RIFF/WAVE chunk walker (no `hound`) validating `fmt `, `fact` (when present) and `data` structure, cross-checked against `hound` on real files.
  - `wav_byte_exact_sine_noise_ramp_roundtrip` / `wav_metadata_change_splits_part2_byte_exact`: Sine, deterministic LCG noise and ramp signals compared sample-by-sample (bit-exact) against the persisted PCM, including mid-stream format-change `_partN` splitting.
  - `enospc_class_failure_mid_stream_marks_failed_and_preserves_partial_wav`: `RLIMIT_FSIZE`/`EFBIG` fault fired mid-stream — observable `Failed` status, RT failure flag, error surfaced on the join and partial WAV preserved bit-exact up to the fault point.
  - `sigint_shutdown_under_high_rate_never_truncates` / `sigterm_producer_drop_drains_and_finalizes_byte_exact`: Simulated SIGINT/SIGTERM under high transfer rate must never truncate the tail (100% of samples persisted).
  - `concurrent_workers_same_dir_atomic_creation_no_clobber`: 20 concurrent instances in one directory at the same instant — every capture distinct, no clobbering (anti-TOCTOU, F-RB-008 / T3.2).
  - `recording_cycles_fd_thread_leak_sweep`: 100 consecutive record/stop cycles returning `/proc/self/fd` and thread counts to the baseline.
- **PipeWire Live Pipeline Integration ([`tests/pw_integration.rs`](../tests/pw_integration.rs)):** Validates the full PipeWire host lifecycle (context creation, dual-stream setup, SPSC parameter injection, real audio quantum execution `last_n_samples > 0`, and atomic shutdown) against a running PipeWire daemon. A silent-tone driver (`ToneDriver`, `pw-play --target NAM-Audio-Pipe-input`) keeps the capture node deterministically scheduled — a sink without an active stream may never process a quantum, and the tone is inaudible (silent WAV, `--volume 0`). Plus two opt-in disruptive acceptances for the bounded reconnect cycle (F-RB-010 / T4.5), run only with `NAM_DAEMON_BOUNCE_TEST=1` (serialized via `TEST_MUTEX`, `#[ignore]`d by default; a staged `restore_pipewire_group` recovers the daemon, pulse bridge and `wireplumber` on drop — even on panic):
  - `test_pipewire_bounded_reconnect_recovers_audio_after_daemon_restart`: restarts the user's PipeWire daemon (`systemctl --user restart pipewire`) mid-session and proves the host re-instantiated its streams and resumed DSP — the fresh capture sink re-registers in the graph and the reconnected stream clock (`capture_host_ticks`) advances again; no loss of models/IRs/recording state.
  - `test_pipewire_reconnect_exhaustion_terminates_with_error`: stops the daemon + socket + pulse bridge (the minimal set that keeps it down without crashing `wireplumber`), proving the bounded budget (3 attempts × progressive backoff) is exhausted and the host terminates cleanly with an error (fail-fast fallback).
  - `test_pipewire_fail_fast_stream_error_terminates_within_sla` (F-RB-010 / T4.4 / T4.6): black-box subprocess acceptance of the **compiled binary** under `--fail-fast` — a forced backend failure (daemon stop) must tear the host down and exit **non-zero inside the SLA** (never a zombie alive without audio, never a graceful 0); exercises the real process exit code that the in-process acceptances cannot.
- **Service Resilience Harness ([`tests/service_resilience.rs`](../tests/service_resilience.rs), ER-4 / T4.6):** Certifies the ER-4 gates. Two live subprocess acceptances (Phase 3, `#[ignore]`d):
  - `sigterm_subprocess_finalizes_wav_gracefully`: spawns the real `nam-audio-pipe --record` binary under live PipeWire, drives the capture sink with a deterministic tone, sends a real `SIGTERM` via `libc::kill` and proves the child exits **0** while the WAV on disk is 100% readable, carries a valid header with a **closed** `data` chunk and **bit-exact** finite samples (two independent readers — a hand-rolled RIFF walker and `hound` — must decode the exact same bits, and the declared `data` size must equal the file tail).
  - `double_signal_force_exits_via_exit1`: two rapid `SIGTERM`s must force immediate termination via the async-signal-safe `_exit(1)` path (exit code 1, not 0 and not a signal death).
  And three daemon-free acceptances that run in every quick pass (Phases 1 & 2):
  - `bridge_starvation_emits_analytical_silence_and_recycles_buffers` (G-RB-001 / T4.2): the playback starvation kernel must emit bit-exact `0.0f32` analytical silence over 100% of both output extensions, stamp/recycle the SPA chunks (`offset=0`, `size=frames×4`, `stride=4`) and never stall — soaked over 2000 quantums with honest xrun telemetry (`playback_bridge_starvation` advanced, no fabricated `output_buffer_miss`, no contract flag).
  - `spa_format_rejection_signals_contract_violation_fail_closed` (G-RB-001 / T4.3): mono, interleaved `F32`, `S16` and surround renegotiations must be rejected with a typed `ContractViolation`, raise `RT_STATUS_HOST_CONTRACT_VIOLATION` and latch the RT mute guard (`format_contract_ok == 0`) — fail-closed — then re-arm on a valid `F32P` stereo renegotiation.
  - `stream_error_observable_and_shutdown_within_sla` (F-RB-010 / T4.4): a forced `StreamState::Error` must publish a **sticky** `Failed` transition observable by the main control loop inside the < 500 ms SLA; the full lifecycle (initial `Unconnected` ≠ failure, `Streaming` → `Running`, post-streaming `Unconnected` → `Failed`, sticky-failure, bounded-reconnect back to `Running`) is proven.
- **Extended Soak Harness ([`tests/soak_extended.rs`](../tests/soak_extended.rs), G-RB-002 / T6.4):** Long-duration endurance battery executed exclusively by the long-audit suite (`utils/tests-long.sh`, Phase 1) with all tests `#[ignore]`d and single-threaded:
  - `test_soak_100k_multichannel_swaps`: 100.000 continuous audio blocks (≈ 2,5 min of accelerated-swap audio) firing thousands of simultaneous swaps — neural models (WaveNet, LSTM, Linear with known gain), stereo CabSim IRs and sample-rate calibration, oversampling factors (`Off`/`X2`/`X4`) and continuous input/output gain variation. Periodic linear-window validation: no channel inversion, no gain asymmetry, no undue silence.
  - `test_soak_rss_memory_stability`: `VmRSS` (`/proc/self/status`) captured at block 1.000 and 100.000 must show zero post-warmup memory drift beyond the OS page margin (< 64 KiB).
  - `swap_soak_extended_concurrent_stress`: 10× concurrent soak (40.000 swaps / 120.000 callbacks) closing the Phase 1 concurrency gate.
  The deterministic builders and synthetic signal factories shared with `tests/swap_stress.rs` live in [`tests/common/swap.rs`](../tests/common/swap.rs) (single source of truth — no drift between harnesses).
- **RT Metrics Harness ([`tests/rt_metrics.rs`](../tests/rt_metrics.rs), G-RB-002 / T6.5):** Nanosecond determinism battery (`libc::clock_gettime(CLOCK_MONOTONIC_RAW)` — immune to NTP), all `#[ignore]`d and exclusive to the long-audit suite (Phases 3/4/5). Three gates:
  - `rt_deadline_gate_10k_quantums`: 10.000 consecutive quantums of real DSP load (WaveNet A1 + A2 + stereo CabSim active); budget `(N/SR)·10⁹·0.85`; metrics min/mean/p99/max; fail-closed typed marker `TEST_RESULT[rt_deadline]=PASS max_ns=... budget_ns=... margin_pct=...` or `=GAP:uncalibrated_environment` on a noisy host (`NAM_RT_STRICT=1` promotes any GAP to a hard failure).
  - `rt_jitter_gate_10k_callbacks`: 10.000 callbacks at the nominal cadence under 6 contention threads; inter-callback dispersion max/p99/std-dev; typed `TEST_RESULT[rt_jitter]=PASS max_jitter_us=... p99_jitter_us=...` or `=GAP:cpu_not_isolated` (Rollback path).
  - `concurrent_state_model_checking_16_threads`: 16 threads racing swap requests, `observe_stream_state` transitions, simulated reconnect cycles and cooperative `SHUTDOWN`; asserts zero deadlocks, no inconsistent sample-rate reads and no leaked failure state; typed `TEST_RESULT[model_check]=PASS threads=16 window_ms=3000 callbacks=... swaps_requested=...`.
- **Distribution QA & Release Audit Harness ([`tests/distribution_qa.rs`](../tests/distribution_qa.rs), ER-5 / T5.6):** Certifies the ER-5 gates — every QA/test/optimization/packaging gate operates fail-closed and no silent failure, undocumented skip or invalid metadata is accepted. Runs in every quick pass (Phases 1 & 2). Four acceptance groups:
  - *(a) AppStream structural validation:* the metainfo XML (`packaging/flatpak/io.github.fabiohl.NAMAudioPipe.metainfo.xml`) is parsed with a strict XML parser (quick-xml, end-tag/comment checking on) and must be well-formed, carry exactly one `<release>` whose `version` equals the crate version, and have the mandatory `date` attribute. Negative tests prove that malformed documents — including the duplicated `</release>` closing tag fixed in F-RB-012 and truncated (unclosed-root) documents — are detected and rejected.
  - *(b) Typed test-receipt validator:* a fail-closed libtest-log parser (mirroring `utils/_lib.sh::assert_ran_target` and the Phase 4 `TEST_RESULT[...]=SKIP:` contract) must detect a removed/renamed mandatory target (no `Running <target> ` banner), a target section that executed zero tests (100% `#[ignore]` selection), and free-text `SKIP:` markers that are not typed `TEST_RESULT[...]=SKIP:reason` — none of these may ever produce PASS.
  - *(c) Provenance integrity:* `target/logs/release-provenance.json` (F-RB-014 / T5.5) is read and every artifact referenced under `artifacts` must exist on disk with a computed SHA-256 and size matching the recorded values byte-for-byte. A missing receipt is a *typed* skip (`TEST_RESULT[provenance_integrity]=SKIP:receipt_not_found` — no release was built); a present but corrupt/stale receipt is a hard failure. Negative tests cover a vanished referenced file and a tampered hash.
  - *(d) Distribution binary smoke:* the `--profile dist` binary (`panic = "abort"`, stripped, LTO fat) — the exact artifact shipped by `utils/build-release.sh` — is exercised as a subprocess from `target/dist/nam-audio-pipe`, the installed `~/.local/bin/nam-audio-pipe`, or `$NAM_DIST_BIN`: `--diagnose` must exit 0, emit the diagnostic bundle and show no crash artifacts, and `--help` must exit 0. Absence of the artifact is a *typed* skip (`TEST_RESULT[dist_bin_smoke]=SKIP:dist_binary_not_found`).
  - *(e) ER-6 certification infrastructure audit (G-RB-002 / G-RB-003, T6.6):* the long-audit runner and its receipt are themselves certified. `utils/tests-long.sh` must be `+x` with a `GPL-3.0-or-later` SPDX header, the mandatory AI-safety governance warning (execution reserved for the human operator), all 5 canonical phases declared verbatim and the `--strict-pre-release` / `--simulate` argument parsing; `--help` must exit 0 and inventory the phases (the sanctioned read-only surface), while an unknown option is rejected fail-closed with exit code 2. A fail-closed parser for `target/logs/long-receipt.txt` validates the typed receipt format (`SUITE:`/`STRICT:`/`MODE:`/`PHASEn:`/`GAP:`/`OVERALL:`), rejects unknown lines, missing `OVERALL:` or truncated phase sets, and cross-checks the `OVERALL:` verdict against the phase/gap evidence (a `PASSED` verdict hiding a GAP is rejected). The live receipt, when present, is parsed and audited (`TEST_RESULT[long_receipt_audit]=PASS`); absence is a *typed* skip that becomes a fatal GAP under `NAM_QUICK_STRICT=1` — a green ER-6 gate always implies a valid long-suite receipt was on disk.
  The ER-5 audit skips are surfaced as typed GAPs in the quick-run receipt (and become fatal under `NAM_QUICK_STRICT=1`), so a green ER-5 gate always implies auditable release artifacts were present and validated.

---

## 3. Verification Scripts & QA Suite (`utils/`)

### 3.1 Static Analysis Quality Gate ([`utils/lints.sh`](../utils/lints.sh))

Executes comprehensive static quality checks:

1. **Code Formatting:** Enforces standard Rust formatting via `cargo fmt --all`.
2. **Compilation Matrix:** Checks all targets with `--all-features` and `--no-default-features`.
3. **Strict Clippy:** Executes `cargo clippy -- -D warnings` across all feature permutations.
4. **SPDX License Headers:** Deterministically verifies that all `.rs` and `.sh` files contain valid `GPL-3.0-or-later` or `MIT` SPDX license identifier headers.
5. **Clippy Suppression Audit:** Flags any undocumented `#[allow(clippy::...)]` attribute that lacks an explanatory comment.

### 3.2 Agile First Line of Defense ([`utils/tests-quick.sh`](../utils/tests-quick.sh))

A 4-phase agile test runner executing under low CPU/IO priority (`nice -n 19`, `ionice -c 3`) to prevent audio system disruption:

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│                       utils/tests-quick.sh Execution Flow                   │
├─────────────────────────────────────────────────────────────────────────────┤
│  Phase 1: Structural (Debug)                                                │
│    - cargo test --features testing --lib --bin nam-audio-pipe               │
│                 --test recording --test recording_fault_injection           │
│                 --test e2e_cli --test service_resilience                    │
│                 --test stereo_fidelity --test swap_stress                   │
│                 --test distribution_qa                                      │
├─────────────────────────────────────────────────────────────────────────────┤
│  Phase 2: Release Verification (Release, S6-T04 / RES-04)                   │
│    - cargo test --features testing --test recording --test e2e_cli          │
│                 --test recording_fault_injection --test service_resilience  │
│                 --test stereo_fidelity --test swap_stress                   │
│                 --test distribution_qa --release                            │
├─────────────────────────────────────────────────────────────────────────────┤
│  Phase 3: PipeWire Live Integration (Release, Daemon Probe)                 │
│    - Probes pw-cli info 0 -> Runs tests/pw_integration.rs (asserts DSP run) │
│      + tests/service_resilience.rs live subprocess acceptances (real        │
│      SIGTERM WAV finalization + double-signal _exit(1))                     │
│    - Fail-closed: Daemon failure inside test causes immediate panic         │
├─────────────────────────────────────────────────────────────────────────────┤
│  Phase 4: Recording io_uring Capability (Release, Native Probe)             │
│    - Probes src/bin/io_uring_probe -> Runs ignored recording suites         │
│      (tests/recording.rs + tests/recording_fault_injection.rs)              │
│    - Asserts WAV data header byte-size match (R-13) and ER-3 certification  │
│      battery (fault injection, byte-by-byte, SIGINT/SIGTERM, leak sweep)    │
└─────────────────────────────────────────────────────────────────────────────┘
```

- Outputs each phase log to `target/logs/quick-phaseN.log`.
- Generates a machine-readable summary receipt at `target/logs/quick-receipt.txt`.
- Supports strict mode (`NAM_QUICK_STRICT=1`) to promote any environment GAP to a hard test failure.
- ER-5 (T5.6): typed `TEST_RESULT[...]=SKIP:` markers emitted by `tests/distribution_qa.rs`
  (release artifacts absent — no release built to audit) are surfaced as typed GAPs in the
  receipt and become fatal under strict mode. A green ER-5 gate therefore implies the release
  artifacts (dist binary + provenance receipt) were present **and** validated.

---

### 3.3 Nightly / Pre-Release Long Audit Suite ([`utils/tests-long.sh`](../utils/tests-long.sh))

The companion runner for the claims the quick suite deliberately leaves out. It is a
**human-operator-only** suite (~30–50 min, requires an isolated calibrated real-time
environment: dedicated CPU affinity, low background load, optionally `SCHED_FIFO`) and
carries a mandatory AI-safety governance warning — **AI agents must never execute it
directly**; structural validation uses the non-executing surfaces (`--help`, `--simulate`)
and the ER-6 structural audit in `tests/distribution_qa.rs`.

It executes 5 isolated phases (a failure in one never interrupts the rest), each writing to
`target/logs/phaseN-*.log` and closing with a typed line appended to
`target/logs/long-receipt.txt` (`PHASEn: PASS|FAIL|GAP log=... duration_ms=...`), followed
by the `GAP:` reasons and the `OVERALL:` verdict (`PASSED` / `COMPLETED_WITH_GAPS` /
`FAILED`):

| Phase | Canonical Name | Harness / Oracle | Log |
|:----- |:-------------- |:---------------- |:--- |
| **PHASE1** | Soak prolongado & concorrência de swaps | `tests/soak_extended.rs` (T6.4: 100k multichannel swaps, RSS stability, concurrent soak) + `tests/swap_stress.rs` ignored soak battery, `--release -- --ignored --test-threads=1`; gated via `assert_ran_target` — a missing target is a hard FAIL, a 0-executed battery is a typed GAP | `phase1-soak.log` |
| **PHASE2** | RT-Safety heap-audit (zero-alloc) | `cargo test --features "testing heap-audit" --release` (lib unit gates `cabsim_swap::heap_audit_tests` + RT paths) + `tests/swap_stress.rs` heap-audit battery; oracle: `assert_eq!(get_alloc_count(), 0)` and shell greps for `swap_soak_heap_audit|heap_audit_` | `phase2-heap-audit.log` |
| **PHASE3** | RT Deadline gate (nanosecond budget) | `tests/rt_metrics.rs` deadline filter (T6.5), 10k quantums, budget `(N/SR)·10⁹·0.85`; oracle: typed `TEST_RESULT[rt_deadline]=PASS`/`GAP:...` — the shell only maps markers, never reclassifies | `phase3-rt-deadline.log` |
| **PHASE4** | RT Jitter gate (inter-callback dispersion) | `tests/rt_metrics.rs` jitter filter (T6.5), 10k callbacks under 6 contention threads; oracle: typed `TEST_RESULT[rt_jitter]=PASS max_jitter_us=... p99_jitter_us=...` | `phase4-rt-jitter.log` |
| **PHASE5** | Concurrency model checking & resilience | `tests/rt_metrics.rs` concurrent filter (T6.5), 16 threads; oracle: typed `TEST_RESULT[model_check]=PASS`/`GAP:...` | `phase5-concurrency.log` |

**Execution criteria (human operator):** run from the project root as
`./utils/tests-long.sh` (or with `--strict-pre-release` to promote every GAP to a
hard, release-blocking failure). `--simulate`/`--dry-run` registers the 5 planned phases in
`target/logs/` without executing any test — the sanctioned surface for AI/CI structural
validation. The receipt verdict is fail-closed and structurally audited by the ER-6
certification harness (`tests/distribution_qa.rs`, group (e)); an unparseable, truncated or
internally inconsistent receipt — e.g. `OVERALL: PASSED` while a GAP phase is declared —
fails the gate and blocks the closing of Épico ER-6.

---

## 4. Compiler Optimization Pipeline ([`utils/build-release.sh`](../utils/build-release.sh))

A multi-stage optimization and packaging pipeline combining Profile-Guided Optimization (PGO), LLVM BOLT, and Flatpak distribution. Since F-RB-013 / T5.3 the pipeline is **fail-closed**: an artifact is only declared PGO/BOLT-optimized with structured, cryptographic evidence:

1. **Phase 1 (Environment Verification):** Validates required tools (`rustc`, `cargo`, `python3`, `tar`, `zstd`, `flatpak`, `llvm-profdata`, `llvm-bolt`, `perf`) and verifies `x86-64-v3` compiler flags.
2. **Phase 2 (PGO Workload Profiling):** Builds `src/bin/pgo_workload.rs` with `-Cprofile-generate`, executes the deterministic multi-topology DSP workload (WaveNet A1, WaveNet A2, LSTM) with the mandatory CabSim IR fixture `tests/fixtures/models/cabsim_ir_pgo.wav` (48 kHz mono, 512 samples, synthetic exponential decay). The workload is fail-fast (any I/O/parse/model/IR failure aborts with exit 1) and emits `target/logs/pgo-workload-receipt.json` recording per-topology block counts, per-mode oversampling coverage (`Off`/`2x`/`4x`), the stereo CabSim frame counter and `no_stage_skipped`. The receipt is validated before merging: each mandatory topology must reach ≥ 1000 blocks, all oversampling modes must run, CabSim must have convolved frames and no stage may be skipped — otherwise the build aborts.
3. **Phase 3 (PGO Compilation):** Recompiles `nam-audio-pipe` using `-Cprofile-use=merged.profdata` and relocation symbols (`-Clink-arg=-Wl,-q`).
4. **Phase 4 (LLVM BOLT Layout Optimization):** Uses Linux `perf` to capture CPU branch and instruction samples, then executes `llvm-bolt` to optimize instruction cache layout and minimize TLB misses. Readiness is proven (PipeWire capture node registered + process CPU sample consumption, replacing the blind `sleep 1.0`); `perf.data` must have ≥ 8 KiB and ≥ 500 samples; `perf.fdata` must have ≥ 20 sampled entries and DSP hot-path symbol coverage; the ELF Build-ID must match the traces (`--ignore-build-id` removed). Any BOLT failure or unavailability is recorded in `target/logs/release-receipt.json` as `OPTIMIZATION: PGO-ONLY (BOLT_FAILED/BOLT_UNAVAILABLE)` with the detailed cause, and becomes fatal under `--strict-release`.
5. **Phase 4.5 (Assembly Hotspot Report):** Disassembles critical DSP loops into `target/dsp_hotpath.asm` for microarchitectural analysis.
6. **Phase 5 (Deployment):** Strips and installs the hyper-optimized binary to `~/.local/bin/nam-audio-pipe`.
7. **Phase 6 (Release Packaging):** Packages the release tarball `~/nam-audio-pipe-vx.y.z-linux-x86_64-v3.tar.zst` with installer script and documentation.
8. **Phase 7 (Flatpak Application Packaging):** Builds the standalone Flatpak bundle `~/nam-audio-pipe-vx.y.z-linux-x86_64-v3.flatpak` with low-latency PipeWire, PulseAudio, IPC permissions, and desktop metadata. Support `--install` flag for automated user installation.

**Receipts produced by the pipeline (T5.3):**
- `target/logs/pgo-workload-receipt.json` — PGO workload coverage (topologies, oversampling, CabSim stereo frames, no-stage-skipped confirmation).
- `target/logs/release-receipt.json` — release optimization status (`PGO+BOLT`, `PGO-ONLY`, `BOLT-ONLY`, `PLAIN`) with BOLT failure cause, cross-referencing the PGO receipt.

---

## 5. Execution Commands & Developer Workflow

All test commands must be executed within `./NAM-Audio-Pipe/`:

```bash
# 1. Run static analysis quality gate
./utils/lints.sh

# 2. Run consolidated quick test suite
./utils/tests-quick.sh

# 3. Run unit tests only
cargo test --features testing --lib

# 4. Run CLI smoke tests
cargo test --features testing --test e2e_cli

# 5. Run live PipeWire integration test (requires running PipeWire daemon)
cargo test --features testing --release --test pw_integration -- --ignored --nocapture

# 6. Run full recording io_uring integration test
cargo test --features testing --release --test recording -- --ignored --nocapture

# 6b. Run the recording fault-injection, byte-by-byte and leak-sweep harness
cargo test --features testing --release --test recording_fault_injection -- --ignored --test-threads=1 --nocapture

# 6c. Run the ER-4 service-resilience harness (daemon-free acceptances)
cargo test --features testing --test service_resilience

# 6d. Run the live service-resilience subprocess acceptances (real SIGTERM /
#     double-signal against the compiled binary; requires live PipeWire)
cargo test --features testing --release --test service_resilience -- --ignored --test-threads=1 --nocapture

# 6e. Run the ER-5 distribution-QA & release-audit harness (strict AppStream
#     XML, typed receipt validator, provenance integrity, dist binary smoke)
cargo test --features testing --test distribution_qa

# 6f. Long-audit suite — HUMAN OPERATOR ONLY (~30-50 min, calibrated RT env).
#     AI agents must never execute it directly; use the non-executing surfaces:
./utils/tests-long.sh --help       # usage & phase inventory
./utils/tests-long.sh --simulate   # registers the 5 planned phases + receipt
#     Full audit (human operator, calibrated machine):
./utils/tests-long.sh              # or: ./utils/tests-long.sh --strict-pre-release

# 7. Run full PGO + LLVM BOLT release build
./utils/build-release.sh
```

---

## 6. Quality Gates & Architectural Constraints

| Metric / Test Gate              | Constraint / Threshold                                        | Enforced In                                  |
|:------------------------------- |:------------------------------------------------------------- |:-------------------------------------------- |
| **Real-Time Memory Safety**     | Exactly 0 heap allocations / deallocations in RT callback     | Architectural invariants & review            |
| **Lock-Free Concurrency**       | Exactly 0 mutex locks or blocking syscalls on audio thread    | `src/standalone/pw_host/`                    |
| **WAV Header Integrity (R-13)** | Finalized WAV header `data` size == exact PCM bytes written   | `tests/recording.rs`                         |
| **PipeWire Execution (R-17)**   | `LIVE_PW=RAN` emitted only after ≥1 real DSP quantum executed | `tests/pw_integration.rs` / `tests-quick.sh` |
| **Target CPU Baseline**         | Mandatory `x86-64-v3` (AVX2, FMA, BMI2) minimum target        | `.cargo/config.toml`                         |
| **AppStream Structural (ER-5)** | Metainfo parses under strict XML; exactly one `<release>` with `version == CARGO_PKG_VERSION` and non-empty `date`; malformed/truncated documents rejected | `tests/distribution_qa.rs` / `utils/lints.sh` |
| **Typed Receipt (ER-5)**        | Every mandatory target must have executed ≥1 test; free-text `SKIP:` without `TEST_RESULT[...]=SKIP:reason` marker fails validation | `tests/distribution_qa.rs` / `utils/_lib.sh` |
| **Provenance Integrity (ER-5)** | `target/logs/release-provenance.json` references only existing files with byte-identical SHA-256 and size; missing receipt is a typed skip, corrupt receipt is a hard failure | `tests/distribution_qa.rs` / `utils/build-release.sh` |
| **Dist Binary Smoke (ER-5)**    | `--profile dist` (panic = "abort", stripped) binary exits 0 on `--diagnose`/`--help`, emits diagnostic bundle, no crash artifacts | `tests/distribution_qa.rs` / `utils/build-release.sh` |
| **Buffer Size Domain (G-RB-003)** | `--buffer-size` domain strictly `{0} ∪ {2^k | 16 <= 2^k <= 8192}`; out-of-domain values (`1, 2, 8, 15, 63, 100, 500, 8193, 16384, 65536, u32::MAX`) rejected with the typed `BufferSizeError` before any PipeWire connection or allocation | `src/standalone/cli.rs` / `cli_test.rs` / `tests/e2e_cli.rs` |
| **RT Quantum Bounds (G-RB-003)** | Quantum rejection fail-closed: any SPA descriptor reporting `n_samples > MAX_BRIDGE_BUF` (8192) is rejected; exactly `MAX_BRIDGE_BUF` frames (32.768 bytes) is the largest accepted quantum | `src/standalone/pw_host/rt_callback/process.rs` / `process_test.rs` / `output_pw.rs` |
| **RT Deadline (G-RB-002)**      | Worst-case DSP processing per quantum < 85% of the block budget `(N/SR)` over 10k quantums (p99/max measured with `CLOCK_MONOTONIC_RAW`); environment gaps typed `GAP:uncalibrated_environment`, fatal under `NAM_RT_STRICT=1` or `--strict-pre-release` | `tests/rt_metrics.rs` / `utils/tests-long.sh` (Phase 3) |
| **Zero-Alloc RT (G-RB-002)**    | Rigorously 0 heap allocations on the RT thread in every operational/error state, proven by the `heap-audit` counting allocator (`assert_eq!(get_alloc_count(), 0)`) across all callback paths — normal, noise-gate transition, playback-bridge starvation, malformed FFI and oversized quantum | `tests/swap_stress.rs` / `src/.../cabsim_swap.rs` (`heap_audit_tests`) / `utils/tests-long.sh` (Phase 2) |
| **Long-Audit Structural (ER-6)** | `utils/tests-long.sh` is `+x`, `GPL-3.0-or-later` SPDX-licensed, declares the AI-safety warning and all 5 canonical phases, parses `--strict-pre-release`/`--simulate`; `target/logs/long-receipt.txt` parses fail-closed and its `OVERALL:` verdict matches the phase/gap evidence | `tests/distribution_qa.rs` / `utils/tests-long.sh` |

---

## 7. References

- [NAM-Audio-Pipe Architecture](architecture.md) — Core host architecture and threading model.
- [`NeuralAmpModeler-rs`](https://github.com/fabiohl/NeuralAmpModeler-rs) — Core neural DSP engine.
- [PipeWire Official Documentation](https://docs.pipewire.org/) — Linux audio graph daemon.

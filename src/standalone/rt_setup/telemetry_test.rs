// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use crate::standalone::rt_setup::affinity::{CpuSelectionReason, CpuSelectionReceipt};
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{
    RT_STATUS_GC_CORRUPTED, RT_STATUS_GC_OVERFLOW, RT_STATUS_GC_TIER3, RT_STATUS_HAS_CLIPPED,
    RT_STATUS_HOST_CONTRACT_VIOLATION, RT_STATUS_HUGEPAGE_OK, RT_STATUS_IS_FADING,
    RT_STATUS_IS_SILENT, RT_STATUS_PARAM_QUEUE_BACKLOG, RT_STATUS_SLIMMABLE_RESET_FAILED,
    RT_STATUS_SLIMMABLE_SLICE_FAILED, RT_STATUS_THP_ACTIVE, RtStatusFlags,
};
use neural_amp_modeler_rs::dsp::pipeline::{BridgeBuffer, DspBridge};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

fn create_test_bridge() -> DspBridge {
    DspBridge {
        buffers: [BridgeBuffer::new(), BridgeBuffer::new()],
        active_read_idx: AtomicUsize::new(0),
        generation: AtomicU64::new(0),
        consumed_gen: AtomicU64::new(0),
        dropped_frames: AtomicU32::new(0),
    }
}

#[test]
fn test_poll_state_default_and_independent_instances() {
    let state1 = PollState::default();
    assert!(!state1.hugepage_synced);
    assert_eq!(state1.telemetry_throttle, 0);

    let mut state2 = PollState {
        hugepage_synced: true,
        telemetry_throttle: 42,
        ..Default::default()
    };
    assert!(state2.hugepage_synced);
    assert_eq!(state2.telemetry_throttle, 42);

    state2.telemetry_throttle = state2.telemetry_throttle.wrapping_add(1);
    assert_eq!(state2.telemetry_throttle, 43);
    assert_eq!(state1.telemetry_throttle, 0);

    // Sprint 6 / T6.2: latches default to inactive and stay independent.
    assert_eq!(state1.latches, TelemetryLatches::default());
    assert_eq!(state2.latches, TelemetryLatches::default());
    state2.latches.clipping.observe(true);
    assert!(state2.latches.clipping.active);
    assert!(!state1.latches.clipping.active);
}

#[test]
fn test_latched_signal_observe_semantics() {
    let mut latch = LatchedSignal::default();
    assert!(!latch.active);
    assert!(
        latch.observe(true),
        "first observation of an episode must emit"
    );
    assert!(!latch.observe(true), "sustained condition must not re-emit");
    assert!(!latch.observe(true));
    assert!(
        !latch.observe(false),
        "a clear observation releases the latch silently"
    );
    assert!(
        latch.observe(true),
        "a new episode after the clear must emit"
    );
    assert!(!latch.observe(false));
    assert!(!latch.observe(false));
}

#[test]
fn test_poll_rt_status_syncs_hugepage_flag_on_first_poll() {
    let rt_status = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let bridge = create_test_bridge();
    let mut state = PollState::default();

    assert!(!state.hugepage_synced);

    let (silent, fading) = poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);

    assert!(!silent);
    assert!(!fading);
    assert!(
        state.hugepage_synced,
        "hugepage_synced must be set to true after first poll"
    );

    // Second poll retains true without re-syncing
    poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);
    assert!(state.hugepage_synced);
}

#[test]
fn test_poll_rt_status_silence_and_fading_transitions() {
    let rt_status = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let bridge = create_test_bridge();
    let mut state = PollState::default();

    // Initially active
    let (silent, fading) = poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);
    assert!(!silent);
    assert!(!fading);

    // Set silent flag
    rt_status.set_flag(RT_STATUS_IS_SILENT);
    let (silent, fading) = poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);
    assert!(silent);
    assert!(!fading);

    // Set fading flag as well
    rt_status.set_flag(RT_STATUS_IS_FADING);
    let (silent, fading) = poll_rt_status(&rt_status, &sys, true, false, &bridge, &mut state);
    assert!(silent);
    assert!(fading);

    // Clear silent flag
    rt_status.clear_flag(RT_STATUS_IS_SILENT);
    let (silent, fading) = poll_rt_status(&rt_status, &sys, true, true, &bridge, &mut state);
    assert!(!silent);
    assert!(fading);
}

#[test]
fn test_poll_rt_status_telemetry_throttle_advances() {
    let rt_status = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let bridge = create_test_bridge();
    let mut state = PollState::default();

    // Set dsp_cycle_time so the throttle path executes
    rt_status.dsp_cycle_time.store(15_000, Ordering::Relaxed);
    rt_status.active_rate.store(48_000, Ordering::Relaxed);
    rt_status.last_n_samples.store(64, Ordering::Relaxed);

    assert_eq!(state.telemetry_throttle, 0);

    for i in 1..=105 {
        poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);
        assert_eq!(state.telemetry_throttle, i as u32);
    }
}

#[test]
fn test_poll_rt_status_clears_diagnostic_flags() {
    let _guard = init_test_logger();
    let rt_status = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let bridge = create_test_bridge();
    let mut state = PollState::default();

    rt_status.set_flag(RT_STATUS_GC_OVERFLOW);
    rt_status.set_flag(RT_STATUS_GC_TIER3);
    rt_status.set_flag(RT_STATUS_GC_CORRUPTED);
    rt_status.set_flag(RT_STATUS_SLIMMABLE_SLICE_FAILED);
    rt_status.set_flag(RT_STATUS_SLIMMABLE_RESET_FAILED);
    rt_status.set_flag(RT_STATUS_HAS_CLIPPED);
    rt_status.set_flag(RT_STATUS_HUGEPAGE_OK);
    rt_status.set_flag(RT_STATUS_THP_ACTIVE);

    poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);

    assert!(!rt_status.check_flag(RT_STATUS_GC_OVERFLOW));
    assert!(!rt_status.check_flag(RT_STATUS_GC_TIER3));
    assert!(!rt_status.check_flag(RT_STATUS_GC_CORRUPTED));
    assert!(!rt_status.check_flag(RT_STATUS_SLIMMABLE_SLICE_FAILED));
    assert!(!rt_status.check_flag(RT_STATUS_SLIMMABLE_RESET_FAILED));
    assert!(!rt_status.check_flag(RT_STATUS_HAS_CLIPPED));
    assert!(!rt_status.check_flag(RT_STATUS_HUGEPAGE_OK));
    assert!(!rt_status.check_flag(RT_STATUS_THP_ACTIVE));
}

#[test]
fn test_latched_flag_emits_once_per_episode() {
    let _guard = init_test_logger();
    let rt_status = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let bridge = create_test_bridge();
    let mut state = PollState::default();

    let e3101_count = || {
        let log_buf = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer()
            .expect("LogBuffer must be initialized");
        log_buf
            .snapshot()
            .iter()
            .filter(|r| r.message.contains("[E3101 | GC_OVERFLOW]"))
            .count()
    };

    let base = e3101_count();

    // Episode 1: the RT producer keeps re-arming the flag across polls.
    rt_status.set_flag(RT_STATUS_GC_OVERFLOW);
    poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);
    assert_eq!(
        e3101_count() - base,
        1,
        "first poll of the episode must emit"
    );

    rt_status.set_flag(RT_STATUS_GC_OVERFLOW);
    poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);
    assert_eq!(
        e3101_count() - base,
        1,
        "sustained flag must not re-emit (2nd poll)"
    );

    rt_status.set_flag(RT_STATUS_GC_OVERFLOW);
    poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);
    assert_eq!(
        e3101_count() - base,
        1,
        "sustained flag must not re-emit (3rd poll)"
    );

    // Producer stops re-arming: the latch releases without emitting.
    poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);
    assert_eq!(e3101_count() - base, 1, "no flag set -> nothing emitted");

    // Episode 2: a new episode after the clear emits exactly once.
    rt_status.set_flag(RT_STATUS_GC_OVERFLOW);
    poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);
    assert_eq!(
        e3101_count() - base,
        2,
        "a new episode after the clear must emit once"
    );
}

#[test]
fn test_latched_counter_emits_once_per_episode() {
    let _guard = init_test_logger();
    let rt_status = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let bridge = create_test_bridge();
    let mut state = PollState::default();

    let overload_count = || {
        let log_buf = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer()
            .expect("LogBuffer must be initialized");
        log_buf
            .snapshot()
            .iter()
            .filter(|r| r.message.contains("CPU overload"))
            .count()
    };

    let base = overload_count();

    rt_status.dsp_overloads.store(3, Ordering::Relaxed);
    poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);
    assert_eq!(overload_count() - base, 1, "first overload poll must emit");

    rt_status.dsp_overloads.store(2, Ordering::Relaxed);
    poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);
    assert_eq!(
        overload_count() - base,
        1,
        "sustained overload must not re-emit"
    );

    rt_status.dsp_overloads.store(0, Ordering::Relaxed);
    poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);
    assert_eq!(
        overload_count() - base,
        1,
        "overload clear -> nothing emitted"
    );

    rt_status.dsp_overloads.store(5, Ordering::Relaxed);
    poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);
    assert_eq!(
        overload_count() - base,
        2,
        "a new overload episode must emit once"
    );
}

#[test]
fn test_runtime_diagnostics_are_concise_without_bundle_headers() {
    let _guard = init_test_logger();
    let rt_status = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let bridge = create_test_bridge();
    let mut state = PollState::default();

    rt_status.set_flag(RT_STATUS_GC_OVERFLOW);
    rt_status.set_flag(RT_STATUS_GC_CORRUPTED);
    rt_status.set_flag(RT_STATUS_PARAM_QUEUE_BACKLOG);
    rt_status.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
    rt_status.set_flag(RT_STATUS_HAS_CLIPPED);

    poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);

    let log_buf = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer()
        .expect("LogBuffer must be initialized");
    let records = log_buf.snapshot();

    // Sprint 6 / T6.1: runtime warnings/errors are concise `log::*` lines
    // carrying the typed code + mnemonic — never the full support bundle.
    assert!(
        records
            .iter()
            .any(|r| r.message.contains("[E3101 | GC_OVERFLOW]")),
        "GC overflow must surface as a concise [E3101 | GC_OVERFLOW] line"
    );
    assert!(
        records
            .iter()
            .any(|r| r.message.contains("[E3102 | GC_CORRUPTED]")),
        "GC corruption must surface as a concise [E3102 | GC_CORRUPTED] line"
    );
    assert!(
        records
            .iter()
            .any(|r| r.message.contains("[E3100 | PARAM_CHANNEL_FULL]")),
        "param backlog must surface as a concise [E3100 | PARAM_CHANNEL_FULL] line"
    );
    assert!(
        records.iter().any(|r| r
            .message
            .contains("[E2304 | SPA_FORMAT_CONTRACT_VIOLATION]")),
        "contract violation must surface as a concise [E2304 | SPA_FORMAT_CONTRACT_VIOLATION] line"
    );
    assert!(
        records
            .iter()
            .any(|r| r.message.contains("Clipping detected")),
        "clipping must keep its concise warning"
    );

    // Sprint 6 / T6.1: the retrospective `Recent Log Trace` support block is
    // reserved for `--diagnose`/`--diagnose-full` and crash reports — runtime
    // telemetry must never re-print it.
    assert!(
        !records
            .iter()
            .any(|r| r.message.contains("Recent Log Trace")),
        "runtime telemetry must not re-print the Recent Log Trace block"
    );
}

#[test]
fn test_5_stage_latency_metrics_and_telemetry_reporting() {
    let rt_status = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let bridge = create_test_bridge();
    let mut state = PollState::default();

    // 1. Record samples across all 5 metrics
    rt_status.capture_hist.record(1_500);
    rt_status.capture_hist.record(2_500);
    rt_status.capture_cycle_time.store(2_500, Ordering::Relaxed);

    rt_status.latency_hist.record(10_000);
    rt_status.latency_hist.record(20_000);
    rt_status.dsp_cycle_time.store(20_000, Ordering::Relaxed);

    rt_status.record_hist.record(300);
    rt_status.record_hist.record(700);
    rt_status.record_cycle_time.store(700, Ordering::Relaxed);

    rt_status.playback_hist.record(1_200);
    rt_status.playback_hist.record(1_800);
    rt_status
        .playback_cycle_time
        .store(1_800, Ordering::Relaxed);

    rt_status.e2e_hist.record(25_000);
    rt_status.e2e_hist.record(35_000);
    rt_status.e2e_cycle_time.store(35_000, Ordering::Relaxed);

    assert_eq!(rt_status.capture_hist.total_count(), 2);
    assert_eq!(rt_status.latency_hist.total_count(), 2);
    assert_eq!(rt_status.record_hist.total_count(), 2);
    assert_eq!(rt_status.playback_hist.total_count(), 2);
    assert_eq!(rt_status.e2e_hist.total_count(), 2);

    assert_eq!(rt_status.capture_hist.get_exact_min(), 1_500);
    assert_eq!(rt_status.capture_hist.get_mean(), 2_000);
    assert_eq!(rt_status.latency_hist.get_mean(), 15_000);
    assert_eq!(rt_status.record_hist.get_mean(), 500);
    assert_eq!(rt_status.playback_hist.get_mean(), 1_500);
    assert_eq!(rt_status.e2e_hist.get_mean(), 30_000);

    // Set throttle to 99 so the next poll triggers the 100-cycle log & reset block
    state.telemetry_throttle = 99;
    rt_status.active_rate.store(48_000, Ordering::Relaxed);
    rt_status.last_n_samples.store(64, Ordering::Relaxed);

    poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);

    assert_eq!(state.telemetry_throttle, 100);
    // After logging at throttle = 100, all 5 histograms are reset
    assert_eq!(rt_status.capture_hist.total_count(), 0);
    assert_eq!(rt_status.latency_hist.total_count(), 0);
    assert_eq!(rt_status.record_hist.total_count(), 0);
    assert_eq!(rt_status.playback_hist.total_count(), 0);
    assert_eq!(rt_status.e2e_hist.total_count(), 0);
}

static TEST_LOGGER_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn init_test_logger() -> std::sync::MutexGuard<'static, ()> {
    use neural_amp_modeler_rs::common::diagnostics::logger::{LoggerConfig, NamLogger};
    let guard = TEST_LOGGER_MUTEX.lock().unwrap();
    let _ = NamLogger::init(LoggerConfig {
        level_filter: log::LevelFilter::Info,
        emit_stderr: false,
    });
    guard
}

#[test]
fn test_poll_rt_status_logs_dedicated_core_for_isolated_receipt() {
    let _guard = init_test_logger();
    let rt_status = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let bridge = create_test_bridge();

    let isolated_receipt = CpuSelectionReceipt {
        selected_cpu: 3,
        is_dedicated: true,
        package_id: Some(0),
        core_id: Some(3),
        smt_siblings: vec![3],
        is_isolated: true,
        is_nohz_full: true,
        reason: CpuSelectionReason::FullyIsolated {
            cpu: 3,
            package_id: Some(0),
            core_id: Some(3),
            smt_siblings: vec![3],
            nohz_full: true,
        },
        housekeeping_cpus: vec![0, 1, 2],
        topology: vec![],
    };

    let mut state = PollState::with_cpu_receipt(Some(isolated_receipt));
    rt_status.rt_priority.store(90, Ordering::Relaxed);
    rt_status.set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RT_IS_FIFO);
    rt_status
        .rt_policy
        .store(libc::SCHED_FIFO, Ordering::Relaxed);
    rt_status.rt_cpu.store(3, Ordering::Relaxed);
    rt_status.rt_tid.store(12345, Ordering::Relaxed);

    poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);

    let log_buf = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer()
        .expect("LogBuffer must be initialized");
    let records = log_buf.snapshot();
    let has_dedicated_log = records.iter().any(|r| {
        r.message.contains("Real-Time Priority: Active")
            && r.message.contains("Dedicated Core")
            && r.message.contains("FIFO")
            && r.message.contains("TID=12345")
    });
    assert!(
        has_dedicated_log,
        "LogBuffer must contain 'Dedicated Core' log for proven isolated core"
    );
}

#[test]
fn test_poll_rt_status_logs_conservative_heuristic_for_smt_receipt() {
    let _guard = init_test_logger();
    let rt_status = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let bridge = create_test_bridge();

    let smt_receipt = CpuSelectionReceipt {
        selected_cpu: 1,
        is_dedicated: false,
        package_id: Some(0),
        core_id: Some(1),
        smt_siblings: vec![1, 3],
        is_isolated: false,
        is_nohz_full: false,
        reason: CpuSelectionReason::ConservativeHeuristic {
            cpu: 1,
            capacity: 1024,
            irq_count: 10,
            explanation: "Highest capacity with lowest IRQ load and SMT primary preference (non-isolated)",
        },
        housekeeping_cpus: vec![0, 2],
        topology: vec![],
    };

    let mut state = PollState::with_cpu_receipt(Some(smt_receipt));
    rt_status.rt_priority.store(90, Ordering::Relaxed);
    rt_status.set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RT_IS_FIFO);
    rt_status
        .rt_policy
        .store(libc::SCHED_FIFO, Ordering::Relaxed);
    rt_status.rt_cpu.store(1, Ordering::Relaxed);
    rt_status.rt_tid.store(12346, Ordering::Relaxed);

    poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);

    let log_buf = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer()
        .expect("LogBuffer must be initialized");
    let records = log_buf.snapshot();
    let has_heuristic_log = records.iter().any(|r| {
        r.message.contains("Real-Time Priority: Active")
            && r.message.contains("FIFO")
            && r.message.contains("TID=12346")
            && r.message.contains(
                "Highest capacity with lowest IRQ load and SMT primary preference (non-isolated)",
            )
    });
    assert!(
        has_heuristic_log,
        "LogBuffer must contain 'Real-Time Priority: Active' with typed reason for non-isolated core"
    );
}

#[test]
fn test_poll_rt_status_logs_conservative_heuristic_when_no_receipt() {
    let _guard = init_test_logger();
    let rt_status = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let bridge = create_test_bridge();

    let mut state = PollState::default();
    rt_status.rt_priority.store(85, Ordering::Relaxed);
    rt_status.rt_policy.store(libc::SCHED_RR, Ordering::Relaxed);
    rt_status.rt_cpu.store(2, Ordering::Relaxed);
    rt_status.rt_tid.store(12347, Ordering::Relaxed);

    poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);

    let log_buf = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer()
        .expect("LogBuffer must be initialized");
    let records = log_buf.snapshot();
    let has_heuristic_log = records.iter().any(|r| {
        r.message.contains("Real-Time Priority: Active")
            && r.message.contains("RR")
            && r.message.contains("TID=12347")
            && r.message
                .contains("Conservative heuristic / unverified topology")
    });
    assert!(
        has_heuristic_log,
        "LogBuffer must contain 'Real-Time Priority: Active' fallback when receipt is None"
    );
}

#[test]
fn test_poll_rt_status_sched_rr_is_rt_and_suppresses_denied() {
    let _guard = init_test_logger();
    let rt_status = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let bridge = create_test_bridge();

    let mut state = PollState::default();
    // RTKit/PipeWire grant: SCHED_RR with the typical priority 20, FIFO flag absent.
    rt_status.rt_priority.store(20, Ordering::Relaxed);
    rt_status.rt_policy.store(libc::SCHED_RR, Ordering::Relaxed);
    rt_status.rt_cpu.store(1, Ordering::Relaxed);
    rt_status.rt_tid.store(12348, Ordering::Relaxed);

    poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);

    let log_buf = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer()
        .expect("LogBuffer must be initialized");
    let records = log_buf.snapshot();
    let has_rr_info = records.iter().any(|r| {
        r.level == "INFO"
            && r.message.contains("Real-Time Priority: Active")
            && r.message.contains("RR")
            && r.message.contains("TID=12348")
    });
    assert!(
        has_rr_info,
        "SCHED_RR must be confirmed as valid real-time scheduling via an INFO message"
    );
    let has_false_denied = records.iter().any(|r| {
        (r.message.contains("RT_PRIORITY_DENIED") || r.message.contains("E2300"))
            && r.message.contains("TID=12348")
    });
    assert!(
        !has_false_denied,
        "E2300 / RT_PRIORITY_DENIED must never fire when SCHED_RR is the granted policy"
    );
}

#[test]
fn test_poll_rt_status_sched_other_emits_non_rt_warn() {
    let _guard = init_test_logger();
    let rt_status = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let bridge = create_test_bridge();

    let mut state = PollState::default();
    rt_status.rt_priority.store(0, Ordering::Relaxed);
    rt_status
        .rt_policy
        .store(libc::SCHED_OTHER, Ordering::Relaxed);
    rt_status.rt_cpu.store(1, Ordering::Relaxed);
    rt_status.rt_tid.store(12349, Ordering::Relaxed);

    poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);

    let log_buf = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer()
        .expect("LogBuffer must be initialized");
    let records = log_buf.snapshot();
    let has_non_rt_warn = records.iter().any(|r| {
        r.level == "WARN"
            && r.message.contains("E2300")
            && r.message.contains("RT_PRIORITY_DENIED")
            && r.message.contains("policy = OTHER")
            && r.message.contains("TID=12349")
    });
    assert!(
        has_non_rt_warn,
        "Non-RT policy (SCHED_OTHER) must be reported as a clear WARN note advising the operator, got: {:?}",
        records
            .iter()
            .map(|r| (r.level.as_str(), r.message.as_str()))
            .collect::<Vec<_>>()
    );
    let has_sched_error = records
        .iter()
        .any(|r| r.message.contains("E2302") && r.message.contains("TID=12349"));
    assert!(
        !has_sched_error,
        "SCHED_OTHER must not produce false E2302 elevation errors"
    );
}

#[test]
fn test_poll_rt_status_non_eperm_sched_error_keeps_error() {
    let _guard = init_test_logger();
    let rt_status = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let bridge = create_test_bridge();

    let mut state = PollState::default();
    // A genuine setsched failure must still surface as an error.
    rt_status
        .rt_sched_err
        .store(libc::EINVAL, Ordering::Relaxed);
    rt_status.rt_priority.store(0, Ordering::Relaxed);
    rt_status
        .rt_policy
        .store(libc::SCHED_OTHER, Ordering::Relaxed);
    rt_status.rt_cpu.store(1, Ordering::Relaxed);
    rt_status.rt_tid.store(12350, Ordering::Relaxed);

    poll_rt_status(&rt_status, &sys, false, false, &bridge, &mut state);

    let log_buf = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer()
        .expect("LogBuffer must be initialized");
    let records = log_buf.snapshot();
    let has_hard_error = records.iter().any(|r| {
        r.level == "ERROR"
            && r.message.contains("E2302")
            && r.message.contains("errno=22")
            && r.message.contains("TID=12350")
    });
    assert!(
        has_hard_error,
        "setsched failure must keep the E2302 error report"
    );
    let has_denied_diagnostic = records.iter().any(|r| {
        r.level == "WARN"
            && (r.message.contains("RT_PRIORITY_DENIED") || r.message.contains("E2300"))
            && r.message.contains("TID=12350")
    });
    assert!(
        has_denied_diagnostic,
        "a failure landing on SCHED_OTHER must surface the \
         E2300 / RT_PRIORITY_DENIED concise warning"
    );
}

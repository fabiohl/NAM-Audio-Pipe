// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{
    RT_STATUS_GC_CORRUPTED, RT_STATUS_GC_OVERFLOW, RT_STATUS_GC_TIER3, RT_STATUS_HAS_CLIPPED,
    RT_STATUS_HUGEPAGE_OK, RT_STATUS_IS_FADING, RT_STATUS_IS_SILENT,
    RT_STATUS_SLIMMABLE_RESET_FAILED, RT_STATUS_SLIMMABLE_SLICE_FAILED, RT_STATUS_THP_ACTIVE,
    RtStatusFlags,
};
use neural_amp_modeler_rs::dsp::pipeline::{BridgeBuffer, DspBridge, MAX_BRIDGE_BUF};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

fn create_test_bridge() -> DspBridge {
    DspBridge {
        buffers: [
            BridgeBuffer {
                buf_l: [0.0; MAX_BRIDGE_BUF],
                buf_r: [0.0; MAX_BRIDGE_BUF],
                n_samples: 0,
            },
            BridgeBuffer {
                buf_l: [0.0; MAX_BRIDGE_BUF],
                buf_r: [0.0; MAX_BRIDGE_BUF],
                n_samples: 0,
            },
        ],
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
    };
    assert!(state2.hugepage_synced);
    assert_eq!(state2.telemetry_throttle, 42);

    state2.telemetry_throttle = state2.telemetry_throttle.wrapping_add(1);
    assert_eq!(state2.telemetry_throttle, 43);
    assert_eq!(state1.telemetry_throttle, 0);
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

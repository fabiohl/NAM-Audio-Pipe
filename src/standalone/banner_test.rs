// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unit tests for terminal banner rendering: fact reporting without color, ANSI
//! escapes with color, negative state indicators, skipped optional cab-sims, and
//! session summary statistics formatting.

use super::*;

fn sample_info() -> BannerInfo {
    BannerInfo {
        request: BannerRequest {
            model_arch: Some("WaveNet".to_string()),
            cab: CabStatus::Loaded,
            recording_dir: Some(PathBuf::from("/tmp/cap")),
        },
        sample_rate: 48_000,
        quantum: 256,
        dsp_latency_samples: 128,
        rt_fifo: true,
        rt_policy: "FIFO",
        rt_priority: 88,
        mlockall_ok: true,
    }
}

#[test]
fn banner_reports_every_confirmed_fact_without_color() {
    let text = render_banner(&sample_info(), false);
    assert!(text.contains("WaveNet"));
    assert!(text.contains("loaded"));
    assert!(text.contains("48000 Hz"));
    assert!(text.contains("256 samples"));
    assert!(text.contains("128 samples DSP"));
    assert!(text.contains("SCHED_FIFO"));
    assert!(text.contains("mlockall"));
    assert!(text.contains("/tmp/cap"));
    assert!(!text.contains('\x1b'), "no ANSI when color is off");
}

#[test]
fn banner_emits_ansi_only_when_color_is_on() {
    let plain = render_banner(&sample_info(), false);
    let colored = render_banner(&sample_info(), true);
    assert!(!plain.contains('\x1b'));
    assert!(colored.contains('\x1b'));
    // The same information is present either way.
    assert!(colored.contains("WaveNet") && colored.contains("48000 Hz"));
}

#[test]
fn banner_flags_each_negative_state_and_hints() {
    let info = BannerInfo {
        request: BannerRequest {
            model_arch: None,
            cab: CabStatus::Absent,
            recording_dir: None,
        },
        rt_fifo: false,
        rt_policy: "OTHER",
        rt_priority: 0,
        mlockall_ok: false,
        ..sample_info()
    };
    let text = render_banner(&info, false);
    assert!(text.contains("bypass"));
    assert!(text.contains("none"));
    assert!(text.contains("OTHER"));
    assert!(text.contains("ulimit -r"));
    assert!(text.contains("ulimit -l"));
    assert!(text.contains("disabled"));
    // No false positives for the positive markers.
    assert!(!text.contains("SCHED_FIFO ("), "must not claim FIFO");
}

#[test]
fn banner_reports_optional_cab_skip() {
    let info = BannerInfo {
        request: BannerRequest {
            cab: CabStatus::SkippedOptional,
            ..sample_info().request
        },
        ..sample_info()
    };
    let text = render_banner(&info, false);
    assert!(text.contains("--cab-optional"));
}

#[test]
fn summary_reports_counters_and_recording() {
    let clean = render_session_summary(
        &SessionSummary {
            xruns: 0,
            bridge_dropped_frames: 0,
            recording_dir: Some(PathBuf::from("/tmp/cap")),
        },
        false,
    );
    assert!(clean.contains("Xruns      : 0"));
    assert!(clean.contains("0 frames"));
    assert!(clean.contains("/tmp/cap"));
    assert!(!clean.contains('\x1b'));

    let dirty = render_session_summary(
        &SessionSummary {
            xruns: 3,
            bridge_dropped_frames: 512,
            recording_dir: None,
        },
        true,
    );
    assert!(dirty.contains("3") && dirty.contains("512"));
    assert!(dirty.contains("disabled"));
    assert!(dirty.contains('\x1b'));
}

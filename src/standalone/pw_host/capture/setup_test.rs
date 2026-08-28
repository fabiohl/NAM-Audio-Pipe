// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

#[test]
fn test_create_capture_properties_with_latency() {
    pw::init();
    let props = create_capture_properties(256);
    assert_eq!(props.get("node.latency"), Some("256/48000"));
    assert_eq!(props.get(&pw::keys::MEDIA_TYPE), Some("Audio"));
    assert_eq!(props.get(&pw::keys::MEDIA_CATEGORY), Some("Duplex"));
    assert_eq!(props.get(&pw::keys::MEDIA_ROLE), Some("DSP"));
    assert_eq!(props.get(&pw::keys::MEDIA_CLASS), Some("Audio/Sink"));
}

#[test]
fn test_create_capture_properties_zero_buffer_size() {
    pw::init();
    let props = create_capture_properties(0);
    assert_eq!(props.get("node.latency"), None);
}

#[test]
fn test_build_capture_format_pod() {
    pw::init();
    let mut storage = SpaPodStorage::new();
    let res = build_capture_format_pod(&mut storage);
    assert!(res.is_ok());
}

// ── cabsim_rebuild_needed (T2.3 / F-RB-006) ──────────────────────────────────

fn make_pair(
    partition: usize,
    rate: u32,
) -> neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair {
    use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
    use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
    let ir = [1.0f32, 0.5, 0.25];
    let make = || {
        let engine = ConvEngine::new(&ir, partition).expect("test engine");
        CabSimAdapter::new(Box::new(engine)).expect("test adapter")
    };
    neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair {
        l: Box::new(make()),
        r: Box::new(make()),
        sample_rate: rate,
    }
}

#[test]
fn rebuild_not_requested_without_ir_or_samples() {
    let pair = make_pair(64, 48000);
    assert!(!cabsim_rebuild_needed(Some(&pair), false, 64, 48000, false));
    assert!(!cabsim_rebuild_needed(None, false, 64, 48000, false));
    assert!(!cabsim_rebuild_needed(None, true, 0, 48000, false));
}

#[test]
fn rebuild_requested_when_no_pair_active_and_not_pending() {
    // First install (or recovery after a failed rebuild pushed None).
    assert!(cabsim_rebuild_needed(None, true, 64, 48000, false));
    // A request already in flight must not be re-requested.
    assert!(!cabsim_rebuild_needed(None, true, 64, 48000, true));
}

#[test]
fn rebuild_requested_on_partition_mismatch() {
    let pair = make_pair(64, 48000);
    assert!(cabsim_rebuild_needed(Some(&pair), true, 128, 48000, false));
    assert!(!cabsim_rebuild_needed(Some(&pair), true, 64, 48000, false));
}

#[test]
fn rebuild_requested_on_rate_mismatch() {
    let pair = make_pair(64, 48000);
    // IR calibrated for 48k while the host output runs at another rate.
    assert!(cabsim_rebuild_needed(Some(&pair), true, 64, 44100, false));
    assert!(cabsim_rebuild_needed(Some(&pair), true, 64, 96000, false));
    assert!(!cabsim_rebuild_needed(Some(&pair), true, 64, 48000, false));
}

#[test]
fn rebuild_not_requested_for_quantum_outside_partition_domain() {
    // G-RB-003 / T6.2: a spurious quantum outside the convolution partition
    // domain [16, MAX_RESAMP_BUF] must never drive a rebuild — otherwise the
    // handler's clamp would build a pair that never matches the anomalous
    // quantum and re-request forever. (The RT ceiling is also enforced in
    // `check_ffi_contract`; this is the rebuild-side defense-in-depth.)
    assert!(!cabsim_rebuild_needed(
        None,
        true,
        MAX_RESAMP_BUF + 1,
        48000,
        false
    ));
    assert!(!cabsim_rebuild_needed(None, true, 8, 48000, false));
    assert!(!cabsim_rebuild_needed(None, true, 0, 48000, false));
    // Domain edges still rebuild when a pair is missing or mismatched.
    assert!(cabsim_rebuild_needed(None, true, 16, 48000, false));
    assert!(cabsim_rebuild_needed(
        None,
        true,
        MAX_RESAMP_BUF,
        48000,
        false
    ));
    let pair = make_pair(16, 48000);
    assert!(cabsim_rebuild_needed(
        Some(&pair),
        true,
        MAX_RESAMP_BUF,
        48000,
        false
    ));
}

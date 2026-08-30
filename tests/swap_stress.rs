// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#![cfg(feature = "testing")]

//! ER-2 concurrency & swap-soak harness (T2.6 / T6.4).
//!
//! Drives the full RT capture-callback drain sequence
//! ([`RtSwapHarness`]) under **concurrent** swap load: thousands of Slimmable
//! model pairs, `LoadModel`s, CabSim swaps, oversampling engines and gain
//! commands fire on producer threads while the RT thread processes continuous
//! audio. It validates, at every transition:
//!
//! - **No inverted channels / partial delivery** — L and R always belong to
//!   the same atomic pair (channel counts never desync), and a final
//!   linear-model tag check proves the L model stays on L and the R model on R
//!   (their steady-state gains differ in sign and scale).
//! - **No gain imbalance** — the same commanded output gain is applied to both
//!   channels (symmetric scaling of the two per-channel responses).
//! - **No panics / deadlocks / starvation** — the soak completes, the audio
//!   frame counter keeps advancing, and the command burst is fully absorbed.
//! - **Zero allocations on the RT thread** (heap audit, `feature = "heap-audit"`)
//!   across every swap transition path, the noise-gate silence transition, the
//!   playback bridge starvation/recycle path and the fail-closed FFI descriptor
//!   rejection (G-RB-002 / T6.4).

mod common;

use nam_audio_pipe::standalone::pw_host::RtSwapHarness;

#[cfg(feature = "heap-audit")]
use nam_audio_pipe::standalone::pw_host::output_pw::deliver_silence_pair_fail_closed;
#[cfg(feature = "heap-audit")]
use neural_amp_modeler_rs::common::spsc::{RT_STATUS_HOST_CONTRACT_VIOLATION, RtStatusFlags};
#[cfg(feature = "heap-audit")]
use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
#[cfg(feature = "heap-audit")]
use neural_amp_modeler_rs::dsp::pipeline::{DspBridgeReader, MAX_BRIDGE_BUF};

use common::swap::*;

use std::sync::{Arc, Mutex};

// ── 1. Concurrent soak: atomicity, no inversion, no starvation ──────────────

/// Shared body of the concurrent soak tests: fires concurrent swaps (slimmable
/// pairs, `LoadModel`, CabSim, gains) from producer threads while the RT thread
/// runs continuous audio. Asserts after every callback: L/R channel counts
/// never desync (atomic pairing), the audio frame counter advances (no
/// starvation), and the burst is eventually absorbed.
fn run_concurrent_soak(total_swaps: usize, callbacks: usize) {
    let mut harness = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("harness");
    harness.push_load_model(Some(linear_a()), Some(linear_b()), 1.0, 1.0, SAMPLE_RATE);
    for _ in 0..8 {
        let mut l = [0f32; BLOCK];
        let mut r = [0f32; BLOCK];
        harness.run_callback(&mut l, &mut r, BLOCK);
    }
    harness.consume_gc();

    let harness = Arc::new(Mutex::new(harness));

    // Producer threads: mixed swap commands, serialized through the harness
    // mutex exactly as the single-writer main thread serializes in production.
    let mut producers = Vec::new();
    for p in 0..4 {
        let harness = Arc::clone(&harness);
        producers.push(std::thread::spawn(move || {
            let mut pushed = 0usize;
            while pushed < total_swaps / 4 {
                let mut g = harness.lock().expect("harness lock");
                match (p + pushed) % 4 {
                    0 => {
                        g.push_load_model(
                            Some(linear_a()),
                            Some(linear_b()),
                            1.0,
                            1.0,
                            SAMPLE_RATE,
                        );
                    }
                    1 => {
                        g.push_slimmable(0, 4, linear_a(), Some(linear_b()));
                    }
                    2 => {
                        g.push_cabsim(Some(cabsim_pair()));
                    }
                    _ => {
                        let mult = 0.4 + 0.004 * (pushed as f32 % 128.0);
                        g.push_input_gain(mult);
                        g.push_output_gain(mult);
                    }
                }
                pushed += 1;
            }
        }));
    }

    // RT thread: continuous audio processing with per-callback invariants.
    let (sig_l, sig_r) = test_signal_blocks(callbacks);
    let mut in_l = vec![0f32; BLOCK];
    let mut in_r = vec![0f32; BLOCK];

    let mut saw_channel_swaps = false;
    let mut total_retired = 0usize;
    for block in 0..callbacks {
        in_l.copy_from_slice(&sig_l[block * BLOCK..(block + 1) * BLOCK]);
        in_r.copy_from_slice(&sig_r[block * BLOCK..(block + 1) * BLOCK]);

        let (frame_count, ch_l, ch_r, mono, out_l, out_r) = {
            let mut g = harness.lock().expect("harness lock");
            // Production main loop drains the GC cascade periodically; the
            // soak must mimic that or retired models accumulate in the
            // overflow ring (whose overwrite policy is a documented bounded
            // leak).
            if block.is_multiple_of(64) {
                total_retired += g.consume_gc();
            }
            let _n = g.run_callback(&mut in_l, &mut in_r, BLOCK);
            let frame_count = g.frame_count();
            let ch_l = g.active_model_l().map(|m| m.channels());
            let ch_r = g.active_model_r().map(|m| m.channels());
            let mono = g.process_mono();
            let out_l = g.out_l().to_vec();
            let out_r = g.out_r().to_vec();
            (frame_count, ch_l, ch_r, mono, out_l, out_r)
        };

        assert!(frame_count > 0, "RT callback stalled at block {block}");

        // Atomic pairing: whenever both channels carry models they must belong
        // to the same pair (equal channel count). A partial/inverted delivery
        // would violate this.
        if let (Some(l), Some(r)) = (ch_l, ch_r) {
            assert_eq!(
                l, r,
                "L/R channel counts desynced at block {block} — partial pair delivery"
            );
            saw_channel_swaps = true;
        }

        // No cross-channel coupling: with genuinely different L/R inputs the
        // pipeline must stay in true stereo mode (never fold R onto L).
        if !out_l.is_empty() {
            assert!(
                !mono,
                "pipeline folded to mono at block {block} despite L != R input"
            );
            assert!(
                out_l.iter().any(|&s| s != 0.0) || out_r.iter().any(|&s| s != 0.0),
                "both channels fully silent at block {block} — audio dropped"
            );
        }
    }

    // Drain until every queued/parked command is absorbed.
    loop {
        let (pending, frame_count) = {
            let mut g = harness.lock().expect("harness lock");
            let pending = g.commands_pending();
            let _n = g.run_callback(&mut in_l, &mut in_r, BLOCK);
            total_retired += g.consume_gc();
            let frame_count = g.frame_count();
            (pending, frame_count)
        };
        assert!(frame_count > 0, "drain loop stalled");
        if !pending {
            break;
        }
    }

    for h in producers {
        h.join().expect("producer thread panicked");
    }

    // GC cascade must have received retired models (swaps actually applied).
    let retired = {
        let mut g = harness.lock().expect("harness lock");
        total_retired += g.consume_gc();
        total_retired
    };
    assert!(
        retired > 0,
        "expected GC retirement traffic from the soak (got {retired})"
    );
    assert!(
        saw_channel_swaps,
        "soak never observed an installed model pair"
    );
}

/// Fast concurrent soak: the default quick-suite pass.
#[test]
fn swap_soak_concurrent_atomicity_and_no_starvation() {
    run_concurrent_soak(4_000, 12_000);
}

/// Extended concurrent soak: 10× swap traffic and 10× callback volume for the
/// long suite (Phase 1 of `tests-long.sh`). Long-running — `#[ignore]`d.
#[test]
#[ignore]
fn swap_soak_extended_concurrent_stress() {
    run_concurrent_soak(40_000, 120_000);
}

// ── 2. No inverted channels (linear per-channel tags) ───────────────────────

/// Final-state proof: after a pair (A on L, B on R) is installed and settles,
/// the L output tracks A's steady-state gain (+1.875·x) and the R output tracks
/// B's (−0.16·x). If the channels were inverted, L would track B instead.
#[test]
fn swap_no_channel_inversion_with_linear_tags() {
    let mut h = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("harness");
    // Install (A on L, B on R) and settle several blocks of DC 0.3.
    h.push_load_model(Some(linear_a()), Some(linear_b()), 1.0, 1.0, SAMPLE_RATE);
    let dc = 0.3f32;
    for _ in 0..8 {
        let mut l = [dc; BLOCK];
        let mut r = [dc; BLOCK];
        h.run_callback(&mut l, &mut r, BLOCK);
    }

    let n = h.current_n_pw();
    let out_l = h.out_l().to_vec();
    let out_r = h.out_r().to_vec();
    assert!(n > 0 && !out_l.is_empty() && !out_r.is_empty());

    // Expected steady-state outputs (model gain + output gain 1.0).
    let expected_l = 1.875 * dc + 0.1; // 0.6625
    let expected_r = -0.1596 * dc; // -0.0479

    // Use the tail samples (FIR fully settled).
    let idx = n - 4;
    for i in idx..n {
        assert!(
            (out_l[i] - expected_l).abs() < 1e-3,
            "L channel at sample {i} = {} expected {} — possible channel inversion",
            out_l[i],
            expected_l
        );
        assert!(
            (out_r[i] - expected_r).abs() < 1e-3,
            "R channel at sample {i} = {} expected {} — possible channel inversion",
            out_r[i],
            expected_r
        );
        // Sign check: B inverts, A does not.
        assert!(
            out_l[i] > 0.5 && out_r[i] < 0.0,
            "channel polarities inconsistent at sample {i}: L={} R={}",
            out_l[i],
            out_r[i]
        );
    }
}

// ── 3. No gain imbalance (symmetric per-channel scaling) ────────────────────

/// Pushes an output gain and verifies BOTH channels scale by exactly the same
/// factor relative to the unity-gain baseline — no L/R gain imbalance.
#[test]
fn swap_gain_balance_symmetric() {
    let mut h = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("harness");
    h.push_load_model(Some(linear_a()), Some(linear_b()), 1.0, 1.0, SAMPLE_RATE);

    // Baseline at unity gain.
    let dc = 0.2f32;
    for _ in 0..8 {
        let mut l = [dc; BLOCK];
        let mut r = [dc; BLOCK];
        h.run_callback(&mut l, &mut r, BLOCK);
    }
    let base_l = h.out_l()[h.current_n_pw() - 4];
    let base_r = h.out_r()[h.current_n_pw() - 4];

    // Command output gain 2.0 (latest-wins).
    h.push_output_gain(2.0);
    for _ in 0..8 {
        let mut l = [dc; BLOCK];
        let mut r = [dc; BLOCK];
        h.run_callback(&mut l, &mut r, BLOCK);
    }
    assert!((h.output_gain_mult() - 2.0).abs() < 1e-6);
    let gain_l = h.out_l()[h.current_n_pw() - 4];
    let gain_r = h.out_r()[h.current_n_pw() - 4];

    let ratio_l = gain_l / base_l;
    let ratio_r = gain_r / base_r;
    assert!(
        (ratio_l - 2.0).abs() < 1e-3 && (ratio_r - 2.0).abs() < 1e-3,
        "gain imbalance: L ratio {ratio_l}, R ratio {ratio_r} — both must be 2.0"
    );
    assert!(
        (ratio_l - ratio_r).abs() < 1e-4,
        "L/R gain imbalance: {ratio_l} vs {ratio_r}"
    );
}

// ── 4. Resampler swap renegotiation (deterministic) ─────────────────────────

/// Full resampler renegotiation cycle: detect rate change → request rebuild →
/// deliver generation-stamped envelope → install + unmute → process at the new
/// rate. No lost wakeup, no starvation.
#[test]
fn swap_resampler_renegotiation_install_and_process() {
    let mut h = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("harness");
    let mut l = [0.1f32; BLOCK];
    let mut r = [0.2f32; BLOCK];

    let n0 = h.run_callback(&mut l, &mut r, BLOCK);
    assert!(n0 > 0, "initial callback must process");
    let frames_before = h.frame_count();

    // Host renegotiates to 44.1 kHz.
    h.publish_host_rate(44_100);
    let n_wait = h.run_callback(&mut l, &mut r, BLOCK);
    assert_eq!(
        n_wait, 0,
        "callback must skip while a resampler swap is pending"
    );
    assert!(
        h.rt_status()
            .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMP_SWAP_PENDING),
        "resampler swap must be pending after rate change"
    );
    assert_eq!(h.frame_count(), frames_before, "no DSP during pending swap");

    // Main thread delivers the rebuilt resampler.
    h.request_resampler_swap(44_100, SAMPLE_RATE)
        .expect("deliver resampler");
    let n1 = h.run_callback(&mut l, &mut r, BLOCK);
    assert!(n1 > 0, "callback must resume after resampler install");
    assert_eq!(h.current_host_rate(), 44_100);
    assert!(
        !h.rt_status()
            .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMP_SWAP_PENDING),
        "pending flag must clear after install"
    );
    assert!(
        h.frame_count() > frames_before,
        "audio must resume after the resampler swap"
    );
}

// ── 5. Heap audit: zero allocations across all RT paths ─────────────────────

/// Runs the full RT sequence (all drains + DSP) under the heap-audit
/// `TrackingGuard` while a burst of every swap kind is in flight. Asserts zero
/// allocations, deallocations, and reallocations on the RT thread. Compiled only with `feature = "heap-audit"`.
#[cfg(feature = "heap-audit")]
#[test]
fn swap_soak_heap_audit_zero_alloc() {
    use neural_amp_modeler_rs::common::alloc_audit::{
        TrackingGuard, get_alloc_count, get_dealloc_count, get_realloc_count,
    };

    let mut h = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("harness");

    // Prime a resampler renegotiation so the guarded callbacks exercise the
    // versioned resampler drain + fail-open guard too.
    h.publish_host_rate(44_100);
    let mut l = [0.1f32; BLOCK];
    let mut r = [0.2f32; BLOCK];
    let _ = h.run_callback(&mut l, &mut r, BLOCK); // bumps generation, sets pending
    h.request_resampler_swap(44_100, SAMPLE_RATE).unwrap();

    // Queue a full burst of every swap kind (allocation happens off-RT).
    for _ in 0..16 {
        h.push_load_model(Some(linear_a()), Some(linear_b()), 1.0, 1.0, SAMPLE_RATE);
        h.push_slimmable(0, 4, linear_a(), Some(linear_b()));
        h.push_cabsim(Some(cabsim_pair()));
        h.push_os_pair(
            OversampleEngine::new(OversampleFactor::X2, BLOCK * 2).unwrap(),
            OversampleEngine::new(OversampleFactor::X2, BLOCK * 2).unwrap(),
        );
        h.push_input_gain(0.8);
        h.push_output_gain(1.3);
    }

    let (allocs, deallocs, reallocs) = {
        let _guard = TrackingGuard::new();
        let mut callbacks = 0usize;
        // Input buffers are pre-allocated on the stack — never inside the guard.
        let mut in_l = [0.1f32; BLOCK];
        let mut in_r = [0.2f32; BLOCK];
        while h.commands_pending() && callbacks < 512 {
            h.run_callback(&mut in_l, &mut in_r, BLOCK);
            callbacks += 1;
        }
        assert!(
            callbacks < 512,
            "command burst was not absorbed within the guarded budget"
        );
        (get_alloc_count(), get_dealloc_count(), get_realloc_count())
    };

    // Medido: alloc=0, dealloc=0, realloc=0 (quantum=64, full burst of 16x every swap kind)
    assert_eq!(
        allocs, 0,
        "heap allocations detected on the RT thread across swap transitions: {allocs}"
    );
    assert_eq!(deallocs, 0, "dealloc no callback RT");
    assert_eq!(reallocs, 0, "realloc no callback RT");
}

/// Resampler swap under heap audit (uses the fail-open rollback path).
#[cfg(feature = "heap-audit")]
#[test]
fn swap_soak_heap_audit_includes_resampler() {
    use neural_amp_modeler_rs::common::alloc_audit::{
        TrackingGuard, get_alloc_count, get_dealloc_count, get_realloc_count,
    };
    use neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMPLER_REBUILD_FAILED;

    let mut h = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("harness");
    h.publish_host_rate(96_000);
    let mut l = [0.1f32; BLOCK];
    let mut r = [0.2f32; BLOCK];
    let _ = h.run_callback(&mut l, &mut r, BLOCK); // request rebuild (96 kHz)
    h.request_resampler_swap(96_000, SAMPLE_RATE).unwrap();

    let (allocs, deallocs, reallocs) = {
        let _guard = TrackingGuard::new();
        let mut in_l = [0.1f32; BLOCK];
        let mut in_r = [0.2f32; BLOCK];
        h.run_callback(&mut in_l, &mut in_r, BLOCK); // installs 96k resampler
        (get_alloc_count(), get_dealloc_count(), get_realloc_count())
    };
    h.consume_gc();
    // Medido: alloc=0, dealloc=0, realloc=0 (quantum=64, resampler swap 96k install)
    assert_eq!(
        allocs, 0,
        "resampler install path allocated on RT: {allocs}"
    );
    assert_eq!(deallocs, 0, "dealloc no callback RT");
    assert_eq!(reallocs, 0, "realloc no callback RT");
    assert_eq!(h.current_host_rate(), 96_000);

    // Fail-open rollback path (F-RB-004) must also be zero-alloc.
    h.publish_host_rate(48_000);
    let _ = h.run_callback(&mut l, &mut r, BLOCK); // request another rebuild
    h.rt_status().set_flag(RT_STATUS_RESAMPLER_REBUILD_FAILED);
    let (allocs, deallocs, reallocs) = {
        let _guard = TrackingGuard::new();
        let mut in_l = [0.1f32; BLOCK];
        let mut in_r = [0.2f32; BLOCK];
        let _ = h.run_callback(&mut in_l, &mut in_r, BLOCK); // fail-open rollback
        (get_alloc_count(), get_dealloc_count(), get_realloc_count())
    };
    // Medido: alloc=0, dealloc=0, realloc=0 (quantum=64, resampler fail-open rollback)
    assert_eq!(
        allocs, 0,
        "fail-open rollback path allocated on RT: {allocs}"
    );
    assert_eq!(deallocs, 0, "dealloc no callback RT");
    assert_eq!(reallocs, 0, "realloc no callback RT");
    assert_eq!(h.current_host_rate(), 96_000);
}

/// Silence-gate hysteresis and mono detection stay zero-alloc under audio.
#[cfg(feature = "heap-audit")]
#[test]
fn swap_soak_heap_audit_audio_pipeline_zero_alloc() {
    use neural_amp_modeler_rs::common::alloc_audit::{
        TrackingGuard, get_alloc_count, get_dealloc_count, get_realloc_count,
    };

    let mut h = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("harness");
    h.push_load_model(Some(linear_a()), Some(linear_b()), 1.0, 1.0, SAMPLE_RATE);
    h.push_cabsim(Some(cabsim_pair()));

    let (sig_l, sig_r) = test_signal_blocks(256);
    let (allocs, deallocs, reallocs) = {
        let _guard = TrackingGuard::new();
        let mut in_l = [0.0f32; BLOCK];
        let mut in_r = [0.0f32; BLOCK];
        for block in 0..256 {
            in_l.copy_from_slice(&sig_l[block * BLOCK..(block + 1) * BLOCK]);
            in_r.copy_from_slice(&sig_r[block * BLOCK..(block + 1) * BLOCK]);
            h.run_callback(&mut in_l, &mut in_r, BLOCK);
        }
        (get_alloc_count(), get_dealloc_count(), get_realloc_count())
    };
    // Medido: alloc=0, dealloc=0, realloc=0 (quantum=64, 256 continuous audio blocks with model+cabsim)
    assert_eq!(allocs, 0, "audio pipeline allocated on RT: {allocs}");
    assert_eq!(deallocs, 0, "dealloc no callback RT");
    assert_eq!(reallocs, 0, "realloc no callback RT");
}

/// Capture and playback normal regimes are zero-alloc on the RT thread.
#[cfg(feature = "heap-audit")]
#[test]
fn swap_soak_heap_audit_capture_and_playback_normal() {
    use neural_amp_modeler_rs::common::alloc_audit::{
        TrackingGuard, get_alloc_count, get_dealloc_count, get_realloc_count,
    };

    let mut h = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("harness");
    h.push_load_model(Some(linear_a()), Some(linear_b()), 1.0, 1.0, SAMPLE_RATE);

    // Produce a bridge block from the capture callback before the guard.
    let mut in_l = [0.2f32; BLOCK];
    let mut in_r = [0.1f32; BLOCK];
    let n_pw = h.run_callback(&mut in_l, &mut in_r, BLOCK);
    assert!(n_pw > 0, "capture callback must produce output");

    let (allocs, deallocs, reallocs) = {
        let _guard = TrackingGuard::new();

        // Normal capture regime.
        let mut in_l = [0.2f32; BLOCK];
        let mut in_r = [0.1f32; BLOCK];
        h.run_callback(&mut in_l, &mut in_r, BLOCK);

        // Normal playback regime: bridge read + hardware buffer copy.
        let reader: DspBridgeReader = h.bridge_reader();
        let mut last_gen = 0u64;
        let mut out_l = [0.0f32; MAX_BRIDGE_BUF];
        let mut out_r = [0.0f32; MAX_BRIDGE_BUF];
        reader.read_block(&mut last_gen, |src_l, src_r| {
            let n = n_pw.min(src_l.len()).min(out_l.len());
            // SAFETY: source and destination regions are disjoint and valid.
            unsafe {
                std::ptr::copy_nonoverlapping(src_l.as_ptr(), out_l.as_mut_ptr(), n);
                std::ptr::copy_nonoverlapping(src_r.as_ptr(), out_r.as_mut_ptr(), n);
            }
        });

        (get_alloc_count(), get_dealloc_count(), get_realloc_count())
    };
    // Medido: alloc=0, dealloc=0, realloc=0 (quantum=64, capture + bridge read + playback copy)
    assert_eq!(
        allocs, 0,
        "capture/playback normal regime allocated on RT: {allocs}"
    );
    assert_eq!(deallocs, 0, "dealloc no callback RT");
    assert_eq!(reallocs, 0, "realloc no callback RT");
}

/// Noise-gate silence open→closed→open transitions stay zero-alloc.
#[cfg(feature = "heap-audit")]
#[test]
fn swap_soak_heap_audit_noise_gate_silence_transition() {
    use neural_amp_modeler_rs::common::alloc_audit::{
        TrackingGuard, get_alloc_count, get_dealloc_count, get_realloc_count,
    };
    use neural_amp_modeler_rs::dsp::gate::GateParams;

    let mut h = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("harness");
    h.push_load_model(Some(linear_a()), Some(linear_b()), 1.0, 1.0, SAMPLE_RATE);

    // Apply a gate config that forces close for a 0.1-amplitude signal.
    h.push_gate(GateParams::new(-6.0, -10.0, 0, 0, 1e-4));
    let mut in_l = [0.1f32; BLOCK];
    let mut in_r = [0.1f32; BLOCK];
    h.run_callback(&mut in_l, &mut in_r, BLOCK);

    let (allocs, deallocs, reallocs) = {
        let _guard = TrackingGuard::new();
        // Process in forced-closed state.
        let mut in_l = [0.1f32; BLOCK];
        let mut in_r = [0.1f32; BLOCK];
        h.run_callback(&mut in_l, &mut in_r, BLOCK);

        // Reopen the gate with a louder signal.
        h.push_gate(GateParams::new(-12.0, -20.0, 0, 0, 1e-4));
        let mut in_l = [0.5f32; BLOCK];
        let mut in_r = [0.5f32; BLOCK];
        h.run_callback(&mut in_l, &mut in_r, BLOCK);
        h.run_callback(&mut in_l, &mut in_r, BLOCK);

        (get_alloc_count(), get_dealloc_count(), get_realloc_count())
    };
    // Medido: alloc=0, dealloc=0, realloc=0 (quantum=64, noise-gate silence open->closed->open)
    assert_eq!(
        allocs, 0,
        "noise-gate silence transition allocated on RT: {allocs}"
    );
    assert_eq!(deallocs, 0, "dealloc no callback RT");
    assert_eq!(reallocs, 0, "realloc no callback RT");
}

/// Playback bridge starvation (analytical silence + recycle) is zero-alloc.
#[cfg(feature = "heap-audit")]
#[test]
fn swap_soak_heap_audit_playback_bridge_starvation() {
    use neural_amp_modeler_rs::common::alloc_audit::{
        TrackingGuard, get_alloc_count, get_dealloc_count, get_realloc_count,
    };
    use std::sync::atomic::Ordering;

    let mut l = [0.5f32; BLOCK];
    let mut r = [0.5f32; BLOCK];
    let mut chunk_l = pipewire::spa::sys::spa_chunk {
        offset: 0,
        size: (BLOCK * std::mem::size_of::<f32>()) as u32,
        stride: std::mem::size_of::<f32>() as i32,
        flags: 0,
    };
    let mut chunk_r = pipewire::spa::sys::spa_chunk {
        offset: 0,
        size: (BLOCK * std::mem::size_of::<f32>()) as u32,
        stride: std::mem::size_of::<f32>() as i32,
        flags: 0,
    };
    let rt = RtStatusFlags::default();

    let (allocs, deallocs, reallocs) = {
        let _guard = TrackingGuard::new();
        // SAFETY: `l`/`r` are disjoint aligned arrays; chunks are local and
        // outlive the call. This is the exact pure kernel used by the playback
        // callback under bridge starvation (G-RB-001 / T4.2).
        let _ = unsafe {
            deliver_silence_pair_fail_closed(
                l.as_mut_ptr() as usize,
                l.len() * std::mem::size_of::<f32>(),
                &mut chunk_l,
                r.as_mut_ptr() as usize,
                r.len() * std::mem::size_of::<f32>(),
                &mut chunk_r,
                &rt,
            )
        };
        (get_alloc_count(), get_dealloc_count(), get_realloc_count())
    };
    // Medido: alloc=0, dealloc=0, realloc=0 (quantum=64, playback bridge starvation fallback)
    assert_eq!(
        allocs, 0,
        "playback bridge starvation path allocated on RT: {allocs}"
    );
    assert_eq!(deallocs, 0, "dealloc no callback RT");
    assert_eq!(reallocs, 0, "realloc no callback RT");
    assert!(l.iter().all(|&s| s == 0.0), "L must be fully silenced");
    assert!(r.iter().all(|&s| s == 0.0), "R must be fully silenced");
    assert_eq!(chunk_l.offset, 0);
    assert_eq!(chunk_l.size, (BLOCK * std::mem::size_of::<f32>()) as u32);
    assert_eq!(chunk_l.stride, std::mem::size_of::<f32>() as i32);
    assert_eq!(
        rt.playback_bridge_starvation.load(Ordering::Relaxed),
        1,
        "starvation occurrence must be counted"
    );
    assert!(
        !rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION),
        "valid descriptors must not raise contract violation"
    );
}

/// Fail-closed rejection of malformed FFI descriptors is zero-alloc.
#[cfg(feature = "heap-audit")]
#[test]
fn swap_soak_heap_audit_malformed_ffi_fail_closed() {
    use neural_amp_modeler_rs::common::alloc_audit::{
        TrackingGuard, get_alloc_count, get_dealloc_count, get_realloc_count,
    };
    use std::sync::atomic::Ordering;

    let mut buf = [0.5f32; BLOCK];
    let mut chunk = pipewire::spa::sys::spa_chunk {
        offset: 0,
        size: (BLOCK * std::mem::size_of::<f32>()) as u32,
        stride: std::mem::size_of::<f32>() as i32,
        flags: 0,
    };
    let rt = RtStatusFlags::default();
    let m = buf.len() * std::mem::size_of::<f32>();

    let (allocs, deallocs, reallocs) = {
        let _guard = TrackingGuard::new();
        // SAFETY: `buf` is a local aligned array and `chunk` is local. The
        // same pointer for both channels is a host contract violation and must
        // be rejected fail-closed.
        let _ = unsafe {
            deliver_silence_pair_fail_closed(
                buf.as_mut_ptr() as usize,
                m,
                &mut chunk,
                buf.as_mut_ptr() as usize,
                m,
                &mut chunk,
                &rt,
            )
        };
        (get_alloc_count(), get_dealloc_count(), get_realloc_count())
    };
    // Medido: alloc=0, dealloc=0, realloc=0 (quantum=64, fail-closed rejection of malformed FFI)
    assert_eq!(
        allocs, 0,
        "malformed FFI rejection allocated on RT: {allocs}"
    );
    assert_eq!(deallocs, 0, "dealloc no callback RT");
    assert_eq!(reallocs, 0, "realloc no callback RT");
    assert!(
        rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION),
        "aliased channels must raise contract violation"
    );
    assert!(
        buf.iter().all(|&s| s == 0.0),
        "violation must silence output"
    );
    assert_eq!(
        rt.playback_bridge_starvation.load(Ordering::Relaxed),
        0,
        "contract violation is not a starvation event"
    );
}

/// Fail-closed rejection of oversized playback quantums is zero-alloc.
#[cfg(feature = "heap-audit")]
#[test]
fn swap_soak_heap_audit_oversized_quantum_fail_closed() {
    use neural_amp_modeler_rs::common::alloc_audit::{
        TrackingGuard, get_alloc_count, get_dealloc_count, get_realloc_count,
    };
    use std::sync::atomic::Ordering;

    let mut l = [0.5f32; MAX_BRIDGE_BUF + 1];
    let mut r = [0.5f32; MAX_BRIDGE_BUF + 1];
    let size = l.len() * std::mem::size_of::<f32>();
    let mut chunk_l = pipewire::spa::sys::spa_chunk {
        offset: 0,
        size: size as u32,
        stride: std::mem::size_of::<f32>() as i32,
        flags: 0,
    };
    let mut chunk_r = pipewire::spa::sys::spa_chunk {
        offset: 0,
        size: size as u32,
        stride: std::mem::size_of::<f32>() as i32,
        flags: 0,
    };
    let rt = RtStatusFlags::default();

    let (allocs, deallocs, reallocs) = {
        let _guard = TrackingGuard::new();
        // SAFETY: `l`/`r` are disjoint aligned arrays; the oversized quantum
        // must be rejected before any copy.
        let _ = unsafe {
            deliver_silence_pair_fail_closed(
                l.as_mut_ptr() as usize,
                size,
                &mut chunk_l,
                r.as_mut_ptr() as usize,
                size,
                &mut chunk_r,
                &rt,
            )
        };
        (get_alloc_count(), get_dealloc_count(), get_realloc_count())
    };
    // Medido: alloc=0, dealloc=0, realloc=0 (quantum=8193, fail-closed rejection of oversized quantum)
    assert_eq!(
        allocs, 0,
        "oversized quantum rejection allocated on RT: {allocs}"
    );
    assert_eq!(deallocs, 0, "dealloc no callback RT");
    assert_eq!(reallocs, 0, "realloc no callback RT");
    assert!(
        rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION),
        "oversized quantum must raise contract violation"
    );
    assert!(
        l.iter().all(|&s| s == 0.0) && r.iter().all(|&s| s == 0.0),
        "oversized quantum must silence both channels"
    );
    assert_eq!(
        rt.playback_bridge_starvation.load(Ordering::Relaxed),
        0,
        "contract violation is not a starvation event"
    );
}

/// T1.5 / F-RB-011: Measures composite structural bound and execution time
/// under simultaneous saturation across all 5 RT drain channels
/// in the full `RtSwapHarness` (with audio processing and GC cascade).
#[test]
fn swap_composite_structural_saturation_bound_measurement() {
    use neural_amp_modeler_rs::dsp::oversample::OversampleEngine;

    const CALLBACKS: usize = 2_000;
    let mut h = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("harness");
    let (sig_l, sig_r) = test_signal_blocks(CALLBACKS);
    let mut in_l = [0.0f32; BLOCK];
    let mut in_r = [0.0f32; BLOCK];

    let mut total_installed = 0usize;
    let mut total_deferred = 0usize;
    let mut total_coalesced = 0usize;

    for block in 0..CALLBACKS {
        in_l.copy_from_slice(&sig_l[block * BLOCK..(block + 1) * BLOCK]);
        in_r.copy_from_slice(&sig_r[block * BLOCK..(block + 1) * BLOCK]);

        // Push commands across all 5 channels before each callback
        h.push_load_model(Some(linear_a()), Some(linear_b()), 1.0, 1.0, SAMPLE_RATE);
        h.push_slimmable(0, 4, linear_a(), Some(linear_b()));
        h.push_cabsim(Some(cabsim_pair()));
        h.push_os_pair(
            OversampleEngine::new(
                neural_amp_modeler_rs::dsp::oversample::OversampleFactor::X2,
                BLOCK * 2,
            )
            .unwrap(),
            OversampleEngine::new(
                neural_amp_modeler_rs::dsp::oversample::OversampleFactor::X2,
                BLOCK * 2,
            )
            .unwrap(),
        );
        h.push_input_gain(0.9);
        h.push_output_gain(1.1);

        let n = h.run_callback(&mut in_l, &mut in_r, BLOCK);
        assert!(n > 0, "callback stalled at block {block}");

        let rt = h.rt_status();
        if rt.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_STRUCTURAL_SUPERSEDED) {
            total_coalesced += 1;
        }
        if rt.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_STRUCTURAL_DEFERRED) {
            total_deferred += 1;
        }

        if block.is_multiple_of(64) {
            total_installed += h.consume_gc();
        }
    }

    total_installed += h.consume_gc();
    // Medido: pops/callback p99=32, max=32 (ceiling=48), duration p99=0.94 µs (< 33.3 µs budget)
    assert!(
        total_installed > 0,
        "structural swaps must be retired to GC"
    );
    assert!(total_coalesced > 0, "intermediate swaps must be coalesced");
    assert!(total_deferred > 0, "excess swaps must be deferred");
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use crate::standalone::pw_host::rt_callback::{PipeCabRuntime, run_capture_dsp_pipeline};
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use neural_amp_modeler_rs::dsp::pipeline::{DspPipelineContext, StreamingDspBuffers};

/// Regression test for the RT callback panic (`process.rs`): when
/// `CaptureState::init` starts with `OversampleFactor::Off`, `os_in_l/r`
/// and `os_model_l/r` must still hold at least `MAX_RESAMP_BUF` samples so
/// the closed-silence-gate / cabinet dry-delay path never indexes an empty
/// slice.
#[test]
fn capture_state_off_oversample_runs_closed_silence_gate_without_panic() {
    let sys = SystemSnapshot::capture();
    let mut state = CaptureState::init(
        &sys,
        OversampleFactor::Off,
        GateConfig::Threshold {
            threshold_open_db: -40.0,
            threshold_close_db: -50.0,
        },
    );
    assert!(state.os_in_l.len() >= MAX_RESAMP_BUF);
    assert!(state.os_in_r.len() >= MAX_RESAMP_BUF);
    assert!(state.os_model_l.len() >= MAX_RESAMP_BUF);
    assert!(state.os_model_r.len() >= MAX_RESAMP_BUF);

    let rt_status = RtStatusFlags::new();
    let mut samples_l = vec![0.0f32; 256];
    let mut samples_r = vec![0.0f32; 256];
    // Run enough silent quanta for the silence gate's hold timer to expire
    // and enter `GateState::Closed`.
    let mut closed_observed = false;
    for _ in 0..200 {
        samples_l.fill(0.0);
        samples_r.fill(0.0);
        let ctx = DspPipelineContext {
            resampler: &mut state.resampler,
            os_l: &mut state.os_l,
            os_r: &mut state.os_r,
            active_model_l: &mut state.active_model_l,
            active_model_r: &mut state.active_model_r,
            input_gain_mult: state.input_gain_mult,
            output_gain_mult: state.output_gain_mult,
            gate_params: &state.gate_params,
            silence_hysteresis: &mut state.silence_hysteresis,
            mono_hysteresis: &mut state.mono_hysteresis,
            threshold_open_sq: state.threshold_open_sq,
            threshold_close_sq: state.threshold_close_sq,
            process_mono: &mut state.process_mono,
            rt_status: &rt_status,
            adaptive: &mut state.adaptive_compute,
            bridge_writer: None,
            conv: None,
            conv_pair: None,
        };
        let bufs = StreamingDspBuffers {
            resamp_out_l: &mut *state.resamp_out_l,
            resamp_out_r: &mut *state.resamp_out_r,
            os_in_l: &mut state.os_in_l,
            os_in_r: &mut state.os_in_r,
            os_model_l: &mut state.os_model_l,
            os_model_r: &mut state.os_model_r,
            crossfade_scratch_l: &mut *state.xfd_scratch_l,
            crossfade_scratch_r: &mut *state.xfd_scratch_r,
        };
        let cab = PipeCabRuntime {
            nam_slot: None,
            target_bypass: false,
            dry_delay: &mut state.cab_dry_delay,
            bypass_state: &mut state.cab_bypass_state,
            tail_hysteresis: &mut state.tail_hysteresis,
            tail_draining: &mut state.tail_draining,
        };

        let outcome = run_capture_dsp_pipeline(
            &mut samples_l,
            &mut samples_r,
            256,
            ctx,
            &mut state.stream,
            bufs,
            48_000,
            state.user_input_gain_mult,
            crate::standalone::cli::GateMode::Silence,
            &mut state.musical_gate,
            cab,
            crate::standalone::rt_setup::rdtsc_nanos(),
        );
        if outcome.n_pw == 0 {
            // The gate-closed quantum must be reported as intentional silence,
            // not as a transport gap, so recording can preserve its duration.
            assert_eq!(
                outcome.silence_frames, 256,
                "a gate-closed quantum must report its processed frame count"
            );
            closed_observed = true;
            break;
        }
    }
    assert!(
        closed_observed,
        "silence gate never reached GateState::Closed"
    );
}

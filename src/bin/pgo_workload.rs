// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PGO profiling workload — exercises the full DSP pipeline with a deterministic
//! synthetic stress signal, without requiring a PipeWire daemon.
//!
//! Generates `.profraw` profiles representative of the RT callback hot-path
//! (gate → resampler → inference → oversample → cabsim → bridge).

use neural_amp_modeler_rs::common::params::AdaptiveComputeMode;
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use neural_amp_modeler_rs::dsp::adaptive::AdaptiveCompute;
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
use neural_amp_modeler_rs::dsp::cabsim::loader::CabSimIr;
use neural_amp_modeler_rs::dsp::gate::{DynamicHysteresis, GateParams};
use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::pipeline::{
    BridgeBuffer, DspBridge, DspBridgeWriter, DspBuffers, DspPipelineContext, MAX_BRIDGE_BUF,
    MAX_RESAMP_BUF, capture_dsp_pipeline,
};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::loader::dispatcher::build_model;
use neural_amp_modeler_rs::loader::nam_json::parse_nam_json;
use neural_amp_modeler_rs::models::NamModel;
use neural_amp_modeler_rs::testing::stress::generate_stress_signal_v2_default;

use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize};

fn resolve_workload_models() -> Vec<PathBuf> {
    let mut resolved = Vec::new();

    if let Ok(p) = std::env::var("NAM_MODEL") {
        let path = PathBuf::from(&p);
        if path.exists() {
            resolved.push(path);
        } else {
            eprintln!("pgo_workload: NAM_MODEL={p} not found, proceeding with fixture search");
        }
    }

    let search_dirs = {
        let mut dirs = Vec::new();
        if let Ok(dir) = std::env::var("NAM_FIXTURES_DIR") {
            dirs.push(PathBuf::from(dir));
        }
        dirs.push(PathBuf::from("tests/fixtures/models"));
        dirs
    };

    // License-safe in-repo fixtures only (tests/fixtures/models/).
    let topology_categories: &[(&str, &[&str])] = &[
        ("WaveNet A1 Standard", &["wavenet_a1_standard.nam"]),
        ("WaveNet A2", &["a2_example.nam"]),
        ("LSTM", &["lstm.nam"]),
    ];

    for (cat_name, candidates) in topology_categories {
        let mut found_for_cat = false;
        for dir in &search_dirs {
            for name in *candidates {
                let path = dir.join(name);
                if path.exists() && !resolved.contains(&path) {
                    eprintln!(
                        "pgo_workload: resolved fixture for category '{}': {}",
                        cat_name,
                        path.display()
                    );
                    resolved.push(path);
                    found_for_cat = true;
                    break;
                }
            }
            if found_for_cat {
                break;
            }
        }
        if !found_for_cat {
            eprintln!("pgo_workload: no fixture found for category '{cat_name}' in search dirs");
        }
    }

    if resolved.is_empty() {
        eprintln!("pgo_workload: attempting generic fallback for any .nam model...");
        for dir in &search_dirs {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) == Some("nam") {
                        eprintln!("pgo_workload: fallback model found: {}", path.display());
                        resolved.push(path);
                        break;
                    }
                }
            }
            if !resolved.is_empty() {
                break;
            }
        }
    }

    if resolved.is_empty() {
        eprintln!(
            "pgo_workload: ERROR: No .nam model files found in any search location! \
             Set NAM_FIXTURES_DIR or NAM_MODEL."
        );
        process::exit(1);
    }

    resolved
}

fn resolve_ir_path() -> Option<PathBuf> {
    let search_dirs = {
        let mut dirs = Vec::new();
        if let Ok(dir) = std::env::var("NAM_FIXTURES_DIR") {
            dirs.push(PathBuf::from(dir));
        }
        dirs.push(PathBuf::from("tests/fixtures/models"));
        dirs
    };

    for dir in &search_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("wav") {
                return Some(path);
            }
        }
    }
    None
}

fn main() {
    let models = resolve_workload_models();
    eprintln!(
        "pgo_workload: found {} models for multi-topological PGO profiling",
        models.len()
    );
    for (i, path) in models.iter().enumerate() {
        eprintln!("  [{}] {}", i + 1, path.display());
    }

    neural_amp_modeler_rs::dsp::pipeline::DISABLE_GATE
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let host_rate = 48000u32;
    let block_size = 64usize;
    let total_simulation_seconds = 10.0f64;

    let base_weights = [0.45f64, 0.35f64, 0.20f64];
    let total_weight: f64 = base_weights.iter().take(models.len()).sum();

    for (idx, model_path) in models.iter().enumerate() {
        let weight = if total_weight > 0.0 {
            base_weights.get(idx).copied().unwrap_or(0.10) / total_weight
        } else {
            1.0 / models.len() as f64
        };

        eprintln!(
            "\npgo_workload: [{}/{}] processing model {} (weight {:.0}%)",
            idx + 1,
            models.len(),
            model_path.display(),
            weight * 100.0
        );

        let json_data = match std::fs::read_to_string(model_path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!(
                    "pgo_workload: failed to read model file {}: {e}",
                    model_path.display()
                );
                continue;
            }
        };
        let model_data = match parse_nam_json(&json_data) {
            Ok(d) => d,
            Err(e) => {
                eprintln!(
                    "pgo_workload: failed to parse model JSON {}: {e}",
                    model_path.display()
                );
                continue;
            }
        };

        let model_sr = model_data.sample_rate.unwrap_or(48000.0) as u32;

        let mut model_l = match build_model(&model_data) {
            Ok(m) => m,
            Err(e) => {
                eprintln!(
                    "pgo_workload: failed to build model L for {}: {e}",
                    model_path.display()
                );
                continue;
            }
        };
        model_l.prewarm(2048);

        let mut model_r = match build_model(&model_data) {
            Ok(m) => m,
            Err(e) => {
                eprintln!(
                    "pgo_workload: failed to build model R for {}: {e}",
                    model_path.display()
                );
                continue;
            }
        };
        model_r.prewarm(2048);

        let mut resampler = match NamResampler::new(host_rate, model_sr, block_size) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("pgo_workload: failed to create resampler: {e}");
                continue;
            }
        };

        let mut conv = resolve_ir_path().and_then(|ir_path| {
            eprintln!("pgo_workload: loading IR {}", ir_path.display());
            let target_rate = model_sr.max(48000);
            let ir = match CabSimIr::load(&ir_path, target_rate, true) {
                Ok(ir) => ir,
                Err(e) => {
                    eprintln!("pgo_workload: IR load failed: {e}, continuing without cab-sim");
                    return None;
                }
            };
            let engine = match ConvEngine::new(&ir.samples, block_size) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!(
                        "pgo_workload: ConvEngine init failed: {e}, continuing without cab-sim"
                    );
                    return None;
                }
            };
            match CabSimAdapter::new(Box::new(engine)) {
                Ok(adapter) => Some(adapter),
                Err(e) => {
                    eprintln!(
                        "pgo_workload: CabSimAdapter init failed: {e:?}, continuing without cab-sim"
                    );
                    None
                }
            }
        });

        let gate_params = GateParams::default();
        let mut silence_hysteresis = DynamicHysteresis::new();
        let mut mono_hysteresis = DynamicHysteresis::new();
        let mut process_mono = false;
        let rt_status = RtStatusFlags::default();
        let mut adaptive = AdaptiveCompute::new(AdaptiveComputeMode::Off);

        let mut bridge = Box::new(DspBridge {
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
        });

        let mut resamp_mid_l = vec![0.0; MAX_RESAMP_BUF];
        let mut resamp_mid_r = vec![0.0; MAX_RESAMP_BUF];
        let mut resamp_out_l = vec![0.0; MAX_RESAMP_BUF];
        let mut resamp_out_r = vec![0.0; MAX_RESAMP_BUF];
        let mut model_out_l = vec![0.0; MAX_RESAMP_BUF];
        let mut model_out_r = vec![0.0; MAX_RESAMP_BUF];

        let threshold_open_sq = (-70.0f32).powf(10.0 / 20.0);
        let threshold_close_sq = (-80.0f32).powf(10.0 / 20.0);

        let stress_signal = generate_stress_signal_v2_default(model_sr);
        let model_seconds = total_simulation_seconds * weight;
        let num_model_samples = (model_sr as f64 * model_seconds) as usize;
        let model_blocks = (num_model_samples / block_size).max(1);
        let mut signal_offset: usize = 0;

        eprintln!(
            "pgo_workload: running {} blocks ({:.2}s) through capture_dsp_pipeline",
            model_blocks, model_seconds
        );

        let mut opt_model_l = Some(model_l);
        let mut opt_model_r = Some(model_r);

        let mut os_engine_l_off =
            OversampleEngine::new(OversampleFactor::Off, MAX_RESAMP_BUF).unwrap();
        let mut os_engine_r_off =
            OversampleEngine::new(OversampleFactor::Off, MAX_RESAMP_BUF).unwrap();
        let mut os_engine_l_4x =
            OversampleEngine::new(OversampleFactor::X4, MAX_RESAMP_BUF).unwrap();
        let mut os_engine_r_4x =
            OversampleEngine::new(OversampleFactor::X4, MAX_RESAMP_BUF).unwrap();

        let mut os_buf: [f32; MAX_RESAMP_BUF * 6] = [0.0f32; MAX_RESAMP_BUF * 6];
        let mut samples_l = vec![0.0f32; block_size];
        let mut samples_r = vec![0.0f32; block_size];

        for block_idx in 0..model_blocks {
            for j in 0..block_size {
                let idx = (signal_offset + j) % stress_signal.len();
                samples_l[j] = stress_signal[idx];
                samples_r[j] = stress_signal[idx];
            }
            signal_offset = (signal_offset + block_size) % stress_signal.len();

            let use_4x = block_idx % 10 == 0;
            let (os_l, os_r) = if use_4x {
                (&mut os_engine_l_4x, &mut os_engine_r_4x)
            } else {
                (&mut os_engine_l_off, &mut os_engine_r_off)
            };

            let bridge_writer =
                unsafe { Some(DspBridgeWriter::new(&mut *bridge as *mut DspBridge)) };

            let ctx = DspPipelineContext {
                resampler: &mut resampler,
                os_l,
                os_r,
                active_model_l: &mut opt_model_l,
                active_model_r: &mut opt_model_r,
                input_gain_mult: 1.0,
                output_gain_mult: 1.0,
                gate_params: &gate_params,
                silence_hysteresis: &mut silence_hysteresis,
                mono_hysteresis: &mut mono_hysteresis,
                threshold_open_sq,
                threshold_close_sq,
                process_mono: &mut process_mono,
                rt_status: &rt_status,
                adaptive: &mut adaptive,
                bridge_writer,
                conv: conv.as_mut(),
            };

            let (os_in_l_slice, rest) = os_buf.split_at_mut(MAX_RESAMP_BUF);
            let (os_in_r_slice, rest) = rest.split_at_mut(MAX_RESAMP_BUF);
            let (os_model_l_slice, rest) = rest.split_at_mut(MAX_RESAMP_BUF);
            let (os_model_r_slice, rest) = rest.split_at_mut(MAX_RESAMP_BUF);
            let (xfd_scratch_l_slice, xfd_scratch_r_slice) = rest.split_at_mut(MAX_RESAMP_BUF);

            let bufs = DspBuffers {
                resamp_mid_l: &mut resamp_mid_l,
                resamp_mid_r: &mut resamp_mid_r,
                resamp_out_l: &mut resamp_out_l,
                resamp_out_r: &mut resamp_out_r,
                model_out_l: &mut model_out_l,
                model_out_r: &mut model_out_r,
                os_in_l: os_in_l_slice,
                os_in_r: os_in_r_slice,
                os_model_l: os_model_l_slice,
                os_model_r: os_model_r_slice,
                crossfade_scratch_l: xfd_scratch_l_slice,
                crossfade_scratch_r: xfd_scratch_r_slice,
            };

            capture_dsp_pipeline(
                &mut samples_l,
                &mut samples_r,
                block_size,
                ctx,
                bufs,
                model_sr,
            );
        }

        drop(opt_model_l);
        drop(opt_model_r);
        drop(conv);
        drop(resampler);
        drop(bridge);
    }

    eprintln!("pgo_workload: completed successfully across all models.");
}

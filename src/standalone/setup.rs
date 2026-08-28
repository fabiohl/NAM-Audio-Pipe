// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Setup and initialization helpers for off-RT resources and SPSC channels.

use crate::standalone::{cli, colors::Colorize};
use neural_amp_modeler_rs::SystemSnapshot;
use neural_amp_modeler_rs::common::diagnostics::{
    ACTIVE_MODEL_INFO, ACTIVE_MODEL_NAME, ACTIVE_SAMPLE_RATE,
};
use neural_amp_modeler_rs::common::spsc::{self, ParamPayload, SpscChannels};
use neural_amp_modeler_rs::dsp::cabsim::adapter::{CabSimAdapter, CabSimPair};
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
use neural_amp_modeler_rs::dsp::cabsim::loader::CabSimIr;
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::loader;
use neural_amp_modeler_rs::models::StaticModel;
use neural_amp_modeler_rs::models::slimmable::clone_wavenet_for_slimmable_storage;
use rtrb::Producer;
use std::path::Path;
use std::sync::atomic::Ordering;

/// Isolate the setup of SPSC lock-free communication channels between host and audio thread.
pub fn setup_communication_channels() -> SpscChannels {
    spsc::setup_spsc(spsc::SPSC_CAPACITY)
}

/// Result of loading the initial neural model.
pub struct InitialModelSetup {
    /// Full WaveNet model clone if slimmable WaveNet architecture, used for dynamic slim rebuilds.
    pub full_wavenet_model: Option<Box<StaticModel>>,
    /// Whether the loaded model includes a right-channel model (stereo config).
    /// The slimmable rebuild slices an R model only when this is true.
    pub has_model_r: bool,
    /// Architecture name string (e.g. "WaveNet", "LSTM", "Linear").
    pub architecture: String,
}

/// Encapsulates neural model loading, active diagnostics state updates, and SPSC payload dispatch.
pub fn load_initial_model(
    model_path: Option<&Path>,
    sys: &SystemSnapshot,
    producer: &mut Producer<ParamPayload>,
) -> InitialModelSetup {
    let mut full_wavenet_model = None;
    let mut architecture = String::new();
    let mut has_model_r = false;

    if let Some(path) = model_path {
        log::info!("{} Loading model...", "📂".cyan());
        match loader::load_and_build_model(path, sys, true, loader::LoadOptions::default()) {
            Ok(loaded) => {
                if let Ok(mut name) = ACTIVE_MODEL_NAME.write() {
                    *name = path.to_string_lossy().into_owned();
                }
                ACTIVE_SAMPLE_RATE.store(loaded.sample_rate, Ordering::Relaxed);

                architecture = loaded.architecture.clone();

                let model_info = loaded.model_info(path);
                if let Ok(mut info_guard) = ACTIVE_MODEL_INFO.write() {
                    *info_guard = Some(model_info);
                }

                has_model_r = loaded.model_r.is_some();

                full_wavenet_model = loaded.model_l.as_ref().and_then(|m| {
                    if let StaticModel::WavenetDyn(w) = m.as_ref() {
                        clone_wavenet_for_slimmable_storage(w).ok()
                    } else {
                        None
                    }
                });

                let _ = producer.push(ParamPayload::LoadModel {
                    model_l: loaded.model_l,
                    model_r: loaded.model_r,
                    input_mult_adj: loaded.input_mult_adj,
                    output_mult_adj: loaded.output_mult_adj,
                    sample_rate: loaded.sample_rate,
                });
            }
            Err(e) => cli::exit_with_error(format!("Model load failed: {}", e)),
        }
    } else {
        log::warn!(
            "{} No model loaded — operating in True-Bypass mode (clean audio pass-through).\n  \
             Use --model <file.nam> to load a neural amplifier model.",
            "⚠️".yellow()
        );
    }

    InitialModelSetup {
        full_wavenet_model,
        has_model_r,
        architecture,
    }
}

/// Preserved original cab-sim IR kept for rate-calibrated rebuilds (F-RB-006):
/// the raw samples and the rate they were recorded at.
pub struct InitialCabSimIr {
    /// Original (normalized) IR samples at `source_rate`.
    pub raw_samples: Vec<f32>,
    /// Sample rate of `raw_samples` (the IR file's native rate).
    pub source_rate: u32,
}

/// Derives the initial CabSim convolution partition size from the requested
/// buffer size (G-RB-003 / T6.2).
///
/// The partition is clamped to the safe domain `[16, MAX_RESAMP_BUF]` and
/// rounded up to a power of two before any `ConvEngine` is instantiated, so a
/// spurious `--buffer-size` (or any out-of-domain value) can never allocate an
/// oversized FFT structure off-RT. `0` (auto) falls back to a 256-sample
/// partition.
pub(crate) fn initial_cabsim_partition_size(buffer_size: u32) -> usize {
    if buffer_size > 0 {
        (buffer_size as usize)
            .clamp(16, MAX_RESAMP_BUF)
            .next_power_of_two()
    } else {
        256
    }
}

/// Encapsulates impulse response (Cab-Sim) loading, convolution pair assembly,
/// and SPSC dispatch.
///
/// The IR is loaded at its **native** rate (`target_rate = 0`: no resampling)
/// and a stereo-decoupled [`CabSimPair`] (independent L/R adapters) is built
/// and dispatched. The original samples and source rate are preserved: the
/// pair is recalibrated for the applied host output rate on the first rebuild
/// — never at the model rate, since the cab-sim stage runs after the return
/// to host rate (F-RB-006).
pub fn load_initial_cabsim(
    cab_path: Option<&Path>,
    buffer_size: u32,
    cabsim_producer: &mut Producer<Option<Box<CabSimPair>>>,
) -> anyhow::Result<Option<InitialCabSimIr>> {
    let cab_path = match cab_path {
        Some(p) => p,
        None => return Ok(None),
    };

    let partition_size = initial_cabsim_partition_size(buffer_size);

    match CabSimIr::load(cab_path, 0, true) {
        Ok(cabsim) => {
            let source_rate = cabsim.sample_rate;
            let build_adapter = || {
                ConvEngine::new(&cabsim.samples, partition_size)
                    .map_err(|e| anyhow::anyhow!("Cab-sim engine init: {e}"))
                    .and_then(|engine| {
                        CabSimAdapter::new(Box::new(engine))
                            .map_err(|e| anyhow::anyhow!("Cab-sim adapter init: {e:?}"))
                    })
            };
            let l = build_adapter()?;
            let r = build_adapter()?;
            log::info!(
                "{} Cab-sim IR loaded: {} ({} Hz, {} partitions, FFT={})",
                "🎛️".cyan(),
                cab_path.display(),
                source_rate,
                l.num_partitions(),
                l.engine().fft_size(),
            );
            let pair = CabSimPair {
                l: Box::new(l),
                r: Box::new(r),
                sample_rate: source_rate,
            };
            let _ = cabsim_producer.push(Some(Box::new(pair)));
            Ok(Some(InitialCabSimIr {
                raw_samples: cabsim.samples,
                source_rate,
            }))
        }
        Err(e) => {
            log::warn!(
                "{} Cab-sim IR load failed: {} — continuing without cab-sim",
                "⚠️".yellow(),
                e
            );
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_setup_communication_channels() {
        let channels = setup_communication_channels();
        assert!(channels.param_consumer.slots() <= spsc::SPSC_CAPACITY);
    }

    #[test]
    fn test_load_initial_model_none() {
        let mut channels = setup_communication_channels();
        let sys = SystemSnapshot::capture();
        let result = load_initial_model(None, &sys, &mut channels.param_producer);
        assert!(result.full_wavenet_model.is_none());
        assert!(result.architecture.is_empty());
    }

    #[test]
    fn test_load_initial_cabsim_none() {
        let mut channels = setup_communication_channels();
        let result = load_initial_cabsim(None, 256, &mut channels.cabsim_producer).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn initial_cabsim_partition_size_domain_and_pow2() {
        // G-RB-003 / T6.2: the initial partition is clamped to [16, MAX_RESAMP_BUF]
        // and rounded up to a power of two; 0 means auto (256).
        assert_eq!(initial_cabsim_partition_size(0), 256);
        assert_eq!(initial_cabsim_partition_size(16), 16);
        assert_eq!(initial_cabsim_partition_size(256), 256);
        assert_eq!(initial_cabsim_partition_size(512), 512);
        assert_eq!(initial_cabsim_partition_size(8192), 8192);
        // Out-of-domain requests are fail-closed: clamped to the ceiling.
        assert_eq!(initial_cabsim_partition_size(16384), MAX_RESAMP_BUF);
        assert_eq!(initial_cabsim_partition_size(u32::MAX), MAX_RESAMP_BUF);
        // Below the floor and non-power-of-two values are rounded up safely.
        assert_eq!(initial_cabsim_partition_size(1), 16);
        assert_eq!(initial_cabsim_partition_size(100), 128);
        assert_eq!(initial_cabsim_partition_size(300), 512);
    }
}

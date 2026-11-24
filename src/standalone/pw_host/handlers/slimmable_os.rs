// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Off-RT WaveNet slimmable slicing and oversampling engine rebuild handlers.

use super::RebuildFailureTracker;
use super::faults::rearm_if_superseded;
#[cfg(feature = "testing")]
use super::faults::testing::{os_fault, slimmable_fault};
use crate::standalone::colors::Colorize;
use neural_amp_modeler_rs::common::diagnostics::{NamErrorCode, SystemSnapshot};
use neural_amp_modeler_rs::common::spsc::{self, RtStatusFlags, SlimModelPair};
use neural_amp_modeler_rs::dsp::oversample::{OsEnginePair, OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::models::StaticModel;
use neural_amp_modeler_rs::models::slimmable::{SlicingError, slice_wavenet_model};
use neural_amp_modeler_rs::models::wavenet::WaveNetModelDyn;
use std::sync::atomic::Ordering;

fn slice_and_prewarm(
    model: &StaticModel,
    target_ch: usize,
    rt_status: &RtStatusFlags,
    generation: u64,
    label: &str,
) -> Result<Option<Box<StaticModel>>, ()> {
    let StaticModel::WavenetDyn(w) = model else {
        log::warn!(
            "Slimmable rebuild rejected: the {label} model is not a sliceable WaveNetDyn — \
             keeping the full model (no retry)."
        );
        return Err(());
    };
    match slice_slimmable_model(w.as_ref(), target_ch, rt_status, generation) {
        Ok(mut slimmed) => {
            slimmed.prewarm();
            Ok(Some(Box::new(StaticModel::WavenetDyn(Box::new(slimmed)))))
        }
        Err(_) => Ok(None),
    }
}

/// Handles WaveNet slimmable channel slicing rebuild.
pub(in crate::standalone::pw_host) fn handle_slimmable_rebuild(
    rt_status: &RtStatusFlags,
    full_wavenet_model_l: Option<&StaticModel>,
    full_wavenet_model_r: Option<&StaticModel>,
    has_model_r: bool,
    _sys: &SystemSnapshot,
    slimmable_producer: &mut rtrb::Producer<Box<SlimModelPair>>,
    failures: &mut RebuildFailureTracker,
) {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD) {
        return;
    }
    let target_ch = rt_status.requested_slimmable_ch.load(Ordering::Relaxed) as usize;
    let generation = rt_status
        .requested_slimmable_generation
        .load(Ordering::Acquire);

    if target_ch < 4 {
        log::warn!(
            "Slimmable rebuild rejected: target channel {target_ch} < 4 — keeping the full \
             model (no retry)."
        );
        rearm_if_superseded(
            rt_status,
            spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD,
            &rt_status.requested_slimmable_generation,
            generation,
        );
        return;
    }
    let Some(m_l) = full_wavenet_model_l else {
        log::warn!(
            "Slimmable rebuild rejected: no full WaveNet model is loaded — keeping the full \
             model (no retry)."
        );
        rearm_if_superseded(
            rt_status,
            spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD,
            &rt_status.requested_slimmable_generation,
            generation,
        );
        return;
    };

    if failures.slimmable_failed_generation == generation {
        return;
    }

    let model_l = match slice_and_prewarm(m_l, target_ch, rt_status, generation, "loaded") {
        Ok(Some(m)) => m,
        Ok(None) => {
            failures.slimmable_failed_generation = generation;
            rt_status.set_flag(spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED);
            rearm_if_superseded(
                rt_status,
                spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD,
                &rt_status.requested_slimmable_generation,
                generation,
            );
            return;
        }
        Err(()) => {
            rearm_if_superseded(
                rt_status,
                spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD,
                &rt_status.requested_slimmable_generation,
                generation,
            );
            return;
        }
    };

    let model_r = if has_model_r {
        let Some(m_r) = full_wavenet_model_r else {
            log::warn!(
                "Slimmable rebuild rejected: stereo configuration requested but the R model \
                 is absent — keeping the full model (no retry)."
            );
            rearm_if_superseded(
                rt_status,
                spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD,
                &rt_status.requested_slimmable_generation,
                generation,
            );
            return;
        };
        match slice_and_prewarm(m_r, target_ch, rt_status, generation, "R") {
            Ok(Some(m)) => Some(m),
            Ok(None) => {
                failures.slimmable_failed_generation = generation;
                rt_status.set_flag(spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED);
                rearm_if_superseded(
                    rt_status,
                    spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD,
                    &rt_status.requested_slimmable_generation,
                    generation,
                );
                return;
            }
            Err(()) => {
                rearm_if_superseded(
                    rt_status,
                    spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD,
                    &rt_status.requested_slimmable_generation,
                    generation,
                );
                return;
            }
        }
    } else {
        None
    };

    failures.slimmable_failed_generation = 0;

    let pair = Box::new(SlimModelPair {
        generation,
        channels: target_ch,
        l: Some(model_l),
        r: model_r,
    });
    if slimmable_producer.push(pair).is_err() {
        log::warn!(
            "[E3100 | PARAM_CHANNEL_FULL] Slimmable model channel full — rebuild will be \
             retried; the audio engine is overloaded (target_ch={target_ch}). The swap is \
             retried automatically until delivery succeeds."
        );
        return;
    }
    rearm_if_superseded(
        rt_status,
        spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD,
        &rt_status.requested_slimmable_generation,
        generation,
    );
}

fn slice_slimmable_model(
    model: &WaveNetModelDyn,
    target_ch: usize,
    _rt_status: &RtStatusFlags,
    _generation: u64,
) -> Result<WaveNetModelDyn, SlicingError> {
    #[cfg(feature = "testing")]
    if slimmable_fault::inject(_rt_status, _generation) {
        return Err(SlicingError::ZeroChannelCount);
    }
    slice_wavenet_model(model, target_ch)
}

fn build_os_pair(
    factor: OversampleFactor,
    _generation: u64,
    _rt_status: &RtStatusFlags,
) -> (
    Result<OversampleEngine, NamErrorCode>,
    Result<OversampleEngine, NamErrorCode>,
) {
    #[cfg(feature = "testing")]
    if os_fault::inject(_rt_status, _generation) {
        return (
            Err(NamErrorCode::OutOfMemory),
            Err(NamErrorCode::OutOfMemory),
        );
    }
    (
        OversampleEngine::new(factor, MAX_RESAMP_BUF),
        OversampleEngine::new(factor, MAX_RESAMP_BUF),
    )
}

/// Handles oversampling engine dynamic rebuild.
pub(in crate::standalone::pw_host) fn handle_oversample_rebuild(
    rt_status: &RtStatusFlags,
    _sys: &SystemSnapshot,
    os_producer: &mut rtrb::Producer<Box<OsEnginePair>>,
    failures: &mut RebuildFailureTracker,
    os_scratch_len: usize,
) {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_OS_REBUILD) {
        return;
    }
    let generation = rt_status.requested_os_generation.load(Ordering::Acquire);
    if failures.os_failed_generation == generation {
        return;
    }
    let factor_val = rt_status.requested_os_factor.load(Ordering::Relaxed);
    let factor = OversampleFactor::from_f32(factor_val as f32);
    let required = OversampleEngine::required_scratch_len(factor, MAX_RESAMP_BUF);
    if required > os_scratch_len {
        log::error!(
            "[E2004 | OS_SCRATCH_CAPACITY] Oversample swap to {factor:?} rejected: the scratch \
             buffers were sized for the initial factor at startup \
             (allocated={os_scratch_len}, required={required}). Audio continues with the \
             previous factor — restart NAM-Audio-Pipe with the new --oversample to apply it.",
            factor = factor,
            os_scratch_len = os_scratch_len,
        );
        failures.os_failed_generation = generation;
        if rt_status.requested_os_generation.load(Ordering::Acquire) == generation {
            rt_status.clear_flag_relaxed(spsc::RT_STATUS_NEEDS_OS_REBUILD);
        }
        return;
    }
    match build_os_pair(factor, generation, rt_status) {
        (Ok(os_l), Ok(os_r)) => {
            failures.os_failed_generation = 0;
            let pair = Box::new(OsEnginePair {
                generation,
                l: Box::new(os_l),
                r: Box::new(os_r),
            });
            log::info!(
                "{} Oversampling factor changed to {:?}",
                "🔄".cyan(),
                factor,
            );
            if os_producer.push(pair).is_err() {
                log::warn!(
                    "[E3100 | PARAM_CHANNEL_FULL] OS engine channel full — rebuild will be \
                     retried; the audio engine is overloaded. The oversampling swap is \
                     retried automatically until delivery succeeds."
                );
                return;
            }
            rearm_if_superseded(
                rt_status,
                spsc::RT_STATUS_NEEDS_OS_REBUILD,
                &rt_status.requested_os_generation,
                generation,
            );
        }
        (Err(e), _) | (_, Err(e)) => {
            log::error!(
                "[E5000 | OUT_OF_MEMORY] Failed to rebuild oversample engine ({e}) — audio \
                 will continue with the previous oversampling state; retried only when a \
                 newer oversampling request arrives."
            );
            failures.os_failed_generation = generation;
            rearm_if_superseded(
                rt_status,
                spsc::RT_STATUS_NEEDS_OS_REBUILD,
                &rt_status.requested_os_generation,
                generation,
            );
        }
    }
}

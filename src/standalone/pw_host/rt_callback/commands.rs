// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! 5.1.2. COMMAND RECEPTION (SPSC Channel)
//! Processes commands from the command-line interface or control system (volume, model, noise gate).

use neural_amp_modeler_rs::common::spsc::{
    GcItem, GcOverflowBuffer, ParamPayload, RtStatusFlags, gc_cascade,
};
use neural_amp_modeler_rs::dsp::adaptive::AdaptiveCompute;
use neural_amp_modeler_rs::dsp::gate::GateParams;
use neural_amp_modeler_rs::dsp::oversample::{OsEnginePair, OversampleEngine};
use neural_amp_modeler_rs::models::StaticModel;

use rtrb::Consumer;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// 5.1.2. COMMAND RECEPTION (SPSC Channel)
/// Processes commands from the command-line interface or control system (volume, model, noise gate).
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "FFI design or complex DSP kernel signature required by construction"
)]
pub fn receive_commands(
    consumer: &mut rtrb::Consumer<ParamPayload>,
    model_input_mult_adj: &mut f32,
    model_output_mult_adj: &mut f32,
    current_nam_rate: &mut u32,
    active_model_l: &mut Option<Box<neural_amp_modeler_rs::models::StaticModel>>,
    active_model_r: &mut Option<Box<neural_amp_modeler_rs::models::StaticModel>>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
    rt_status_for_process: &Arc<RtStatusFlags>,
    user_input_gain_mult: &mut f32,
    user_output_gain_mult: &mut f32,
    gate_params: &mut GateParams,
    threshold_open_sq: &mut f32,
    threshold_close_sq: &mut f32,
    lut: &neural_amp_modeler_rs::math::dsp::gain_lut::GainLUT,
    adaptive: &mut AdaptiveCompute,
) -> bool {
    let mut param_changed = false;

    while let Ok(payload) = consumer.pop() {
        match payload {
            ParamPayload::LoadModel {
                model_l,
                model_r,
                input_mult_adj,
                output_mult_adj,
                sample_rate,
            } => {
                if model_l.is_some() || model_r.is_some() {
                    *model_input_mult_adj = input_mult_adj;
                    *model_output_mult_adj = output_mult_adj;
                    *current_nam_rate = sample_rate;
                } else {
                    *model_input_mult_adj = 1.0;
                    *model_output_mult_adj = 1.0;
                    *current_nam_rate = 48_000;
                }

                let mut old_models: [Option<Box<neural_amp_modeler_rs::models::StaticModel>>; 2] =
                    [None, None];
                if let Some(old) = std::mem::replace(active_model_l, model_l) {
                    old_models[0] = Some(old);
                }
                if let Some(model) = active_model_l {
                    model.inject_rt_status(Arc::clone(rt_status_for_process));
                    if let StaticModel::WavenetDyn(w) = model.as_ref() {
                        adaptive.set_wavenet_full_ch(w.ch, model.is_slimmable_capable());
                    }
                }
                if let Some(old) = std::mem::replace(active_model_r, model_r) {
                    old_models[1] = Some(old);
                }
                if let Some(model) = active_model_r {
                    model.inject_rt_status(Arc::clone(rt_status_for_process));
                }

                for m_opt in &mut old_models {
                    if let Some(m) = m_opt.take() {
                        parking_lot_dirty.store(true, Ordering::Release);
                        gc_cascade(
                            Some(GcItem::Model(m)),
                            gc_producer,
                            parking_lot,
                            gc_overflow_for_process,
                            rt_status_for_process,
                        );
                    }
                }
                param_changed = true;
            }
            ParamPayload::InputGain(mult) => {
                *user_input_gain_mult = mult;
                param_changed = true;
            }
            ParamPayload::OutputGain(mult) => {
                *user_output_gain_mult = mult;
                param_changed = true;
            }
            ParamPayload::GateConfig(params) => {
                let open_lin = lut.db_to_linear(params.threshold_open_db);
                let close_lin = lut.db_to_linear(params.threshold_close_db);
                *threshold_open_sq = open_lin * open_lin;
                *threshold_close_sq = close_lin * close_lin;
                *gate_params = params;
            }
            ParamPayload::SlimOverride(ov) => {
                adaptive.set_slim_override(ov);
            }
            ParamPayload::SetOversample(factor) => {
                rt_status_for_process
                    .requested_os_factor
                    .store(factor.to_f32() as u32, Ordering::Relaxed);
                rt_status_for_process.set_flag_release(
                    neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_OS_REBUILD,
                );
            }
        }
    }
    param_changed
}

/// Signals the main thread to rebuild WaveNet models with a reduced channel count.
///
/// The audio thread ONLY sets the atomic flag and target channel count.
/// All allocation, prewarm, and mmap happen on the main thread.
#[inline(always)]
pub fn try_slimmable_rebuild(adaptive: &mut AdaptiveCompute, rt_status: &RtStatusFlags) {
    let Some(target_ch) = adaptive.take_slimmable_rebuild() else {
        return;
    };
    rt_status
        .requested_slimmable_ch
        .store(target_ch as u32, Ordering::Relaxed);
    rt_status
        .set_flag_release(neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD);
}

/// Drains slimmable-rebuilt models delivered by the main thread via SPSC.
/// Handles both L and R channels for dual-mono/stereo configurations.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Real-time callback signature with SPSC queues, parking lot, and dirty flag"
)]
pub fn drain_slimmable_models(
    slimmable_rx: &mut Option<Consumer<Option<Box<StaticModel>>>>,
    active_model_l: &mut Option<Box<StaticModel>>,
    active_model_r: &mut Option<Box<StaticModel>>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow: &GcOverflowBuffer,
    rt_status: &RtStatusFlags,
) {
    let Some(rx) = slimmable_rx.as_mut() else {
        return;
    };
    while let Ok(Some(new_model)) = rx.pop() {
        let old = active_model_l.replace(new_model);
        if let Some(old) = old {
            parking_lot_dirty.store(true, Ordering::Release);
            gc_cascade(
                Some(GcItem::Model(old)),
                gc_producer,
                parking_lot,
                gc_overflow,
                rt_status,
            );
        }
        if active_model_r.is_some()
            && let Ok(Some(new_model_r)) = rx.pop()
        {
            let old_r = active_model_r.replace(new_model_r);
            if let Some(old_r) = old_r {
                parking_lot_dirty.store(true, Ordering::Release);
                gc_cascade(
                    Some(GcItem::Model(old_r)),
                    gc_producer,
                    parking_lot,
                    gc_overflow,
                    rt_status,
                );
            }
        }
    }
}

/// Drains oversampling engines delivered by the main thread via SPSC.
/// Swaps both L and R engines and sends the obsolete ones to the GC cascade.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Real-time callback signature with SPSC queues, parking lot, and dirty flag"
)]
pub fn drain_os_engines(
    os_rx: &mut Option<Consumer<Box<OsEnginePair>>>,
    os_l: &mut Box<OversampleEngine>,
    os_r: &mut Box<OversampleEngine>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow: &GcOverflowBuffer,
    rt_status: &RtStatusFlags,
) {
    let Some(rx) = os_rx.as_mut() else {
        return;
    };
    while let Ok(pair) = rx.pop() {
        let old_l = std::mem::replace(os_l, pair.l);
        let old_r = std::mem::replace(os_r, pair.r);
        parking_lot_dirty.store(true, Ordering::Release);
        gc_cascade(
            Some(GcItem::Oversample(old_l)),
            gc_producer,
            parking_lot,
            gc_overflow,
            rt_status,
        );
        gc_cascade(
            Some(GcItem::Oversample(old_r)),
            gc_producer,
            parking_lot,
            gc_overflow,
            rt_status,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
    use std::assert_matches;

    #[test]
    fn drain_slimmable_empty_no_change() {
        let mut rx = None;
        let mut model_l = None;
        let mut model_r = None;
        let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(4);
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let gc_overflow = GcOverflowBuffer::default();
        let flags = RtStatusFlags::new();

        drain_slimmable_models(
            &mut rx,
            &mut model_l,
            &mut model_r,
            &mut gc_p,
            &mut parking_lot,
            &parking_lot_dirty,
            &gc_overflow,
            &flags,
        );

        assert!(!parking_lot_dirty.load(Ordering::Acquire));
        assert!(gc_c.pop().is_err());
    }

    #[test]
    fn drain_os_engines_swaps_and_sets_dirty() {
        let (mut prod, cons) = rtrb::RingBuffer::new(4);
        let mut rx = Some(cons);
        let mut os_l = Box::new(OversampleEngine::new(OversampleFactor::Off, 64).unwrap());
        let mut os_r = Box::new(OversampleEngine::new(OversampleFactor::Off, 64).unwrap());
        let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(4);
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let gc_overflow = GcOverflowBuffer::default();
        let flags = RtStatusFlags::new();

        let pair = Box::new(OsEnginePair {
            l: Box::new(OversampleEngine::new(OversampleFactor::X2, 64).unwrap()),
            r: Box::new(OversampleEngine::new(OversampleFactor::X2, 64).unwrap()),
        });
        prod.push(pair).unwrap();

        drain_os_engines(
            &mut rx,
            &mut os_l,
            &mut os_r,
            &mut gc_p,
            &mut parking_lot,
            &parking_lot_dirty,
            &gc_overflow,
            &flags,
        );

        assert!(parking_lot_dirty.load(Ordering::Acquire));
        let old1 = gc_c.pop().unwrap();
        let old2 = gc_c.pop().unwrap();
        assert_matches!(old1, GcItem::Oversample(_));
        assert_matches!(old2, GcItem::Oversample(_));
    }
}

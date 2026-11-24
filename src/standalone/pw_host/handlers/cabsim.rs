// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Off-RT CabSim IR and neural cabinet dynamic rebuild handlers.

use super::faults::rearm_if_superseded;
use crate::standalone::colors::Colorize;
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{self, CabSimSwapPayload, RtStatusFlags};
use neural_amp_modeler_rs::dsp::cabsim::adapter::{CabSimAdapter, CabSimPair};
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
use neural_amp_modeler_rs::dsp::cabsim::loader::CabSimIr;
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use std::sync::atomic::Ordering;

/// Handles CabSim IR dynamic rebuild (partition policy and rate calibration).
pub(in crate::standalone::pw_host) fn handle_cabsim_rebuild(
    rt_status: &RtStatusFlags,
    ir_raw_samples: Option<&[f32]>,
    ir_source_rate: u32,
    ir_level_policy: crate::standalone::cli_domain::IrLevelPolicy,
    _sys: &SystemSnapshot,
    cabsim_producer: &mut rtrb::Producer<Box<CabSimSwapPayload>>,
) -> Option<u32> {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD) {
        return None;
    }
    let generation = rt_status
        .requested_cabsim_generation
        .load(Ordering::Acquire);
    let requested_partition = rt_status
        .requested_cabsim_partition_size
        .load(Ordering::Relaxed) as usize;
    let target_host_rate = rt_status.requested_cabsim_host_rate.load(Ordering::Relaxed);
    if requested_partition == 0 || target_host_rate == 0 {
        return None;
    }
    let partition_size = requested_partition.clamp(16, MAX_RESAMP_BUF);
    if partition_size != requested_partition {
        log::warn!(
            "Requested cabsim partition_size {} clamped to {}",
            requested_partition,
            partition_size
        );
    }
    let Some(raw_samples) = ir_raw_samples else {
        rt_status.clear_flag_relaxed(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD);
        return Some(0);
    };

    match build_cabsim_pair(
        raw_samples,
        ir_source_rate,
        target_host_rate,
        partition_size,
        ir_level_policy,
    ) {
        Ok(pair) => {
            let latency_samples = pair.l.latency_samples();
            let latency_ms = latency_samples as f64 * 1000.0 / f64::from(target_host_rate);
            log::info!(
                "{} Cab-sim IR rebuilt: rate={} Hz, partition_size={} ({} partitions, FFT={}, \
                 latency={} samples = {:.2} ms)",
                "🔄".cyan(),
                target_host_rate,
                partition_size,
                pair.l.num_partitions(),
                pair.l.engine().fft_size(),
                latency_samples,
                latency_ms,
            );
            let payload = Box::new(CabSimSwapPayload {
                generation,
                pair: Some(Box::new(pair)),
            });
            if cabsim_producer.push(payload).is_err() {
                log::warn!(
                    "[E3100 | PARAM_CHANNEL_FULL] Cab-sim rebuild channel full — rebuild will \
                     be retried; the audio engine is overloaded (partition={}, PW={} Hz). \
                     The swap is retried automatically until delivery succeeds.",
                    partition_size,
                    target_host_rate
                );
                return None;
            }
            rearm_if_superseded(
                rt_status,
                spsc::RT_STATUS_NEEDS_CABSIM_REBUILD,
                &rt_status.requested_cabsim_generation,
                generation,
            );
            Some(latency_samples as u32)
        }
        Err(e) => {
            log::error!(
                "[E4103 | CABSIM_BUILD_FAILED] Failed to rebuild Cab-sim IR ({} -> {} Hz, partition={}): {e:#} \
                 — bypassing cab-sim. The engine will run without cabinet simulation until a \
                 successful rebuild or the IR is cleared.",
                ir_source_rate,
                target_host_rate,
                partition_size,
            );
            let payload = Box::new(CabSimSwapPayload {
                generation,
                pair: None,
            });
            if cabsim_producer.push(payload).is_err() {
                log::warn!(
                    "[E3100 | PARAM_CHANNEL_FULL] Cab-sim bypass channel full — rebuild will \
                     be retried; the audio engine is overloaded. The swap is retried \
                     automatically until delivery succeeds."
                );
                return None;
            }
            rearm_if_superseded(
                rt_status,
                spsc::RT_STATUS_NEEDS_CABSIM_REBUILD,
                &rt_status.requested_cabsim_generation,
                generation,
            );
            Some(0)
        }
    }
}

/// Handles dynamic host-rate recalibration of a loaded 1-in/1-out neural cabinet (`.nam`/`.namb`).
pub(in crate::standalone::pw_host) fn handle_cabinet_nam_rebuild(
    rt_status: &RtStatusFlags,
    raw_bytes: &[u8],
    kind: crate::standalone::cabinet::CabAssetKind,
    display_name: &str,
    sys: &SystemSnapshot,
    cab_nam_producer: &mut rtrb::Producer<crate::standalone::cabinet::PipeCabNamPayload>,
) -> Option<u32> {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD) {
        return None;
    }
    let generation = rt_status
        .requested_cabsim_generation
        .load(Ordering::Acquire);
    let target_host_rate = rt_status.requested_cabsim_host_rate.load(Ordering::Relaxed);
    if target_host_rate == 0 {
        return None;
    }

    match crate::standalone::cabinet::build_cab_nam_slot(
        raw_bytes,
        kind,
        display_name,
        target_host_rate,
        sys,
    ) {
        Ok(slot) => {
            let lat = slot.latency_samples();
            log::info!(
                "{} Cabinet [{}] recalibrated for host rate {} Hz (latency={} samples)",
                "🔄".cyan(),
                kind.badge(),
                target_host_rate,
                lat,
            );
            let payload =
                crate::standalone::cabinet::PipeCabNamPayload::from_slot(generation, slot);
            if cab_nam_producer.push(payload).is_err() {
                log::warn!(
                    "[E3100 | PARAM_CHANNEL_FULL] Neural cabinet channel full — rebuild will be retried (PW={} Hz).",
                    target_host_rate
                );
                return None;
            }
            rearm_if_superseded(
                rt_status,
                spsc::RT_STATUS_NEEDS_CABSIM_REBUILD,
                &rt_status.requested_cabsim_generation,
                generation,
            );
            Some(lat)
        }
        Err(e) => {
            log::error!(
                "[E4103 | CABSIM_BUILD_FAILED] Failed to recalibrate neural cabinet [{}] ({} Hz): {e:#}",
                kind.badge(),
                target_host_rate,
            );
            let partition = rt_status
                .requested_cabsim_partition_size
                .load(Ordering::Relaxed);
            let payload = crate::standalone::cabinet::PipeCabNamPayload::failed(
                generation,
                kind,
                target_host_rate,
                partition,
            );
            if cab_nam_producer.push(payload).is_err() {
                log::warn!(
                    "[E3100 | PARAM_CHANNEL_FULL] Neural cabinet failure notice channel full — the rebuild decision will be retried (PW={} Hz).",
                    target_host_rate
                );
                return None;
            }
            rearm_if_superseded(
                rt_status,
                spsc::RT_STATUS_NEEDS_CABSIM_REBUILD,
                &rt_status.requested_cabsim_generation,
                generation,
            );
            Some(0)
        }
    }
}

/// Builds a stereo-decoupled [`CabSimPair`] from the preserved original IR.
fn build_cabsim_pair(
    raw_samples: &[f32],
    ir_source_rate: u32,
    target_host_rate: u32,
    partition_size: usize,
    policy: crate::standalone::cli_domain::IrLevelPolicy,
) -> anyhow::Result<CabSimPair> {
    if raw_samples.is_empty() {
        anyhow::bail!("IR has no samples");
    }
    let mut samples: Vec<f32> = if ir_source_rate != 0 && ir_source_rate != target_host_rate {
        CabSimIr::resample(raw_samples, ir_source_rate, target_host_rate).map_err(|e| {
            anyhow::anyhow!("IR resample ({ir_source_rate} -> {target_host_rate} Hz): {e}")
        })?
    } else {
        raw_samples.to_vec()
    };
    let _ = crate::standalone::cli_domain::apply_ir_level_policy(
        &mut samples,
        policy,
        target_host_rate,
    );

    let build_adapter = || {
        ConvEngine::new(&samples, partition_size)
            .map_err(|e| anyhow::anyhow!("Cab-sim engine: {e}"))
            .and_then(|engine| {
                CabSimAdapter::new(Box::new(engine))
                    .map_err(|e| anyhow::anyhow!("Cab-sim adapter: {e:?}"))
            })
    };
    let l = build_adapter()?;
    let r = build_adapter()?;
    Ok(CabSimPair {
        l: Box::new(l),
        r: Box::new(r),
        sample_rate: target_host_rate,
    })
}

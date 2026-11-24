// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Neural cabinet RT runtime slot, off-RT builder, and SPSC swap payload.

use super::asset::{CabAssetKind, parse_and_validate_cab_nam, prewarm_cab_model};
use anyhow::bail;
use neural_amp_modeler_rs::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{GcItem, GcSink};
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair;
use neural_amp_modeler_rs::dsp::cabsim::{compute_nam_tail_budget, validate_single_channel_model};
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
use neural_amp_modeler_rs::loader::{LoadOptions, load_and_build_model_from_bytes_named};
use neural_amp_modeler_rs::models::{NamModel, StaticModel};

/// Latency contributed by the single active cabinet source (WAV pair or neural slot).
///
/// The two sources are mutually exclusive, so at most one contributes. Used both by the
/// reconnect path (initial latency snapshot) and the RT prologue (dry-delay alignment), so the
/// two never disagree about what the installed cabinet adds to the graph.
#[must_use]
#[inline]
pub fn active_cabinet_latency(wav: Option<&CabSimPair>, nam: Option<&CabNamSlot>) -> u32 {
    wav.map(|pair| pair.l.latency_samples() as u32)
        .or_else(|| nam.map(CabNamSlot::latency_samples))
        .unwrap_or(0)
}

/// Active neural cabinet runtime slot owned by the RT audio callback.
pub struct CabNamSlot {
    pub kind: CabAssetKind,
    pub host_rate: u32,
    pub source_rate: u32,
    pub resampler: Box<StreamingResampleBuffer>,
    pub model_l: Box<StaticModel>,
    #[cfg(feature = "dual-mono")]
    pub model_r: Option<Box<StaticModel>>,
    /// Maximum tail drain budget in host-rate samples after input gate closure.
    pub tail_budget: usize,
    /// Remaining tail samples eligible for draining when the gate is closed.
    pub remaining_tail: usize,
}

impl CabNamSlot {
    /// Algorithmic latency introduced by the cabinet streaming resampler in host samples.
    #[must_use]
    #[inline]
    pub fn latency_samples(&self) -> u32 {
        self.resampler.latency_samples()
    }

    /// Remaining host-rate tail samples available for gate-closed draining.
    #[must_use]
    #[inline]
    pub const fn remaining_tail_samples(&self) -> usize {
        self.remaining_tail
    }

    /// Rearms the full tail budget on active audio input (called after a wet block).
    #[inline]
    pub fn rearm_tail(&mut self) {
        self.remaining_tail = self.tail_budget;
    }

    /// Starts re-engaging from bypass: resets the resampler and arms the model prewarm
    /// without running it. The silence feed is drained over several callbacks by
    /// [`Self::warm_step`], so the RT callback never pays the full prewarm in one block.
    pub fn begin_reset(&mut self) {
        self.resampler.reset();
        self.model_l.prewarm_reset();
        #[cfg(feature = "dual-mono")]
        if let Some(ref mut model_r) = self.model_r {
            model_r.prewarm_reset();
        }
    }

    /// Advances the armed prewarm by at most `budget` silence samples per model and reports
    /// whether every model is warm. Each call is bounded by the caller's per-callback budget.
    pub fn warm_step(&mut self, budget: usize) -> bool {
        self.model_l.prewarm_step(budget);
        let left_warm = self.model_l.prewarm_complete();
        #[cfg(feature = "dual-mono")]
        let right_warm = self.model_r.as_mut().is_none_or(|model_r| {
            model_r.prewarm_step(budget);
            model_r.prewarm_complete()
        });
        #[cfg(not(feature = "dual-mono"))]
        let right_warm = true;
        left_warm && right_warm
    }

    /// Processes stereo planar buffers in-place at unity gain using preallocated RT scratch buffers.
    pub fn process_block(
        &mut self,
        samples_l: &mut [f32],
        samples_r: &mut [f32],
        n: usize,
        process_mono: bool,
        scratch_l: &mut [f32],
        scratch_r: &mut [f32],
    ) {
        if n == 0 || samples_l.len() < n || samples_r.len() < n {
            return;
        }
        let model_l = &mut self.model_l;
        #[cfg(feature = "dual-mono")]
        let model_r = &mut self.model_r;

        self.resampler.process(
            &samples_l[..n],
            &samples_r[..n],
            &mut scratch_l[..n],
            &mut scratch_r[..n],
            |in_l, in_r, out_l, out_r| {
                let len = in_l.len().min(out_l.len());
                model_l.process(&in_l[..len], &mut out_l[..len]);
                if !process_mono {
                    #[cfg(feature = "dual-mono")]
                    if let Some(mr) = model_r.as_mut() {
                        mr.process(&in_r[..len], &mut out_r[..len]);
                        return len;
                    }
                    let _ = in_r;
                    out_r[..len].copy_from_slice(&out_l[..len]);
                } else {
                    out_r[..len].copy_from_slice(&out_l[..len]);
                }
                len
            },
        );
        samples_l[..n].copy_from_slice(&scratch_l[..n]);
        if process_mono {
            samples_r[..n].copy_from_slice(&scratch_l[..n]);
        } else {
            samples_r[..n].copy_from_slice(&scratch_r[..n]);
        }
    }

    /// Drains up to `n` tail frames with zero input into `samples_l`/`samples_r`.
    pub fn drain_tail(
        &mut self,
        samples_l: &mut [f32],
        samples_r: &mut [f32],
        n: usize,
        process_mono: bool,
        scratch_l: &mut [f32],
        scratch_r: &mut [f32],
    ) -> usize {
        let frames = n.min(samples_l.len()).min(samples_r.len());
        let count = frames.min(self.remaining_tail);
        if count == 0 {
            return 0;
        }
        samples_l[..frames].fill(0.0);
        samples_r[..frames].fill(0.0);
        self.process_block(
            samples_l,
            samples_r,
            count,
            process_mono,
            scratch_l,
            scratch_r,
        );
        self.remaining_tail -= count;
        count
    }

    /// Retires heap allocations directly to the GC ring without freeing on the RT thread.
    #[cold]
    pub fn retire_to_gc(self, gc: &mut GcSink<'_>) {
        gc.retire(GcItem::Model(self.model_l));
        #[cfg(feature = "dual-mono")]
        if let Some(model_r) = self.model_r {
            gc.retire(GcItem::Model(model_r));
        }
        gc.retire(GcItem::Streaming(self.resampler));
    }
}

/// Off-RT builder for a calibrated neural cabinet slot (zero calibration gain, full prewarm).
pub fn build_cab_nam_slot(
    raw_bytes: &[u8],
    kind: CabAssetKind,
    label: &str,
    host_rate: u32,
    sys: &SystemSnapshot,
) -> anyhow::Result<CabNamSlot> {
    let (data, _) = parse_and_validate_cab_nam(raw_bytes, kind)?;
    let source_rate = data
        .sample_rate
        .unwrap_or(48_000.0)
        .round()
        .clamp(1.0, 768_000.0) as u32;
    let dual_mono = cfg!(feature = "dual-mono");
    let loaded = load_and_build_model_from_bytes_named(
        raw_bytes,
        label,
        sys,
        dual_mono,
        LoadOptions::default(),
    )
    .map_err(|e| anyhow::anyhow!("Cabinet model load failed: {e}"))?;

    let mut model_l = loaded
        .model_l
        .ok_or_else(|| anyhow::anyhow!("Cabinet model loader produced no left channel"))?;
    if let Err((in_ch, out_ch)) = validate_single_channel_model(&model_l) {
        bail!("Cabinet neural models must be strictly 1-in/1-out (built {in_ch}x{out_ch}).");
    }
    model_l
        .set_max_buffer_size(MAX_RESAMP_BUF)
        .map_err(|e| anyhow::anyhow!("Cabinet model_l max buffer init failed: {e}"))?;
    prewarm_cab_model(&mut model_l);

    #[cfg(feature = "dual-mono")]
    let model_r = {
        let mut mr = loaded.model_r;
        if let Some(ref mut model_r) = mr {
            if let Err((in_ch, out_ch)) = validate_single_channel_model(model_r) {
                bail!(
                    "Cabinet right model channel must be strictly 1-in/1-out (built {in_ch}x{out_ch})."
                );
            }
            model_r
                .set_max_buffer_size(MAX_RESAMP_BUF)
                .map_err(|e| anyhow::anyhow!("Cabinet model_r max buffer init failed: {e}"))?;
            prewarm_cab_model(model_r);
        }
        mr
    };

    let resampler = StreamingResampleBuffer::new(host_rate.max(1), source_rate, MAX_RESAMP_BUF)
        .map_err(|e| anyhow::anyhow!("Cabinet resampler init failed: {e:?}"))?;

    let tail_budget = compute_nam_tail_budget(&model_l, source_rate, host_rate.max(1));

    Ok(CabNamSlot {
        kind,
        host_rate: host_rate.max(1),
        source_rate,
        resampler: Box::new(resampler),
        model_l,
        #[cfg(feature = "dual-mono")]
        model_r,
        tail_budget,
        remaining_tail: 0,
    })
}

/// Unboxed SPSC payload carrying a newly built neural cabinet slot (or clear) off-RT -> RT.
pub struct PipeCabNamPayload {
    pub generation: u64,
    pub host_rate: u32,
    pub kind: CabAssetKind,
    pub source_rate: u32,
    pub resampler: Option<Box<StreamingResampleBuffer>>,
    pub model_l: Option<Box<StaticModel>>,
    #[cfg(feature = "dual-mono")]
    pub model_r: Option<Box<StaticModel>>,
    /// Precomputed host-rate tail budget for gate-closed draining.
    pub tail_budget: usize,
    /// `Some(partition)` marks a failed build: nothing is installed and the RT drain
    /// latches `(host_rate, partition)` so the same request is not rebuilt until it changes.
    pub failed_partition: Option<u32>,
}

impl PipeCabNamPayload {
    /// Wraps a newly built [`CabNamSlot`] into an unboxed SPSC swap payload.
    #[must_use]
    pub fn from_slot(generation: u64, slot: CabNamSlot) -> Self {
        Self {
            generation,
            host_rate: slot.host_rate,
            kind: slot.kind,
            source_rate: slot.source_rate,
            resampler: Some(slot.resampler),
            model_l: Some(slot.model_l),
            #[cfg(feature = "dual-mono")]
            model_r: slot.model_r,
            tail_budget: slot.tail_budget,
            failed_partition: None,
        }
    }

    /// Builds the failure notice for a neural cabinet rebuild that produced no slot.
    #[must_use]
    pub fn failed(generation: u64, kind: CabAssetKind, host_rate: u32, partition: u32) -> Self {
        Self {
            generation,
            host_rate,
            kind,
            source_rate: 0,
            resampler: None,
            model_l: None,
            #[cfg(feature = "dual-mono")]
            model_r: None,
            tail_budget: 0,
            failed_partition: Some(partition),
        }
    }

    /// Consumes a clear/empty payload by retiring any leftover boxed components to GC.
    #[cold]
    pub fn retire_remainder_to_gc(self, gc: &mut GcSink<'_>) {
        if let Some(m) = self.model_l {
            gc.retire(GcItem::Model(m));
        }
        #[cfg(feature = "dual-mono")]
        if let Some(m) = self.model_r {
            gc.retire(GcItem::Model(m));
        }
        if let Some(r) = self.resampler {
            gc.retire(GcItem::Streaming(r));
        }
    }
}

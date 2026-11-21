// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Single-slot cabinet simulation (`.wav` IR or 1-in/1-out `.nam`/`.namb`) and lock-free bypass.

use anyhow::{Context, bail};
use neural_amp_modeler_rs::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{
    GcItem, GcSink, RT_STATUS_SPSC_DRAIN_TRUNCATED, RT_STATUS_STRUCTURAL_DEFERRED,
    RT_STATUS_STRUCTURAL_SUPERSEDED, RtStatusFlags, SwapBudget,
};
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair;
use neural_amp_modeler_rs::dsp::cabsim::{compute_nam_tail_budget, validate_single_channel_model};
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
use neural_amp_modeler_rs::dsp::utils::DelayLine;
use neural_amp_modeler_rs::loader::nam_json::{NamModelData, parse_nam_json};
use neural_amp_modeler_rs::loader::namb::parse_namb_typed;
use neural_amp_modeler_rs::loader::{LoadOptions, load_and_build_model_from_bytes_named};
use neural_amp_modeler_rs::models::{NamModel, StaticModel};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Maximum additional delay capacity for the cabinet dry-alignment line.
pub const CAB_DRY_DELAY_MAX_EXTRA: usize = 3200;

/// Length in samples of the raised-cosine cabinet bypass crossfade (~1.33 ms @ 48 kHz).
pub const CAB_BYPASS_CROSSFADE_SAMPLES: usize = 64;

/// Format kind currently held by the exclusive cabinet slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CabAssetKind {
    /// Impulse response loaded from a `.wav` file (UPOLS convolution).
    WavIr,
    /// Neural cabinet model loaded from a `.nam` JSON file (1-in/1-out).
    NamModel,
    /// Neural cabinet model loaded from a `.namb` binary file (1-in/1-out).
    NambModel,
}

impl CabAssetKind {
    /// Short uppercase format badge for CLI banner and logs.
    #[must_use]
    pub const fn badge(self) -> &'static str {
        match self {
            Self::WavIr => "WAV",
            Self::NamModel => "NAM",
            Self::NambModel => "NAMB",
        }
    }

    /// Detects whether a file belongs to `.wav`, `.nam`, or `.namb`.
    pub fn detect(path: &Path, bytes: &[u8]) -> anyhow::Result<Self> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        match ext.as_deref() {
            Some("wav") => Ok(Self::WavIr),
            Some("namb") => Ok(Self::NambModel),
            Some("nam") => Ok(Self::NamModel),
            _ if bytes.starts_with(b"NAMB") => Ok(Self::NambModel),
            _ if bytes.starts_with(b"RIFF") => Ok(Self::WavIr),
            _ => bail!(
                "Unsupported cabinet file extension '{}'. Expected .wav, .nam, or .namb.",
                path.display()
            ),
        }
    }
}

/// Informational metadata extracted from a neural cabinet file (display only).
#[derive(Debug, Clone, Default)]
pub struct CabNamMeta {
    pub architecture: String,
    pub gear_type: Option<String>,
    pub gear_make: Option<String>,
    pub gear_model: Option<String>,
    pub source_rate: u32,
}

/// Initial cabinet asset loaded at startup and retained off-RT for host rate renegotiation.
#[derive(Debug, Clone)]
pub enum InitialCabSetup {
    /// Unmodified WAV IR samples + native sample rate (policy applied per rebuild).
    WavIr {
        raw_samples: Vec<f32>,
        source_rate: u32,
        display_name: String,
    },
    /// Unmodified raw bytes of a validated 1-in/1-out neural cabinet model.
    Nam {
        raw_bytes: Vec<u8>,
        kind: CabAssetKind,
        source_rate: u32,
        display_name: String,
        meta: CabNamMeta,
    },
}

impl InitialCabSetup {
    /// Format kind of this loaded cabinet asset.
    #[must_use]
    pub const fn kind(&self) -> CabAssetKind {
        match self {
            Self::WavIr { .. } => CabAssetKind::WavIr,
            Self::Nam { kind, .. } => *kind,
        }
    }

    /// Disambiguated `parent/file.ext` label for banner and logs.
    #[must_use]
    pub fn display_name(&self) -> &str {
        match self {
            Self::WavIr { display_name, .. } | Self::Nam { display_name, .. } => display_name,
        }
    }
}

/// Formats `path` as `parent/file.ext` to distinguish duplicate filenames across folders.
#[must_use]
pub fn disambiguate_cab_path(path: &Path) -> String {
    let file = path
        .file_name()
        .map(|s| s.to_string_lossy())
        .unwrap_or_else(|| path.to_string_lossy());
    if let Some(parent) = path.parent().and_then(|p| p.file_name()) {
        format!("{}/{file}", parent.to_string_lossy())
    } else {
        file.into_owned()
    }
}

fn config_tree_is_single_channel(data: &NamModelData) -> bool {
    if data.config.in_channels.unwrap_or(1) != 1 || data.config.out_channels.unwrap_or(1) != 1 {
        return false;
    }
    if let Some(models) = &data.config.models {
        for child in models {
            if let Ok(sub) = serde_json::from_value::<NamModelData>(child.clone())
                && !config_tree_is_single_channel(&sub)
            {
                return false;
            }
        }
    }
    true
}

/// Validates that a parsed neural model is a single-channel (1-in/1-out) cabinet candidate.
///
/// Post-stack WaveNet `config.head` (including the canonical multi-stage and
/// legacy forms) is accepted here: the engine 3.0.0 loader builds and validates
/// the `PostStackHead` fail-closed, so the cabinet slot only enforces the
/// channel contract.
pub fn validate_cab_nam_data(data: &NamModelData) -> anyhow::Result<()> {
    if !config_tree_is_single_channel(data) {
        bail!(
            "Cabinet neural models must be strictly 1-in/1-out (found {}x{}).",
            data.config.in_channels.unwrap_or(1),
            data.config.out_channels.unwrap_or(1)
        );
    }
    Ok(())
}

/// Parses `.nam` or `.namb` cabinet bytes, enforcing the 1x1 channel contract while
/// ignoring amp `gear_*` metadata restrictions and calibration fields.
pub fn parse_and_validate_cab_nam(
    bytes: &[u8],
    kind: CabAssetKind,
) -> anyhow::Result<(NamModelData, CabNamMeta)> {
    let data = match kind {
        CabAssetKind::NamModel => {
            let text =
                std::str::from_utf8(bytes).context("Cabinet .nam file is not valid UTF-8")?;
            parse_nam_json(text).context("Failed to parse cabinet .nam JSON")?
        }
        CabAssetKind::NambModel => {
            parse_namb_typed(bytes).context("Failed to parse cabinet .namb binary")?
        }
        CabAssetKind::WavIr => bail!("Expected neural cabinet bytes, got WAV IR kind"),
    };
    validate_cab_nam_data(&data)?;
    let source_rate = data
        .sample_rate
        .unwrap_or(48_000.0)
        .round()
        .clamp(1.0, 768_000.0) as u32;
    let meta = CabNamMeta {
        architecture: data.architecture.clone(),
        gear_type: data.metadata.as_ref().and_then(|m| m.gear_type.clone()),
        gear_make: data.metadata.as_ref().and_then(|m| m.gear_make.clone()),
        gear_model: data.metadata.as_ref().and_then(|m| m.gear_model.clone()),
        source_rate,
    };
    Ok((data, meta))
}

fn prewarm_cab_model(model: &mut StaticModel) {
    model.prewarm_reset();
    for _ in 0..64 {
        let rem = model.prewarm_step(MAX_RESAMP_BUF);
        if rem == 0 || model.prewarm_complete() {
            break;
        }
    }
}

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
    ///
    /// The first `min(n, remaining_tail)` frames are rendered through the model with a
    /// zero feed (ring-out) and the rest of both buffers is filled with silence. Does
    /// not re-arm the tail budget; returns the number of frames actually drained.
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

/// Pre-allocated dual-mono dry delay line keeping bypassed audio aligned with cabinet latency.
pub struct CabDryDelay {
    line_l: DelayLine<f32>,
    line_r: DelayLine<f32>,
    last_mono: bool,
}

impl CabDryDelay {
    /// Allocates both L/R circular buffers off-RT for `[0, MAX_RESAMP_BUF + CAB_DRY_DELAY_MAX_EXTRA]`.
    #[cold]
    #[must_use]
    pub fn new(delay: usize) -> Self {
        let cap = MAX_RESAMP_BUF + CAB_DRY_DELAY_MAX_EXTRA;
        Self {
            line_l: DelayLine::with_capacity(cap, delay),
            line_r: DelayLine::with_capacity(cap, delay),
            last_mono: false,
        }
    }

    /// Sets the delay in samples without reallocating.
    #[inline]
    pub fn set_delay(&mut self, delay: usize) {
        self.line_l.set_delay(delay);
        self.line_r.set_delay(delay);
    }

    /// Processes L/R blocks through the dry delay rings into `out_l`/`out_r`.
    pub fn process_block(
        &mut self,
        in_l: &[f32],
        in_r: &[f32],
        out_l: &mut [f32],
        out_r: &mut [f32],
        n: usize,
        mono: bool,
    ) {
        if n == 0 {
            return;
        }
        if self.last_mono && !mono {
            self.line_r.copy_state_from(&self.line_l);
        }
        self.last_mono = mono;
        self.line_l.process_block(&in_l[..n], &mut out_l[..n]);
        if mono {
            out_r[..n].copy_from_slice(&out_l[..n]);
        } else {
            self.line_r.process_block(&in_r[..n], &mut out_r[..n]);
        }
    }

    /// Feeds zeros when the upstream Silence gate closes so stale samples do not linger in the ring.
    pub fn process_zeros(&mut self, zeros: &[f32], scratch: &mut [f32], n: usize, mono: bool) {
        if n == 0 {
            return;
        }
        self.line_l.process_block(&zeros[..n], &mut scratch[..n]);
        if !mono {
            self.line_r.process_block(&zeros[..n], &mut scratch[..n]);
        }
        self.last_mono = mono;
    }
}

/// Routes the delayed dry signal to both wet outputs (unity, no cabinet processing).
#[inline]
fn emit_dry(
    wet_l: &mut [f32],
    wet_r: &mut [f32],
    dry_l: &[f32],
    dry_r: &[f32],
    n: usize,
    process_mono: bool,
) {
    wet_l[..n].copy_from_slice(&dry_l[..n]);
    if process_mono {
        wet_r[..n].copy_from_slice(&dry_l[..n]);
    } else {
        wet_r[..n].copy_from_slice(&dry_r[..n]);
    }
}

/// Lock-free 64-sample raised-cosine crossfade state for the cabinet bypass switch.
pub struct CabBypassState {
    curve: [f32; CAB_BYPASS_CROSSFADE_SAMPLES + 1],
    step: usize,
    was_bypassed: bool,
    /// Set whenever the cabinet was bypassed; consumed by the next re-engage reset.
    warm_pending: bool,
    /// True while a neural cabinet's prewarm is being drained (output stays dry).
    warming: bool,
}

impl Default for CabBypassState {
    fn default() -> Self {
        Self::new(false)
    }
}

impl CabBypassState {
    /// Initializes precomputed raised-cosine crossfade coefficients off-RT.
    #[cold]
    #[must_use]
    pub fn new(initially_bypassed: bool) -> Self {
        let mut curve = [0.0f32; CAB_BYPASS_CROSSFADE_SAMPLES + 1];
        for (i, slot) in curve.iter_mut().enumerate() {
            let phase = core::f64::consts::PI * (i as f64) / (CAB_BYPASS_CROSSFADE_SAMPLES as f64);
            *slot = (0.5 * (1.0 - phase.cos())) as f32;
        }
        curve[0] = 0.0;
        curve[CAB_BYPASS_CROSSFADE_SAMPLES] = 1.0;
        Self {
            curve,
            step: if initially_bypassed {
                0
            } else {
                CAB_BYPASS_CROSSFADE_SAMPLES
            },
            was_bypassed: initially_bypassed,
            warm_pending: initially_bypassed,
            warming: false,
        }
    }

    /// Returns true if the previous block ran in steady bypassed mode (no wet signal processed).
    #[inline]
    #[must_use]
    pub const fn was_steady_bypassed(&self) -> bool {
        self.was_bypassed && self.step == 0
    }

    /// Executes the exclusive cabinet stage on `resamp_out_l`/`resamp_out_r` (in-place).
    ///
    /// Keeps delayed dry time-aligned with cabinet latency, skips UPOLS/NAM execution when in
    /// steady-state bypass, calls `reset()` before re-engaging wet from bypass, and blends
    /// smoothly over 64 samples without glitches. Returns `true` if a cabinet asset processed or
    /// bypassed while loaded.
    #[expect(
        clippy::too_many_arguments,
        reason = "Cabinet stage combines exclusive WavIr/NamModel slots, dry delay, and RT scratch buffers cleanly"
    )]
    pub fn process_stage(
        &mut self,
        target_bypass: bool,
        wav_pair: Option<&mut CabSimPair>,
        nam_slot: Option<&mut CabNamSlot>,
        dry_delay: &mut CabDryDelay,
        dry_l: &mut [f32],
        dry_r: &mut [f32],
        wet_l: &mut [f32],
        wet_r: &mut [f32],
        scratch_l: &mut [f32],
        scratch_r: &mut [f32],
        n: usize,
        process_mono: bool,
        rt_status: &RtStatusFlags,
    ) -> bool {
        if n == 0 || (wav_pair.is_none() && nam_slot.is_none()) {
            return false;
        }
        let has_active_wav = wav_pair.is_some();
        let mut wav_pair = wav_pair;
        let mut nam_slot = nam_slot;

        // Always advance the dry delay with pre-cabinet audio so bypass transitions never jump.
        dry_delay.process_block(&wet_l[..n], &wet_r[..n], dry_l, dry_r, n, process_mono);

        if target_bypass {
            self.warm_pending = true;
        }

        // Steady bypass: output delayed dry directly and skip heavy DSP.
        if target_bypass && self.step == 0 {
            self.warming = false;
            self.was_bypassed = true;
            emit_dry(wet_l, wet_r, dry_l, dry_r, n, process_mono);
            return true;
        }

        // Reset stale convolution tails / recurrent states right before re-engaging wet.
        // A neural cabinet only arms its prewarm here; the feed is drained below.
        if !target_bypass && self.step == 0 && self.warm_pending {
            self.warm_pending = false;
            if let Some(pair) = wav_pair.as_deref_mut() {
                pair.l.reset();
                pair.r.reset();
            } else if let Some(nam) = nam_slot.as_deref_mut() {
                nam.begin_reset();
                self.warming = true;
            }
        }

        // Neural warm-up: stay dry while the prewarm drains within the per-callback budget,
        // then engage wet from step 0 so the crossfade starts from a fully warm model.
        if self.warming {
            let warm = nam_slot.as_deref_mut().is_none_or(|nam| nam.warm_step(n));
            if !warm {
                self.was_bypassed = true;
                emit_dry(wet_l, wet_r, dry_l, dry_r, n, process_mono);
                return true;
            }
            self.warming = false;
        }
        self.was_bypassed = false;

        if let Some(pair) = wav_pair {
            if process_mono {
                pair.l.process_block(&mut wet_l[..n], Some(rt_status));
                wet_r[..n].copy_from_slice(&wet_l[..n]);
            } else {
                pair.process_block_stereo(&mut wet_l[..n], &mut wet_r[..n], Some(rt_status));
            }
            pair.rearm_tail();
        } else if let Some(nam) = nam_slot {
            nam.process_block(wet_l, wet_r, n, process_mono, scratch_l, scratch_r);
            nam.rearm_tail();
        }

        let target_step = if target_bypass {
            0
        } else {
            CAB_BYPASS_CROSSFADE_SAMPLES
        };
        if self.step != target_step {
            for i in 0..n {
                if self.step < target_step {
                    self.step += 1;
                } else if self.step > target_step {
                    self.step -= 1;
                }
                let g_wet = self.curve[self.step];
                let g_dry = 1.0 - g_wet;
                wet_l[i] = wet_l[i] * g_wet + dry_l[i] * g_dry;
                wet_r[i] = if process_mono {
                    wet_l[i]
                } else {
                    wet_r[i] * g_wet + dry_r[i] * g_dry
                };
            }
        } else if !has_active_wav && process_mono {
            wet_r[..n].copy_from_slice(&wet_l[..n]);
        }

        true
    }
}

/// Per-callback accounting returned by [`drain_cab_nam_payloads`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CabNamDrainStats {
    /// Payloads popped from the SPSC consumer during this callback.
    pub pops: usize,
    /// Neural cabinet slots installed into `active_nam` (`0` or `1`).
    pub installed: usize,
    /// Payloads retired to GC without becoming the active slot (stale, superseded, failed, or malformed).
    pub retired: usize,
    /// Valid non-stale payloads left queued because `SwapBudget` was exhausted (`0` or `1`).
    pub deferred: usize,
}

/// Checks if `requested_nam_gen` exceeds `applied_gen` atomically on the RT drain,
/// enforcing the callback-shared [`SwapBudget`] via `peek()` before `pop()`.
///
/// Kept out of line: the callback invokes it on every quantum, but the
/// payload-carrying code paths and their unwind cleanup belong here instead of
/// inflating the capture closure or emitting structural drops there.
#[inline(never)]
#[expect(
    clippy::too_many_arguments,
    reason = "The drain needs the budget, both generation counters, the exclusive WAV/NAM slots, the latency sink, the failure latch and the GC sink"
)]
pub fn drain_cab_nam_payloads(
    consumer: &mut rtrb::Consumer<PipeCabNamPayload>,
    budget: &mut SwapBudget,
    requested_generation: &AtomicU64,
    applied_generation: &AtomicU64,
    active_wav: &mut Option<Box<CabSimPair>>,
    active_nam: &mut Option<CabNamSlot>,
    active_cabsim_latency: &dyn Fn(u32),
    failed_key: Option<&mut Option<(u32, usize)>>,
    gc: &mut GcSink<'_>,
) -> CabNamDrainStats {
    let max_pops = crate::standalone::pw_host::STRUCTURAL_POPS_PER_CALLBACK;
    let target_gen = requested_generation.load(Ordering::Acquire);
    let mut stats = CabNamDrainStats::default();
    let mut latest: Option<PipeCabNamPayload> = None;

    while stats.pops < max_pops {
        let Ok(head) = consumer.peek() else {
            break;
        };
        if head.generation < target_gen {
            if let Ok(payload) = consumer.pop() {
                stats.pops += 1;
                stats.retired += 1;
                payload.retire_remainder_to_gc(gc);
            }
            continue;
        }
        if latest.is_some() {
            // Coalesce latest-wins inside the pop window without extra budget.
            if let Ok(payload) = consumer.pop() {
                stats.pops += 1;
                stats.retired += 1;
                gc.rt_status.set_flag(RT_STATUS_STRUCTURAL_SUPERSEDED);
                gc.rt_status
                    .structural_superseded_total
                    .fetch_add(1, Ordering::Relaxed);
                if let Some(prev) = latest.replace(payload) {
                    prev.retire_remainder_to_gc(gc);
                }
            }
            continue;
        }
        if !budget.can_apply() {
            // Keep valid payload in the ring when budget is exhausted (zero-loss, FIFO).
            stats.deferred += 1;
            gc.rt_status.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            gc.rt_status
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
            break;
        }
        if let Ok(payload) = consumer.pop() {
            stats.pops += 1;
            // The `latest.is_some()` arm above handled coalescing, so this
            // branch parks into an empty slot. Forget the replaced `None` to
            // keep the payload destructor out of the RT callback.
            debug_assert!(latest.is_none(), "cabinet candidate slot must be empty");
            std::mem::forget(latest.replace(payload));
        }
    }

    if stats.pops >= max_pops && !consumer.is_empty() {
        gc.rt_status.set_flag(RT_STATUS_SPSC_DRAIN_TRUNCATED);
    }

    let Some(mut chosen) = latest else {
        return stats;
    };
    if let Some(partition) = chosen.failed_partition {
        // Fail-closed: the last installed slot is kept (its host rate no longer matches,
        // so the stage gates it off), and the request is latched until the key changes.
        if let Some(failed) = failed_key {
            *failed = Some((chosen.host_rate, partition as usize));
        }
        applied_generation.store(chosen.generation, Ordering::Release);
        chosen.retire_remainder_to_gc(gc);
        stats.retired += 1;
        return stats;
    }
    let model_l = chosen.model_l.take();
    let resampler = chosen.resampler.take();
    let (model_l, resampler) = match (model_l, resampler) {
        (Some(model_l), Some(resampler)) => (model_l, resampler),
        (model_l, resampler) => {
            // Retire every extracted component explicitly: dropping the tuple
            // here would run `StaticModel`/`StreamingResampleBuffer` destructors
            // on the audio thread.
            if let Some(model) = model_l {
                gc.retire(GcItem::Model(model));
            }
            if let Some(resampler) = resampler {
                gc.retire(GcItem::Streaming(resampler));
            }
            chosen.retire_remainder_to_gc(gc);
            stats.retired += 1;
            return stats;
        }
    };
    #[cfg(feature = "dual-mono")]
    let model_r = chosen.model_r.take();

    // Enforce strict exclusivity: retire any active WAV pair before installing neural cabinet.
    if let Some(old_wav) = active_wav.take() {
        gc.retire(GcItem::CabSimPair(old_wav));
    }
    if let Some(old_nam) = active_nam.take() {
        old_nam.retire_to_gc(gc);
    }

    let slot = CabNamSlot {
        kind: chosen.kind,
        host_rate: chosen.host_rate,
        source_rate: chosen.source_rate,
        resampler,
        model_l,
        #[cfg(feature = "dual-mono")]
        model_r,
        tail_budget: chosen.tail_budget,
        remaining_tail: 0,
    };
    let lat = slot.latency_samples();
    debug_assert!(active_nam.is_none());
    std::mem::forget(active_nam.replace(slot));
    budget.consume();
    stats.installed += 1;
    if let Some(failed) = failed_key {
        *failed = None;
    }
    active_cabsim_latency(lat);
    applied_generation.store(chosen.generation, Ordering::Release);
    // Every heap component was moved into the active slot; forget the emptied
    // envelope so no `drop_glue::<PipeCabNamPayload>` is emitted on the RT path.
    std::mem::forget(chosen);
    stats
}

#[cfg(test)]
#[path = "cabinet_test.rs"]
mod cabinet_test;

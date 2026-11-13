// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Single-slot cabinet simulation (`.wav` IR or 1-in/1-out `.nam`/`.namb`) and lock-free bypass.

use anyhow::{Context, bail};
use neural_amp_modeler_rs::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{GcItem, GcSink, RtStatusFlags};
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair;
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
use neural_amp_modeler_rs::dsp::utils::DelayLine;
use neural_amp_modeler_rs::loader::nam_json::{NamModelData, parse_nam_json};
use neural_amp_modeler_rs::loader::namb::parse_namb_typed;
use neural_amp_modeler_rs::loader::{LoadOptions, load_and_build_model_from_bytes_named};
use neural_amp_modeler_rs::models::{NamModel, StaticModel};
use serde_json::Value;
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

fn has_non_null_head_field(value: &Value) -> bool {
    value.as_object().is_some_and(|obj| {
        obj.get("head")
            .is_some_and(|head| !head.is_null() && head.is_object())
    })
}

fn value_tree_has_head(value: &Value, depth: usize) -> bool {
    if depth > 16 {
        return true;
    }
    let Some(obj) = value.as_object() else {
        return false;
    };
    let is_wavenet = obj
        .get("architecture")
        .and_then(Value::as_str)
        .is_some_and(|a| a.eq_ignore_ascii_case("WaveNet"));
    let cfg = obj.get("config").unwrap_or(value);
    if is_wavenet && has_non_null_head_field(cfg) {
        return true;
    }
    if let Some(cond) = cfg.get("condition_dsp")
        && value_tree_has_head(cond, depth + 1)
    {
        return true;
    }
    if let Some(models) = cfg.get("models").and_then(Value::as_array) {
        for child in models {
            if value_tree_has_head(child, depth + 1) {
                return true;
            }
        }
    }
    if let Some(variants) = cfg.get("models").and_then(Value::as_object) {
        for child in variants.values() {
            if value_tree_has_head(child, depth + 1) {
                return true;
            }
        }
    }
    false
}

/// Rejects `WaveNet` post-stack `head` in the cabinet slot (fail-closed until post-stack WaveNet heads are supported).
fn has_wavenet_post_stack_head(data: &NamModelData) -> bool {
    if data.architecture.eq_ignore_ascii_case("WaveNet") && data.config.head.is_some() {
        return true;
    }
    if let Some(cond) = &data.config.condition_dsp
        && value_tree_has_head(cond, 1)
    {
        return true;
    }
    if let Some(models) = &data.config.models {
        for child in models {
            if value_tree_has_head(child, 1) {
                return true;
            }
        }
    }
    false
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

/// Validates that a parsed neural model is a single-channel (1-in/1-out) cabinet candidate
/// without an unsupported WaveNet post-stack `head`.
pub fn validate_cab_nam_data(data: &NamModelData) -> anyhow::Result<()> {
    if !config_tree_is_single_channel(data) {
        bail!(
            "Cabinet neural models must be strictly 1-in/1-out (found {}x{}).",
            data.config.in_channels.unwrap_or(1),
            data.config.out_channels.unwrap_or(1)
        );
    }
    if has_wavenet_post_stack_head(data) {
        bail!("WaveNet post-stack 'head' in cabinet slot is not yet supported.");
    }
    Ok(())
}

/// Parses `.nam` or `.namb` cabinet bytes, enforcing 1x1 channels and post-stack `head` guards
/// while ignoring amp `gear_*` metadata restrictions and calibration fields.
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

/// Active neural cabinet runtime slot owned by the RT audio callback.
pub struct CabNamSlot {
    pub kind: CabAssetKind,
    pub host_rate: u32,
    pub source_rate: u32,
    pub resampler: Box<StreamingResampleBuffer>,
    pub model_l: Box<StaticModel>,
    #[cfg(feature = "dual-mono")]
    pub model_r: Option<Box<StaticModel>>,
}

impl CabNamSlot {
    /// Algorithmic latency introduced by the cabinet streaming resampler in host samples.
    #[must_use]
    #[inline]
    pub fn latency_samples(&self) -> u32 {
        self.resampler.latency_samples()
    }

    /// Resets resampler and neural model state without allocations (called before re-engaging from bypass).
    pub fn reset(&mut self) {
        self.resampler.reset();
        prewarm_cab_model(&mut self.model_l);
        #[cfg(feature = "dual-mono")]
        if let Some(ref mut model_r) = self.model_r {
            prewarm_cab_model(model_r);
        }
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
    if model_l.in_channels() != 1 || model_l.num_output_channels() != 1 {
        bail!(
            "Cabinet neural models must be strictly 1-in/1-out (built {}x{}).",
            model_l.in_channels(),
            model_l.num_output_channels()
        );
    }
    model_l
        .set_max_buffer_size(MAX_RESAMP_BUF)
        .map_err(|e| anyhow::anyhow!("Cabinet model_l max buffer init failed: {e}"))?;
    prewarm_cab_model(&mut model_l);

    #[cfg(feature = "dual-mono")]
    let model_r = {
        let mut mr = loaded.model_r;
        if let Some(ref mut model_r) = mr {
            if model_r.in_channels() != 1 || model_r.num_output_channels() != 1 {
                bail!(
                    "Cabinet right model channel must be strictly 1-in/1-out (built {}x{}).",
                    model_r.in_channels(),
                    model_r.num_output_channels()
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

    Ok(CabNamSlot {
        kind,
        host_rate: host_rate.max(1),
        source_rate,
        resampler: Box::new(resampler),
        model_l,
        #[cfg(feature = "dual-mono")]
        model_r,
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

/// Lock-free 64-sample raised-cosine crossfade state for the cabinet bypass switch.
pub struct CabBypassState {
    curve: [f32; CAB_BYPASS_CROSSFADE_SAMPLES + 1],
    step: usize,
    was_bypassed: bool,
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

        // Steady bypass: output delayed dry directly and skip heavy DSP.
        if target_bypass && self.step == 0 {
            self.was_bypassed = true;
            wet_l[..n].copy_from_slice(&dry_l[..n]);
            if process_mono {
                wet_r[..n].copy_from_slice(&dry_l[..n]);
            } else {
                wet_r[..n].copy_from_slice(&dry_r[..n]);
            }
            return true;
        }

        // Reset stale convolution tails / recurrent states right before re-engaging wet.
        if !target_bypass && self.step == 0 {
            if let Some(pair) = wav_pair.as_deref_mut() {
                pair.l.reset();
                pair.r.reset();
            } else if let Some(nam) = nam_slot.as_deref_mut() {
                nam.reset();
            }
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

/// Checks if `requested_nam_gen` exceeds `applied_gen` atomically on the RT drain.
#[inline]
pub fn drain_cab_nam_payloads(
    consumer: &mut rtrb::Consumer<PipeCabNamPayload>,
    requested_generation: &AtomicU64,
    applied_generation: &AtomicU64,
    active_wav: &mut Option<Box<CabSimPair>>,
    active_nam: &mut Option<CabNamSlot>,
    active_cabsim_latency: &dyn Fn(u32),
    gc: &mut GcSink<'_>,
) {
    let target_gen = requested_generation.load(Ordering::Acquire);
    let mut latest: Option<PipeCabNamPayload> = None;
    let mut popped = 0usize;
    while popped < 8 {
        let Ok(payload) = consumer.pop() else {
            break;
        };
        popped += 1;
        if payload.generation < target_gen {
            payload.retire_remainder_to_gc(gc);
            continue;
        }
        if let Some(prev) = latest.replace(payload) {
            prev.retire_remainder_to_gc(gc);
        }
    }
    let Some(mut chosen) = latest else {
        return;
    };
    let (Some(model_l), Some(resampler)) = (chosen.model_l.take(), chosen.resampler.take()) else {
        chosen.retire_remainder_to_gc(gc);
        return;
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
    };
    let lat = slot.latency_samples();
    *active_nam = Some(slot);
    active_cabsim_latency(lat);
    applied_generation.store(chosen.generation, Ordering::Release);
}

#[cfg(test)]
#[path = "cabinet_test.rs"]
mod cabinet_test;

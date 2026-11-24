// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Constant-latency cabinet dry alignment and lock-free raised-cosine bypass crossfade.

use super::slot::CabNamSlot;
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair;
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::dsp::utils::DelayLine;

/// Maximum additional delay capacity for the cabinet dry-alignment line.
pub const CAB_DRY_DELAY_MAX_EXTRA: usize = 3200;

/// Length in samples of the raised-cosine cabinet bypass crossfade (~1.33 ms @ 48 kHz).
pub const CAB_BYPASS_CROSSFADE_SAMPLES: usize = 64;

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

        dry_delay.process_block(&wet_l[..n], &wet_r[..n], dry_l, dry_r, n, process_mono);

        if target_bypass {
            self.warm_pending = true;
        }

        if target_bypass && self.step == 0 {
            self.warming = false;
            self.was_bypassed = true;
            emit_dry(wet_l, wet_r, dry_l, dry_r, n, process_mono);
            return true;
        }

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

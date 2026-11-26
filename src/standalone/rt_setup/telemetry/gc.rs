// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! GC and command-flag telemetry.

use super::state::TelemetryLatches;
use neural_amp_modeler_rs::common::spsc::{
    RT_STATUS_GC_CORRUPTED, RT_STATUS_GC_OVERFLOW, RT_STATUS_GC_TIER3,
    RT_STATUS_PARAM_QUEUE_BACKLOG, RT_STATUS_SLIMMABLE_RESET_FAILED,
    RT_STATUS_SLIMMABLE_SLICE_FAILED, RT_STATUS_STRUCTURAL_DEFERRED,
    RT_STATUS_STRUCTURAL_SUPERSEDED, RtStatusFlags,
};

pub(super) fn poll_gc_and_command_flags(rt_status: &RtStatusFlags, latches: &mut TelemetryLatches) {
    if latches
        .gc_overflow
        .observe(rt_status.check_and_clear_flag(RT_STATUS_GC_OVERFLOW))
    {
        log::warn!(
            "[E3101 | GC_OVERFLOW] Garbage Collection (GC) channel overflow detected — \
             the audio thread had to leak memory to avoid dropouts in the hot-path; \
             NAM-Audio-Pipe will drain the buffer aggressively now."
        );
    }

    if latches
        .gc_tier3
        .observe(rt_status.check_and_clear_flag(RT_STATUS_GC_TIER3))
    {
        log::warn!(
            "[E3101 | GC_OVERFLOW] GC cascade reached Tier 3 (SPSC channel and parking lot \
             both full) — items are being parked in the overflow buffer; \
             NAM-Audio-Pipe will drain the overflow buffer aggressively now."
        );
    }

    if latches
        .gc_corrupted
        .observe(rt_status.check_and_clear_flag(RT_STATUS_GC_CORRUPTED))
    {
        log::error!(
            "[E3102 | GC_CORRUPTED] Garbage Collection overflow buffer corruption detected — \
             a GC slot had inconsistent type/pointer data; the pointer was leaked to avoid \
             undefined behavior. This should never happen — report it."
        );
    }

    if latches
        .param_backlog
        .observe(rt_status.check_and_clear_flag(RT_STATUS_PARAM_QUEUE_BACKLOG))
    {
        log::warn!(
            "[E3100 | PARAM_CHANNEL_FULL] Command queue backlog: the scalar parameter drain \
             budget (16/callback) was exhausted — a producer (CLI/UI/automation) filled the \
             queue faster than it can drain; the remainder is processed by the next callback, \
             preserving the audio deadline."
        );
    }

    if latches
        .structural_deferred
        .observe(rt_status.check_and_clear_flag(RT_STATUS_STRUCTURAL_DEFERRED))
    {
        log::info!(
            "Structural command deferred to the next callback (structural budget 1/callback) — FIFO order preserved."
        );
    }

    if latches
        .structural_superseded
        .observe(rt_status.check_and_clear_flag(RT_STATUS_STRUCTURAL_SUPERSEDED))
    {
        log::info!(
            "Deferred structural command superseded by a newer same-kind command; obsolete resources discarded off-RT (coalescing)."
        );
    }

    if latches
        .slimmable_slice_failed
        .observe(rt_status.check_and_clear_flag(RT_STATUS_SLIMMABLE_SLICE_FAILED))
    {
        log::error!(
            "[E3104 | SLIMMABLE_SLICE_FAILED] WaveNet slimmable slice_channels rebuild failed — \
             model may run in reduced state. Check that the active channel count is within the \
             model's supported slimmable range."
        );
    }

    if latches
        .slimmable_reset_failed
        .observe(rt_status.check_and_clear_flag(RT_STATUS_SLIMMABLE_RESET_FAILED))
    {
        log::error!(
            "[E3105 | SLIMMABLE_RESET_FAILED] ContainerModel submodel reset failed — \
             model may run in previous state. This is a transient RT signal; if persistent, \
             check model graph integrity."
        );
    }
}

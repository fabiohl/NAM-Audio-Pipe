// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Consolidated startup banner and end-of-session summary (F-UX-13).
//!
//! At `info` level a healthy session should read like a dashboard, not like a
//! firehose: the first thing support asks for (model, cab-sim, rate, quantum,
//! latency, real-time scheduling, mlockall, recording) and the user understands
//! at a glance. This module renders that consolidated block **once**, after the
//! first RT confirmations arrive, plus an analogous session summary at the end
//! (xruns, bridge dropped frames, recording).
//!
//! Rendering is a pair of **pure functions** taking the color decision
//! explicitly ([`render_banner`], [`render_session_summary`]) so every `✓`/`✗`
//! combination is unit-testable with and without ANSI sequences, without
//! mutating the process-global color switch.

use std::fmt::Write as _;
use std::path::PathBuf;

/// Cab-sim state shown in the banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CabStatus {
    /// An IR was loaded and is active.
    Loaded,
    /// No cab-sim IR is loaded (`--cab` absent).
    #[default]
    Absent,
    /// A `--cab` was given but `--cab-optional` let the host continue without it.
    /// Surfaced here because the one-line `warn` is easy to miss.
    SkippedOptional,
}

/// Static banner facts resolved by `main` before the host starts.
///
/// These do not change during a session, so the control loop only needs to add
/// the runtime facts (rate/quantum/latency/SCHED/mlockall) read from `rt_status`.
#[derive(Debug, Clone, Default)]
pub struct BannerRequest {
    /// Loaded model architecture (`None` = bypass, no model).
    pub model_arch: Option<String>,
    /// Cab-sim state.
    pub cab: CabStatus,
    /// Capture directory when `--record` is active.
    pub recording_dir: Option<PathBuf>,
}

/// Runtime facts for [`render_banner`], captured once the RT path is confirmed.
#[derive(Debug, Clone, Default)]
pub struct BannerInfo {
    /// Static facts resolved at startup.
    pub request: BannerRequest,
    /// Negotiated sample rate in Hz (`0` renders the 48 kHz nominal).
    pub sample_rate: u32,
    /// Effective PipeWire quantum in samples (`0` = unknown).
    pub quantum: u32,
    /// DSP algorithmic latency in samples (cab-sim partition + resampler).
    pub dsp_latency_samples: u32,
    /// Whether `SCHED_FIFO` was confirmed on the RT data thread.
    pub rt_fifo: bool,
    /// Real-time scheduler policy name (`"FIFO"`, `"RR"`, `"OTHER"`).
    pub rt_policy: &'static str,
    /// Confirmed RT priority (informational).
    pub rt_priority: i32,
    /// Whether `mlockall` succeeded.
    pub mlockall_ok: bool,
}

/// Aggregated end-of-session counters for [`render_session_summary`].
#[derive(Debug, Clone, Default)]
pub struct SessionSummary {
    /// Cumulative xrun count reported by the engine.
    pub xruns: u32,
    /// Clock-drift frames dropped by the DSP bridge.
    pub bridge_dropped_frames: u32,
    /// Capture directory when `--record` was active.
    pub recording_dir: Option<PathBuf>,
}

/// Wraps `text` in the SGR `code` when `color` is set.
fn paint(text: &str, code: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Green `✓` when `ok`, red `✗` otherwise.
fn mark(ok: bool, color: bool) -> String {
    if ok {
        paint("✓", "32", color)
    } else {
        paint("✗", "31", color)
    }
}

/// Renders the consolidated startup banner.
///
/// `color` is the resolved color decision (`true` emits ANSI sequences); it is
/// an explicit parameter so the formatter is a pure function.
pub fn render_banner(info: &BannerInfo, color: bool) -> String {
    let rate = if info.sample_rate == 0 {
        48_000
    } else {
        info.sample_rate
    };
    let quantum_ms = info.quantum as f64 * 1000.0 / rate as f64;
    let dsp_ms = info.dsp_latency_samples as f64 * 1000.0 / rate as f64;
    let total_ms = quantum_ms + dsp_ms;

    let model = match &info.request.model_arch {
        Some(arch) => format!("{} {arch}", mark(true, color)),
        None => format!("{} bypass (no model)", paint("○", "33", color)),
    };
    let cab = match info.request.cab {
        CabStatus::Loaded => format!("{} loaded", mark(true, color)),
        CabStatus::Absent => format!("{} none (optional)", paint("○", "33", color)),
        CabStatus::SkippedOptional => {
            format!("{} skipped (--cab-optional)", paint("⚠", "33", color))
        }
    };
    let recording = match &info.request.recording_dir {
        Some(dir) => format!("{} live → {}", mark(true, color), dir.display()),
        None => format!("{} disabled", mark(false, color)),
    };
    let rt = if info.rt_fifo {
        format!(
            "{} SCHED_FIFO (prio {})",
            mark(true, color),
            info.rt_priority
        )
    } else {
        format!(
            "{} {} (no real-time scheduling)",
            mark(false, color),
            info.rt_policy
        )
    };
    let mlock = format!("{} mlockall", mark(info.mlockall_ok, color));

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}",
        paint("🎛️  NAM-Audio-Pipe — session status", "1", color)
    );
    let _ = writeln!(out, "  Model     : {model}");
    let _ = writeln!(out, "  Cab-sim   : {cab}");
    let _ = writeln!(
        out,
        "  Rate      : {rate} Hz | Quantum: {} samples ({quantum_ms:.2} ms)",
        info.quantum
    );
    let _ = writeln!(
        out,
        "  Latency   : {} samples DSP ({dsp_ms:.2} ms) | total ≈ {total_ms:.2} ms",
        info.dsp_latency_samples
    );
    let _ = writeln!(out, "  RT        : {rt} | {mlock}");
    let _ = writeln!(out, "  Recording : {recording}");

    if !info.rt_fifo {
        let _ = writeln!(
            out,
            "  {} Real-time scheduling unavailable — check `ulimit -r` and rtkit \
             permissions for the audio user.",
            paint("💡", "33", color)
        );
    }
    if !info.mlockall_ok {
        let _ = writeln!(
            out,
            "  {} mlockall failed — increase the `memlock` limit (ulimit -l) to \
             avoid page-fault jitter.",
            paint("💡", "33", color)
        );
    }
    out
}

/// Renders the end-of-session summary (analogous layout to the banner).
pub fn render_session_summary(summary: &SessionSummary, color: bool) -> String {
    let xruns_ok = summary.xruns == 0;
    let drops_ok = summary.bridge_dropped_frames == 0;
    let recording = match &summary.recording_dir {
        Some(dir) => format!("{} {}", mark(true, color), dir.display()),
        None => format!("{} disabled", mark(false, color)),
    };

    let mut out = String::new();
    let _ = writeln!(out, "{}", paint("📊 Session summary", "1", color));
    let _ = writeln!(
        out,
        "  Xruns      : {} {}",
        summary.xruns,
        mark(xruns_ok, color)
    );
    let _ = writeln!(
        out,
        "  Bridge drops: {} frames {}",
        summary.bridge_dropped_frames,
        mark(drops_ok, color)
    );
    let _ = writeln!(out, "  Recording  : {recording}");
    out
}

#[cfg(test)]
#[path = "banner_test.rs"]
mod banner_test;

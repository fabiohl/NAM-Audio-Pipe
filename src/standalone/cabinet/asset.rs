// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Cabinet asset detection, 1-in/1-out neural model validation, and path labeling.

use anyhow::{Context, bail};
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::loader::nam_json::{NamModelData, parse_nam_json};
use neural_amp_modeler_rs::loader::namb::parse_namb_typed;
use neural_amp_modeler_rs::models::{NamModel, StaticModel};
use std::path::Path;

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

pub(super) fn prewarm_cab_model(model: &mut StaticModel) {
    model.prewarm_reset();
    for _ in 0..64 {
        let rem = model.prewarm_step(MAX_RESAMP_BUF);
        if rem == 0 || model.prewarm_complete() {
            break;
        }
    }
}

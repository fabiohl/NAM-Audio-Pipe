// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Input calibration and output mode contracts for NAM-Audio-Pipe.
//!
//! Aligns CLI model level handling with upstream `NeuralAmpModelerPlugin`:
//! - New invocations default to `calibrate_input = false`,
//!   `input_calibration_level_dbu = 12.0 dBu`, and `output_mode = Normalized`.
//! - Multipliers are computed directly via `f64::powf` (no `GainLUT` ±30 dB
//!   truncation) across `[-60.0, +60.0]` dBu.
//! - Startup validation rejects explicit calibration/mode requests when the
//!   model lacks the required metadata and never fabricates missing dBu values.

use std::fmt;

/// Minimum supported ADC input calibration level in dBu.
pub const INPUT_CALIBRATION_LEVEL_MIN_DBU: f32 = -60.0;
/// Maximum supported ADC input calibration level in dBu.
pub const INPUT_CALIBRATION_LEVEL_MAX_DBU: f32 = 60.0;
/// Upstream default ADC input calibration level in dBu (0 dBFS reference).
pub const INPUT_CALIBRATION_LEVEL_DEFAULT_DBU: f32 = 12.0;
/// Upstream LUFS normalization target in dB.
pub const TARGET_LOUDNESS_DB: f32 = -18.0;

/// Output gain mode matching upstream `NeuralAmpModelerPlugin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputMode {
    /// Unity output multiplier (0 dB model output scaling).
    Raw,
    /// Normalize output to `TARGET_LOUDNESS_DB` (`-18 dB`) using model `loudness`.
    #[default]
    Normalized,
    /// Compensate output against `input_calibration_level_dbu` using model `output_level_dbu`.
    Calibrated,
}

impl OutputMode {
    /// Stable CLI/log identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Normalized => "normalized",
            Self::Calibrated => "calibrated",
        }
    }
}

impl fmt::Display for OutputMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// User-selected calibration configuration passed to startup model setup.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalibrationConfig {
    /// Whether ADC-to-model input level compensation is active.
    pub calibrate_input: bool,
    /// Host interface 0 dBFS reference level in dBu (`[-60.0, 60.0]`).
    pub input_calibration_level_dbu: f32,
    /// Model output level mode (`Raw`, `Normalized`, `Calibrated`).
    pub output_mode: OutputMode,
}

impl CalibrationConfig {
    /// Default configuration for new invocations (`calibrate_input = false`).
    pub const fn new_project_default() -> Self {
        Self {
            calibrate_input: false,
            input_calibration_level_dbu: INPUT_CALIBRATION_LEVEL_DEFAULT_DBU,
            output_mode: OutputMode::Normalized,
        }
    }
}

impl Default for CalibrationConfig {
    fn default() -> Self {
        Self::new_project_default()
    }
}

/// Optional calibration metadata extracted from a loaded `.nam` / `.namb` model.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ModelCalibrationMeta {
    /// Whether a neural model is active (`false` during true bypass).
    pub has_model: bool,
    /// Integrated model loudness in LUFS.
    pub loudness: Option<f32>,
    /// Capture interface 0 dBFS input level in dBu.
    pub input_level_dbu: Option<f32>,
    /// Capture interface 0 dBFS output level in dBu.
    pub output_level_dbu: Option<f32>,
}

/// Resolved linear multipliers and optional startup warning for model loading.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedCalibration {
    /// Pre-nonlinearity input gain multiplier applied after noise gate.
    pub input_mult: f32,
    /// Post-model output gain multiplier applied before user output gain.
    pub output_mult: f32,
    /// Non-fatal startup warning (emitted for implicit `normalized` lacking `loudness`).
    pub warning: Option<String>,
}

/// Parses `--calibrate-input <on|off>`.
pub fn parse_calibrate_input(val: &str) -> Result<bool, String> {
    match val.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" => Ok(true),
        "off" | "false" | "0" => Ok(false),
        _ => Err(format!(
            "Invalid --calibrate-input: '{val}'. Expected 'on' or 'off'."
        )),
    }
}

/// Parses and range-validates `--input-calibration-level <DBU>` (`[-60.0, 60.0]`).
pub fn parse_input_calibration_level(val: &str) -> Result<f32, String> {
    let trimmed = val
        .trim()
        .trim_end_matches("dBu")
        .trim_end_matches("dbu")
        .trim();
    let level = trimmed.parse::<f32>().map_err(|_| {
        format!("Invalid --input-calibration-level: '{val}'. Must be a number in dBu.")
    })?;
    if !level.is_finite()
        || !(INPUT_CALIBRATION_LEVEL_MIN_DBU..=INPUT_CALIBRATION_LEVEL_MAX_DBU).contains(&level)
    {
        return Err(format!(
            "Input calibration level ({level} dBu) out of range [{INPUT_CALIBRATION_LEVEL_MIN_DBU:.1}, {INPUT_CALIBRATION_LEVEL_MAX_DBU:.1}]."
        ));
    }
    Ok(level)
}

/// Parses `--output-mode <raw|normalized|calibrated>`.
pub fn parse_output_mode(val: &str) -> Result<OutputMode, String> {
    match val.trim().to_ascii_lowercase().as_str() {
        "raw" => Ok(OutputMode::Raw),
        "normalized" | "norm" => Ok(OutputMode::Normalized),
        "calibrated" | "cal" => Ok(OutputMode::Calibrated),
        _ => Err(format!(
            "Invalid --output-mode: '{val}'. Use raw, normalized, or calibrated."
        )),
    }
}

#[inline]
fn db_to_linear_f64(db: f64) -> f32 {
    10.0_f64.powf(db / 20.0) as f32
}

#[inline]
fn sanitize_level_dbu(level: f32) -> f32 {
    if level.is_finite() {
        level.clamp(
            INPUT_CALIBRATION_LEVEL_MIN_DBU,
            INPUT_CALIBRATION_LEVEL_MAX_DBU,
        )
    } else {
        INPUT_CALIBRATION_LEVEL_DEFAULT_DBU
    }
}

/// Computes `(input_mult, output_mult)` directly without failing on missing metadata.
///
/// Missing metadata never synthesizes fake dBu values and yields unity (`1.0`).
pub fn compute_calibration_multipliers(
    meta: ModelCalibrationMeta,
    config: CalibrationConfig,
) -> (f32, f32) {
    if !meta.has_model {
        return (1.0, 1.0);
    }

    let cal_dbu = f64::from(sanitize_level_dbu(config.input_calibration_level_dbu));

    let in_mult = if config.calibrate_input {
        match meta.input_level_dbu.filter(|v| v.is_finite()) {
            Some(in_dbu) => db_to_linear_f64(cal_dbu - f64::from(in_dbu)),
            None => 1.0,
        }
    } else {
        1.0
    };

    let out_mult = match config.output_mode {
        OutputMode::Raw => 1.0,
        OutputMode::Normalized => match meta.loudness.filter(|v| v.is_finite()) {
            Some(loudness) => db_to_linear_f64(f64::from(TARGET_LOUDNESS_DB) - f64::from(loudness)),
            None => 1.0,
        },
        OutputMode::Calibrated => match meta.output_level_dbu.filter(|v| v.is_finite()) {
            Some(out_dbu) => db_to_linear_f64(f64::from(out_dbu) - cal_dbu),
            None => 1.0,
        },
    };

    (in_mult, out_mult)
}

/// Validates model metadata against `config` and resolves startup multipliers.
///
/// Explicit calibration/output mode requests missing required model metadata return `Err`,
/// whereas implicit default `Normalized` missing `loudness` returns a non-fatal warning
/// and unity `0 dB` (`1.0x`) output gain.
pub fn resolve_model_calibration(
    meta: ModelCalibrationMeta,
    config: CalibrationConfig,
    output_mode_explicit: bool,
) -> Result<ResolvedCalibration, String> {
    if !meta.has_model {
        return Ok(ResolvedCalibration {
            input_mult: 1.0,
            output_mult: 1.0,
            warning: None,
        });
    }

    if !config.input_calibration_level_dbu.is_finite()
        || !(INPUT_CALIBRATION_LEVEL_MIN_DBU..=INPUT_CALIBRATION_LEVEL_MAX_DBU)
            .contains(&config.input_calibration_level_dbu)
    {
        return Err(format!(
            "Input calibration level ({} dBu) out of range [{INPUT_CALIBRATION_LEVEL_MIN_DBU:.1}, {INPUT_CALIBRATION_LEVEL_MAX_DBU:.1}].",
            config.input_calibration_level_dbu
        ));
    }

    if config.calibrate_input && meta.input_level_dbu.filter(|v| v.is_finite()).is_none() {
        return Err(
            "Cannot enable --calibrate-input on: loaded model metadata is missing 'input_level_dbu'."
                .to_string(),
        );
    }

    let mut warning = None;
    match config.output_mode {
        OutputMode::Raw => {}
        OutputMode::Calibrated => {
            if meta.output_level_dbu.filter(|v| v.is_finite()).is_none() {
                return Err(
                    "Cannot use --output-mode calibrated: loaded model metadata is missing 'output_level_dbu'."
                        .to_string(),
                );
            }
        }
        OutputMode::Normalized => {
            if meta.loudness.filter(|v| v.is_finite()).is_none() {
                if output_mode_explicit {
                    return Err(
                        "Cannot use --output-mode normalized: loaded model metadata is missing 'loudness'."
                            .to_string(),
                    );
                }
                warning = Some(
                    "Model metadata lacks 'loudness'; falling back to 0 dB (1.0x) output gain in implicit normalized mode."
                        .to_string(),
                );
            }
        }
    }

    let (input_mult, output_mult) = compute_calibration_multipliers(meta, config);
    Ok(ResolvedCalibration {
        input_mult,
        output_mult,
        warning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lstm_meta() -> ModelCalibrationMeta {
        ModelCalibrationMeta {
            has_model: true,
            loudness: Some(-37.840_687),
            input_level_dbu: Some(18.3),
            output_level_dbu: Some(12.3),
        }
    }

    #[test]
    fn equations_match_upstream_plugin_without_lut_saturation() {
        let meta = lstm_meta();
        let cfg = CalibrationConfig {
            calibrate_input: true,
            input_calibration_level_dbu: -60.0,
            output_mode: OutputMode::Calibrated,
        };
        let (in_m, out_m) = compute_calibration_multipliers(meta, cfg);
        let exp_in = 10.0_f64.powf((-60.0 - 18.3) / 20.0) as f32;
        let exp_out = 10.0_f64.powf((12.3 - (-60.0)) / 20.0) as f32;
        assert!((in_m - exp_in).abs() <= 1e-9);
        assert!((out_m - exp_out).abs() <= 1e-3);

        let raw = compute_calibration_multipliers(meta, CalibrationConfig::new_project_default());
        assert_eq!(raw.0, 1.0);
        let exp_norm = 10.0_f64.powf((-18.0 - (-37.840_687_f64)) / 20.0) as f32;
        assert!((raw.1 - exp_norm).abs() <= 1e-5);
    }

    #[test]
    fn validation_enforces_explicit_vs_implicit_metadata_policy() {
        let no_meta = ModelCalibrationMeta {
            has_model: true,
            loudness: None,
            input_level_dbu: None,
            output_level_dbu: None,
        };

        let implicit =
            resolve_model_calibration(no_meta, CalibrationConfig::new_project_default(), false)
                .expect("implicit normalized without loudness warns and falls back");
        assert_eq!((implicit.input_mult, implicit.output_mult), (1.0, 1.0));
        assert!(implicit.warning.is_some());

        assert!(
            resolve_model_calibration(no_meta, CalibrationConfig::new_project_default(), true)
                .is_err()
        );
        assert!(
            resolve_model_calibration(
                no_meta,
                CalibrationConfig {
                    calibrate_input: true,
                    ..CalibrationConfig::new_project_default()
                },
                false,
            )
            .is_err()
        );
        assert!(
            resolve_model_calibration(
                no_meta,
                CalibrationConfig {
                    output_mode: OutputMode::Calibrated,
                    ..CalibrationConfig::new_project_default()
                },
                true,
            )
            .is_err()
        );
    }

    #[test]
    fn cli_parsers_accept_valid_and_reject_invalid() {
        assert_eq!(parse_calibrate_input("on"), Ok(true));
        assert_eq!(parse_calibrate_input("OFF"), Ok(false));
        assert!(parse_calibrate_input("maybe").is_err());

        assert_eq!(parse_input_calibration_level("-60"), Ok(-60.0));
        assert_eq!(parse_input_calibration_level("12.5 dBu"), Ok(12.5));
        assert!(parse_input_calibration_level("-60.1").is_err());
        assert!(parse_input_calibration_level("NaN").is_err());

        assert_eq!(parse_output_mode("raw"), Ok(OutputMode::Raw));
        assert_eq!(parse_output_mode("Norm"), Ok(OutputMode::Normalized));
        assert_eq!(parse_output_mode("calibrated"), Ok(OutputMode::Calibrated));
        assert!(parse_output_mode("auto").is_err());
    }
}

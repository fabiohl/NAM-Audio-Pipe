// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! CLI calibration configuration layered on the shared engine math.
//!
//! The dBu bounds, [`OutputMode`] semantics, [`ModelCalibrationMeta`] snapshot
//! and [`compute_calibration_gains`] equations live in
//! `neural_amp_modeler_rs::dsp::calibration`. This module only adds the
//! standalone CLI adapter ([`CalibrationConfig`]), the `--calibrate-input` /
//! `--input-calibration-level` / `--output-mode` parsers and the startup
//! validation policy (`resolve_model_calibration`), while keeping the
//! historical [`compute_calibration_multipliers`] tuple API used across setup.
//!
//! Semantics preserved from the previous local copy:
//! - New invocations default to `calibrate_input = false`,
//!   `input_calibration_level_dbu = 12.0 dBu`, and `output_mode = Normalized`.
//! - Multipliers are computed in `f64` (no `GainLUT` ±30 dB truncation) across
//!   `[-60.0, +60.0]` dBu.
//! - Startup validation rejects explicit calibration/mode requests when the
//!   model lacks the required metadata and never fabricates missing dBu values.

pub use neural_amp_modeler_rs::dsp::calibration::{
    CalibrationGains, DEFAULT_CALIBRATION_DBU, MAX_CALIBRATION_DBU, MIN_CALIBRATION_DBU,
    ModelCalibrationMeta, OutputMode, TARGET_LOUDNESS_DB, compute_calibration_gains,
    sanitize_calibration_level_dbu,
};

/// Upstream default ADC input calibration level in dBu (0 dBFS reference).
pub use neural_amp_modeler_rs::dsp::calibration::DEFAULT_CALIBRATION_DBU as INPUT_CALIBRATION_LEVEL_DEFAULT_DBU;
/// Maximum supported ADC input calibration level in dBu.
pub use neural_amp_modeler_rs::dsp::calibration::MAX_CALIBRATION_DBU as INPUT_CALIBRATION_LEVEL_MAX_DBU;
/// Minimum supported ADC input calibration level in dBu.
pub use neural_amp_modeler_rs::dsp::calibration::MIN_CALIBRATION_DBU as INPUT_CALIBRATION_LEVEL_MIN_DBU;

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

/// Drops non-finite metadata before feeding the shared calibration equations.
#[inline]
fn sanitize_meta(meta: ModelCalibrationMeta) -> ModelCalibrationMeta {
    ModelCalibrationMeta {
        has_model: meta.has_model,
        input_level_dbu: meta.input_level_dbu.filter(|v| v.is_finite()),
        output_level_dbu: meta.output_level_dbu.filter(|v| v.is_finite()),
        loudness: meta.loudness.filter(|v| v.is_finite()),
    }
}

/// Computes `(input_mult, output_mult)` directly without failing on missing metadata.
///
/// Missing metadata never synthesizes fake dBu values and yields unity (`1.0`).
/// Thin tuple adapter over [`compute_calibration_gains`].
pub fn compute_calibration_multipliers(
    meta: ModelCalibrationMeta,
    config: CalibrationConfig,
) -> (f32, f32) {
    let gains = compute_calibration_gains(
        sanitize_meta(meta),
        config.calibrate_input,
        config.input_calibration_level_dbu,
        config.output_mode,
    );
    (gains.input_mult, gains.output_mult)
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
        // The shared engine computes `output_level_dbu - cal_dbu` in f32 and then
        // converts to f64; on a ~4121x calibrated gain that is ~0.35 ppm, well
        // below any audible threshold but above a 1e-3 absolute tolerance.
        assert!((out_m - exp_out).abs() <= 2e-3);

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

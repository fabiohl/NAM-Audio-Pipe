// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Helper functions for Command Line Interface (CLI).
//!
//! Handles the display of help and parsing of arguments
//! provided by the user via terminal.

use neural_amp_modeler_rs::math::activations::ActivationPrecision;
use neural_amp_modeler_rs::math::constants::{GAIN_MAX_DB, GAIN_MIN_DB};

pub use super::calibration::{
    CalibrationConfig, INPUT_CALIBRATION_LEVEL_DEFAULT_DBU, OutputMode, parse_calibrate_input,
    parse_input_calibration_level, parse_output_mode,
};
pub use super::cli_domain::{
    BUFFER_SIZE_AUTO, BUFFER_SIZE_MAX, BUFFER_SIZE_MIN, BufferSizeError, CABSIM_PARTITION_ALLOWED,
    CABSIM_PARTITION_DEFAULT, CabSimPartitionError, GateConfig, GateMode, IrLevelPolicy,
    OutputLimitPolicy, exit_with_error, expand_user_path, parse_ir_level_policy,
    parse_output_limit, print_help, resolve_record_dir, validate_buffer_size,
    validate_cabsim_partition, validate_readable_file, write_stdout,
};
pub(crate) use super::cli_domain::{parse_gate_mode, parse_gate_selector};
use crate::standalone::colors::ColorFlag;
use lexopt::prelude::*;
use neural_amp_modeler_rs::dsp::adaptive::SlimOverride;
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;

use std::path::PathBuf;
use std::time::Duration;

/// Validates the presence/readability of every explicitly provided input file.
///
/// This is an *argument* validation (legitimate `exit_with_error` territory):
/// it runs right after parsing, before any PipeWire connection or allocation,
/// so a typo in `--model`/`--cab` fails fast with a clear path.
///
/// `--cab` is the one exception: when `--cab-optional` is set, a missing or
/// unreadable IR is left for the loader, which warns and continues without
/// cab-sim (the same contract as a load failure). Only the default fatal mode
/// rejects an unusable `--cab` here.
pub fn validate_cli_inputs(args: &CliArgs) -> Result<(), String> {
    if let Some(model) = &args.model_path {
        validate_readable_file("--model", model)?;
    }
    if let Some(cab) = &args.cab_path
        && !args.cab_optional
    {
        validate_readable_file("--cab", cab)?;
    }
    Ok(())
}

/// Parsed command line arguments.
#[derive(Debug, Clone)]
pub struct CliArgs {
    /// Path to the neural model file.
    pub model_path: Option<PathBuf>,
    /// Path to the cab-sim impulse response (.wav) file.
    pub cab_path: Option<PathBuf>,
    /// When set (`--cab-optional`), a failed cab-sim IR load is non-fatal:
    /// the host warns and continues without cab-sim. Without it, an explicitly
    /// requested IR that cannot be loaded aborts the process.
    pub cab_optional: bool,
    /// Input gain in dB.
    pub input_gain: f32,
    /// Output gain in dB.
    pub output_gain: f32,
    /// Desired buffer size.
    pub buffer_size: u32,
    /// Cab-sim UPOLS partition policy in samples (from `--cabsim-partition`):
    /// the exact algorithmic latency of the cab-sim stage, decoupled from the
    /// host quantum. Defaults to [`CABSIM_PARTITION_DEFAULT`].
    pub cabsim_partition: u32,
    /// Cabinet IR level policy (`--ir-level-policy`, defaults to `PeakNormalized`).
    pub ir_level_policy: IrLevelPolicy,
    /// Enable ADC-to-model input calibration (`--calibrate-input <on|off>`, default `off`).
    pub calibrate_input: bool,
    /// ADC 0 dBFS calibration level in dBu (`--input-calibration-level <DBU>`, default `12.0`).
    pub input_calibration_level_dbu: f32,
    /// Model output gain mode (`--output-mode <raw|normalized|calibrated>`, default `normalized`).
    pub output_mode: OutputMode,
    /// Tracks whether `--output-mode` was explicitly specified on the command line.
    pub output_mode_explicit: bool,
    /// Optional hardware playback output limit (`--output-limit`, defaults to `Off`).
    pub output_limit: OutputLimitPolicy,
    /// Immediate diagnostic flag.
    pub diagnose: bool,
    /// Immediate diagnostic flag with full (unredacted) paths.
    pub diagnose_full: bool,
    /// Manual slim override quality level.
    pub slim_override: SlimOverride,
    /// Oversampling factor for the neural stage (off, 2x, 4x).
    pub oversample: OversampleFactor,
    /// Activation precision mode (`Standard`, the universal default, or `Fast`).
    /// `None` means the user did not pass `--activation`; the library-level
    /// default (`Standard`) applies.
    pub activation: Option<ActivationPrecision>,
    /// Explicit CPU index to pin the RT audio thread (overrides auto-selection).
    pub cpu: Option<usize>,
    /// Enable WAV recording of the raw PipeWire capture stream.
    pub record: bool,
    /// Output directory for WAV captures (`--record-dir`). `None` resolves to
    /// the platform music directory at startup (`$XDG_MUSIC_DIR/NAM-Audio-Pipe`,
    /// else `$HOME/Music/NAM-Audio-Pipe`, else the current directory).
    pub record_dir: Option<PathBuf>,
    /// Disable the bounded backend reconnect cycle (`--fail-fast`): the first
    /// PipeWire stream failure triggers fail-fast teardown instead of
    /// a bounded reconnection attempt.
    pub fail_fast: bool,
    /// Optional per-episode reconnect backoff budget (`--reconnect-timeout <s>`).
    /// `None` uses the production schedule (6 attempts, 250 ms → 4 s,
    /// cumulative 11.75 s). Ignored under `--fail-fast`.
    pub reconnect_timeout: Option<Duration>,
    /// Noise gate configuration: `Off`, or `Threshold { open, close }` in dBFS
    /// (defaults to [`GateConfig::default_on`] = open at `-70.0 dB`).
    pub gate: GateConfig,
    /// Noise gate topology from `--gate-mode <silence|musical>` (defaults to [`GateMode::Silence`]).
    pub gate_mode: GateMode,
    /// Terminal color policy from `--color` (defaults to [`ColorFlag::Auto`]).
    pub color: ColorFlag,
    /// Wait for Enter before exiting after a terminal-only, exit-immediate
    /// invocation (`--diagnose`) when stdout is a terminal. Ignored for the
    /// long-running audio host.
    pub pause: bool,
}

impl CliArgs {
    /// Returns the grouped calibration configuration derived from parsed CLI options.
    pub fn calibration_config(&self) -> CalibrationConfig {
        CalibrationConfig {
            calibrate_input: self.calibrate_input,
            input_calibration_level_dbu: self.input_calibration_level_dbu,
            output_mode: self.output_mode,
        }
    }
}

/// Parses command-line arguments and validates any explicitly provided input files.
///
/// `--diagnose`/`--diagnose-full` skip the file validation (they must work
/// without the referenced models/IRs).
pub fn parse_args() -> CliArgs {
    let args = parse_args_from(lexopt::Parser::from_env());
    if !args.diagnose
        && !args.diagnose_full
        && let Err(e) = validate_cli_inputs(&args)
    {
        exit_with_error(e);
    }
    args
}

/// Helper parsing function that accepts any lexopt::Parser to facilitate testing.
pub fn parse_args_from(mut parser: lexopt::Parser) -> CliArgs {
    let mut model_path = None;
    let mut cab_path = None;
    let mut cab_optional = false;
    let mut input_gain = 0.0;
    let mut output_gain = 0.0;
    let mut buffer_size = 256;
    let mut cabsim_partition = CABSIM_PARTITION_DEFAULT;
    let mut ir_level_policy = IrLevelPolicy::default();
    let mut calibrate_input = false;
    let mut input_calibration_level_dbu = INPUT_CALIBRATION_LEVEL_DEFAULT_DBU;
    let mut output_mode = OutputMode::default();
    let mut output_mode_explicit = false;
    let mut output_limit = OutputLimitPolicy::default();
    let mut diagnose = false;
    let mut diagnose_full = false;
    let mut slim_override = SlimOverride::Auto;
    let mut oversample = OversampleFactor::Off;
    let mut activation = None;
    let mut cpu = None;
    let mut record = false;
    let mut record_dir = None;
    let mut fail_fast = false;
    let mut reconnect_timeout = None;
    let mut gate = GateConfig::default_on();
    let mut gate_mode = GateMode::default();
    let mut color = ColorFlag::Auto;
    let mut pause = false;
    let mut has_args = false;

    while let Some(arg) = parser.next().unwrap_or_else(|e| exit_with_error(e)) {
        has_args = true;
        match arg {
            Short('h') | Long("help") => {
                print_help();
                std::process::exit(0);
            }
            Long("diagnose") => {
                diagnose = true;
            }
            Long("diagnose-full") => {
                diagnose_full = true;
            }
            Long("record") => {
                record = true;
            }
            Long("pause") => {
                pause = true;
            }
            Long("record-dir") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let p_str = val.into_string().unwrap_or_else(|_| {
                    exit_with_error("Invalid recording directory path (UTF-8).")
                });
                let home = std::env::var_os("HOME");
                record_dir = Some(
                    expand_user_path(&p_str, home.as_deref())
                        .unwrap_or_else(|e| exit_with_error(e)),
                );
            }
            Long("cab-optional") => {
                cab_optional = true;
            }
            Long("fail-fast") => {
                fail_fast = true;
            }
            Long("reconnect-timeout") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let val_str = val.into_string().unwrap_or_else(|_| {
                    exit_with_error("Invalid reconnect timeout value (expected seconds).")
                });
                let secs = val_str.parse::<u64>().unwrap_or_else(|_| {
                    exit_with_error(format!(
                        "Invalid reconnect timeout: '{}'. Must be a non-negative integer \
                         number of seconds.",
                        val_str
                    ))
                });
                reconnect_timeout = Some(Duration::from_secs(secs));
            }
            Long("gate") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let val_str = val
                    .into_string()
                    .unwrap_or_else(|_| exit_with_error("Invalid gate mode value."));
                gate = parse_gate_mode(&val_str).unwrap_or_else(|e| exit_with_error(e));
            }
            Long("gate-mode") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let val_str = val
                    .into_string()
                    .unwrap_or_else(|_| exit_with_error("Invalid --gate-mode value."));
                gate_mode = parse_gate_selector(&val_str).unwrap_or_else(|e| exit_with_error(e));
            }
            Long("color") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let val_str = val
                    .into_string()
                    .unwrap_or_else(|_| exit_with_error("Invalid color mode value."));
                color = match val_str.to_lowercase().as_str() {
                    "auto" => ColorFlag::Auto,
                    "always" => ColorFlag::Always,
                    "never" => ColorFlag::Never,
                    other => exit_with_error(format!(
                        "Invalid color mode: '{}'. Expected 'auto', 'always', or 'never'.",
                        other
                    )),
                };
                // Applied immediately: any output produced after this point
                // (including `--help`) uses the explicit policy.
                crate::standalone::colors::apply_flag(color);
            }
            Long("cpu") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let val_str = val
                    .into_string()
                    .unwrap_or_else(|_| exit_with_error("Invalid CPU index value."));
                let parsed_cpu = val_str.parse::<usize>().unwrap_or_else(|_| {
                    exit_with_error(format!(
                        "Invalid CPU index: '{}'. Must be a non-negative integer.",
                        val_str
                    ))
                });
                cpu = Some(parsed_cpu);
            }
            Long("slim") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let val_str = val
                    .into_string()
                    .unwrap_or_else(|_| exit_with_error("Invalid slim override value."));
                slim_override = match val_str.to_lowercase().as_str() {
                    "auto" => SlimOverride::Auto,
                    "full" => SlimOverride::ForceFull,
                    "lite" => SlimOverride::ForceLite,
                    other => exit_with_error(format!(
                        "Invalid slim override: '{}'. Expected 'auto', 'full', or 'lite'.",
                        other
                    )),
                };
            }
            Long("oversample") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let val_str = val
                    .into_string()
                    .unwrap_or_else(|_| exit_with_error("Invalid oversample value."));
                oversample = match val_str.to_lowercase().as_str() {
                    "off" | "0" => OversampleFactor::Off,
                    "2x" | "2" => OversampleFactor::X2,
                    "4x" | "4" => OversampleFactor::X4,
                    other => exit_with_error(format!(
                        "Invalid oversample factor: '{}'. Expected 'off', '2x', or '4x'.",
                        other
                    )),
                };
            }
            Long("activation") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let val_str = val
                    .into_string()
                    .unwrap_or_else(|_| exit_with_error("Invalid activation precision value."));
                activation = Some(match val_str.to_lowercase().as_str() {
                    "standard" | "std" => ActivationPrecision::Standard,
                    "fast" => ActivationPrecision::Fast,
                    other => exit_with_error(format!(
                        "Invalid activation precision: '{}'. Expected 'standard' or 'fast'.",
                        other
                    )),
                });
            }
            Short('m') | Long("model") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let p_str = val
                    .into_string()
                    .unwrap_or_else(|_| exit_with_error("Invalid model path (UTF-8)."));
                let home = std::env::var_os("HOME");
                model_path = Some(
                    expand_user_path(&p_str, home.as_deref())
                        .unwrap_or_else(|e| exit_with_error(e)),
                );
            }
            Short('c') | Long("cab") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let p_str = val
                    .into_string()
                    .unwrap_or_else(|_| exit_with_error("Invalid cab IR path (UTF-8)."));
                let home = std::env::var_os("HOME");
                cab_path = Some(
                    expand_user_path(&p_str, home.as_deref())
                        .unwrap_or_else(|e| exit_with_error(e)),
                );
            }
            Short('i') | Long("input-gain") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let val_str = val
                    .into_string()
                    .unwrap_or_else(|_| exit_with_error("Invalid input gain value."));
                input_gain = val_str.parse::<f32>().unwrap_or_else(|_| {
                    exit_with_error(format!(
                        "Invalid input gain: '{}'. Must be a number.",
                        val_str
                    ))
                });

                if !(GAIN_MIN_DB..=GAIN_MAX_DB).contains(&input_gain) {
                    exit_with_error(format!(
                        "Input gain ({:.1} dB) out of range [{:.1}, {:.1}].",
                        input_gain, GAIN_MIN_DB, GAIN_MAX_DB
                    ));
                }
            }
            Short('o') | Long("output-gain") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let val_str = val
                    .into_string()
                    .unwrap_or_else(|_| exit_with_error("Invalid output gain value."));
                output_gain = val_str.parse::<f32>().unwrap_or_else(|_| {
                    exit_with_error(format!(
                        "Invalid output gain: '{}'. Must be a number.",
                        val_str
                    ))
                });

                if !(GAIN_MIN_DB..=GAIN_MAX_DB).contains(&output_gain) {
                    exit_with_error(format!(
                        "Output gain ({:.1} dB) out of range [{:.1}, {:.1}].",
                        output_gain, GAIN_MIN_DB, GAIN_MAX_DB
                    ));
                }
            }
            Short('b') | Long("buffer-size") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let val_str = val
                    .into_string()
                    .unwrap_or_else(|_| exit_with_error("Invalid buffer size value."));
                buffer_size = val_str.parse::<u32>().unwrap_or_else(|_| {
                    exit_with_error(format!(
                        "Invalid buffer size: '{}'. Must be an integer.",
                        val_str
                    ))
                });
                validate_buffer_size(buffer_size).unwrap_or_else(|e| exit_with_error(e));
            }
            Long("cabsim-partition") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let val_str = val
                    .into_string()
                    .unwrap_or_else(|_| exit_with_error("Invalid cabsim partition value."));
                let parsed = val_str.parse::<u32>().unwrap_or_else(|_| {
                    exit_with_error(format!(
                        "Invalid cabsim partition: '{}'. Must be an integer.",
                        val_str
                    ))
                });
                cabsim_partition =
                    validate_cabsim_partition(parsed).unwrap_or_else(|e| exit_with_error(e));
            }
            Long("ir-level-policy") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let val_str = val
                    .into_string()
                    .unwrap_or_else(|_| exit_with_error("Invalid --ir-level-policy value."));
                ir_level_policy =
                    parse_ir_level_policy(&val_str).unwrap_or_else(|e| exit_with_error(e));
            }
            Long("calibrate-input") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let val_str = val
                    .into_string()
                    .unwrap_or_else(|_| exit_with_error("Invalid --calibrate-input value."));
                calibrate_input =
                    parse_calibrate_input(&val_str).unwrap_or_else(|e| exit_with_error(e));
            }
            Long("input-calibration-level") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let val_str = val.into_string().unwrap_or_else(|_| {
                    exit_with_error("Invalid --input-calibration-level value.")
                });
                input_calibration_level_dbu =
                    parse_input_calibration_level(&val_str).unwrap_or_else(|e| exit_with_error(e));
            }
            Long("output-mode") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let val_str = val
                    .into_string()
                    .unwrap_or_else(|_| exit_with_error("Invalid --output-mode value."));
                output_mode = parse_output_mode(&val_str).unwrap_or_else(|e| exit_with_error(e));
                output_mode_explicit = true;
            }
            Long("output-limit") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let val_str = val
                    .into_string()
                    .unwrap_or_else(|_| exit_with_error("Invalid --output-limit value."));
                output_limit = parse_output_limit(&val_str).unwrap_or_else(|e| exit_with_error(e));
            }
            _ => exit_with_error(arg.unexpected()),
        }
    }

    if !has_args {
        // No arguments: print help and exit immediately (non-interactive CLI).
        crate::standalone::launcher::handle_empty_invocation();
    }

    CliArgs {
        model_path,
        cab_path,
        cab_optional,
        input_gain,
        output_gain,
        buffer_size,
        cabsim_partition,
        ir_level_policy,
        calibrate_input,
        input_calibration_level_dbu,
        output_mode,
        output_mode_explicit,
        output_limit,
        diagnose,
        diagnose_full,
        slim_override,
        oversample,
        activation,
        cpu,
        record,
        record_dir,
        fail_fast,
        reconnect_timeout,
        gate,
        gate_mode,
        color,
        pause,
    }
}

#[cfg(test)]
#[path = "cli_test.rs"]
mod tests;

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Helper functions for Command Line Interface (CLI).
//!
//! Handles the display of help and parsing of arguments
//! provided by the user via terminal.

use neural_amp_modeler_rs::math::activations::ActivationPrecision;
use neural_amp_modeler_rs::math::constants::{GAIN_MAX_DB, GAIN_MIN_DB};

use crate::standalone::colors::Colorize;
use lexopt::prelude::*;
use neural_amp_modeler_rs::dsp::adaptive::SlimOverride;
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;

use std::path::PathBuf;

// Domain contract for `--buffer-size` (G-RB-003 / T6.1).
//
// The accepted set is `{0} ∪ {2^k | 4 <= k <= 13}`:
// - `BUFFER_SIZE_AUTO` (`0`) delegates the quantum choice to the PipeWire
//   audio graph (auto-negotiation; no `node.latency` property is set).
// - Explicit sizes must be exact powers of two in `[16, 8192]`. The ceiling
//   matches the engine's static DSP buffers (`MAX_RESAMP_BUF` /
//   `MAX_BRIDGE_BUF`, both 8192 in `neural_amp_modeler_rs::dsp::pipeline`),
//   so no `--buffer-size` value can drive an out-of-bounds access or an
//   over-allocation panic downstream (latency, CabSim partition, FFT).
pub const BUFFER_SIZE_AUTO: u32 = 0;
pub const BUFFER_SIZE_MIN: u32 = 16;
pub const BUFFER_SIZE_MAX: u32 = 8192;

/// Structured rejection reason for an out-of-domain `--buffer-size` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferSizeError {
    /// Non-zero `size` below [`BUFFER_SIZE_MIN`].
    BelowMinimum { size: u32 },
    /// `size` above [`BUFFER_SIZE_MAX`] (max bridge capacity).
    AboveMaximum { size: u32 },
    /// `size` inside the inclusive range but not an exact power of two.
    NotPowerOfTwo { size: u32 },
}

impl std::fmt::Display for BufferSizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            BufferSizeError::BelowMinimum { size } => write!(
                f,
                "Buffer size must be at least {BUFFER_SIZE_MIN} (or {BUFFER_SIZE_AUTO} for auto), got {size}"
            ),
            BufferSizeError::AboveMaximum { size } => write!(
                f,
                "Buffer size cannot exceed {BUFFER_SIZE_MAX} (max bridge capacity), got {size}"
            ),
            BufferSizeError::NotPowerOfTwo { size } => write!(
                f,
                "Buffer size must be a power of two (e.g. 64, 128, 256, 512, 1024, 2048, 4096, 8192), got {size}"
            ),
        }
    }
}

impl std::error::Error for BufferSizeError {}

/// Validates a parsed `--buffer-size` value against the documented domain.
///
/// `0` is the auto-negotiation mode (valid). Any non-zero value must satisfy,
/// cumulatively: `>= BUFFER_SIZE_MIN`, `<= BUFFER_SIZE_MAX`, and be an exact
/// power of two. The check is pure and runs before any PipeWire connection or
/// allocation, so malformed input fails fast with a typed, explainable error.
pub fn validate_buffer_size(size: u32) -> Result<u32, BufferSizeError> {
    match size {
        BUFFER_SIZE_AUTO => Ok(size),
        s if s < BUFFER_SIZE_MIN => Err(BufferSizeError::BelowMinimum { size: s }),
        s if s > BUFFER_SIZE_MAX => Err(BufferSizeError::AboveMaximum { size: s }),
        s if !s.is_power_of_two() => Err(BufferSizeError::NotPowerOfTwo { size: s }),
        s => Ok(s),
    }
}

// NOTA DE DESIGN: As funções abaixo usam `println!` (stdout) e `eprintln!` (stderr)
// intencionalmente — o backend `NamLogger` ainda não foi inicializado no momento
// do parsing dos argumentos CLI. Isso é prática POSIX padrão para ferramentas de
// linha de comando. NÃO substituir por `log::*` sem antes garantir que o logger
// esteja inicializado antes de `parse_args()`.

/// Prints usage instructions and help in the terminal.
pub fn print_help() {
    println!(
        "{}",
        format!(
            "🎸 NAM-Audio-Pipe v{} — Neural Amp Modeler",
            env!("CARGO_PKG_VERSION")
        )
        .bright_green()
        .bold()
    );
    println!("\n{}", "Usage:".yellow().bold());
    println!("  nam-audio-pipe [OPTIONS]");
    println!("\n{}", "Options:".yellow().bold());
    println!("  -m, --model <FILE>      Path to the model (.nam or .namb). Supports ~, ../, etc.");
    println!("  -c, --cab <FILE>        Path to the cab-sim IR (.wav). Supports ~, ../, etc.");
    println!("  -i, --input-gain <DB>   Input gain in dB (e.g. -3.5, 12, 0) [default: 0]");
    println!("  -o, --output-gain <DB>  Output gain in dB (e.g. 5.0, -10) [default: 0]");
    println!(
        "  -b, --buffer-size <SAMPLES> Fixed block size: 0 for auto, or a power of two in [16, 8192] (e.g. 64, 256, 512) [default: 256]"
    );
    println!("      --diagnose          Print technical support block and exit");
    println!("      --diagnose-full     Print technical support block with raw paths and exit");
    println!(
        "      --slim auto|full|lite  Adaptive Compute (downgrades the active model) Auto = Under CPU Pressure [default: auto]"
    );
    println!(
        "      --oversample off|2x|4x Half-band oversampling around neural stage [default: off]"
    );
    println!(
        "      --activation MODE    Activation precision: standard (default, exact-grade) or fast"
    );
    println!("      --cpu INDEX          Pin RT audio thread to explicit CPU core index");
    println!("      --record             Record raw PipeWire input to a WAV file");
    println!(
        "      --fail-fast          Disable the bounded backend reconnect cycle (F-RB-010 / T4.5): \
         exit with an error on the first PipeWire stream failure"
    );
    println!("  -h, --help              Show this help message and exit");
}

/// Displays a styled error message and exits the process with code 1.
pub fn exit_with_error(msg: impl std::fmt::Display) -> ! {
    eprintln!("{} {}", "❌ Argument error:".red().bold(), msg);
    eprintln!("{}", "👉 Use '-h' to show the help screen".yellow());
    std::process::exit(1);
}

/// Parsed command line arguments.
#[derive(Debug, Clone)]
pub struct CliArgs {
    /// Path to the neural model file.
    pub model_path: Option<PathBuf>,
    /// Path to the cab-sim impulse response (.wav) file.
    pub cab_path: Option<PathBuf>,
    /// Input gain in dB.
    pub input_gain: f32,
    /// Output gain in dB.
    pub output_gain: f32,
    /// Desired buffer size.
    pub buffer_size: u32,
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
    /// Disable the bounded backend reconnect cycle (`--fail-fast`): the first
    /// PipeWire stream failure triggers the T4.4 fail-fast teardown instead of
    /// a bounded reconnection attempt.
    pub fail_fast: bool,
}

/// Parses command-line arguments.
pub fn parse_args() -> CliArgs {
    parse_args_from(lexopt::Parser::from_env())
}

/// Helper parsing function that accepts any lexopt::Parser to facilitate testing.
pub fn parse_args_from(mut parser: lexopt::Parser) -> CliArgs {
    let mut model_path = None;
    let mut cab_path = None;
    let mut input_gain = 0.0;
    let mut output_gain = 0.0;
    let mut buffer_size = 256;
    let mut diagnose = false;
    let mut diagnose_full = false;
    let mut slim_override = SlimOverride::Auto;
    let mut oversample = OversampleFactor::Off;
    let mut activation = None;
    let mut cpu = None;
    let mut record = false;
    let mut fail_fast = false;
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
            Long("fail-fast") => {
                fail_fast = true;
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

                // Simplified tilde expansion implementation
                let expanded = if p_str.starts_with("~/") {
                    if let Ok(home) = std::env::var("HOME") {
                        p_str.replacen("~", &home, 1)
                    } else {
                        p_str
                    }
                } else {
                    p_str
                };
                model_path = Some(PathBuf::from(expanded));
            }
            Short('c') | Long("cab") => {
                let val = parser.value().unwrap_or_else(|e| exit_with_error(e));
                let p_str = val
                    .into_string()
                    .unwrap_or_else(|_| exit_with_error("Invalid cab IR path (UTF-8)."));

                let expanded = if p_str.starts_with("~/") {
                    if let Ok(home) = std::env::var("HOME") {
                        p_str.replacen("~", &home, 1)
                    } else {
                        p_str
                    }
                } else {
                    p_str
                };
                cab_path = Some(PathBuf::from(expanded));
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
            _ => exit_with_error(arg.unexpected()),
        }
    }

    if !has_args {
        print_help();
        std::process::exit(0);
    }

    CliArgs {
        model_path,
        cab_path,
        input_gain,
        output_gain,
        buffer_size,
        diagnose,
        diagnose_full,
        slim_override,
        oversample,
        activation,
        cpu,
        record,
        fail_fast,
    }
}

#[cfg(test)]
#[path = "cli_test.rs"]
mod tests;

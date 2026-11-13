// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! CLI domain contracts, input validators, path expansion, and help rendering.

use crate::standalone::colors::Colorize;
use neural_amp_modeler_rs::math::constants::GAIN_MIN_DB;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

// Domain contract for `--buffer-size`.
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

// Domain contract for `--cabsim-partition`.
//
// The UPOLS partition is an installation-time policy that fixes the cab-sim
// algorithmic latency (exactly `partition_size` samples) and the RFFT event
// rate (`sample_rate / partition_size` events per second). The accepted set
// is `{32, 64, 128, 256}`:
// - `32`/`64`: live monitoring (lowest IR latency, highest steady-state CPU);
// - `128`: the safe default (live/session balance, the CPU-safe fallback of
//   the risk matrix);
// - `256`: headroom for CPU-constrained machines (offline/mastering bias).
//
// The policy is decoupled from the host quantum (`--buffer-size`): the
// block-agnostic `process_block` driver chunks any quantum against the fixed
// partition, so the latency no longer tracks the PipeWire buffer size.
pub const CABSIM_PARTITION_DEFAULT: u32 = 128;
pub const CABSIM_PARTITION_ALLOWED: [u32; 4] = [32, 64, 128, 256];

/// Structured rejection reason for an out-of-domain `--cabsim-partition`
/// value (the accepted set is the exact [`CABSIM_PARTITION_ALLOWED`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CabSimPartitionError {
    /// The rejected value.
    pub value: u32,
}

impl std::fmt::Display for CabSimPartitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Cab-sim partition must be one of {:?} samples (latency = partition size), got {}",
            CABSIM_PARTITION_ALLOWED, self.value
        )
    }
}

impl std::error::Error for CabSimPartitionError {}

/// Validates a parsed `--cabsim-partition` value against the documented
/// domain (exact membership in [`CABSIM_PARTITION_ALLOWED`]).
///
/// The check is pure and runs before any PipeWire connection or allocation,
/// so malformed input fails fast with a typed, explainable error.
pub fn validate_cabsim_partition(partition: u32) -> Result<u32, CabSimPartitionError> {
    if CABSIM_PARTITION_ALLOWED.contains(&partition) {
        Ok(partition)
    } else {
        Err(CabSimPartitionError { value: partition })
    }
}

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

// DESIGN NOTE: The functions below write to stdout/stderr directly — the
// `NamLogger` backend is initialized before parsing, but `print_help` and
// `exit_with_error` are the canonical CLI surface and print unconditionally.
// This is standard POSIX practice for command-line tools.

/// Writes `text` to stdout, tolerating a closed pipe instead of panicking.
///
/// `println!` panics when the write fails with `BrokenPipe` (e.g.
/// `nam-audio-pipe --help | head -1` closes the pipe after the first line).
/// A `BrokenPipe` means the downstream consumer already stopped reading, so
/// there is nothing left to do and the process must exit cleanly.
pub fn write_stdout(text: &str) {
    use std::io::Write as _;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(text.as_bytes());
    let _ = lock.flush();
}

/// Expands a leading `~` in a user-supplied path against `home`.
///
/// Supported forms (a POSIX-shell subset; no `~user` lookup):
/// - `~`    → `home`;
/// - `~/x`  → `home/x`;
/// - `~user/..` → error: explicit user-home expansion is unsupported;
/// - a leading `~` with no `home` available → error requesting an absolute path;
/// - any other path is returned unchanged.
///
/// The function is pure (the `home` directory is injected) so the expansion
/// matrix is fully unit-testable without touching the process environment.
pub fn expand_user_path(raw: &str, home: Option<&OsStr>) -> Result<PathBuf, String> {
    if !raw.starts_with('~') {
        return Ok(PathBuf::from(raw));
    }

    // `~user` (tilde immediately followed by anything but a separator) is a
    // different feature (password-database lookup) that we deliberately do not
    // implement: fail with an actionable message instead of a literal `~user`.
    let rest = &raw[1..];
    if !(rest.is_empty() || rest.starts_with('/')) {
        return Err(format!(
            "Unsupported '~user' path expansion: '{raw}'. Use '~/' or an absolute path."
        ));
    }

    // An empty `HOME` is treated as unset: `Path::new("").join(tail)` would
    // otherwise produce a CWD-relative path and silently resolve `~/x` against
    // the current directory instead of failing with the documented error.
    let home = home.filter(|h| !h.is_empty()).ok_or_else(|| {
        "HOME is not set; use an absolute path instead of a '~' prefix.".to_string()
    })?;
    let home_path = Path::new(home);
    let tail = rest.trim_start_matches('/');
    if tail.is_empty() {
        Ok(home_path.to_path_buf())
    } else {
        Ok(home_path.join(tail))
    }
}

/// Validates that an explicitly provided input file exists and is a readable
/// regular file, returning its resolved absolute path on success.
///
/// `flag` is the spelling of the originating option (e.g. `--model`) and is
/// included in the error message so the user knows which argument is at fault.
pub fn validate_readable_file(flag: &str, path: &Path) -> Result<PathBuf, String> {
    // `absolute` makes the path absolute (relative to the CWD) without resolving
    // symlinks and without requiring the path to exist, so the error message
    // always shows the fully resolved location the user actually meant.
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let meta = std::fs::metadata(&abs).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!(
            "{flag}: file not found: '{}'. Check the path; '~' is only expanded at the start of the argument.",
            abs.display()
        ),
        _ => format!("{flag}: cannot access '{}': {e}", abs.display()),
    })?;
    if !meta.is_file() {
        return Err(format!(
            "{flag}: '{}' is not a regular file (a directory was given?).",
            abs.display()
        ));
    }
    std::fs::File::open(&abs)
        .map_err(|e| format!("{flag}: cannot read '{}': {e}", abs.display()))?;
    Ok(abs)
}

/// Resolves the capture output directory for `--record`.
///
/// Precedence (first match wins):
/// 1. an explicit `--record-dir` path;
/// 2. `$XDG_MUSIC_DIR/NAM-Audio-Pipe` when `XDG_MUSIC_DIR` is set and non-empty;
/// 3. `$HOME/Music/NAM-Audio-Pipe` when `HOME` is set and non-empty;
/// 4. the current working directory `cwd` — the returned `bool` is `true` to
///    signal this last-resort fallback so `main` can warn the user.
///
/// Pure (every environment value is injected), so the precedence matrix is
/// unit-testable without touching the process environment.
pub fn resolve_record_dir(
    explicit: Option<&Path>,
    xdg_music: Option<&OsStr>,
    home: Option<&OsStr>,
    cwd: &Path,
) -> (PathBuf, bool) {
    if let Some(dir) = explicit {
        return (dir.to_path_buf(), false);
    }
    if let Some(music) = xdg_music.filter(|v| !v.is_empty()) {
        return (Path::new(music).join("NAM-Audio-Pipe"), false);
    }
    if let Some(home) = home.filter(|v| !v.is_empty()) {
        return (Path::new(home).join("Music").join("NAM-Audio-Pipe"), false);
    }
    (cwd.to_path_buf(), true)
}

/// Prints usage instructions and help in the terminal.
pub fn print_help() {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}",
        format!(
            "🎸 NAM-Audio-Pipe v{} — Neural Amp Modeler",
            env!("CARGO_PKG_VERSION")
        )
        .bright_green()
        .bold()
    );
    let _ = writeln!(out, "\n{}", "Usage:".yellow().bold());
    let _ = writeln!(out, "  nam-audio-pipe [OPTIONS]");
    let _ = writeln!(out, "\n{}", "Options:".yellow().bold());
    let _ = writeln!(
        out,
        "  -m, --model <FILE>      Path to the model (.nam or .namb). Supports ~, ../, etc."
    );
    let _ = writeln!(
        out,
        "  -c, --cab <FILE>        Path to the cab-sim IR (.wav). Supports ~, ../, etc."
    );
    let _ = writeln!(
        out,
        "      --cab-optional      Continue without cab-sim when --cab is missing \
         or fails to load (default: a bad --cab is fatal)"
    );
    let _ = writeln!(
        out,
        "  -i, --input-gain <DB>   Input gain in dB (e.g. -3.5, 12, 0) [default: 0]"
    );
    let _ = writeln!(
        out,
        "  -o, --output-gain <DB>  Output gain in dB (e.g. 5.0, -10) [default: 0]"
    );
    let _ = writeln!(
        out,
        "  -b, --buffer-size <SAMPLES> Fixed block size: 0 for auto, or a power of two in [16, 8192] (e.g. 64, 256, 512) [default: 256]"
    );
    let _ = writeln!(
        out,
        concat!(
            "      --cabsim-partition <N> Cab-sim UPOLS partition: 32, 64, 128 or 256 samples —\n",
            "                           fixes the IR latency (N samples; lower = less latency,\n",
            "                           more CPU) [default: 128]"
        )
    );
    let _ = writeln!(
        out,
        "      --ir-level-policy <MODE> Cab-sim IR level policy: peak-normalized (default),\n\
                                   preserved (1:1 WAV amplitude), or legacy-nam-plugin\n\
                                   (10^(-18/20) * 48000/fs)"
    );
    let _ = writeln!(
        out,
        "      --calibrate-input <on|off> Compensate ADC input level against model metadata\n\
                                   'input_level_dbu' [default: off]"
    );
    let _ = writeln!(
        out,
        "      --input-calibration-level <DBU> Interface 0 dBFS reference level in dBu\n\
                                   [-60.0, 60.0] [default: 12.0]"
    );
    let _ = writeln!(
        out,
        "      --output-mode <MODE> Model output level mode: raw, normalized (default, -18 LUFS),\n\
                                   or calibrated (uses model 'output_level_dbu')"
    );
    let _ = writeln!(
        out,
        "      --output-limit <MODE> Playback hardware output limit: off (default) or clamp\n\
                                   (hard clamp to [-1.0, 1.0]; not a dynamic limiter; float\n\
                                   WAV --record output is never clamped)"
    );
    let _ = writeln!(
        out,
        "      --diagnose          Print technical support block and exit"
    );
    let _ = writeln!(
        out,
        "      --diagnose-full     Print technical support block with raw paths and exit"
    );
    let _ = writeln!(
        out,
        "      --pause             Wait for Enter before exiting an immediate-exit \
         invocation (--diagnose) on a terminal"
    );
    let _ = writeln!(
        out,
        "      --slim auto|full|lite  Adaptive Compute (downgrades the active model) Auto = Under CPU Pressure [default: auto]"
    );
    let _ = writeln!(
        out,
        "      --oversample off|2x|4x Half-band oversampling around neural stage [default: off]"
    );
    let _ = writeln!(
        out,
        "      --activation MODE    Activation precision: standard (default, exact-grade) or fast"
    );
    let _ = writeln!(
        out,
        concat!(
            "      --gate MODE          Silence gate: 'on' (-70 dB default), 'off' (disabled),\n",
            "                           or explicit threshold in dB (e.g. -60, -45dB) [default: on]"
        )
    );
    let _ = writeln!(
        out,
        "      --cpu INDEX          Pin RT audio thread to explicit CPU core index"
    );
    let _ = writeln!(
        out,
        "      --color WHEN         Colorize output: 'auto' (TTY-based), 'always', 'never' [default: auto]"
    );
    let _ = writeln!(
        out,
        "      --record             Record raw PipeWire input to a WAV file"
    );
    let _ = writeln!(
        out,
        concat!(
            "      --record-dir <DIR>   Directory for WAV captures (created if missing)\n",
            "                           [default: $XDG_MUSIC_DIR/NAM-Audio-Pipe, else\n",
            "                           ~/Music/NAM-Audio-Pipe, else the current directory]"
        )
    );
    let _ = writeln!(
        out,
        "      --fail-fast          Disable the bounded backend reconnect cycle: \
         exit with an error on the first PipeWire stream failure"
    );
    let _ = writeln!(
        out,
        concat!(
            "      --reconnect-timeout <SECS>\n",
            "                           Cap the per-episode reconnect backoff budget in\n",
            "                           seconds (default: 11.75 s over 6 attempts)"
        )
    );
    let _ = writeln!(
        out,
        "  -h, --help              Show this help message and exit"
    );
    write_stdout(&out);
}

/// Displays a styled *argument* error and exits with the usage code
/// ([`ExitKind::Usage`](crate::standalone::exit::ExitKind::Usage)).
///
/// Argument errors keep their own wording because they are the only failures
/// where reading the help screen is the remediation; runtime failures use
/// [`exit_runtime_error`](crate::standalone::exit::exit_runtime_error) instead.
pub fn exit_with_error(msg: impl std::fmt::Display) -> ! {
    eprintln!("{} {}", "❌ Argument error:".red().bold(), msg);
    eprintln!("{}", "👉 Use '-h' to show the help screen".yellow());
    std::process::exit(crate::standalone::exit::ExitKind::Usage.code());
}

/// Silence gate configuration for monitoring and recording.
///
/// Expressive domain model that supersedes the legacy binary on/off switch:
/// the gate is either disabled ([`GateConfig::Off`], whose zeroed linear
/// thresholds are the mathematical equivalent of `-inf dBFS`) or enabled with
/// an explicit opening threshold in dBFS plus the Schmitt-trigger closing
/// threshold (opening minus [`GateConfig::DEFAULT_HYSTERESIS_DB`], floored at
/// [`GateConfig::MIN_THRESHOLD_DB`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GateConfig {
    /// Gate disabled: linear thresholds `(0.0, 0.0)` keep the FSM permanently
    /// `Open` — silence passes through gracefully to monitoring and `--record`.
    Off,
    /// Gate enabled with the configured dBFS thresholds.
    Threshold {
        /// Opening threshold (dBFS): energy above it opens the gate.
        threshold_open_db: f32,
        /// Closing threshold (dBFS): Schmitt-hysteresis floor that re-engages
        /// the gate only after a sustained drop below it.
        threshold_close_db: f32,
    },
}

impl GateConfig {
    /// Default opening threshold: `-70.0 dBFS`.
    pub const DEFAULT_OPEN_DB: f32 = -70.0;
    /// Default hysteresis band between the opening and closing thresholds: `10.0 dB`.
    pub const DEFAULT_HYSTERESIS_DB: f32 = 10.0;
    /// Lowest accepted threshold: `-96.0 dBFS`, matching the gain LUT floor
    /// (`GAIN_MIN_DB` — the PCM-16 quantization noise floor).
    pub const MIN_THRESHOLD_DB: f32 = GAIN_MIN_DB;
    /// Highest accepted threshold: `-20.0 dBFS` — safety ceiling that prevents
    /// the gate from chopping soft-picked musical notes.
    pub const MAX_THRESHOLD_DB: f32 = -20.0;

    /// The canonical default: open at [`GateConfig::DEFAULT_OPEN_DB`] with the
    /// standard 10 dB hysteresis (`-80.0 dB` close threshold).
    pub fn default_on() -> Self {
        Self::from_open_db(Self::DEFAULT_OPEN_DB)
    }

    /// Builds an enabled gate from an opening threshold in dBFS, deriving the
    /// closing threshold via the Schmitt-trigger relation
    /// `close = max(open - DEFAULT_HYSTERESIS_DB, MIN_THRESHOLD_DB)`.
    pub fn from_open_db(open_db: f32) -> Self {
        Self::Threshold {
            threshold_open_db: open_db,
            threshold_close_db: (open_db - Self::DEFAULT_HYSTERESIS_DB).max(Self::MIN_THRESHOLD_DB),
        }
    }
}

/// Maps a `--gate` value to its [`GateConfig`], case-insensitively.
///
/// Accepted syntax (polymorphic):
/// - mode literals: `on` / `true` / `default` → [`GateConfig::default_on`];
///   `off` / `false` / `0` → [`GateConfig::Off`];
/// - numeric thresholds in dBFS, optionally suffixed with `db`/`dbfs`
///   (`-60`, `-65.5`, `-45dB`, `-50dbfs`);
/// - positive finite values (`60`) are auto-normalized to `-60.0 dBFS` with an
///   informative off-RT [`log::info!`] (the logger is initialized before
///   parsing in `main`), then validated against `[-96.0, -20.0] dBFS`.
///
/// Returns the exact fatal message [`exit_with_error`] would print for an
/// invalid value, so the caller owns the exit decision (making the mapping
/// unit-testable without terminating the test process).
pub(crate) fn parse_gate_mode(value: &str) -> Result<GateConfig, String> {
    let trimmed = value.trim();
    let lower = trimmed.to_lowercase();
    match lower.as_str() {
        "on" | "true" | "default" => return Ok(GateConfig::default_on()),
        "off" | "false" | "0" => return Ok(GateConfig::Off),
        _ => {}
    }

    // Tolerates an optional unit suffix: "-45db" / "-45dbfs" (case-insensitive).
    let numeric = if let Some(stripped) = lower.strip_suffix("dbfs") {
        stripped.trim()
    } else if let Some(stripped) = lower.strip_suffix("db") {
        stripped.trim()
    } else {
        lower.as_str()
    };

    let parsed = numeric.parse::<f32>();
    let mut open_db = match parsed {
        Ok(v) if v.is_finite() => v,
        _ => {
            return Err(format!(
                "Invalid gate mode: '{trimmed}'. Expected 'on', 'off', 'false', '0', \
                 or a dBFS threshold in [-{min:.0}, -{max:.0}] (e.g. -60, -45dB).",
                min = GateConfig::MIN_THRESHOLD_DB.abs(),
                max = GateConfig::MAX_THRESHOLD_DB.abs(),
            ));
        }
    };

    // UX heuristic: guitarists type "--gate 60" meaning 60 dB of attenuation.
    // Auto-normalize the sign with an informative off-RT log.
    let typed_positive = open_db > 0.0;
    if typed_positive {
        log::info!("Normalizing gate threshold from +{open_db} dB to -{open_db} dBFS.");
        open_db = -open_db;
    }

    if !(GateConfig::MIN_THRESHOLD_DB..=GateConfig::MAX_THRESHOLD_DB).contains(&open_db) {
        let sign_hint = if typed_positive {
            " Positive values are read as attenuation and auto-normalized \
             (e.g. '10' becomes -10.0 dBFS)."
        } else {
            ""
        };
        return Err(format!(
            "Invalid gate mode: '{trimmed}'. Threshold {open_db:.1} dBFS is out of the accepted \
             range [{min:.1}, {max:.1}] dBFS.{sign_hint}",
            min = GateConfig::MIN_THRESHOLD_DB,
            max = GateConfig::MAX_THRESHOLD_DB,
        ));
    }

    Ok(GateConfig::from_open_db(open_db))
}

/// Level policy applied to a cabinet impulse response before convolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IrLevelPolicy {
    /// Normalize the IR kernel so its absolute peak is 1.0 (0 dBFS).
    #[default]
    PeakNormalized,
    /// Preserve the original WAV amplitude (1:1 gain, no peak normalization).
    Preserved,
    /// Apply the legacy C++ NeuralAmpModelerPlugin level scaling:
    /// `10^(-18/20) * (48000 / target_rate)` on the unnormalized IR.
    /// Preserves Rust polyphase sinc resampling and full IR length.
    LegacyNamPlugin,
}

impl IrLevelPolicy {
    /// Stable CLI/log identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PeakNormalized => "peak-normalized",
            Self::Preserved => "preserved",
            Self::LegacyNamPlugin => "legacy-nam-plugin",
        }
    }
}

/// Parses `--ir-level-policy <MODE>`.
pub fn parse_ir_level_policy(val: &str) -> Result<IrLevelPolicy, String> {
    match val.to_ascii_lowercase().as_str() {
        "peak-normalized" | "peak" => Ok(IrLevelPolicy::PeakNormalized),
        "preserved" | "raw" | "unity" => Ok(IrLevelPolicy::Preserved),
        "legacy-nam-plugin" | "legacy" => Ok(IrLevelPolicy::LegacyNamPlugin),
        _ => Err(format!(
            "Invalid --ir-level-policy: '{val}'. Use peak-normalized, preserved, or legacy-nam-plugin."
        )),
    }
}

/// Optional hardware playback output limit policy.
///
/// Explicitly distinguishes a stateless hard clamp from a dynamic limiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputLimitPolicy {
    /// No output limiting (default): playback delivers raw f32 samples.
    #[default]
    Off,
    /// Hard clamp playback samples to `[-1.0, 1.0]` (maps `NaN` to `0.0`).
    /// This is a stateless hard clip, NOT a dynamic limiter; float WAV
    /// recording (`--record`) is never clamped.
    Clamp,
}

impl OutputLimitPolicy {
    /// Stable CLI/log identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Clamp => "clamp",
        }
    }
}

/// Parses `--output-limit <MODE>`.
pub fn parse_output_limit(val: &str) -> Result<OutputLimitPolicy, String> {
    match val.to_ascii_lowercase().as_str() {
        "off" | "none" => Ok(OutputLimitPolicy::Off),
        "clamp" | "hard-clamp" => Ok(OutputLimitPolicy::Clamp),
        _ => Err(format!(
            "Invalid --output-limit: '{val}'. Use off or clamp."
        )),
    }
}

/// Applies `policy` in-place to `samples` at `target_rate` Hz.
///
/// Returns the scalar gain factor applied (1.0 when unchanged or silent).
pub fn apply_ir_level_policy(samples: &mut [f32], policy: IrLevelPolicy, target_rate: u32) -> f32 {
    match policy {
        IrLevelPolicy::PeakNormalized => {
            let peak =
                samples.iter().fold(
                    0.0f32,
                    |acc, &s| {
                        if s.is_finite() { acc.max(s.abs()) } else { acc }
                    },
                );
            if peak <= 0.0 || (peak - 1.0).abs() < f32::EPSILON {
                return 1.0;
            }
            let gain = 1.0 / peak;
            for s in samples.iter_mut() {
                *s *= gain;
            }
            gain
        }
        IrLevelPolicy::Preserved => 1.0,
        IrLevelPolicy::LegacyNamPlugin => {
            let rate = f64::from(target_rate.max(1));
            let gain = (10.0_f64.powf(-18.0 / 20.0) * (48_000.0 / rate)) as f32;
            for s in samples.iter_mut() {
                *s *= gain;
            }
            gain
        }
    }
}

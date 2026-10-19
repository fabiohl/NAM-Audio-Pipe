// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Helper functions for Command Line Interface (CLI).
//!
//! Handles the display of help and parsing of arguments
//! provided by the user via terminal.

use neural_amp_modeler_rs::math::activations::ActivationPrecision;
use neural_amp_modeler_rs::math::constants::{GAIN_MAX_DB, GAIN_MIN_DB};

use crate::standalone::colors::{ColorFlag, Colorize};
use lexopt::prelude::*;
use neural_amp_modeler_rs::dsp::adaptive::SlimOverride;
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// Validates the presence/readability of every explicitly provided input file.
///
/// This is an *argument* validation (legitimate `exit_with_error` territory):
/// it runs right after parsing, before any PipeWire connection or allocation,
/// so a typo in `--model`/`--cab` fails fast with a clear path.
///
/// `--cab` is the one exception: when `--cab-optional` is set, a missing or
/// unreadable IR is left for the loader, which warns and continues without
/// cab-sim (the same contract as a load failure). Only the default fatal mode
/// rejects an unusable `--cab` here (R-2).
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
fn parse_gate_mode(value: &str) -> Result<GateConfig, String> {
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

/// Parsed command line arguments.
#[derive(Debug, Clone)]
pub struct CliArgs {
    /// Path to the neural model file.
    pub model_path: Option<PathBuf>,
    /// Path to the cab-sim impulse response (.wav) file.
    pub cab_path: Option<PathBuf>,
    /// When set (`--cab-optional`), a failed cab-sim IR load is non-fatal:
    /// the host warns and continues without cab-sim. Without it, an explicitly
    /// requested IR that cannot be loaded aborts the process (F-UX-07).
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
    /// Terminal color policy from `--color` (defaults to [`ColorFlag::Auto`]).
    pub color: ColorFlag,
    /// Wait for Enter before exiting after a terminal-only, exit-immediate
    /// invocation (`--diagnose`; F-UX-05 `.desktop` "Diagnostics" action) when
    /// stdout is a terminal. Ignored for the long-running audio host.
    pub pause: bool,
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
        color,
        pause,
    }
}

#[cfg(test)]
#[path = "cli_test.rs"]
mod tests;

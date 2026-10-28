// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Runtime error reporting and the process exit-code contract.
//!
//! Command-line *argument* errors keep their own printer
//! ([`crate::standalone::cli::exit_with_error`]); this module owns the
//! actionable, *classified* runtime failures that occur after a successful
//! argument parse (model/IR loading, backend startup) so that automation and
//! support can tell the failure class apart from a single exit code.
//!
//! ## Exit-code contract (documented in `README.md`)
//!
//! | Code | Meaning                                              |
//! | ---- | ---------------------------------------------------- |
//! | 0    | success                                              |
//! | 1    | uncategorized / forced failure (legacy, signals)     |
//! | 2    | usage error (invalid command-line arguments)         |
//! | 3    | neural model load/validation failure                 |
//! | 4    | cab-sim impulse response (IR) load failure           |
//! | 5    | backend / host initialization failure                |
//! | 6    | recording (capture) failure                          |
//!
//! The recording worker's own [`crate::recording::guard::RecordingWorkerOutcome::exit_code`]
//! delegates to [`ExitKind::Recording`] so the two surfaces never diverge.

use crate::standalone::colors::Colorize;

/// Process exit code for uncategorized or forced failures.
///
/// Reserved for the force-exit signal path (a second Ctrl+C after the grace
/// window, [`crate::standalone::signals`]) and for any runtime failure that is
/// not worth its own class. The force-exit path reads this constant instead of
/// a bare `1` so the reserved code has a single source of truth.
pub const EXIT_GENERIC: i32 = 1;

/// Shared remediation hint for a PipeWire backend that cannot be reached.
///
/// Single source of truth for the "daemon unreachable" text printed by both the
/// first-connection diagnostic (`crate::standalone::pw_host::run`) and the
/// classified `Backend` exit in `main`, so the two never drift into showing
/// different commands for the same failure.
pub const BACKEND_UNREACHABLE_HINT: &str = "Check that the PipeWire daemon is running (`systemctl --user status pipewire \
     wireplumber`); start or restart it if needed. Inside Flatpak, grant socket \
     access (`--filesystem=xdg-run/pipewire-0`).";

/// Rollback switch for the classified runtime errors.
///
/// When set to `true`, [`exit_runtime_error`] degrades to the legacy printer:
/// the message is printed through
/// [`crate::standalone::cli::exit_with_error`] and the process exits with the
/// usage code. The switch only affects [`exit_runtime_error`] call sites —
/// argument errors ([`crate::standalone::cli::exit_with_error`]) and recording
/// failures ([`crate::recording::guard::RecordingWorkerOutcome::exit_code`])
/// keep their own codes regardless. Flipping this single constant reverts the
/// classification without touching any call site.
///
/// The `true` branch is intentionally compile-time-gated by this constant: it is
/// not exercised in normal builds, only when an operator flips it during a
/// problematic rollout.
pub const RUNTIME_ERRORS_USE_LEGACY: bool = false;

/// Runtime failure classes and their process exit codes.
///
/// The discriminants are the process exit codes (`ExitKind::Model as i32 == 3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitKind {
    /// Invalid command-line arguments (usage error).
    Usage = 2,
    /// Neural model load or validation failure.
    Model = 3,
    /// Cab-sim impulse response (IR) load failure.
    CabIr = 4,
    /// Audio backend or host initialization failure.
    Backend = 5,
    /// Recording (capture) failure.
    Recording = 6,
}

impl ExitKind {
    /// Returns the process exit code for this failure class.
    pub fn code(self) -> i32 {
        self as i32
    }

    /// Returns the short, user-facing label used in the error line
    /// (e.g. `Model`, `Cab IR`, `Backend`).
    pub fn label(self) -> &'static str {
        match self {
            ExitKind::Usage => "Usage",
            ExitKind::Model => "Model",
            ExitKind::CabIr => "Cab IR",
            ExitKind::Backend => "Backend",
            ExitKind::Recording => "Recording",
        }
    }
}

/// Renders the two-line actionable runtime error without printing it.
///
/// Pure (no I/O, no process exit) so the exact wording and layout are unit
/// testable; [`exit_runtime_error`] is the only production printer. The hint
/// line is omitted when `hint` is empty, which lets a caller that already
/// emitted a richer [`neural_amp_modeler_rs::common::diagnostics::NamDiagnostic`]
/// avoid duplicating it.
pub fn format_runtime_error(kind: ExitKind, msg: &str, hint: &str) -> String {
    let mut out = format!(
        "{} {}",
        format!("❌ {} error:", kind.label()).red().bold(),
        msg
    );
    if !hint.is_empty() {
        out.push('\n');
        out.push_str(&format!("💡 {hint}"));
    }
    out
}

/// Prints an actionable runtime error to `stderr` and exits with the class code.
///
/// Unlike the argument-error printer, the output never suggests `-h`: these
/// failures are not fixed by reading the help screen, they carry their own
/// remediation hint. Never emits a panic backtrace.
pub fn exit_runtime_error(
    kind: ExitKind,
    msg: impl std::fmt::Display,
    hint: impl std::fmt::Display,
) -> ! {
    if RUNTIME_ERRORS_USE_LEGACY {
        crate::standalone::cli::exit_with_error(msg);
    }
    eprintln!(
        "{}",
        format_runtime_error(kind, &msg.to_string(), &hint.to_string())
    );
    std::process::exit(kind.code());
}

#[cfg(test)]
#[path = "exit_test.rs"]
mod tests;

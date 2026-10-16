// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Provides a simple `Colorize` trait for formatting strings with
//! ANSI escape sequences in POSIX terminals.
//!
//! **Design Decision:** We opted to keep this minimal local implementation
//! instead of adding external crates (e.g.: `colored`, `owo-colors`) to keep
//! the dependency graph as lean as possible, since the project's need
//! is basic and restricted to non-critical threads (CLI/Logging).
//!
//! ## Color policy
//!
//! ANSI sequences are emitted **only** when the process-global
//! [`color_enabled`] flag is set. The flag is initialized once from the
//! environment and terminal state by [`init_from_env`] (called at the very top
//! of `main`, before any output) and may be overridden by the explicit
//! `--color auto|always|never` CLI flag through [`apply_flag`].
//!
//! The pure decision function is [`decide_color`]; [`ColorFlag::Auto`] honors,
//! in order of precedence: `CLICOLOR_FORCE` (non-empty, non-`0` → on),
//! `NO_COLOR` (non-empty → off), `TERM=dumb` (→ off), then the detected TTY
//! state. `std::io::IsTerminal` is stable and dependency-free.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

/// Terminal color mode selected on the command line (`--color`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorFlag {
    /// Decide from the environment and TTY detection (default).
    #[default]
    Auto,
    /// Always emit ANSI sequences, even when the output is not a TTY.
    Always,
    /// Never emit ANSI sequences.
    Never,
}

/// Process-global color switch read by every [`Colorize`] method.
///
/// Defaults to `false` (no color) until [`init_from_env`] runs at the top of
/// `main`; this keeps uninitialized code paths (e.g. unit tests) deterministic
/// and free of escape sequences.
static COLOR_ENABLED: AtomicBool = AtomicBool::new(false);

/// Returns whether ANSI styling is currently enabled.
pub fn color_enabled() -> bool {
    COLOR_ENABLED.load(Ordering::Relaxed)
}

/// Pure color-decision function (environment × TTY × explicit flag).
///
/// `--color`/`flag` has the highest precedence: [`ColorFlag::Always`] and
/// [`ColorFlag::Never`] are unconditional. In [`ColorFlag::Auto`] the order is:
/// `clicolor_force` (non-empty and not `"0"` → on), then `no_color` (non-empty
/// → off), then `term == "dumb"` (→ off), then `is_tty`.
pub fn decide_color(
    no_color: Option<&str>,
    clicolor_force: Option<&str>,
    term: Option<&str>,
    is_tty: bool,
    flag: ColorFlag,
) -> bool {
    match flag {
        ColorFlag::Always => true,
        ColorFlag::Never => false,
        ColorFlag::Auto => {
            if clicolor_force.is_some_and(|v| !v.is_empty() && v != "0") {
                return true;
            }
            if no_color.is_some_and(|v| !v.is_empty()) {
                return false;
            }
            if term == Some("dumb") {
                return false;
            }
            is_tty
        }
    }
}

/// Evaluates the environment + TTY state for a given [`ColorFlag`].
///
/// Both standard streams must be terminals: help/diagnostics go to `stdout`,
/// while logs and error messages go to `stderr`; redirecting either one to a
/// file or pipe (e.g. `nam-audio-pipe --help | grep`, `2> log.txt`) must
/// silence ANSI for that destination.
fn env_decision(flag: ColorFlag) -> bool {
    let no_color = std::env::var("NO_COLOR").ok();
    let clicolor_force = std::env::var("CLICOLOR_FORCE").ok();
    let term = std::env::var("TERM").ok();
    let is_tty = std::io::stdout().is_terminal() && std::io::stderr().is_terminal();
    decide_color(
        no_color.as_deref(),
        clicolor_force.as_deref(),
        term.as_deref(),
        is_tty,
        flag,
    )
}

/// Initializes the global color policy from the environment (`Auto`).
///
/// Must run before the first colored output (including `print_help` and
/// `exit_with_error`).
pub fn init_from_env() {
    COLOR_ENABLED.store(env_decision(ColorFlag::Auto), Ordering::Relaxed);
}

/// Recomputes and stores the global color policy for an explicit `--color` flag.
///
/// Applied when the flag is parsed, overriding the `Auto` value set by
/// [`init_from_env`]. Arguments parsed before the flag (or parse errors raised
/// before it) keep the environment-based decision.
pub fn apply_flag(flag: ColorFlag) {
    COLOR_ENABLED.store(env_decision(flag), Ordering::Relaxed);
}

/// Wraps `text` in the given SGR `code` when color is enabled, otherwise
/// returns the raw text unchanged.
fn styled(code: &str, text: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Simple trait for formatting strings with ANSI escape sequences in POSIX terminals.
pub trait Colorize {
    /// Applies bold formatting.
    fn bold(&self) -> String;
    /// Colors the text red.
    fn red(&self) -> String;
    /// Colors the text green.
    fn green(&self) -> String;
    /// Colors the text yellow.
    fn yellow(&self) -> String;
    /// Colors the text blue.
    fn blue(&self) -> String;
    /// Colors the text cyan.
    fn cyan(&self) -> String;
    /// Colors the text bright red.
    fn bright_red(&self) -> String;
    /// Colors the text bright green.
    fn bright_green(&self) -> String;
    /// Colors the text bright blue.
    fn bright_blue(&self) -> String;
    /// Colors the text bright cyan.
    fn bright_cyan(&self) -> String;
}

impl<T: AsRef<str>> Colorize for T {
    fn bold(&self) -> String {
        styled("1", self.as_ref())
    }

    fn red(&self) -> String {
        styled("31", self.as_ref())
    }

    fn green(&self) -> String {
        styled("32", self.as_ref())
    }

    fn yellow(&self) -> String {
        styled("33", self.as_ref())
    }

    fn blue(&self) -> String {
        styled("34", self.as_ref())
    }

    fn cyan(&self) -> String {
        styled("36", self.as_ref())
    }

    fn bright_red(&self) -> String {
        styled("91", self.as_ref())
    }

    fn bright_green(&self) -> String {
        styled("92", self.as_ref())
    }

    fn bright_blue(&self) -> String {
        styled("94", self.as_ref())
    }

    fn bright_cyan(&self) -> String {
        styled("96", self.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_flags_override_environment_and_tty() {
        // `--color always` forces color even with NO_COLOR and no TTY.
        assert!(decide_color(
            Some("1"),
            None,
            Some("dumb"),
            false,
            ColorFlag::Always
        ));
        // `--color never` disables color even with CLICOLOR_FORCE and a TTY.
        assert!(!decide_color(
            None,
            Some("1"),
            Some("xterm-256color"),
            true,
            ColorFlag::Never
        ));
    }

    #[test]
    fn auto_prefers_clicolor_force_over_no_color() {
        assert!(decide_color(
            Some("1"),
            Some("1"),
            Some("dumb"),
            false,
            ColorFlag::Auto
        ));
    }

    #[test]
    fn auto_no_color_disables_regardless_of_tty() {
        assert!(!decide_color(
            Some("1"),
            None,
            Some("xterm-256color"),
            true,
            ColorFlag::Auto
        ));
        // Empty `NO_COLOR` carries no intent (spec: only a non-empty value opts out).
        assert!(decide_color(
            Some(""),
            None,
            Some("xterm-256color"),
            true,
            ColorFlag::Auto
        ));
    }

    #[test]
    fn auto_dumb_terminal_disables_color() {
        assert!(!decide_color(
            None,
            None,
            Some("dumb"),
            true,
            ColorFlag::Auto
        ));
        // `CLICOLOR_FORCE=0` is explicitly "not forced".
        assert!(!decide_color(
            None,
            Some("0"),
            Some("dumb"),
            true,
            ColorFlag::Auto
        ));
    }

    #[test]
    fn auto_falls_back_to_tty_state() {
        assert!(decide_color(
            None,
            None,
            Some("xterm-256color"),
            true,
            ColorFlag::Auto
        ));
        assert!(!decide_color(
            None,
            None,
            Some("xterm-256color"),
            false,
            ColorFlag::Auto
        ));
    }
}

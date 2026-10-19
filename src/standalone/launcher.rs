// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Launcher exit behavior and terminal pause utilities (F-UX-05).
//!
//! When the program is started with no arguments, the help is printed and the
//! process exits immediately with code 0 (non-interactive CLI by design).
//!
//! The `--pause` flag remains available for explicit terminal pauses
//! (e.g. the `.desktop` "Diagnostics" action) via [`pause_if_interactive`].

use std::io::IsTerminal;

/// What to do when the program is invoked with no arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupMode {
    /// Arguments were provided: run normally (only produced by
    /// [`startup_mode`] for completeness; the launcher path never uses it).
    Run,
    /// Print the help and exit immediately (non-interactive invocation).
    ExitImmediately,
}

/// Decides the invocation behavior from the argument state.
///
/// * any argument → [`StartupMode::Run`];
/// * no arguments → [`StartupMode::ExitImmediately`].
pub fn startup_mode(args_empty: bool) -> StartupMode {
    if !args_empty {
        StartupMode::Run
    } else {
        StartupMode::ExitImmediately
    }
}

/// Waits for a line on stdin, flushing stdout first so the prompt is visible.
///
/// Best-effort: EOF or an I/O error simply ends the wait (the process is about
/// to exit anyway).
pub fn pause_for_enter() {
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

/// Prints the launcher's "press Enter" prompt and waits for a line, flushing
/// stdout first so the prompt is visible immediately.
fn prompt_and_pause() {
    crate::standalone::cli::write_stdout("\nPress Enter to close...\n");
    pause_for_enter();
}

/// Waits for Enter before exiting, but only when **both** `stdin` and `stdout`
/// are terminals.
///
/// Used by the `.desktop` "Diagnostics" action (`--diagnose --pause`). Requiring
/// both streams ensures a redirected `stdin` (pipe/FIFO) can never block automation
/// on an unread `read_line`.
pub fn pause_if_interactive() {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        prompt_and_pause();
    }
}

/// Handles a no-arguments invocation: prints the help and exits immediately.
/// Never returns.
pub fn handle_empty_invocation() -> ! {
    crate::standalone::cli::print_help();
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_always_run_normally() {
        assert_eq!(startup_mode(false), StartupMode::Run);
    }

    #[test]
    fn empty_invocation_always_exits_immediately() {
        assert_eq!(startup_mode(true), StartupMode::ExitImmediately);
    }
}

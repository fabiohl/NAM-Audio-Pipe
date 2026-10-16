// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! First-use launcher experience for the installed `.desktop` entry (F-UX-05).
//!
//! The Flatpak/`.desktop` launcher runs `nam-audio-pipe` with `Terminal=true`
//! and no arguments. Historically the process printed the help and exited
//! immediately, so the terminal window opened by the launcher vanished before
//! the user could read anything — the first impression of the installed app was
//! "it does not open".
//!
//! The fix is deliberately minimal and GUI-free: when the program is started
//! with **no arguments** *and* both streams are terminals (the interactive
//! launcher case), the help is printed and the process waits for Enter before
//! exiting. Every non-interactive invocation (pipes, scripts, `systemd`,
//! `--diagnose`) keeps the historical exit-immediately behavior, so automation
//! is never blocked on a stdin read.
//!
//! A persistent "last session" config (T5.1b) is intentionally **not**
//! implemented here: it is a new persistent surface that requires an explicit
//! product decision.

use std::io::IsTerminal;

/// What to do when the program is invoked with no arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupMode {
    /// Arguments were provided: run normally (only produced by
    /// [`startup_mode`] for completeness; the launcher path never uses it).
    Run,
    /// Print the help and wait for Enter so the launcher's terminal stays open.
    PauseBeforeExit,
    /// Print the help and exit immediately (non-interactive invocation).
    ExitImmediately,
}

/// Decides the first-use behavior from the invocation shape.
///
/// Pure (all inputs injected), so the decision matrix is unit-testable without
/// touching the real process streams:
/// * any argument → [`StartupMode::Run`];
/// * no arguments and `stdin`+`stdout` both terminals → [`StartupMode::PauseBeforeExit`];
/// * no arguments and either stream redirected → [`StartupMode::ExitImmediately`].
pub fn startup_mode(args_empty: bool, stdin_tty: bool, stdout_tty: bool) -> StartupMode {
    if !args_empty {
        StartupMode::Run
    } else if stdin_tty && stdout_tty {
        StartupMode::PauseBeforeExit
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
/// both streams mirrors [`startup_mode`]'s pause precondition, so a redirected
/// `stdin` (pipe/FIFO) can never block automation on an unread `read_line`.
pub fn pause_if_interactive() {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        prompt_and_pause();
    }
}

/// Handles a no-arguments invocation: prints the help and either pauses for the
/// launcher's terminal or exits immediately. Never returns.
pub fn handle_empty_invocation() -> ! {
    let mode = startup_mode(
        true,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    );
    crate::standalone::cli::print_help();
    if mode == StartupMode::PauseBeforeExit {
        prompt_and_pause();
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_always_run_normally() {
        assert_eq!(startup_mode(false, true, true), StartupMode::Run);
        assert_eq!(startup_mode(false, false, false), StartupMode::Run);
    }

    #[test]
    fn empty_invocation_on_a_terminal_pauses_before_exit() {
        assert_eq!(startup_mode(true, true, true), StartupMode::PauseBeforeExit);
    }

    #[test]
    fn empty_invocation_redirected_exits_immediately() {
        // Either stream redirected is enough to keep automation non-blocking.
        assert_eq!(
            startup_mode(true, false, true),
            StartupMode::ExitImmediately
        );
        assert_eq!(
            startup_mode(true, true, false),
            StartupMode::ExitImmediately
        );
        assert_eq!(
            startup_mode(true, false, false),
            StartupMode::ExitImmediately
        );
    }
}

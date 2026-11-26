// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Post-instance teardown observations and reconnect decisions.

use super::super::SharedBackendStatus;
use super::super::reconnect::ReconnectCycle;
use super::super::run_lifecycle::{interrupted_backoff_failure, sleep_interruptible};
use crate::standalone::colors::Colorize;
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;

pub(super) enum AfterTeardown {
    Exit,
    Reconnect,
}

pub(super) fn observe_late_panic(
    rt_status: &RtStatusFlags,
    instance_failure: &mut Option<(&'static str, String)>,
) -> bool {
    let mut panic_is_terminal = false;
    // A panic may have been deferred on the RT thread and only observed
    // here (for example when SHUTDOWN latched during the control-loop
    // wait, or an earlier backend failure broke the loop). Flush it before
    // teardown so the payload/crash report is never lost.
    if rt_status.check_flag(super::super::rt_callback::RT_STATUS_PANIC_CAPTURED) {
        super::super::rt_panic::flush_deferred_panic();
    }

    if instance_failure.is_none()
        && rt_status.check_flag(super::super::rt_callback::RT_STATUS_PANIC_CAPTURED)
    {
        panic_is_terminal = true;
        *instance_failure = Some((
            "rt_callback",
            "panic captured in an RT callback closure (observed at shutdown — never a clean exit)"
                .to_owned(),
        ));
    }
    panic_is_terminal
}

pub(super) fn decide_after_teardown(
    instance_failure: Option<(&'static str, String)>,
    panic_is_terminal: bool,
    reconnect: &mut ReconnectCycle,
    backend_status: &SharedBackendStatus,
) -> (AfterTeardown, Option<(&'static str, String)>) {
    match instance_failure {
        None => (AfterTeardown::Exit, None),
        Some((stream, reason)) => {
            if panic_is_terminal {
                return (AfterTeardown::Exit, Some((stream, reason)));
            }
            if let Some(backoff) = reconnect.begin_attempt() {
                log::warn!(
                    "{} PipeWire backend lost the '{stream}' stream ({reason}). \
                     Reconnect attempt {}/{} in {:?} (total backoff budget {:?}) — \
                     DSP state (models/IRs/recording) is preserved.",
                    "🔁".yellow(),
                    reconnect.attempts_made(),
                    reconnect.policy().max_attempts,
                    backoff,
                    reconnect.policy().total_backoff_budget(),
                );
                backend_status.begin_reconnect(
                    reconnect.attempts_made(),
                    reconnect.policy().max_attempts,
                    backoff,
                );
                if sleep_interruptible(backoff) {
                    // SHUTDOWN interrupted the retry: the backend already
                    // failed, so the exit code must preserve that failure.
                    return (
                        AfterTeardown::Exit,
                        Some(interrupted_backoff_failure(stream, reason)),
                    );
                }
                (AfterTeardown::Reconnect, None)
            } else {
                (AfterTeardown::Exit, Some((stream, reason)))
            }
        }
    }
}

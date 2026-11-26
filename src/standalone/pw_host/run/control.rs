// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Per-iteration control-loop helpers for the host runner.

use super::super::SharedBackendStatus;
use super::super::reconnect::{ReconnectCycle, STABLE_RESET, StabilityEvent};
use crate::recording::alert::RecordingAlert;
use crate::recording::status::SharedRecordingStatus;

pub(super) fn poll_recording_alert(
    recording_alert: &mut Option<RecordingAlert>,
    recording_status: &Option<SharedRecordingStatus>,
) {
    if let (Some(alert), Some(status)) = (&mut *recording_alert, recording_status)
        && let Some(emitted) = alert.poll(status)
    {
        log::error!("{}", emitted.render());
    }
}

pub(super) fn note_stream_stability(
    backend_status: &SharedBackendStatus,
    reconnect: &mut ReconnectCycle,
) {
    if backend_status.is_running() {
        match reconnect.note_stable() {
            StabilityEvent::Restored { attempts } => {
                log::info!("🎸 Audio restored after {attempts} attempt(s)");
            }
            StabilityEvent::Replenished => {
                log::debug!(
                    "Reconnect budget replenished after {STABLE_RESET:?} of stable streaming"
                );
            }
            StabilityEvent::None => {}
        }
    } else {
        reconnect.note_unstable();
    }
}

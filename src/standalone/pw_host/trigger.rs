// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! RT-safe trigger slot for causal dual-stream ordering (`capture -> playback`).
//!
//! When the playback stream is connected with `StreamFlags::TRIGGER`, PipeWire
//! does not schedule its `process` callback until `pw_stream_trigger_process`
//! is called explicitly. [`PlaybackTriggerSlot`] lets the capture RT callback
//! trigger playback immediately after publishing the DSP block to `DspBridge`,
//! while keeping stream ownership inside `AppState` on the control thread.

use pipewire as pw;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

/// Outcome of a single [`PlaybackTriggerSlot::trigger`] call on the RT thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerResult {
    /// Slot is currently disarmed (startup/teardown window); no FFI call made.
    Disarmed,
    /// `pw_stream_trigger_process` succeeded (`rc >= 0`).
    Triggered,
    /// `pw_stream_trigger_process` returned a negative errno code.
    Failed(i32),
}

/// Lock-free atomic pointer slot holding the active playback `pw_stream` handle.
///
/// Armed by the control thread after `AppState` is constructed (before
/// `thread_loop.start()`) and disarmed after `thread_loop.stop()` (before
/// `drop(app_state)`), so the RT capture callback never dereferences a freed
/// stream across reconnect cycles.
pub struct PlaybackTriggerSlot {
    stream_ptr: AtomicPtr<pw::sys::pw_stream>,
}

impl Default for PlaybackTriggerSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackTriggerSlot {
    /// Creates a new disarmed trigger slot.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            stream_ptr: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Arms the slot with the raw `pw_stream` handle of the active playback stream.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that:
    /// 1. `stream` is valid and its underlying C `pw_stream` allocation remains
    ///    live and unmoved for the entire armed window (between `thread_loop.start()`
    ///    and `thread_loop.stop()`).
    /// 2. [`Self::disarm`] is called after `thread_loop.stop()` — when no RT
    ///    callback can execute — and strictly before `drop(app_state)` destroys
    ///    the `pw_stream`.
    pub unsafe fn arm(&self, stream: &pw::stream::Stream) {
        self.stream_ptr
            .store(stream.as_raw_ptr(), Ordering::Release);
    }

    /// Disarms the slot so subsequent [`Self::trigger`] calls become no-ops.
    ///
    /// Must be called after `thread_loop.stop()` and before `drop(app_state)`.
    pub fn disarm(&self) {
        self.stream_ptr.store(ptr::null_mut(), Ordering::Release);
    }

    /// Triggers playback processing for the current PipeWire quantum.
    ///
    /// RT-safe: performs one `Acquire` load and, when armed, one non-blocking
    /// `pw_stream_trigger_process` call with zero allocation, zero lock, and
    /// zero logging.
    #[inline]
    pub fn trigger(&self) -> TriggerResult {
        let ptr = self.stream_ptr.load(Ordering::Acquire);
        if ptr.is_null() {
            return TriggerResult::Disarmed;
        }
        // SAFETY: `arm` guarantees `ptr` points to a valid C `pw_stream` owned
        // by `AppState` for the entire active loop window, and `disarm` clears
        // it after `thread_loop.stop()` before `drop(app_state)`. PipeWire
        // documents `pw_stream_trigger_process` as RT-safe when driving a
        // `PW_STREAM_FLAG_TRIGGER` stream within the same loop.
        let rc = unsafe { pw::sys::pw_stream_trigger_process(ptr) };
        if rc >= 0 {
            TriggerResult::Triggered
        } else {
            TriggerResult::Failed(rc)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_slot_starts_disarmed_without_panic() {
        let slot = PlaybackTriggerSlot::new();
        assert_eq!(slot.trigger(), TriggerResult::Disarmed);
    }

    #[test]
    fn disarm_keeps_slot_disarmed_and_idempotent() {
        let slot = PlaybackTriggerSlot::default();
        slot.disarm();
        assert_eq!(slot.trigger(), TriggerResult::Disarmed);
        slot.disarm();
        assert_eq!(slot.trigger(), TriggerResult::Disarmed);
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Off-RT ring for PipeWire log events raised inside the RT data thread.
//!
//! With the custom SPA log handler installed, PipeWire internals may log from
//! the RT data loop, and the synchronous `log::*` dispatch there reaches
//! `NamLogger` (mutex + formatting allocations). This module gives the marked
//! RT thread (`rt_setup::rt_log_mark`) an alternative: the log event is copied
//! — zero heap, zero lock, zero syscall — into a pre-allocated SPSC ring, and
//! an off-RT drainer thread then replays the records through the very same
//! `log::*` facade. `LogBuffer` capture, crash reports and diagnostics keep
//! seeing PW logs (the redirect is not removed — what is removed is the
//! synchronous `log::*` call on the RT thread).
//!
//! # Contracts
//!
//! - *Single producer*: in production the only live marked thread is the
//!   current instance's PW data thread. This exclusivity is structural, not
//!   enforced by the mark itself: there is a single arming call-site (the
//!   first `process()` quantum, `capture/setup.rs`), the TLS mark dies with
//!   its thread, and the bounded-reconnect cycle joins the previous
//!   instance's data thread (`thread_loop.stop()` in `pw_host::run`) before
//!   the next one arms — see `rt_log_mark`. The producer seat is `Sync`
//!   under that documented invariant. Note: arming is unconditional and
//!   re-asserting (no refusal of a second mark exists in code); concurrent
//!   writers are prevented by the thread lifecycle above, and test threads
//!   exercising the seat are serialized by a test-only lock.
//! - *Overflow policy*: with the ring full the event is DISCARDED and the
//!   count of lost events is kept in `pw_log_dropped()`, surfaced by the
//!   telemetry poll. The RT thread never blocks and never falls back to the
//!   synchronous path under overload.
//! - *Byte budget per event*: msg ≤ 256, topic ≤ 32, file ≤ 48; longer
//!   strings truncate. `file` rides along because the dispatch shape
//!   `[PipeWire] msg (file:line)` of ERROR/WARN/DEBUG/TRACE needs it (INFO
//!   omits it), and the topic text is copied at capture time so the drainer
//!   never dereferences foreign pointers.
//! - *Rollback at (re)initialization*: the `NAM_PW_LOG_SYNC` environment
//!   variable is consulted once by `install()` (cold path): when set to `"1"`
//!   or `"true"`, there is no ring and no drainer — behavior stays
//!   bit-identical to the pre-ring host. It is never re-read while the
//!   process runs; turning the ring off at runtime is `restore`, which
//!   flips the routing gate off immediately (cold `Release` store read by
//!   the hot path's `Acquire` load).
//!
//! The drainer is a daemon thread: it sleeps while the ring is empty and
//! drains in sweeps until the process itself ends (records queued in the
//! final teardown window are lost by design — diagnostics degrade
//! gracefully and it deliberately consults no process-global flag, so tests
//! flipping `SHUTDOWN` can never stall it).

#[cfg(test)]
use super::log_redirect::render_decoded_pipewire_log;
use super::log_redirect::{decode_with_file, decode_with_topic, emit_decoded_pipewire_log};
use pipewire::spa::sys::{spa_log_level, spa_log_topic};
use std::ffi::{c_char, c_int};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Rollback switch consulted by `install()` at (re)initialization: when set to
/// `"1"` or `"true"`, the synchronous path is kept (no ring, no drainer).
/// Switching a running ring off at runtime is `restore`, not this variable.
pub const SYNC_ROLLBACK_ENV: &str = "NAM_PW_LOG_SYNC";

/// Ring capacity in records (~352 B each → ~344 KiB pre-allocated at install).
const RT_RING_CAPACITY: usize = 1024;

/// Upper bound of the `file` field; longer paths truncate.
const FILE_CAP: usize = 48;

/// Upper bound of the `topic` field; longer names truncate.
const TOPIC_CAP: usize = 32;

/// Upper bound of the message body (msg ≤ 256).
const MSG_CAP: usize = 256;

/// Marker prefixed by the drainer to a record queued unformatted from the RT
/// thread (variadic arguments were intentionally not expanded).
pub(crate) const RT_UNFORMATTED_MARKER: &str = "[rt-unformatted]";

/// Compact captured log event transiting the SPSC ring.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RtLogRecord {
    /// Raw `spa_log_level` value as received (dispatch semantics preserved).
    level: u32,
    /// Source line as received.
    line: i32,
    /// Bytes of the decoded `file` field (valid UTF-8).
    file_len: u16,
    /// Bytes of the decoded topic text (valid UTF-8).
    topic_len: u16,
    /// Bytes of the message body (valid UTF-8).
    msg_len: u16,
    /// Copy of the file field (≤ [`FILE_CAP`], zero-padded tail).
    file: [u8; FILE_CAP],
    /// Copy of the topic text (≤ [`TOPIC_CAP`], zero-padded tail).
    topic: [u8; TOPIC_CAP],
    /// Copy of the message body (≤ [`MSG_CAP`], zero-padded tail). For an
    /// `rt_unformatted` record this is the raw format literal, not expanded.
    msg: [u8; MSG_CAP],
    /// `true` when the message came from the marked RT thread and must not be
    /// run through libc formatting; the drainer renders the marker.
    rt_unformatted: bool,
}

impl RtLogRecord {
    /// Copies the already-decoded strings into fixed fields — zero
    /// allocation. Sources come from the handler's `str` decoders, so the
    /// drain side only re-validates defensively.
    fn capture(
        level: spa_log_level,
        line: i32,
        file_str: &str,
        topic_str: &str,
        msg: &str,
    ) -> Self {
        let mut record = Self {
            level,
            line,
            file_len: 0,
            topic_len: 0,
            msg_len: 0,
            file: [0; FILE_CAP],
            topic: [0; TOPIC_CAP],
            msg: [0; MSG_CAP],
            rt_unformatted: false,
        };
        record.file_len = copy_bounded(&mut record.file, file_str);
        record.topic_len = copy_bounded(&mut record.topic, topic_str);
        record.msg_len = copy_bounded(&mut record.msg, msg);
        record
    }

    /// Captures a record whose message is the raw format literal from a marked
    /// RT thread. No libc formatter and no `strlen` are involved: the literal
    /// is copied with a bounded byte loop until the NUL terminator or the
    /// field capacity.
    ///
    /// # Safety
    ///
    /// `fmt`, when non-null, must point at a valid NUL-terminated C string for
    /// the duration of the call (PipeWire's SPA log contract).
    unsafe fn capture_unformatted(
        level: spa_log_level,
        line: i32,
        file_str: &str,
        topic_str: &str,
        fmt: *const c_char,
    ) -> Option<Self> {
        if fmt.is_null() {
            return None;
        }
        let mut record = Self {
            level,
            line,
            file_len: 0,
            topic_len: 0,
            msg_len: 0,
            file: [0; FILE_CAP],
            topic: [0; TOPIC_CAP],
            msg: [0; MSG_CAP],
            rt_unformatted: true,
        };
        record.file_len = copy_bounded(&mut record.file, file_str);
        record.topic_len = copy_bounded(&mut record.topic, topic_str);
        // SAFETY: documented precondition (valid NUL-terminated string).
        record.msg_len = unsafe { copy_cstr_bounded(fmt, &mut record.msg) };
        Some(record)
    }

    fn file_str(&self) -> &str {
        let end = self.file_len.min(FILE_CAP as u16) as usize;
        std::str::from_utf8(&self.file[..end]).unwrap_or("<invalid>")
    }

    fn topic_str(&self) -> &str {
        let end = self.topic_len.min(TOPIC_CAP as u16) as usize;
        std::str::from_utf8(&self.topic[..end]).unwrap_or("")
    }

    fn msg_str(&self) -> &str {
        let end = self.msg_len.min(MSG_CAP as u16) as usize;
        std::str::from_utf8(&self.msg[..end]).unwrap_or("<non-utf8>")
    }
}

/// Copies a NUL-terminated C string into a fixed field without `strlen` or any
/// libc call, truncating at the capacity. Returns the stored length.
///
/// # Safety
///
/// `src` must be non-null and point at a valid NUL-terminated byte string.
#[inline]
unsafe fn copy_cstr_bounded(src: *const c_char, dst: &mut [u8]) -> u16 {
    let mut n = 0usize;
    while n < dst.len() {
        // SAFETY: the caller guarantees a valid NUL-terminated string; the
        // loop stops at the terminator before reading past it.
        let byte = unsafe { *src.cast::<u8>().add(n) };
        if byte == 0 {
            break;
        }
        dst[n] = byte;
        n += 1;
    }
    n as u16
}

/// Copies `src` bytes into a fixed field, truncating; returns the stored
/// length. Zero allocation, no locks.
#[inline]
fn copy_bounded(dst: &mut [u8], src: &str) -> u16 {
    let src_bytes = src.as_bytes();
    let n = src_bytes.len().min(dst.len());
    dst[..n].copy_from_slice(&src_bytes[..n]);
    n as u16
}

/// Producer seat. rtrb 0.4 requires `&mut self` for `push`, so interior
/// mutability carries the single-writer contract instead of moving the
/// producer around.
struct RtProducer(std::cell::UnsafeCell<rtrb::Producer<RtLogRecord>>);
// SAFETY: rtrb's `push(&mut self)` requires a single writer by design; the
// only writer is the marked RT data thread of the current host instance.
// This exclusivity is structural and lifecycle-driven, not a runtime refusal
// in `rt_log_mark` (arming is unconditional by design): a single arming
// call-site (the first `process()` quantum), the mark dying with its thread,
// and the bounded-reconnect cycle joining the previous instance's data
// thread before the next one arms — so two concurrent writers to this seat
// never coexist (`ThreadLoopBox` creation/teardown in `pw_host::run`).
// Test threads that exercise the real consult path are serialized by a
// test-only lock. `Sync` exists only so the seat can live in the
// install-time `OnceLock` consulted exclusively by marked threads.
unsafe impl Sync for RtProducer {}

impl RtProducer {
    /// Queues a record: `true` when queued, `false` when the ring is full
    /// (the preallocated capacity is never extended, by design).
    #[inline]
    fn push(&self, record: RtLogRecord) -> bool {
        // SAFETY: single-writer invariant documented on the type (sole
        // marked RT data thread; reconnect joins before the next mark). The
        // drainer holds the paired consumer half, never this producer.
        let producer = unsafe { &mut *self.0.get() };
        producer.push(record).is_ok()
    }
}

/// Producer handle, published before the routing mode is enabled.
static RT_PRODUCER: OnceLock<RtProducer> = OnceLock::new();

/// Hot-path routing gate (`true` offloads the marked RT thread). Written
/// only by the install/restore cold paths (`Release`); read per event by the
/// hot path (`Acquire`).
static RING_MODE: AtomicBool = AtomicBool::new(false);

/// Install latch so repeated `init_pipewire_logging` cycles never spawn a
/// second drainer; re-install only re-arms the routing mode.
static RT_RING_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Cumulative count of PW log events discarded with a full ring
/// (plan identifier: `pw_log_dropped`; surfaced by the telemetry poll).
static PW_LOG_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Hot decision point for an intercepted PW log event.
///
/// Returns `true` when the event is fully consumed by the ring path — queued
/// (producer accepted) or discarded-and-counted (ring full). `false`
/// instructs the caller to keep the legacy synchronous dispatch; only an
/// unmarked thread, a rollback install or a pre-install window reaches that
/// branch. Zero heap, zero lock, zero syscall on every path.
#[inline]
pub(crate) fn try_offload(
    level: spa_log_level,
    topic: *const spa_log_topic,
    file: *const c_char,
    line: c_int,
    msg: &str,
) -> bool {
    // Hot consult: one atomic load, one TLS load, one branch.
    if !RING_MODE.load(Ordering::Acquire) {
        return false;
    }
    if !crate::standalone::rt_setup::rt_log_mark::rt_is_current_thread_marked() {
        return false;
    }
    let Some(producer) = RT_PRODUCER.get() else {
        return false;
    };
    // Decode at capture time (this context), inside scoped borrows: same
    // pointer-deref pattern as the synchronous dispatch; after the copy the
    // drainer never dereferences foreign pointers. Zero allocation.
    // SAFETY: `topic`/`file` are NUL-terminated C strings supplied by PipeWire for
    // the duration of this capture; the decode helpers read them inside scoped
    // borrows only.
    unsafe {
        decode_with_topic(topic, |topic_str| {
            decode_with_file(file, |file_str| {
                let record = RtLogRecord::capture(level, line, file_str, topic_str, msg);
                push_or_count(producer, record);
                // The event is consumed on this path in either outcome
                // (queued or discarded-and-counted): the marked thread never
                // falls back to the synchronous dispatch, not even under
                // ring saturation.
                true
            })
        })
    }
}

/// Hot path for a PW log event raised on the marked RT thread.
///
/// Unlike [`try_offload`], this never runs a variadic formatter: the raw format
/// literal is copied with a bounded byte loop and queued with the
/// `rt_unformatted` flag, so the RT thread makes zero libc formatting calls and
/// zero allocations. Variadic arguments are dropped by design; the drainer adds
/// [`RT_UNFORMATTED_MARKER`] so the loss is visible in diagnostics.
///
/// Returns `true` when the event was consumed by the ring (queued or
/// discarded-and-counted). `false` means the caller must take its normal path
/// (unmarked thread, rollback install, or ring not installed).
///
/// # Safety
///
/// `topic`/`file` follow the SPA decode contracts; `fmt` must be null or point
/// at a valid NUL-terminated C format string for the duration of the call.
#[inline]
pub(crate) unsafe fn try_offload_unformatted(
    level: spa_log_level,
    topic: *const spa_log_topic,
    file: *const c_char,
    line: c_int,
    fmt: *const c_char,
) -> bool {
    if !RING_MODE.load(Ordering::Acquire) {
        return false;
    }
    if !crate::standalone::rt_setup::rt_log_mark::rt_is_current_thread_marked() {
        return false;
    }
    let Some(producer) = RT_PRODUCER.get() else {
        return false;
    };
    // SAFETY: the decode helpers' contracts are forwarded from this function's
    // safety section; the copies happen inside their scoped closures.
    unsafe {
        decode_with_topic(topic, |topic_str| {
            decode_with_file(file, |file_str| {
                let Some(record) =
                    RtLogRecord::capture_unformatted(level, line, file_str, topic_str, fmt)
                else {
                    return false;
                };
                push_or_count(producer, record);
                true
            })
        })
    }
}

/// Pushes the record or — with a full ring — discards it and counts the
/// loss. Either way the marked RT thread stays lock-free, allocation-free
/// and syscall-free.
#[inline]
fn push_or_count(producer: &RtProducer, record: RtLogRecord) {
    if !producer.push(record) {
        // RT deadline first: discard + count, never fall back.
        PW_LOG_DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Test-only reset of the cumulative drop counter (exactness assertions).
#[cfg(test)]
pub(crate) fn reset_pw_log_dropped_for_test() {
    PW_LOG_DROPPED.store(0, Ordering::Relaxed);
}

/// Test-only increment of the cumulative drop counter (telemetry latch).
#[cfg(test)]
pub(crate) fn bump_pw_log_dropped_for_test(delta: u64) {
    PW_LOG_DROPPED.fetch_add(delta, Ordering::Relaxed);
}

/// Test-only counter of total drained records (event-driven test synchronization).
#[cfg(test)]
pub(crate) static DRAINED_RECORDS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Test-only snapshot of total drained records.
#[cfg(test)]
pub(crate) fn drained_records_for_test() -> u64 {
    DRAINED_RECORDS.load(Ordering::Acquire)
}

/// Cumulative count of PW log events discarded with a full ring
/// (telemetry-visible via `poll_rt_status`).
#[must_use]
pub fn pw_log_dropped() -> u64 {
    PW_LOG_DROPPED.load(Ordering::Relaxed)
}

/// Whether the ring routing is currently active (install/rollback state).
#[must_use]
pub fn is_ring_routing_active() -> bool {
    RING_MODE.load(Ordering::Acquire)
}

/// Cold-path install for the PW log ring; idempotent across re-init cycles.
///
/// Fail-closed: a failed drainer spawn keeps the synchronous path for every
/// event (the ring is never enabled without a consumer).
pub fn install() {
    if matches!(
        std::env::var(SYNC_ROLLBACK_ENV)
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1" | "true")
    ) {
        log::debug!(
            "[PW log] synchronous rollback requested via {SYNC_ROLLBACK_ENV} — ring not installed"
        );
        return;
    }
    if !RT_RING_INSTALLED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        // Re-init cycle: ring/drainer already exist (restore only flipped
        // the routing mode off); re-arm and return.
        RING_MODE.store(true, Ordering::Release);
        return;
    }

    let (producer, consumer) = rtrb::RingBuffer::<RtLogRecord>::new(RT_RING_CAPACITY);
    let drainer = std::thread::Builder::new()
        .name("nam-pw-log-drain".to_owned())
        .spawn(move || drain_loop(consumer));
    match drainer {
        Err(error) => {
            // Fail closed: no consumer → keep today's synchronous behavior.
            RT_RING_INSTALLED.store(false, Ordering::Release);
            RING_MODE.store(false, Ordering::Release);
            log::error!(
                "[PW log] ring install failed to spawn drainer ({error}) — synchronous path kept"
            );
            return;
        }
        Ok(handle) => handle,
    };
    if RT_PRODUCER
        .set(RtProducer(std::cell::UnsafeCell::new(producer)))
        .is_err()
    {
        // Unreachable under the install latch; fail closed if it happened.
        RT_RING_INSTALLED.store(false, Ordering::Release);
        RING_MODE.store(false, Ordering::Release);
        log::error!("[PW log] ring producer already set — synchronous path kept");
        return;
    }
    // Ordering: the producer is published under `OnceLock`'s Release fence
    // before the routing mode is enabled, so the first hot consult that
    // observes `RING_MODE == true` also observes the producer.
    RING_MODE.store(true, Ordering::Release);
    log::info!(
        "[PW log] RT log ring installed (capacity={RT_RING_CAPACITY}); the marked data thread \
         queues PW events off-RT (rollback: set {SYNC_ROLLBACK_ENV})"
    );
}

/// Deactivates the routing gate (called by `restore_pipewire_logging`).
///
/// The drainer keeps draining whatever is queued and stays parked: a later
/// `install()` re-arms the mode without respawning anything.
pub fn restore_deactivate() {
    RING_MODE.store(false, Ordering::Release);
}

/// Full sweeps until the process ends; idle parks between sweeps keep this
/// thread off anyone's critical path.
fn drain_loop(mut consumer: rtrb::Consumer<RtLogRecord>) {
    loop {
        while let Ok(record) = consumer.pop() {
            // A panic must never kill the drainer (it would turn every later
            // PW log event into a silent drop+count): contain it per record —
            // same rationale as `run_rt_callback_body`'s unwind guard — and
            // keep draining.
            let contained = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                emit_rt_record(&record);
            }));
            if contained.is_err() {
                log::warn!(
                    "[PW log] drain of one ring record panicked and was contained \
                     (drainer alive; diagnostic loss limited to that record)"
                );
            }
        }
        // Diagnostics-only latency: a fixed 10 ms park suffices between
        // sweeps and costs nothing on the RT side. The daemon exits only
        // with the process itself (records queued in the final teardown
        // window are lost — documented teardown behavior), which also
        // insulates it from any test code flipping process-global flags.
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Formats a queued ring record into its target log level and message shape.
/// Message body the drainer dispatches for a record. Unformatted records are
/// prefixed off-RT so the dropped variadic arguments are visible.
fn dispatch_message(record: &RtLogRecord) -> std::borrow::Cow<'_, str> {
    if record.rt_unformatted {
        std::borrow::Cow::Owned(format!("{} {}", RT_UNFORMATTED_MARKER, record.msg_str()))
    } else {
        std::borrow::Cow::Borrowed(record.msg_str())
    }
}

#[cfg(test)]
pub(crate) fn format_rt_record(record: &RtLogRecord) -> (log::Level, String) {
    render_decoded_pipewire_log(
        record.level,
        record.topic_str(),
        record.file_str(),
        record.line,
        &dispatch_message(record),
    )
}

/// Drain-side replay: record → the shared dispatch shape of `log_redirect`,
/// so sync and ring paths can never drift apart. Operates purely on copies
/// of already-decoded strings; no foreign pointers.
fn emit_rt_record(record: &RtLogRecord) {
    emit_decoded_pipewire_log(
        record.level,
        record.topic_str(),
        record.file_str(),
        record.line,
        &dispatch_message(record),
    );
    #[cfg(test)]
    DRAINED_RECORDS.fetch_add(1, Ordering::Release);
}

#[cfg(test)]
#[path = "rt_log_ring_test.rs"]
mod tests;

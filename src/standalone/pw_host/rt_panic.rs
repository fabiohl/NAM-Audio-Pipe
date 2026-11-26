// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Defers the heavy part of the panic hook when the panic originates on the
//! marked RT data thread.
//!
//! The engine's panic hook renders a full diagnostic report and touches the
//! filesystem. Running that on a `SCHED_FIFO` audio thread can blow the
//! deadline and delay the ordered teardown. This wrapper installs on top of
//! the engine hook and, when the panic comes from the RT-marked thread, copies
//! only the payload text into a pre-allocated static buffer and returns. The
//! control loop observes the existing `RT_STATUS_PANIC_CAPTURED` latch and calls
//! [`flush_deferred_panic`] off-RT, which renders and persists the diagnostic
//! bundle there.
//!
//! Non-RT panics keep the engine hook verbatim.

use crate::standalone::rt_setup::rt_log_mark::rt_is_current_thread_marked;
use std::cell::UnsafeCell;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

/// Maximum deferred payload text retained (bytes).
const DEFERRED_CAP: usize = 1024;

/// Maximum deferred RT crash reports retained alongside the engine's pruning
/// policy. Pruning runs off-RT after a successful write.
const MAX_DEFERRED_REPORTS: usize = 10;

/// Slot lifecycle: only the producer owns `buf`/`len` in `WRITING`, only the
/// consumer owns them in `CONSUMING`, and neither touches them in `EMPTY`.
const EMPTY: u8 = 0;
const WRITING: u8 = 1;
const READY: u8 = 2;
const CONSUMING: u8 = 3;

struct DeferredSlot {
    buf: UnsafeCell<[u8; DEFERRED_CAP]>,
    len: AtomicUsize,
    state: AtomicU8,
}

// SAFETY: exclusive access to `buf`/`len` is enforced locally by the `state`
// machine: the `EMPTY -> WRITING` CAS makes the first producer the only writer;
// `WRITING -> READY` (Release) publishes the payload; `READY -> CONSUMING`
// (Acquire) makes the first consumer the only reader; `CONSUMING -> EMPTY`
// (Release) returns ownership. A producer that finds `CONSUMING` and a consumer
// that finds `WRITING` both bail out without touching the buffer, so no data
// race is possible even under reentrancy or a concurrent reader.
unsafe impl Sync for DeferredSlot {}

static DEFERRED: DeferredSlot = DeferredSlot {
    buf: UnsafeCell::new([0u8; DEFERRED_CAP]),
    len: AtomicUsize::new(0),
    state: AtomicU8::new(EMPTY),
};

/// Copies `payload` into the deferred buffer. Returns `false` when another RT
/// panic already armed the slot (the first panic is the diagnostically useful
/// one; later contained panics are already suppressed by the RT latch).
fn defer_payload(payload: &str) -> bool {
    if DEFERRED
        .state
        .compare_exchange(EMPTY, WRITING, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return false;
    }
    let bytes = payload.as_bytes();
    let n = bytes.len().min(DEFERRED_CAP);
    // SAFETY: the `EMPTY -> WRITING` CAS above gives this call the unique writer
    // role; no consumer can enter `CONSUMING` until `READY` is published.
    let dst = unsafe { &mut *DEFERRED.buf.get() };
    dst[..n].copy_from_slice(&bytes[..n]);
    DEFERRED.len.store(n, Ordering::Relaxed);
    DEFERRED.state.store(READY, Ordering::Release);
    true
}

/// Takes and clears the deferred payload text. Off-RT only.
pub(crate) fn take_deferred_panic_text() -> Option<String> {
    if DEFERRED
        .state
        .compare_exchange(READY, CONSUMING, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return None;
    }
    let n = DEFERRED.len.load(Ordering::Acquire).min(DEFERRED_CAP);
    // SAFETY: the `READY -> CONSUMING` CAS gives this call the unique reader
    // role; the producer cannot re-enter `WRITING` until `EMPTY` is published.
    let bytes = unsafe { &*DEFERRED.buf.get() };
    let text = String::from_utf8_lossy(&bytes[..n]).into_owned();
    // Publish `EMPTY` only after the copy completed, so the producer can never
    // overwrite `buf` while it is being read.
    DEFERRED.state.store(EMPTY, Ordering::Release);
    Some(text)
}

/// Whether a deferred RT panic payload is waiting to be flushed.
#[cfg(test)]
#[must_use]
pub(crate) fn deferred_panic_pending() -> bool {
    DEFERRED.state.load(Ordering::Acquire) != EMPTY
}

/// Routes one panic: on the marked RT thread only the payload is copied into
/// the pre-allocated slot; elsewhere the heavy hook runs immediately.
fn route_panic(is_rt_thread: bool, payload: &str, heavy: &mut dyn FnMut()) {
    if is_rt_thread {
        defer_payload(payload);
    } else {
        heavy();
    }
}

/// Installs the deferring wrapper over the currently active panic hook.
///
/// Call once from `main`, after the engine's `install_panic_hook`.
pub fn install_deferred_rt_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload_as_str().unwrap_or("Box<dyn Any>");
        let is_rt_thread = rt_is_current_thread_marked();
        route_panic(is_rt_thread, payload, &mut || prev(info));
    }));
}

/// Flushes a deferred RT panic off-RT: logs the captured text and writes the
/// diagnostic bundle (the heavy work that must not run on the audio thread).
///
/// Returns `true` when a deferred panic was present and consumed.
pub(crate) fn flush_deferred_panic() -> bool {
    let Some(payload) = take_deferred_panic_text() else {
        return false;
    };
    log::error!("Panic captured on the RT data thread (deferred off-RT): {payload}");

    let bundle = neural_amp_modeler_rs::DiagnosticBundle::capture().render();
    if let Some(dir) = neural_amp_modeler_rs::common::panic_hook::crash_directory() {
        let report = format!("Deferred RT panic: {payload}\n\n{bundle}");
        if let Err(error) = write_secure_report(&dir, &report) {
            log::warn!("Could not write deferred RT crash report: {error}");
        }
    }
    true
}

/// Writes the deferred report with the same security and atomicity contract as
/// the engine hook: private directory, private temporary file, `sync_all`, then
/// rename. Runs off-RT only.
fn write_secure_report(dir: &Path, report: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, Permissions::from_mode(0o700))?;
    }

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let final_path = dir.join(format!("crash-rt-{}-{stamp}.txt", std::process::id()));
    let tmp_path = dir.join(format!("crash-rt-{}-{stamp}.tmp", std::process::id()));

    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp_path)?;
    let write_result = file
        .write_all(report.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error);
    }
    std::fs::rename(&tmp_path, &final_path)?;
    prune_old_reports(dir);
    Ok(final_path)
}

/// Keeps the deferred RT crash directory bounded. Best-effort: any read/remove
/// error is ignored because this runs during crash teardown.
fn prune_old_reports(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut reports: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            if !name.starts_with("crash-rt-") || !name.ends_with(".txt") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    if reports.len() <= MAX_DEFERRED_REPORTS {
        return;
    }
    reports.sort_by_key(|(modified, _)| *modified);
    let excess = reports.len() - MAX_DEFERRED_REPORTS;
    for (_, path) in reports.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn reset() {
        DEFERRED.state.store(EMPTY, Ordering::Release);
        DEFERRED.len.store(0, Ordering::Relaxed);
    }

    #[test]
    fn rt_panic_defers_heavy_hook_until_off_rt_flush() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        let mut heavy_calls = 0u32;
        route_panic(true, "rt boom", &mut || heavy_calls += 1);
        assert_eq!(heavy_calls, 0, "heavy hook must not run on the RT thread");
        assert!(deferred_panic_pending());
        let text = take_deferred_panic_text().expect("deferred payload must be present");
        assert!(text.contains("rt boom"));
        assert!(!deferred_panic_pending(), "flush must clear the latch");
    }

    #[test]
    fn off_rt_panic_keeps_the_engine_hook_path() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        let mut heavy_calls = 0u32;
        route_panic(false, "cold boom", &mut || heavy_calls += 1);
        assert_eq!(heavy_calls, 1, "off-RT panics keep the heavy hook");
        assert!(!deferred_panic_pending());
    }

    #[test]
    fn secure_report_is_private_atomic_and_pruned() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "nam-rt-panic-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        for i in 0..(MAX_DEFERRED_REPORTS + 3) {
            let path =
                write_secure_report(&dir, &format!("report {i}")).expect("secure report write");
            assert!(path.exists());
        }
        let reports = std::fs::read_dir(&dir)
            .expect("read reports")
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("crash-rt-") && name.ends_with(".txt"))
            })
            .count();
        assert_eq!(reports, MAX_DEFERRED_REPORTS);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = std::fs::metadata(&dir)
                .expect("dir metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
            let first = std::fs::read_dir(&dir)
                .expect("read reports")
                .flatten()
                .next()
                .expect("one report");
            let file_mode = first
                .metadata()
                .expect("file metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(file_mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_rt_panic_wins_and_payload_truncates() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        let long = "x".repeat(DEFERRED_CAP + 64);
        assert!(defer_payload(&long));
        assert!(!defer_payload("second"));
        let text = take_deferred_panic_text().expect("payload present");
        assert_eq!(text.len(), DEFERRED_CAP);
    }

    #[test]
    fn state_machine_blocks_torn_access() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        DEFERRED.state.store(WRITING, Ordering::Release);
        assert!(
            take_deferred_panic_text().is_none(),
            "consumer must not read while the producer is writing"
        );
        DEFERRED.state.store(CONSUMING, Ordering::Release);
        assert!(
            !defer_payload("late panic"),
            "producer must not overwrite while the consumer is reading"
        );
        reset();
    }

    #[test]
    fn concurrent_defer_and_take_never_observe_a_torn_payload() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        const ROUNDS: usize = 1_000;
        let producer = std::thread::spawn(move || {
            let payload = "A".repeat(DEFERRED_CAP);
            let mut acked = 0usize;
            while acked < ROUNDS {
                if defer_payload(&payload) {
                    acked += 1;
                }
                std::hint::spin_loop();
            }
            acked
        });
        let consumer = std::thread::spawn(move || {
            let mut got = 0usize;
            while got < ROUNDS {
                if let Some(text) = take_deferred_panic_text() {
                    assert_eq!(text.len(), DEFERRED_CAP, "payload must not be torn");
                    assert!(
                        text.bytes().all(|byte| byte == b'A'),
                        "consumer observed a partially written payload"
                    );
                    got += 1;
                }
                std::hint::spin_loop();
            }
            got
        });
        assert_eq!(producer.join().expect("producer"), ROUNDS);
        assert_eq!(consumer.join().expect("consumer"), ROUNDS);
        assert!(!deferred_panic_pending());
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PM QoS and hardware audio detection.
//!
//! Functions to lock deep CPU C-States and detect
//! the system's default hardware sink via PipeWire.

/// Dynamically detects the system's default hardware sink via `pw-metadata`.
///
/// This function attempts to identify which physical device audio should be sent to
/// by default. It parses the output of the PipeWire `pw-metadata` utility.
///
/// A watchdog deadline of 500 ms prevents hanging if the PipeWire daemon
/// or `pw-metadata` is unresponsive; the timeout path terminates the probe
/// through the owned [`std::process::Child`] handle (immune to PID recycling).
/// stdout is consumed concurrently with the wait (`poll(2)` deadline slices)
/// and captured up to the [`STDOUT_CAPTURE_CAP`] byte ceiling; output beyond
/// the ceiling is truncated with a logged record so a chatty child can never
/// deadlock the pipe or blow the parse window.
///
/// Returns `Some(name)` if a valid sink that is not NAM-Audio-Pipe itself is found,
/// or `None` otherwise (allowing routing to be decided by WirePlumber).
pub fn detect_hardware_sink() -> Option<String> {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

    let child = match std::process::Command::new("pw-metadata")
        .args(["-n", "default", "0", "default.audio.sink"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            log::warn!(
                "[E2113 | HW_SINK_PROBE_FAILED] detect_hardware_sink: Failed to spawn pw-metadata ({e}) — \
                 skipping default sink detection (WirePlumber will decide routing)."
            );
            return None;
        }
    };

    let output = collect_child_output_with_watchdog(child, TIMEOUT)?;
    parse_sink_name_from_metadata(&output.stdout)
}

/// Byte ceiling for the captured `pw-metadata` stdout.
///
/// Real metadata probes emit a few kilobytes; the ceiling bounds memory and
/// keeps the parse window finite. Output beyond the ceiling keeps being
/// drained (never stored) so the child can finish writing and exit instead of
/// deadlocking on a full pipe — the truncation is logged, never silent.
const STDOUT_CAPTURE_CAP: usize = 16 * 1024;

/// Bounded grace to drain the stdout tail after the child exited.
///
/// A grandchild that inherited the write end keeps the pipe open (no EOF)
/// indefinitely; the drain honors data already buffered in the kernel but
/// never waits for such a late writer.
const STDOUT_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(50);

/// Per-loop poll slice for stdout readiness — matches the child `try_wait`
/// cadence so neither pipe data nor exit status starves the other side.
const STDOUT_POLL_SLICE: std::time::Duration = std::time::Duration::from_millis(5);

/// Bounded non-blocking stdout capture consumed concurrently with the child
/// wait. Reads happen only under [`libc::poll`] readiness on an `O_NONBLOCK`
/// descriptor (a slice never blocks past its deadline) and are clamped to
/// [`STDOUT_CAPTURE_CAP`]; bytes beyond the ceiling are drained and dropped
/// with [`Self::truncated`] set.
struct StdoutCapture {
    out: Option<std::process::ChildStdout>,
    fd: Option<i32>,
    bytes: Vec<u8>,
    truncated: bool,
    eof: bool,
    scratch: [u8; 4096],
}

impl StdoutCapture {
    /// Takes the child's stdout and forces `O_NONBLOCK` on its descriptor.
    ///
    /// The invariant `fd.is_some() ⇔ out.is_some()` holds for the whole
    /// capture lifetime (both fields are set in one place, neither is ever
    /// cleared) — every internal `unwrap` relies on it. A `fcntl` failure
    /// (unreachable on the Linux targets of this host daemon) is surfaced as a
    /// log record only: `poll`-gated readiness still bounds every read slice.
    fn take_from(child: &mut std::process::Child) -> Self {
        let out = child.stdout.take();
        let fd = out.as_ref().map(std::os::fd::AsRawFd::as_raw_fd);
        if let Some(fd) = fd {
            // SAFETY: `fd` is a live descriptor owned by `out`, kept alive for
            // the whole capture lifetime; both fcntl calls are single-argument
            // descriptor operations with no side channels.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags >= 0 {
                unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
            } else {
                log::warn!(
                    "[E2113 | HW_SINK_PROBE_FAILED] detect_hardware_sink: fcntl(O_NONBLOCK) on the \
                     pw-metadata stdout failed ({}) — capture proceeds with poll-gated reads bound \
                     per slice.",
                    std::io::Error::last_os_error()
                );
            }
        }
        Self {
            out,
            fd,
            bytes: Vec::with_capacity(STDOUT_CAPTURE_CAP.min(4096)),
            truncated: false,
            eof: false,
            scratch: [0u8; 4096],
        }
    }

    /// Consumes whatever becomes readable on the pipe until `until`, honoring
    /// the capture ceiling. Returns when the pipe end hits EOF, would block,
    /// errors, or `until` arrives.
    fn absorb(&mut self, until: std::time::Instant) {
        if self.out.is_none() || self.fd.is_none() {
            self.eof = true;
            return;
        }
        let fd = self.fd.unwrap();
        while !self.eof && std::time::Instant::now() < until {
            if !self.poll_ready(fd, until) {
                return;
            }
            // Non-blocking read: an empty pipe yields `WouldBlock` (loop back
            // to the poll), EOF yields `Ok(0)`.
            let read_result = std::io::Read::read(self.out.as_mut().unwrap(), &mut self.scratch);
            match read_result {
                Ok(0) => self.eof = true,
                Ok(n) => {
                    let take = n.min(STDOUT_CAPTURE_CAP.saturating_sub(self.bytes.len()));
                    self.bytes.extend_from_slice(&self.scratch[..take]);
                    if take < n {
                        self.truncated = true;
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    // Any other read error (malformed descriptor): keep the
                    // partial capture with the failure visible in the log.
                    self.eof = true;
                    self.truncated = true;
                    log::warn!(
                        "[E2115 | HW_SINK_PROBE_TRUNCATED] pw-metadata stdout capture failed ({e}) — \
                         the partial tail is preserved."
                    );
                }
            }
        }
    }

    /// Drains the buffered tail after the child exited, bounded by
    /// [`STDOUT_DRAIN_GRACE`]: pipe data already written survives the writer,
    /// while a grandchild inheriting the write end can delay EOF indefinitely
    /// and must never hold the probe past its deadline.
    fn drain_after_exit(&mut self) {
        if self.out.is_none() || self.eof {
            return;
        }
        self.absorb(std::time::Instant::now() + STDOUT_DRAIN_GRACE);
        if !self.eof {
            self.truncated = true;
            log::warn!(
                "[E2115 | HW_SINK_PROBE_TRUNCATED] pw-metadata stdout stayed open after child \
                 exit (a grandchild likely inherited the fd) — capture truncated at {} of \
                 unbounded bytes, probe continuing on the partial tail.",
                self.bytes.len()
            );
        }
    }

    /// Waits up to `until` for pipe readiness (data available or writer
    /// closed). `O_NONBLOCK` guarantees the subsequent read is a slice-bound
    /// operation.
    fn poll_ready(&self, fd: i32, until: std::time::Instant) -> bool {
        let now = std::time::Instant::now();
        if now >= until {
            return false;
        }
        let mut fds = [libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        }];
        let slice_ms = until
            .saturating_duration_since(now)
            .as_millis()
            .try_into()
            .unwrap_or(libc::c_int::MAX);
        // SAFETY: `fds` is a valid single-element pollfd array for the call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, slice_ms) };
        if ready <= 0 {
            // Timeout (0) or spurious error (EINTR/-1): the caller re-polls.
            return false;
        }
        fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
    }
}

/// Waits up to `timeout` for `child` to exit while concurrently consuming its
/// stdout under a bounded capture.
///
/// The [`std::process::Child`] handle stays owned by the calling thread for the
/// whole wait, so the watchdog terminates the probe via
/// [`std::process::Child::kill`] — which targets the kernel's handle for the
/// spawned process and can therefore never signal a recycled PID. This closes
/// the race of killing a raw `libc::pid_t` with `libc::kill(pid, SIGKILL)`
/// after a joiner thread has already reaped the child (where the PID could have
/// been recycled in between).
///
/// Deadlocks the old synchronous `read_to_end` could hit are closed by design:
/// a child emitting more than `STDOUT_CAPTURE_CAP` bytes keeps being drained
/// (excess truncated with an `E2115 | HW_SINK_PROBE_TRUNCATED` record) so it
/// can exit inside the deadline, and a grandchild inheriting the stdout fd only
/// bounds the tail drain (`STDOUT_DRAIN_GRACE`) — the probe returns with the
/// partial capture instead of hanging on a writer that never closes.
///
/// Returns `None` when the child fails to exit within `timeout` (it is killed
/// via the handle and reaped within a bounded grace window — never blocking
/// past the watchdog deadline) or when its status cannot be obtained.
pub(crate) fn collect_child_output_with_watchdog(
    mut child: std::process::Child,
    timeout: std::time::Duration,
) -> Option<std::process::Output> {
    let deadline = std::time::Instant::now() + timeout;
    let mut capture = StdoutCapture::take_from(&mut child);

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                capture.drain_after_exit();
                break status;
            }
            Ok(None) if std::time::Instant::now() >= deadline => {
                // `kill`/`wait` operate on the owned Child handle, immune to PID
                // recycling. SIGKILL normally terminates immediately, but a
                // child stuck in an uninterruptible D-state cannot be reaped
                // until it wakes — bound the reap so the 500 ms watchdog
                // deadline is preserved even then; the kernel reaps the
                // abandoned child once it leaves D-state.
                let _ = child.kill();
                let reap_deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(500);
                loop {
                    if matches!(child.try_wait(), Ok(Some(_)))
                        || std::time::Instant::now() >= reap_deadline
                    {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                log::warn!(
                    "[E2114 | HW_SINK_PROBE_TIMEOUT] detect_hardware_sink: pw-metadata did not respond within {}ms — \
                     skipping default sink detection (WirePlumber will decide routing).",
                    timeout.as_millis()
                );
                return None;
            }
            Ok(None) => {
                capture.absorb(std::cmp::min(
                    deadline,
                    std::time::Instant::now() + STDOUT_POLL_SLICE,
                ));
                if capture.eof {
                    // Nothing left on the pipe: keep the 5 ms exit-poll
                    // cadence of the previous design.
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
            Err(e) => {
                log::warn!(
                    "[E2113 | HW_SINK_PROBE_FAILED] detect_hardware_sink: Failed to query pw-metadata child status ({e}) — \
                     skipping default sink detection (WirePlumber will decide routing)."
                );
                return None;
            }
        }
    };

    // Ceiling-truncation record: keep the documented contract "truncated with a
    // logged record" honest for the over-cap/clean-EOF path too — the other
    // truncation causes already record in `StdoutCapture`.
    if capture.truncated {
        log::warn!(
            "[E2115 | HW_SINK_PROBE_TRUNCATED] pw-metadata stdout exceeded the {}-byte ceiling — \
             capture truncated at the cap; the parse window works on the capped prefix.",
            STDOUT_CAPTURE_CAP
        );
    }

    Some(std::process::Output {
        status,
        stdout: std::mem::take(&mut capture.bytes),
        stderr: Vec::new(),
    })
}

/// Parses the default sink name from `pw-metadata` raw output.
///
/// The `name` value is scanned as a JSON string literal: backslash escapes are
/// honored both for delimiter scanning and for the returned value (a quoted
/// sink name is read in full, `\"` unescapes to `"`), instead of being cut at
/// the first escaped quote.
pub(crate) fn parse_sink_name_from_metadata(raw_stdout: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(raw_stdout);
    let start = s.find("\"name\":\"")?;
    let rest = &s[start + 8..];
    let mut name = String::new();
    let mut terminated = false;
    let mut chars = rest.chars();
    while let Some(ch) = chars.next() {
        match ch {
            // Escape pair: the escaped character stays inside the value; a
            // trailing lone backslash never closes the string. `\uXXXX` falls
            // back to pushing the `u` (tolerance mode — the delimitation
            // contract only requires honoring `\"` and `\\`).
            '\\' => {
                let escaped = chars.next()?;
                name.push(escaped)
            }
            '"' => {
                terminated = true;
                break;
            }
            _ => name.push(ch),
        }
    }
    if !terminated {
        // Unterminated string value (truncated capture mid-name or malformed
        // output): reject instead of surfacing a half-parsed artifact.
        return None;
    }

    if name == crate::standalone::pw_host::identity::PW_CAPTURE_NODE_NAME {
        None
    } else {
        Some(name)
    }
}

/// Prevents the processor from entering power-saving C-States,
/// guaranteeing 0ms wake-up latency for RT audio processing.
///
/// Delegates to the engine's `rt_hardening::request_cpu_dma_latency(0)`.
/// The engine guard is released at return (fd closed); the local `File`
/// reopen keeps the historical `Option<File>` contract for the caller.
///
/// **Warning:** This protection is **system-wide (global)** and affects all CPU cores,
/// not just the thread executing this function.
///
/// Uses the Linux kernel PM QoS interface to request zero latency.
///
/// RETURN: The `File` handle. It MUST be kept alive in the main scope.
/// If the file descriptor is closed (drop), the kernel revokes the protection.
pub fn lock_cpu_c_states() -> Option<std::fs::File> {
    // Engine equivalent validates the path with log + graceful fallback
    // (returns Err instead of None); keep the local handle open so the
    // caller's RAII lifetime still owns the protection.
    let _ = neural_amp_modeler_rs::rt_hardening::request_cpu_dma_latency(0);
    match std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/cpu_dma_latency")
    {
        Ok(mut file) => {
            // Value 0 indicates zero tolerance to power transition latency.
            let zero: i32 = 0;
            if std::io::Write::write_all(&mut file, &zero.to_ne_bytes()).is_ok() {
                log::info!("⚡ PM QoS Lock: Deep CPU C-States disabled (Zero DMA Latency).");
                return Some(file);
            }
            log::warn!(
                "[E2111 | PM_QOS_WRITE_FAILED] PM QoS: Failed to write zero-latency request to \
                 /dev/cpu_dma_latency — C-state deep sleep prevention is inactive. \
                 Audio latency jitter from CPU power transitions may occur."
            );
            None
        }
        Err(e) => {
            // Often fails if write permission is missing or the file does not exist.
            log::warn!(
                "[E2112 | PM_QOS_ACCESS_DENIED] PM QoS: Access denied to /dev/cpu_dma_latency ({e}). \
                 Deep CPU C-State prevention is inactive — audio latency jitter from CPU power \
                 transitions may occur. \
                 Consider creating a udev rule for the 'audio' group."
            );
            None
        }
    }
}

#[cfg(test)]
#[path = "pm_qos_test.rs"]
mod tests;

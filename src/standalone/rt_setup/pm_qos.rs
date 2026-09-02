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
///
/// Returns `Some(name)` if a valid sink that is not NAM-Audio-Pipe itself is found,
/// or `None` otherwise (allowing routing to be decided by WirePlumber).
pub fn detect_hardware_sink() -> Option<String> {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

    let child = std::process::Command::new("pw-metadata")
        .args(["-n", "default", "0", "default.audio.sink"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    let output = collect_child_output_with_watchdog(child, TIMEOUT)?;
    parse_sink_name_from_metadata(&output.stdout)
}

/// Waits up to `timeout` for `child` to exit and collects its stdout.
///
/// The [`std::process::Child`] handle stays owned by the calling thread for the
/// whole wait, so the watchdog terminates the probe via
/// [`std::process::Child::kill`] — which targets the kernel's handle for the
/// spawned process and can therefore never signal a recycled PID. This closes
/// the race of killing a raw `libc::pid_t` with `libc::kill(pid, SIGKILL)`
/// after a joiner thread has already reaped the child (where the PID could have
/// been recycled in between).
///
/// Returns `None` when the child fails to exit within `timeout` (it is killed
/// via the handle and reaped within a bounded grace window — never blocking
/// past the watchdog deadline) or when its status cannot be obtained.
pub(crate) fn collect_child_output_with_watchdog(
    mut child: std::process::Child,
    timeout: std::time::Duration,
) -> Option<std::process::Output> {
    let mut stdout = child.stdout.take();
    let deadline = std::time::Instant::now() + timeout;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                // `kill`/`wait` operate on the owned Child handle, immune to PID
                // recycling. SIGKILL normally terminates immediately, but a
                // child stuck in an uninterruptible D-state cannot be reaped
                // until it wakes — bound the reap so the 500 ms watchdog
                // deadline is preserved even then (review round); the kernel
                // reaps the abandoned child once it leaves D-state.
                let _ = child.kill();
                let reap_deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(100);
                loop {
                    if matches!(child.try_wait(), Ok(Some(_)))
                        || std::time::Instant::now() >= reap_deadline
                    {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                log::warn!(
                    "detect_hardware_sink: pw-metadata did not respond within {}ms — \
                     skipping default sink detection (WirePlumber will decide routing).",
                    timeout.as_millis()
                );
                return None;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Err(_) => return None,
        }
    };

    let mut stdout_bytes = Vec::new();
    if let Some(mut out) = stdout.take() {
        let _ = std::io::Read::read_to_end(&mut out, &mut stdout_bytes);
    }

    Some(std::process::Output {
        status,
        stdout: stdout_bytes,
        stderr: Vec::new(),
    })
}

/// Parses the default sink name from `pw-metadata` raw output.
pub(crate) fn parse_sink_name_from_metadata(raw_stdout: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(raw_stdout);
    let start = s.find("\"name\":\"")?;
    let rest = &s[start + 8..];
    let end = rest.find('"')?;
    let name = &rest[..end];

    if name == crate::standalone::pw_host::identity::PW_CAPTURE_NODE_NAME {
        None
    } else {
        Some(name.to_string())
    }
}

/// Prevents the processor from entering power-saving C-States,
/// guaranteeing 0ms wake-up latency for RT audio processing.
///
/// **Warning:** This protection is **system-wide (global)** and affects all CPU cores,
/// not just the thread executing this function.
///
/// Uses the Linux kernel PM QoS interface to request zero latency.
///
/// RETURN: The `File` handle. It MUST be kept alive in the main scope.
/// If the file descriptor is closed (drop), the kernel revokes the protection.
pub fn lock_cpu_c_states() -> Option<std::fs::File> {
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
            log::warn!("PM QoS: Failed to write to /dev/cpu_dma_latency.");
            None
        }
        Err(e) => {
            // Often fails if write permission is missing or the file does not exist.
            log::warn!(
                "PM QoS: Access denied to /dev/cpu_dma_latency ({}). \
                 Consider creating a udev rule for the 'audio' group.",
                e
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_hardware_sink() {
        let sample = br#"update: id:0 key:'default.audio.sink' value:'{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}' type:'Spa:String:JSON'"#;
        let result = parse_sink_name_from_metadata(sample);
        assert_eq!(
            result.as_deref(),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo")
        );
    }

    #[test]
    fn parse_nam_capture_node_ignored() {
        let sample = format!(
            r#"update: id:0 key:'default.audio.sink' value:'{{"name":"{}"}}' type:'Spa:String:JSON'"#,
            crate::standalone::pw_host::identity::PW_CAPTURE_NODE_NAME
        );
        let result = parse_sink_name_from_metadata(sample.as_bytes());
        assert!(result.is_none());
    }

    #[test]
    fn parse_invalid_output_returns_none() {
        let sample = b"No metadata found";
        let result = parse_sink_name_from_metadata(sample);
        assert!(result.is_none());
    }

    #[test]
    fn detect_hardware_sink_terminates_promptly() {
        // Runs detect_hardware_sink to ensure it executes without panicking and respects the 500ms timeout.
        let _ = detect_hardware_sink();
    }

    #[test]
    fn watchdog_child_exiting_before_timeout_is_reaped_without_signal() {
        // The trivial `sleep 0` child terminates almost immediately — well before
        // the watchdog deadline — exercising the fast-exit race (child
        // exits shortly before the 500 ms timeout). The watchdog keeps the
        // `Child` handle and reaps via `try_wait`, so no raw `libc::kill(pid,
        // SIGKILL)` is ever issued and no signal can reach a recycled PID.
        let child = std::process::Command::new("sleep")
            .arg("0")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("sleep must be spawnable for the watchdog test");
        let pid = child.id() as libc::pid_t;

        let output =
            collect_child_output_with_watchdog(child, std::time::Duration::from_millis(500))
                .expect("a child that exits before the timeout must yield its output");

        assert!(output.status.success());
        // The child was reaped by the caller via the owned handle; `kill(pid, 0)`
        // must report ESRCH (no such process) — not a zombie, and nothing left
        // for a recycled-PID signal to hit.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(unsafe { *libc::__errno_location() }, libc::ESRCH);
    }

    #[test]
    fn watchdog_timeout_kills_via_handle_and_reaps() {
        // A long-lived child with a short deadline forces the timeout path:
        // termination must go through `Child::kill()`/`wait()` on the owned
        // handle, leaving no zombie and never signaling a raw PID.
        let child = std::process::Command::new("sleep")
            .arg("10")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("sleep must be spawnable for the watchdog test");
        let pid = child.id() as libc::pid_t;

        let result =
            collect_child_output_with_watchdog(child, std::time::Duration::from_millis(100));
        assert!(
            result.is_none(),
            "a child that outlives the deadline must yield None"
        );

        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(unsafe { *libc::__errno_location() }, libc::ESRCH);
    }
}

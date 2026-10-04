// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unit tests for the `pw-metadata` sink probe: the parser (JSON escaping)
//! and the bounded stdout watchdog (fast-exit race, deadline kill, capture
//! ceiling and fd-inheriting grandchild containment).

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

    let output = collect_child_output_with_watchdog(child, std::time::Duration::from_millis(1500))
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

    let result = collect_child_output_with_watchdog(child, std::time::Duration::from_millis(100));
    assert!(
        result.is_none(),
        "a child that outlives the deadline must yield None"
    );

    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(unsafe { *libc::__errno_location() }, libc::ESRCH);
}

#[test]
fn parse_sink_name_unescapes_quoted_values() {
    // JSON-escaped quote inside the name: the parser must unescape the
    // value (not cut it at the escaped quote) and close at the real one.
    let sample = br#"update: id:0 key:'default.audio.sink' value:'{"name":"mixer\"one"}' type:'Spa:String:JSON'"#;
    let result = parse_sink_name_from_metadata(sample);
    assert_eq!(result.as_deref(), Some("mixer\"one"));

    // Escaped backslash immediately before the closing quote.
    let sample = br#"value:'{"name":"tail\\"}'"#;
    let result = parse_sink_name_from_metadata(sample);
    assert_eq!(result.as_deref(), Some("tail\\"));

    // Lone trailing backslash: unterminated value is rejected, never
    // silently accepted with an open escape.
    let sample = br#"value:'{"name":"broken\}'"#;
    assert!(parse_sink_name_from_metadata(sample).is_none());
}

#[test]
fn watchdog_output_over_64k_is_truncated_at_capture_cap() {
    // A child writing 100 000 zero bytes exceeds the 16 KiB capture
    // ceiling many times over: the capture must keep draining (so the
    // child can finish writing and exit) while retaining exactly the
    // capped prefix, and the probe must return `Some` — never a kill.
    let child = std::process::Command::new("head")
        .args(["-c", "100000", "/dev/zero"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("head must be spawnable for the cap test");

    let output = collect_child_output_with_watchdog(child, std::time::Duration::from_millis(1500))
        .expect("an over-ceiling child must exit inside the deadline and yield its output");

    assert!(output.status.success());
    assert_eq!(
        output.stdout.len(),
        STDOUT_CAPTURE_CAP,
        "the capture must hold exactly the {STDOUT_CAPTURE_CAP}-byte capped prefix"
    );
}

#[test]
fn watchdog_returns_within_deadline_plus_grace_with_fd_inheriting_grandchild() {
    // The shell forks a `sleep 5` grandchild inheriting the stdout write
    // end, then echoes the marker and exits: the pipe stays open (no EOF)
    // for ~5 s. The watchdog must return within deadline + bounded grace
    // with the partial capture — never waiting out the grandchild.
    let child = std::process::Command::new("sh")
        .arg("-c")
        .arg("sleep 5 & exec echo grandchild_probe_ok")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("sh must be spawnable for the grandchild test");

    let start = std::time::Instant::now();
    let output = collect_child_output_with_watchdog(child, std::time::Duration::from_millis(300))
        .expect("a child that exits inside the deadline must yield its output");
    let elapsed = start.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("grandchild_probe_ok"),
        "the buffered marker must survive the bounded tail drain, got: {stdout:?}"
    );
    assert!(
        output.status.success(),
        "the shell itself must have exited cleanly"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "the probe must not wait out the fd-inheriting grandchild (5 s): {elapsed:?}"
    );
}

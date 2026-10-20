// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! CLI Black-Box Smoke Tests for nam-audio-pipe.
//!
//! Validates the binary's command-line interface via `std::process::Command`,
//! using the `CARGO_BIN_EXE_nam-audio-pipe` environment variable injected by Cargo.

use std::process::Command;

fn binary() -> Command {
    let path = env!("CARGO_BIN_EXE_nam-audio-pipe");
    Command::new(path)
}

/// Runs `command` capturing stdout/stderr, failing the test if it does not
/// exit within `timeout` (the child is then killed, so a hung host never
/// blocks the suite).
fn output_with_timeout(mut command: Command, timeout: std::time::Duration) -> std::process::Output {
    use std::process::Stdio;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().expect("failed to spawn binary");
    let start = std::time::Instant::now();
    loop {
        if child.try_wait().expect("try_wait failed").is_some() {
            return child.wait_with_output().expect("collect child output");
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            panic!("binary did not exit within {timeout:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn help_flag_exits_zero_and_prints_usage() {
    let output = binary()
        .arg("--help")
        .output()
        .expect("failed to execute binary");

    assert!(output.status.success(), "expected exit code 0 from --help");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Usage:"),
        "--help should print usage information"
    );
    assert!(
        stdout.contains("--model"),
        "--help should list --model option"
    );
    assert!(
        stdout.contains("--gate MODE"),
        "--help should list --gate MODE option, got:\n{stdout}"
    );
    assert!(
        stdout.contains("-70 dB default") && stdout.contains("explicit threshold in dB"),
        "--help should document the polymorphic gate (default on, numeric dBFS), got:\n{stdout}"
    );
    assert!(
        stdout.contains("[default: on]"),
        "--help should declare default for --gate, got:\n{stdout}"
    );
}

#[test]
fn diagnose_flag_exits_zero_and_prints_diagnostics() {
    let output = binary()
        .arg("--diagnose")
        .output()
        .expect("failed to execute binary");

    assert!(
        output.status.success(),
        "expected exit code 0 from --diagnose"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("NeuralAmpModeler-rs Diagnostic")
            || stdout.contains("NAM-rs Diagnostic")
            || stdout.contains("System Information")
            || stdout.contains("Runtime State"),
        "--diagnose should print diagnostic sections, got: {}",
        stdout.lines().take(3).collect::<Vec<_>>().join(" | ")
    );
}

#[test]
fn diagnose_full_exits_zero() {
    let output = binary()
        .arg("--diagnose-full")
        .output()
        .expect("failed to execute binary");

    assert!(
        output.status.success(),
        "expected exit code 0 from --diagnose-full"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("NeuralAmpModeler-rs Diagnostic")
            || stdout.contains("NAM-rs Diagnostic")
            || stdout.contains("System Information")
            || stdout.contains("Runtime State"),
        "--diagnose-full should print diagnostic sections"
    );
}

#[test]
fn invalid_option_exits_with_error() {
    let output = binary()
        .arg("--nonexistent-flag-xyzzy")
        .output()
        .expect("failed to execute binary");

    assert!(
        !output.status.success(),
        "expected non-zero exit code for invalid option"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Argument error")
            || stderr.contains("unexpected")
            || stderr.contains("error"),
        "stderr should contain error message, got: {}",
        stderr
    );
}

#[test]
fn invalid_gain_value_exits_with_error() {
    let output = binary()
        .args(["--input-gain", "notanumber"])
        .output()
        .expect("failed to execute binary");

    assert!(
        !output.status.success(),
        "expected non-zero exit code for invalid gain"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Argument error"),
        "stderr should contain 'Argument error', got: {}",
        stderr
    );
}

#[test]
fn gain_out_of_range_exits_with_error() {
    let output = binary()
        .args(["--output-gain", "999"])
        .output()
        .expect("failed to execute binary");

    assert!(
        !output.status.success(),
        "expected non-zero exit code for out-of-range gain"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("out of range"),
        "stderr should indicate out of range, got: {}",
        stderr
    );
}

#[test]
fn no_args_prints_help_and_exits_zero() {
    let output = binary().output().expect("failed to execute binary");

    assert!(
        output.status.success(),
        "expected exit code 0 when invoked with no arguments"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Usage:"),
        "no-args invocation should print usage information"
    );
}

// --buffer-size domain contract -------------------------------------------
// The negative acceptances must fail fast with a non-zero exit and a clear
// stderr message BEFORE any PipeWire connection is attempted.

#[test]
fn buffer_size_non_power_of_two_exits_with_error_before_pipewire() {
    let output = binary()
        .args(["-b", "100"])
        .output()
        .expect("failed to execute binary");

    assert!(
        !output.status.success(),
        "expected non-zero exit code for non-power-of-two buffer size"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Argument error"),
        "stderr should contain 'Argument error', got: {}",
        stderr
    );
    assert!(
        stderr.contains("power of two"),
        "stderr should explain the power-of-two requirement, got: {}",
        stderr
    );
    assert!(
        !stderr.contains("PipeWire"),
        "validation must reject before any PipeWire connection, got: {}",
        stderr
    );
}

#[test]
fn buffer_size_below_minimum_exits_with_error_before_pipewire() {
    let output = binary()
        .args(["-b", "1"])
        .output()
        .expect("failed to execute binary");

    assert!(
        !output.status.success(),
        "expected non-zero exit code for below-minimum buffer size"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Argument error"),
        "stderr should contain 'Argument error', got: {}",
        stderr
    );
    assert!(
        stderr.contains("at least 16"),
        "stderr should explain the minimum, got: {}",
        stderr
    );
    assert!(
        !stderr.contains("PipeWire"),
        "validation must reject before any PipeWire connection, got: {}",
        stderr
    );
}

#[test]
fn buffer_size_above_max_exits_with_error_before_pipewire() {
    let output = binary()
        .args(["-b", "16384"])
        .output()
        .expect("failed to execute binary");

    assert!(
        !output.status.success(),
        "expected non-zero exit code for above-max buffer size"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Argument error"),
        "stderr should contain 'Argument error', got: {}",
        stderr
    );
    assert!(
        stderr.contains("cannot exceed 8192"),
        "stderr should explain the maximum, got: {}",
        stderr
    );
    assert!(
        !stderr.contains("PipeWire"),
        "validation must reject before any PipeWire connection, got: {}",
        stderr
    );
}

// --cabsim-partition domain contract ---------------------------------------
// The UPOLS partition policy accepts only the documented latency/CPU set;
// anything else must fail fast with a non-zero exit BEFORE any PipeWire
// connection is attempted.

#[test]
fn cabsim_partition_out_of_domain_exits_with_error_before_pipewire() {
    let output = binary()
        .args(["--cabsim-partition", "100"])
        .output()
        .expect("failed to execute binary");

    assert!(
        !output.status.success(),
        "expected non-zero exit code for out-of-domain cabsim partition"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Argument error"),
        "stderr should contain 'Argument error', got: {}",
        stderr
    );
    assert!(
        stderr.contains("Cab-sim partition must be one of"),
        "stderr should explain the allowed set, got: {}",
        stderr
    );
    assert!(
        !stderr.contains("PipeWire"),
        "validation must reject before any PipeWire connection, got: {}",
        stderr
    );
}

#[test]
fn cabsim_partition_accepted_values_in_help() {
    let output = binary()
        .arg("-h")
        .output()
        .expect("failed to execute binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--cabsim-partition"),
        "help must document --cabsim-partition, got: {stdout}"
    );
    assert!(
        stdout.contains("128"),
        "help must document the 128 default, got: {stdout}"
    );
}

#[test]
fn invalid_gate_mode_exits_with_error() {
    let output = binary()
        .args(["--gate", "invalid"])
        .output()
        .expect("failed to execute binary");

    assert!(
        !output.status.success(),
        "expected non-zero exit code for invalid gate mode"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Argument error"),
        "stderr should contain 'Argument error', got: {}",
        stderr
    );
    assert!(
        stderr.contains("Invalid gate mode"),
        "stderr should explain the invalid gate mode, got: {}",
        stderr
    );
}

#[test]
fn gate_flag_options_accepted_in_diagnose() {
    for mode in ["on", "off"] {
        let output = binary()
            .args(["--gate", mode, "--diagnose"])
            .output()
            .expect("failed to execute binary");

        assert!(
            output.status.success(),
            "expected exit code 0 when passing --gate {mode} with --diagnose, got: {:?}",
            output.status
        );
    }
}

#[test]
fn gate_numeric_thresholds_accepted_in_diagnose() {
    // E2E acceptance: numeric dBFS thresholds (and the positive
    // auto-normalized alias) must survive CLI parsing and reach --diagnose with
    // exit code 0 — before any live PipeWire stream is attempted.
    for value in ["-60", "-65.5", "-50dB", "-45db", "60"] {
        let output = binary()
            .args(["--gate", value, "--diagnose"])
            .output()
            .expect("failed to execute binary");

        assert!(
            output.status.success(),
            "expected exit code 0 when passing --gate {value} with --diagnose, got: {:?}",
            output.status
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("Argument error"),
            "--gate {value} must not be rejected by the parser, got: {}",
            stderr
        );
    }
}

#[test]
fn invalid_gate_threshold_exits_with_error_before_pipewire() {
    // Out-of-domain thresholds (both as typed negatives and as positive values
    // auto-normalized past the -20 dBFS safety ceiling) must fail fast with
    // the usage exit code (2) and a didactic message BEFORE any PipeWire connection.
    for value in ["-120", "-100", "-10", "10", "15"] {
        let output = binary()
            .args(["--gate", value])
            .output()
            .expect("failed to execute binary");

        assert_eq!(
            output.status.code(),
            Some(2),
            "expected usage exit code 2 when passing --gate {value}, got: {:?}",
            output.status
        );

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Argument error"),
            "stderr should contain 'Argument error', got: {}",
            stderr
        );
        assert!(
            stderr.contains("Invalid gate mode"),
            "stderr should explain the invalid gate mode, got: {}",
            stderr
        );
        assert!(
            stderr.contains("out of the accepted range"),
            "stderr should explain the accepted dBFS range, got: {}",
            stderr
        );
        assert!(
            !stderr.contains("PipeWire"),
            "validation must reject before any PipeWire connection, got: {}",
            stderr
        );
    }
}

// --input file validation (F-UX-12) ----------------------------------------
// An explicitly provided --model/--cab that does not resolve to a readable
// regular file is a legitimate *argument* error: it must fail fast with the
// usage exit code (2) and the resolved absolute path, before any PipeWire
// connection.

#[test]
fn missing_model_path_exits_with_argument_error_before_pipewire() {
    let output = binary()
        .args(["--model", "/nonexistent/definitely/missing_model.nam"])
        .output()
        .expect("failed to execute binary");

    assert_eq!(
        output.status.code(),
        Some(2),
        "a missing --model must exit with the usage code 2, got: {:?}",
        output.status
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Argument error"),
        "stderr should contain 'Argument error', got: {stderr}"
    );
    assert!(
        stderr.contains("--model") && stderr.contains("not found"),
        "stderr should name --model and the missing path, got: {stderr}"
    );
    assert!(
        !stderr.contains("PipeWire"),
        "validation must reject before any PipeWire connection, got: {stderr}"
    );
}

#[test]
fn directory_as_model_path_exits_with_argument_error() {
    let output = binary()
        .args(["--model", "/tmp"])
        .output()
        .expect("failed to execute binary");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Argument error") && stderr.contains("not a regular file"),
        "a directory passed to --model must be rejected didactically, got: {stderr}"
    );
}

// --color policy (F-UX-10) -------------------------------------------------

#[test]
fn color_always_forces_ansi_in_help() {
    let output = binary()
        .args(["--color", "always", "--help"])
        .output()
        .expect("failed to execute binary");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains('\u{1b}'),
        "--color always must emit ANSI even when stdout is a pipe, got: {stdout:?}"
    );
}

#[test]
fn no_color_env_disables_ansi_in_help() {
    // Acceptance (F-UX-10): `NO_COLOR=1 … --help | grep -c $'\x1b'` == 0.
    let output = binary()
        .env("NO_COLOR", "1")
        .arg("--help")
        .output()
        .expect("failed to execute binary");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains('\u{1b}'),
        "NO_COLOR=1 must suppress every ANSI sequence, got: {stdout:?}"
    );
}

// --BrokenPipe tolerance (F-UX-03) -----------------------------------------
// `nam-audio-pipe --help | head -1` closes the pipe while the process is still
// writing; the write must be tolerated (no `println!` panic).

#[test]
fn help_with_closed_stdout_pipe_does_not_panic() {
    use std::process::Stdio;

    let mut child = binary()
        .arg("--help")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn binary");

    // Close the read end immediately so any subsequent write hits EPIPE.
    drop(child.stdout.take());

    let output = child.wait_with_output().expect("wait for binary");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "--help must tolerate a closed stdout pipe, got: {stderr}"
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "--help must exit 0 even with a closed pipe, got: {:?} / {stderr}",
        output.status
    );
}

// --runtime error classes (F-UX-06/07/08) ----------------------------------
// A file that exists (so it passes the argument pre-validation) but cannot be
// loaded is a *runtime* failure: it must be labelled with its own class and
// exit with the documented code, never as "Argument error".

#[test]
fn unloadable_model_fixtures_exit_with_model_code() {
    let dir = std::env::temp_dir().join(format!("nam_s2_models_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let empty = dir.join("empty.nam");
    std::fs::write(&empty, b"").expect("write empty fixture");
    let truncated = dir.join("truncated.nam");
    std::fs::write(&truncated, b"{\"version\"").expect("write truncated fixture");
    let future = dir.join("future.nam");
    std::fs::write(
        &future,
        br#"{"version":"9.9.9","architecture":"WaveNet","config":{},"weights":[]}"#,
    )
    .expect("write future-version fixture");

    for fixture in [&empty, &truncated, &future] {
        let output = binary()
            .args(["--model", fixture.to_str().expect("utf-8 fixture path")])
            .output()
            .expect("failed to execute binary");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(3),
            "an unloadable model must exit with the model code 3 ({}): {stderr}",
            fixture.display()
        );
        assert!(
            stderr.contains("Model"),
            "the error line must name the Model class, got: {stderr}"
        );
        assert!(
            !stderr.contains("Argument error"),
            "a load failure is not an argument error, got: {stderr}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn explicit_unloadable_cab_is_fatal_with_cab_code() {
    let dir = std::env::temp_dir().join(format!("nam_s2_cab_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let bad = dir.join("bad.wav");
    std::fs::write(&bad, b"not a wav file at all").expect("write bad IR fixture");

    let output = binary()
        .args(["--cab", bad.to_str().expect("utf-8 IR path")])
        .output()
        .expect("failed to execute binary");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(4),
        "an explicit unloadable --cab must be fatal with the cab-IR code 4: {stderr}"
    );
    assert!(
        stderr.contains("Cab IR"),
        "the error line must name the Cab IR class, got: {stderr}"
    );
    assert!(
        !stderr.contains("Argument error"),
        "a load failure is not an argument error, got: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pipewire_first_connection_failure_exits_backend_with_hint() {
    // A fresh, empty runtime dir makes the very first `context.connect` fail:
    // the startup failure must carry a remediation hint and the backend exit
    // code, never a raw panic/backtrace.
    let rt = std::env::temp_dir().join(format!("nam_s2_rt_{}", std::process::id()));
    std::fs::create_dir_all(&rt).expect("create empty runtime dir");
    let mut command = binary();
    command
        .env("PIPEWIRE_RUNTIME_DIR", &rt)
        .env("XDG_RUNTIME_DIR", &rt)
        .args(["--fail-fast", "--gate", "off"]);
    let output = output_with_timeout(command, std::time::Duration::from_secs(30));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(5),
        "a first-connection failure must exit with the backend code 5: {stderr}"
    );
    assert!(
        stderr.contains("systemctl --user"),
        "the remediation hint must be reachable, got: {stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "no panic backtrace is allowed on the user's screen, got: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&rt);
}

// --recording startup failures (R-1) ---------------------------------------
// A `--record` startup that cannot bring the disk worker up (unusable output
// directory, unavailable io_uring, ...) must exit with the documented recording
// code (6), never the legacy generic 1. Regression guard for the exit-code
// contract in `exit.rs` / `README.md`.

#[test]
fn recording_startup_failure_exits_with_recording_code() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("nam_s7_recording_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    // Make the directory unwritable so the worker's startup probe fails.
    let mut perms = std::fs::metadata(&dir)
        .expect("stat temp dir")
        .permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(&dir, perms).expect("chmod temp dir read-only");

    // Permission bits do not apply to root: the startup would then succeed and
    // the host would run indefinitely. Detect that case and skip.
    let root_probe = dir.join(".nam_euid_probe");
    if std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&root_probe)
        .is_ok()
    {
        let _ = std::fs::remove_file(&root_probe);
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("skipping recording_startup_failure test: permission bits ignored (root)");
        return;
    }

    let mut command = binary();
    command.args([
        "--record",
        "--record-dir",
        dir.to_str().expect("utf-8 dir path"),
        "--gate",
        "off",
    ]);
    let output = output_with_timeout(command, std::time::Duration::from_secs(30));
    let stderr = String::from_utf8_lossy(&output.stderr);

    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        output.status.code(),
        Some(6),
        "a recording startup failure must exit with the recording code 6: {stderr}"
    );
    assert!(
        stderr.contains("Recording"),
        "the error line must name the Recording class, got: {stderr}"
    );
    assert!(
        !stderr.contains("Argument error"),
        "a startup failure is not an argument error, got: {stderr}"
    );
}

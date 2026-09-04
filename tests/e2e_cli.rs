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
    // Tarefa 2.1 E2E acceptance: numeric dBFS thresholds (and the positive
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
    // exit code 1 and a didactic message BEFORE any PipeWire connection.
    for value in ["-120", "-100", "-10", "10", "15"] {
        let output = binary()
            .args(["--gate", value])
            .output()
            .expect("failed to execute binary");

        assert_eq!(
            output.status.code(),
            Some(1),
            "expected exit code 1 when passing --gate {value}, got: {:?}",
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

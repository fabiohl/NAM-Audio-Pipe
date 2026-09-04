// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

#[test]
fn test_parse_args_diagnose() {
    let args = vec!["nam-audio-pipe", "--diagnose"];
    let parser = lexopt::Parser::from_iter(args);
    let cli_args = parse_args_from(parser);
    assert!(cli_args.diagnose);
    assert!(!cli_args.diagnose_full);
}

#[test]
fn test_parse_args_diagnose_full() {
    let args = vec!["nam-audio-pipe", "--diagnose-full"];
    let parser = lexopt::Parser::from_iter(args);
    let cli_args = parse_args_from(parser);
    assert!(!cli_args.diagnose);
    assert!(cli_args.diagnose_full);
}

#[test]
fn test_parse_args_model_and_gains() {
    let args = vec![
        "nam-audio-pipe",
        "-m",
        "my_model.nam",
        "-i",
        "6.0",
        "-o",
        "-3.5",
        "-b",
        "512",
    ];
    let parser = lexopt::Parser::from_iter(args);
    let cli_args = parse_args_from(parser);
    assert_eq!(cli_args.model_path, Some(PathBuf::from("my_model.nam")));
    assert_eq!(cli_args.input_gain, 6.0);
    assert_eq!(cli_args.output_gain, -3.5);
    assert_eq!(cli_args.buffer_size, 512);
    assert!(!cli_args.diagnose);
    assert!(!cli_args.diagnose_full);
}

#[test]
fn test_parse_args_activation_standard() {
    let args = vec!["nam-audio-pipe", "--activation", "standard"];
    let parser = lexopt::Parser::from_iter(args);
    let cli_args = parse_args_from(parser);
    assert_eq!(cli_args.activation, Some(ActivationPrecision::Standard));
}

#[test]
fn test_parse_args_activation_fast() {
    let args = vec!["nam-audio-pipe", "--activation", "fast"];
    let parser = lexopt::Parser::from_iter(args);
    let cli_args = parse_args_from(parser);
    assert_eq!(cli_args.activation, Some(ActivationPrecision::Fast));
}

#[test]
fn test_parse_args_activation_std_alias() {
    let args = vec!["nam-audio-pipe", "--activation", "std"];
    let parser = lexopt::Parser::from_iter(args);
    let cli_args = parse_args_from(parser);
    assert_eq!(cli_args.activation, Some(ActivationPrecision::Standard));
}

// Note: Legacy aliases ("hf", "highfidelity", "high") were retired in favor of
// "standard"/"std" and "fast". Invalid arguments trigger `process::exit(1)`,
// which is tested via black-box binary tests in `tests/e2e_cli.rs`.

#[test]
fn test_parse_args_activation_default() {
    // Pre-existing hazard (found while auditing this rename, unrelated to it):
    // `parse_args_from` treats a zero-real-argument invocation (only the
    // program name, which lexopt's `Parser::from_iter` consumes as the bin
    // name) as "no args at all" and calls `print_help(); std::process::exit(0);`.
    // That is a *real* process exit — inside a `#[test]` it would silently
    // kill the entire test binary process (not just this test), truncating
    // the whole `cargo test` run while still reporting exit code 0. Passing
    // a harmless real flag (`--buffer-size 256`, the documented default)
    // keeps `has_args = true` and exercises the same "activation not passed"
    // path without tripping that exit.
    let args: Vec<&str> = vec!["nam-audio-pipe", "--buffer-size", "256"];
    let parser = lexopt::Parser::from_iter(args);
    let cli_args = parse_args_from(parser);
    assert_eq!(cli_args.activation, None);
}

#[test]
fn test_parse_args_fail_fast_defaults_to_disabled() {
    let args = vec!["nam-audio-pipe", "--buffer-size", "256"];
    let parser = lexopt::Parser::from_iter(args);
    let cli_args = parse_args_from(parser);
    assert!(!cli_args.fail_fast, "reconnect must be enabled by default");
}

#[test]
fn test_parse_args_fail_fast_flag_enables_fail_fast() {
    let args = vec!["nam-audio-pipe", "--fail-fast", "--buffer-size", "256"];
    let parser = lexopt::Parser::from_iter(args);
    let cli_args = parse_args_from(parser);
    assert!(cli_args.fail_fast);
}

#[test]
fn test_parse_args_record_keeps_reconnect_enabled() {
    let args = vec!["nam-audio-pipe", "--record", "--buffer-size", "256"];
    let parser = lexopt::Parser::from_iter(args);
    let cli_args = parse_args_from(parser);
    assert!(cli_args.record);
    assert!(!cli_args.fail_fast);
}

#[test]
fn test_parse_args_gate_defaults_to_on() {
    let args = vec!["nam-audio-pipe", "--buffer-size", "256"];
    let parser = lexopt::Parser::from_iter(args);
    let cli_args = parse_args_from(parser);
    assert_eq!(cli_args.gate, GateConfig::default_on());
}

#[test]
fn test_parse_args_gate_off_flag() {
    let args = vec!["nam-audio-pipe", "--gate", "off"];
    let parser = lexopt::Parser::from_iter(args);
    let cli_args = parse_args_from(parser);
    assert_eq!(cli_args.gate, GateConfig::Off);
}

#[test]
fn test_parse_args_gate_on_explicit() {
    let args = vec!["nam-audio-pipe", "--gate", "on"];
    let parser = lexopt::Parser::from_iter(args);
    let cli_args = parse_args_from(parser);
    assert_eq!(cli_args.gate, GateConfig::default_on());
}

#[test]
fn test_parse_args_gate_case_insensitive() {
    let args = vec!["nam-audio-pipe", "--gate", "OFF"];
    let parser = lexopt::Parser::from_iter(args);
    let cli_args = parse_args_from(parser);
    assert_eq!(cli_args.gate, GateConfig::Off);
}

#[test]
fn test_parse_args_gate_numeric_threshold_accepted() {
    let args = vec!["nam-audio-pipe", "--gate", "-60"];
    let parser = lexopt::Parser::from_iter(args);
    let cli_args = parse_args_from(parser);
    assert_eq!(cli_args.gate, GateConfig::from_open_db(-60.0));
}

// --gate polymorphic threshold contract (Sprint 1, Tarefa 1.1 acceptance) --

#[test]
fn test_parse_args_gate_invalid_value_exits() {
    // `exit_with_error` terminates the process via std::process::exit(1), so the
    // invalid-value path is proven end-to-end through the real binary in
    // tests/e2e_cli.rs::invalid_gate_mode_exits_with_error. Here we pin the
    // mapping contract that drives that exit: any value that is neither a mode
    // literal nor a numeric dBFS threshold yields an "Invalid gate mode" error.
    let err = parse_gate_mode("invalid").unwrap_err();
    assert!(
        err.contains("Invalid gate mode"),
        "the mapping error must carry the exact prefix exit_with_error prints: {err}"
    );
    let err = parse_gate_mode("").unwrap_err();
    assert!(
        err.contains("Invalid gate mode"),
        "empty values must be rejected with the Invalid gate mode prefix: {err}"
    );
    assert_eq!(parse_gate_mode("ON"), Ok(GateConfig::default_on()));
    assert_eq!(parse_gate_mode("oFf"), Ok(GateConfig::Off));
}

#[test]
fn gate_mode_literals_accept_all_synonyms() {
    for literal in ["on", "ON", "On", "true", "True", "default"] {
        assert_eq!(
            parse_gate_mode(literal),
            Ok(GateConfig::default_on()),
            "literal '{literal}' must enable the default gate"
        );
    }
    for literal in ["off", "oFf", "OFF", "false", "FALSE", "0"] {
        assert_eq!(
            parse_gate_mode(literal),
            Ok(GateConfig::Off),
            "literal '{literal}' must disable the gate"
        );
    }
}

#[test]
fn gate_numeric_thresholds_are_accepted() {
    // Tarefa 1.1 acceptance: "-60", "-65.5", "-45dB" and the "60" sign alias.
    assert_eq!(parse_gate_mode("-60"), Ok(GateConfig::from_open_db(-60.0)));
    assert_eq!(
        parse_gate_mode("-65.5"),
        Ok(GateConfig::from_open_db(-65.5))
    );
    assert_eq!(
        parse_gate_mode("-45dB"),
        Ok(GateConfig::from_open_db(-45.0))
    );
    assert_eq!(
        parse_gate_mode("-45db"),
        Ok(GateConfig::from_open_db(-45.0))
    );
    assert_eq!(
        parse_gate_mode("-50dbfs"),
        Ok(GateConfig::from_open_db(-50.0))
    );
    // Tarefa 2.1 unit matrix: "-50dB" and "-50db" (suffix case-insensitivity).
    assert_eq!(
        parse_gate_mode("-50dB"),
        Ok(GateConfig::from_open_db(-50.0))
    );
    assert_eq!(
        parse_gate_mode("-50db"),
        Ok(GateConfig::from_open_db(-50.0))
    );
    // Positive finite values are auto-normalized to their negative equivalent.
    assert_eq!(parse_gate_mode("60"), Ok(GateConfig::from_open_db(-60.0)));
    assert_eq!(parse_gate_mode("60dB"), Ok(GateConfig::from_open_db(-60.0)));
}

#[test]
fn gate_numeric_thresholds_derive_schmitt_hysteresis() {
    // T_close = max(T_open - 10 dB, -96 dB): a 10 dB hysteresis band.
    assert_eq!(
        parse_gate_mode("-60"),
        Ok(GateConfig::Threshold {
            threshold_open_db: -60.0,
            threshold_close_db: -70.0,
        })
    );
    assert_eq!(
        parse_gate_mode("-65.5"),
        Ok(GateConfig::Threshold {
            threshold_open_db: -65.5,
            threshold_close_db: -75.5,
        })
    );
    // Hysteresis floors at the LUT minimum: -90 dB open closes at -96 dB, not -100 dB.
    assert_eq!(
        parse_gate_mode("-90"),
        Ok(GateConfig::Threshold {
            threshold_open_db: -90.0,
            threshold_close_db: -96.0,
        })
    );
    // Domain edges remain valid (open == close at the very floor is allowed).
    assert!(parse_gate_mode("-20").is_ok());
    assert!(parse_gate_mode("-96").is_ok());
}

#[test]
fn gate_out_of_range_and_malformed_values_are_rejected() {
    // Below the LUT floor and above the -20 dB safety ceiling (both as typed
    // negatives and as auto-normalized positives).
    for bad in ["-120", "-100", "-10", "-5", "10", "15"] {
        let err = parse_gate_mode(bad).expect_err(&format!("'{bad}' must be rejected"));
        assert!(
            err.contains("Invalid gate mode") && err.contains("out of the accepted range"),
            "rejection of '{bad}' must be didactic, got: {err}"
        );
    }
    for bad in [
        "abc", "", "invalid", "NaN", "inf", "-60dBx", "-60dbs", "--60",
    ] {
        let err = parse_gate_mode(bad).expect_err(&format!("'{bad}' must be rejected"));
        assert!(
            err.contains("Invalid gate mode"),
            "malformed '{bad}' must carry the Invalid gate mode prefix, got: {err}"
        );
    }
}

// --buffer-size domain contract -------------------------------------------

#[test]
fn buffer_size_domain_constants_document_the_contract() {
    assert_eq!(BUFFER_SIZE_AUTO, 0);
    assert_eq!(BUFFER_SIZE_MIN, 16);
    assert_eq!(BUFFER_SIZE_MAX, 8192);
}

#[test]
fn validate_buffer_size_accepts_auto_and_all_valid_powers_of_two() {
    for size in [0u32, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192] {
        assert_eq!(
            validate_buffer_size(size),
            Ok(size),
            "size {size} must be accepted"
        );
    }
}

#[test]
fn validate_buffer_size_rejects_out_of_domain_values() {
    for size in [1u32, 2, 8, 15, 63, 100, 500, 8193, 16384, 65536, u32::MAX] {
        assert!(
            validate_buffer_size(size).is_err(),
            "size {size} must be rejected"
        );
    }
}

#[test]
fn validate_buffer_size_rejects_with_typed_variants() {
    assert_eq!(
        validate_buffer_size(1),
        Err(BufferSizeError::BelowMinimum { size: 1 })
    );
    assert_eq!(
        validate_buffer_size(15),
        Err(BufferSizeError::BelowMinimum { size: 15 })
    );
    assert_eq!(
        validate_buffer_size(8193),
        Err(BufferSizeError::AboveMaximum { size: 8193 })
    );
    assert_eq!(
        validate_buffer_size(u32::MAX),
        Err(BufferSizeError::AboveMaximum { size: u32::MAX })
    );
    assert_eq!(
        validate_buffer_size(100),
        Err(BufferSizeError::NotPowerOfTwo { size: 100 })
    );
    assert_eq!(
        validate_buffer_size(500),
        Err(BufferSizeError::NotPowerOfTwo { size: 500 })
    );
}

#[test]
fn validate_buffer_size_errors_are_explanatory_and_implement_std_error() {
    fn assert_std_error<E: std::error::Error>() {}
    assert_std_error::<BufferSizeError>();

    assert_eq!(
        validate_buffer_size(1).unwrap_err().to_string(),
        "Buffer size must be at least 16 (or 0 for auto), got 1"
    );
    assert_eq!(
        validate_buffer_size(16384).unwrap_err().to_string(),
        "Buffer size cannot exceed 8192 (max bridge capacity), got 16384"
    );
    assert_eq!(
        validate_buffer_size(100).unwrap_err().to_string(),
        "Buffer size must be a power of two (e.g. 64, 128, 256, 512, 1024, 2048, 4096, 8192), got 100"
    );
}

#[test]
fn parse_args_accepts_buffer_size_auto_zero() {
    let args = vec!["nam-audio-pipe", "-b", "0"];
    let parser = lexopt::Parser::from_iter(args);
    let cli_args = parse_args_from(parser);
    assert_eq!(cli_args.buffer_size, 0);
}

#[test]
fn parse_args_accepts_buffer_size_max_boundary() {
    let args = vec!["nam-audio-pipe", "-b", "8192"];
    let parser = lexopt::Parser::from_iter(args);
    let cli_args = parse_args_from(parser);
    assert_eq!(cli_args.buffer_size, 8192);
}

#[test]
fn parse_args_accepts_cpu_flag() {
    let args = vec!["nam-audio-pipe", "--cpu", "3"];
    let parser = lexopt::Parser::from_iter(args);
    let cli_args = parse_args_from(parser);
    assert_eq!(cli_args.cpu, Some(3));
}

#[test]
fn parse_args_accepts_cpu_zero() {
    let args = vec!["nam-audio-pipe", "--cpu", "0"];
    let parser = lexopt::Parser::from_iter(args);
    let cli_args = parse_args_from(parser);
    assert_eq!(cli_args.cpu, Some(0));
}

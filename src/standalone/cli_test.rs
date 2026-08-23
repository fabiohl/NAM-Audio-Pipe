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

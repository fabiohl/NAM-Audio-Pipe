// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

#[test]
fn stats_percentiles_are_correct() {
    let v: Vec<u64> = (0..1000).collect();
    let s = Stats::of(v);
    assert_eq!(s.n, 1000);
    assert_eq!(s.min, 0);
    assert_eq!(s.p50, 499);
    assert_eq!(s.p99, 989);
    assert_eq!(s.max, 999);
}

#[test]
fn stats_empty_is_default() {
    assert_eq!(Stats::of(Vec::<u64>::new()), Stats::default());
}

#[test]
fn json_emitter_escapes_and_sorts_keys() {
    let v = obj(vec![
        kv("a", JsonValue::Int(1)),
        kv("b", JsonValue::Int(42)),
        kv("c", JsonValue::Str("x\"\n".into())),
        kv("d", JsonValue::Num(1.5)),
    ]);
    assert_eq!(v.to_json_string(), r#"{"a":1,"b":42,"c":"x\"\n","d":1.50}"#);
}

#[test]
fn pin_thread_bounds_check() {
    assert!(pin_thread(None).is_ok());
    assert_eq!(
        pin_thread(Some(libc::CPU_SETSIZE as usize)),
        Err(PinThreadError::CpuOutOfBounds {
            cpu: libc::CPU_SETSIZE as usize,
            max: libc::CPU_SETSIZE as usize,
        })
    );
    assert_eq!(
        pin_thread(Some(99999)),
        Err(PinThreadError::CpuOutOfBounds {
            cpu: 99999,
            max: libc::CPU_SETSIZE as usize,
        })
    );
}

#[test]
fn parse_args_rejects_missing_value_without_panicking() {
    // `--runs` with no following token must yield a typed usage error (which the
    // CLI wrapper turns into exit code 2), never a panic.
    let err = parse_args_from(vec!["--runs".to_string()]).unwrap_err();
    assert!(
        err.contains("--runs"),
        "typed error must name the offending flag: {err}"
    );
}

#[test]
fn parse_args_rejects_unknown_argument_and_bad_enum() {
    assert!(
        parse_args_from(vec!["--nope".to_string()])
            .unwrap_err()
            .contains("unknown argument")
    );
    assert!(
        parse_args_from(vec!["--oversample".to_string(), "8x".to_string()])
            .unwrap_err()
            .contains("--oversample")
    );
}

#[test]
fn parse_args_accepts_valid_values() {
    let cfg = parse_args_from(vec!["--runs".to_string(), "3".to_string()]).unwrap();
    assert_eq!(cfg.runs, 3);
}

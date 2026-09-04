// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

#[test]
fn stats_percentiles_are_correct() {
    let v: Vec<u64> = (0..1000).collect();
    let s = Stats::of(v);
    assert_eq!(s.n, 1000);
    assert_eq!(s.min, 0);
    // Nearest-rank: p = samples[ceil(n·p) - 1].
    assert_eq!(s.p50, 499);
    assert_eq!(s.p99, 989);
    assert_eq!(s.max, 999);
}

#[test]
fn stats_empty_is_default() {
    assert_eq!(Stats::of(Vec::<u64>::new()), Stats::default());
}

#[test]
fn percent_delta_sign_and_magnitude() {
    assert!((percent_delta(95.0, 100.0) - (-5.0)).abs() < 1e-9);
    assert!((percent_delta(105.0, 100.0) - 5.0).abs() < 1e-9);
    assert_eq!(percent_delta(1.0, 0.0), 0.0);
}

#[test]
fn cache_proxy_line_counts_match_memory_model() {
    let frames = 512usize;
    let bytes = frames * 2 * 4;
    let lines = bytes.div_ceil(64) as u64;
    let inline_lines = INLINE_LINE_ACCESSES * lines;
    let pool_lines = POOL_LINE_ACCESSES * lines;
    // 512 frames → 4 KiB payload → 64 lines → inline 448, pool 192.
    assert_eq!(inline_lines, 448);
    assert_eq!(pool_lines, 192);
    assert!(inline_lines > pool_lines);
}

#[test]
fn json_emitter_escapes_and_renders() {
    let v = obj(vec![
        kv("a", JsonValue::Bool(true)),
        kv("b", JsonValue::Int(42)),
        kv("c", JsonValue::Str("x\"\n".into())),
    ]);
    assert_eq!(v.to_string(), r#"{"a":true,"b":42,"c":"x\"\n"}"#);
}

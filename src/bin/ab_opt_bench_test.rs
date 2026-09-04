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

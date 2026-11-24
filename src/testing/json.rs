// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Minimal dependency-free JSON emitter shared across profiling and benchmark binaries.
//!
//! Object keys live in a `BTreeMap`, so emitted documents are byte-deterministic.

use std::collections::BTreeMap;
use std::fmt;

/// Typed JSON node used by benchmark and PGO workload receipt writers.
#[derive(Debug, Clone)]
pub enum JsonValue {
    Bool(bool),
    Int(u64),
    Num(f64),
    Str(String),
    Arr(Vec<JsonValue>),
    Obj(BTreeMap<String, JsonValue>),
}

impl JsonValue {
    /// Constructs an empty JSON object (`{}`).
    pub fn new_obj() -> Self {
        Self::Obj(BTreeMap::new())
    }

    /// Inserts a key/value pair when `self` is [`JsonValue::Obj`].
    pub fn insert(&mut self, key: &str, value: JsonValue) {
        if let Self::Obj(map) = self {
            map.insert(key.to_string(), value);
        }
    }

    /// Appends the compact JSON serialization to `out`, formatting floating-point
    /// numbers with `decimals` fractional digits.
    pub fn render_with_decimals(&self, out: &mut String, decimals: usize) {
        match self {
            Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Self::Int(n) => out.push_str(&n.to_string()),
            Self::Num(n) => out.push_str(&format!("{n:.decimals$}")),
            Self::Str(s) => {
                out.push('"');
                for ch in s.chars() {
                    match ch {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\t' => out.push_str("\\t"),
                        c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            Self::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.render_with_decimals(out, decimals);
                }
                out.push(']');
            }
            Self::Obj(map) => {
                out.push('{');
                for (i, (key, value)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    Self::Str(key.clone()).render_with_decimals(out, decimals);
                    out.push(':');
                    value.render_with_decimals(out, decimals);
                }
                out.push('}');
            }
        }
    }

    /// Serializes to compact JSON with 2 decimal places for floats.
    pub fn to_json_string(&self) -> String {
        self.to_json_string_with_decimals(2)
    }

    /// Serializes to compact JSON with a caller-specified float decimal precision.
    pub fn to_json_string_with_decimals(&self, decimals: usize) -> String {
        let mut out = String::new();
        self.render_with_decimals(&mut out, decimals);
        out
    }
}

impl fmt::Display for JsonValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        self.render_with_decimals(&mut out, 2);
        f.write_str(&out)
    }
}

/// Builds a `(key, value)` pair for [`obj`].
pub fn kv(key: &str, value: JsonValue) -> (String, JsonValue) {
    (key.to_string(), value)
}

/// Constructs a [`JsonValue::Obj`] from key/value pairs.
pub fn obj(items: Vec<(String, JsonValue)>) -> JsonValue {
    JsonValue::Obj(items.into_iter().collect())
}

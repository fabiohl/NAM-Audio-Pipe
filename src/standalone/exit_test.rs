// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unit tests for the runtime error exit-code contract ([`super::exit`]).

use super::*;

#[test]
fn exit_codes_are_distinct_and_match_the_documented_table() {
    assert_eq!(ExitKind::Usage.code(), 2);
    assert_eq!(ExitKind::Model.code(), 3);
    assert_eq!(ExitKind::CabIr.code(), 4);
    assert_eq!(ExitKind::Backend.code(), 5);
    assert_eq!(ExitKind::Recording.code(), 6);
    // Generic/forced failures keep the legacy code and never collide with a class.
    assert_eq!(EXIT_GENERIC, 1);
    for kind in [
        ExitKind::Usage,
        ExitKind::Model,
        ExitKind::CabIr,
        ExitKind::Backend,
        ExitKind::Recording,
    ] {
        assert_ne!(kind.code(), 0, "no failure class may report success");
        assert_ne!(
            kind.code(),
            EXIT_GENERIC,
            "no failure class may reuse the generic code"
        );
    }
}

#[test]
fn labels_are_human_readable() {
    assert_eq!(ExitKind::Usage.label(), "Usage");
    assert_eq!(ExitKind::Model.label(), "Model");
    assert_eq!(ExitKind::CabIr.label(), "Cab IR");
    assert_eq!(ExitKind::Backend.label(), "Backend");
    assert_eq!(ExitKind::Recording.label(), "Recording");
}

#[test]
fn format_runtime_error_prints_kind_message_and_hint_without_help_pointer() {
    // Colors are disabled by default in tests, so no ANSI suffixes leak in.
    let rendered = format_runtime_error(
        ExitKind::Model,
        "Model load failed: bad file",
        "Re-export the model.",
    );
    assert!(rendered.contains("❌ Model error:"), "got: {rendered}");
    assert!(rendered.contains("Model load failed: bad file"));
    assert!(
        rendered.contains("💡 Re-export the model."),
        "hint must be actionable, got: {rendered}"
    );
    // Runtime errors are never fixed by reading `--help`.
    assert!(
        !rendered.contains("Use '-h'"),
        "runtime errors must not suggest -h, got: {rendered}"
    );
}

#[test]
fn empty_hint_omits_the_hint_line() {
    let rendered = format_runtime_error(ExitKind::Backend, "backend down", "");
    assert!(rendered.contains("❌ Backend error: backend down"));
    assert!(
        !rendered.contains('💡'),
        "an empty hint must not emit a dangling bullet, got: {rendered}"
    );
    assert_eq!(
        rendered.lines().count(),
        1,
        "no hint means a single line, got: {rendered}"
    );
}

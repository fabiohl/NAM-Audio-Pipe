// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::compute_build_status;

#[test]
fn test_compute_build_status_strict_fatal_gaps() {
    let gaps = vec!["pgo_not_applied".to_string()];
    let (status, fatal) = compute_build_status(&gaps, "skipped", true);
    assert_eq!(status, "FAILED");
    assert_eq!(fatal, vec!["pgo_not_applied"]);

    let gaps = vec!["bolt_not_applied".to_string()];
    let (status, fatal) = compute_build_status(&gaps, "unverified:missing", true);
    assert_eq!(status, "FAILED");
    assert_eq!(fatal, vec!["bolt_not_applied"]);
}

#[test]
fn test_compute_build_status_fallback_plain() {
    let gaps = vec![
        "bolt_not_applied".to_string(),
        "bolt_not_proven".to_string(),
        "pgo_not_applied".to_string(),
    ];
    let (status, fatal) = compute_build_status(&gaps, "fallback_plain", true);
    assert_eq!(status, "PASS_WITH_GAPS");
    assert!(fatal.is_empty());

    let gaps = vec!["bolt_not_applied".to_string(), "tool_missing".to_string()];
    let (status, fatal) = compute_build_status(&gaps, "fallback_plain", true);
    assert_eq!(status, "FAILED");
    assert_eq!(fatal, vec!["tool_missing"]);
}

#[test]
fn test_compute_build_status_fallback_pgo_only() {
    let gaps = vec![
        "bolt_not_applied".to_string(),
        "bolt_not_proven".to_string(),
    ];
    let (status, fatal) = compute_build_status(&gaps, "fallback_pgo_only", true);
    assert_eq!(status, "PASS_WITH_GAPS");
    assert!(fatal.is_empty());

    let gaps = vec!["bolt_not_proven".to_string(), "pgo_not_applied".to_string()];
    let (status, fatal) = compute_build_status(&gaps, "fallback_pgo_only", true);
    assert_eq!(status, "FAILED");
    assert_eq!(fatal, vec!["pgo_not_applied"]);
}

#[test]
fn test_compute_build_status_non_strict() {
    let gaps = vec!["pgo_not_applied".to_string()];
    let (status, fatal) = compute_build_status(&gaps, "skipped", false);
    assert_eq!(status, "PASS_WITH_GAPS");
    assert_eq!(fatal, vec!["pgo_not_applied"]);
}

#[test]
fn test_compute_build_status_clean_pass() {
    let gaps = Vec::new();
    let (status, fatal) = compute_build_status(&gaps, "applied", true);
    assert_eq!(status, "PASSED");
    assert!(fatal.is_empty());
}

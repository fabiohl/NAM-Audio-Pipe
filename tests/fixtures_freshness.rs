// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#![cfg(feature = "testing")]

//! Freshness gate for the committed NAM-Audio-Pipe fixtures.
//!
//! Drives the engine's shared `testing::freshness` checker against this
//! repository's `tests/fixtures/.manifest.sha256`. The Pipe ships models, a
//! CabSim IR and log fixtures that the offline parity/soak harnesses consume;
//! this gate keeps them attested and self-contained (no sibling-repository
//! path is ever consulted).

use neural_amp_modeler_rs::testing::freshness::{FreshnessMode, FreshnessReason, check_freshness};
use std::path::PathBuf;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nam-audio-pipe-fixture-freshness-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("tests/fixtures/models")).expect("create temp fixture tree");
    dir
}

#[test]
fn committed_fixtures_are_fresh() {
    let outcome = check_freshness(manifest_dir(), FreshnessMode::HardFail)
        .expect("the committed .manifest.sha256 must be present and parseable");
    assert!(
        outcome.is_ok(),
        "committed fixtures drifted from .manifest.sha256: reason={}, missing={:?}, stale={:?}, orphans={:?}, generator_drift={:?}",
        outcome.reason,
        outcome.missing,
        outcome.stale,
        outcome.orphans,
        outcome.generator_drift,
    );
}

#[test]
fn absent_fixtures_fail_closed() {
    let root = temp_root("fail-closed");
    std::fs::copy(
        manifest_dir().join("tests/fixtures/.manifest.sha256"),
        root.join("tests/fixtures/.manifest.sha256"),
    )
    .expect("copy manifest into temp root");

    let outcome =
        check_freshness(&root, FreshnessMode::HardFail).expect("manifest parses in temp root");
    assert!(!outcome.is_ok());
    assert_eq!(outcome.reason, FreshnessReason::MissingFixtures);
    assert!(!outcome.missing.is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

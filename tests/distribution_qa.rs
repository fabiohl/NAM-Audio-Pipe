// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! ER-5 distribution QA & release audit harness (T5.6).
//!
//! Certifies that the release pipeline operates fail-closed. Four acceptance
//! groups (findings F-RB-012 .. F-RB-015):
//!
//! * **(a) AppStream structural validation** — the repo metainfo must parse
//!   under a strict XML parser (quick-xml, end-tag/comment checking enabled)
//!   and its `<release>` entry must carry a version matching the crate plus a
//!   mandatory date. Any malformed document — such as the duplicated
//!   `</release>` closing tag fixed in F-RB-012 — must be detected and
//!   rejected.
//! * **(b) Typed test-receipt validator** — a fail-closed parser for libtest
//!   logs: a mandatory target that was removed/renamed/filtered out, a target
//!   section that executed zero tests (100% `#[ignore]` selection), or an
//!   *untyped* skip (free-text `SKIP:` without a `TEST_RESULT[...]=SKIP:`
//!   marker) must fail validation. Mirrors `utils/_lib.sh` `assert_ran_target`
//!   and the Phase 4 skip contract in Rust (F-RB-015).
//! * **(c) Provenance integrity** — `target/logs/release-provenance.json`
//!   (F-RB-014 / T5.5) is read and every referenced artifact must exist on
//!   disk with a SHA-256 that matches the recorded hash byte-for-byte. A
//!   missing receipt is a *typed* skip (no release was built); a present but
//!   corrupt receipt is a hard failure.
//! * **(d) Distribution binary smoke** — the `--profile dist` (panic = "abort",
//!   stripped) binary is exercised as a subprocess: `--diagnose` must exit 0,
//!   emit the diagnostic bundle and show no crash artifacts, and `--help` must
//!   exit 0. The artifact is located at `target/dist/nam-audio-pipe`, the
//!   installed `~/.local/bin/nam-audio-pipe`, or `$NAM_DIST_BIN`; its absence
//!   is a *typed* skip.

mod common;

use common::{DirGuard, temp_dir};
use quick_xml::Reader;
use quick_xml::events::Event;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

/// AppStream metainfo location relative to the crate root (F-RB-012 / T5.1).
const METAINFO_REL: &str = "packaging/flatpak/io.github.fabiohl.NAMAudioPipe.metainfo.xml";
/// Cryptographic chain-of-custody receipt produced by `utils/build-release.sh`
/// (F-RB-014 / T5.5).
const PROVENANCE_REL: &str = "target/logs/release-provenance.json";

// ER-6 / T6.6 certification audit constants (G-RB-002, G-RB-003). The long
// audit suite is a human-operator-only runner; this harness validates it
// structurally — never by executing the full suite.

/// Long-audit runner location relative to the crate root.
const LONG_SUITE_REL: &str = "utils/tests-long.sh";
/// Structured long-suite receipt produced by `utils/tests-long.sh`.
const LONG_RECEIPT_REL: &str = "target/logs/long-receipt.txt";
/// The five canonical long-audit phases (G-RB-002 / T6.3).
const LONG_PHASE_IDS: [&str; 5] = ["PHASE1", "PHASE2", "PHASE3", "PHASE4", "PHASE5"];
/// Canonical `run_phase` names the runner must declare verbatim.
const LONG_PHASE_NAMES: [&str; 5] = [
    "Phase 1: Soak prolongado & concorrência de swaps",
    "Phase 2: RT-Safety heap-audit (zero-alloc)",
    "Phase 3: RT Deadline gate (nanosecond budget)",
    "Phase 4: RT Jitter gate (inter-callback dispersion)",
    "Phase 5: Concurrency model checking & resilience",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

// ---------------------------------------------------------------------------
// (a) AppStream structural validation (F-RB-012)
// ---------------------------------------------------------------------------

/// Runs a strict quick-xml parse over `xml` and returns every structural
/// error. quick-xml's defaults already enforce well-formedness: end-tag names
/// must match their open tag (`check_end_names`), comments are syntax-checked
/// (`check_comments`) and an unbalanced close — like the duplicate
/// `</release>` from F-RB-012 — is rejected with a typed error. The element
/// nesting depth is tracked additionally so that a document that reaches EOF
/// with an unclosed root element (truncation) is rejected too.
fn strict_xml_errors(xml: &str) -> Vec<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut errors = Vec::new();
    let mut depth = 0usize;
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => {
                if depth != 0 {
                    errors.push(format!(
                        "document ended with {depth} unclosed element(s) (truncated XML)"
                    ));
                }
                break;
            }
            Ok(Event::Start(_)) => depth += 1,
            Ok(Event::End(_)) => depth = depth.saturating_sub(1),
            Ok(_) => {}
            Err(e) => {
                errors.push(format!("{e} at byte {}", reader.error_position()));
                break;
            }
        }
    }
    errors
}

/// Extracts the `(version, date)` attributes of every `<release>` element.
///
/// Returns `Err` with the structural errors when the document is not
/// well-formed — the attribute walk never runs on a malformed tree.
fn parse_release_entries(xml: &str) -> Result<Vec<(String, String)>, Vec<String>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut errors = Vec::new();
    let mut releases = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) if e.name().as_ref() == b"release" => {
                let mut version = String::new();
                let mut date = String::new();
                for attr in e.attributes() {
                    let attr = match attr {
                        Ok(a) => a,
                        Err(err) => {
                            errors.push(format!("<release> attribute error: {err}"));
                            continue;
                        }
                    };
                    match attr.key.as_ref() {
                        b"version" => {
                            version = attr
                                .normalized_value(quick_xml::XmlVersion::Explicit1_0)
                                .map(|c| c.into_owned())
                                .unwrap_or_default();
                        }
                        b"date" => {
                            date = attr
                                .normalized_value(quick_xml::XmlVersion::Explicit1_0)
                                .map(|c| c.into_owned())
                                .unwrap_or_default();
                        }
                        _ => {}
                    }
                }
                releases.push((version, date));
            }
            Ok(_) => {}
            Err(e) => {
                errors.push(format!("{e} at byte {}", reader.error_position()));
                break;
            }
        }
    }
    if errors.is_empty() {
        Ok(releases)
    } else {
        Err(errors)
    }
}

/// (a) Acceptance: the shipped metainfo must be well-formed under a strict XML
/// parser, expose exactly one `<release>` whose version equals the crate
/// version and carry the mandatory `date` attribute (F-RB-012 acceptance:
/// "XML corrigido, appstreamcli zero").
#[test]
fn appstream_metainfo_is_strictly_well_formed_and_version_synced() {
    let path = repo_root().join(METAINFO_REL);
    let xml = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read metainfo {}: {e}", path.display()));

    let errors = strict_xml_errors(&xml);
    assert!(
        errors.is_empty(),
        "AppStream metainfo must be well-formed XML; strict parser rejected it: {errors:?}"
    );

    let releases = parse_release_entries(&xml)
        .unwrap_or_else(|errs| panic!("metainfo failed strict parse: {errs:?}"));
    assert_eq!(
        releases.len(),
        1,
        "exactly one <release> entry expected (found {})",
        releases.len()
    );

    let (version, date) = &releases[0];
    assert_eq!(
        version,
        env!("CARGO_PKG_VERSION"),
        "<release version> must be kept in sync with Cargo.toml"
    );
    assert!(
        !date.is_empty(),
        "<release> is missing the mandatory date attribute (F-RB-012 semantic gate)"
    );
}

/// (a) Negative acceptance: a malformed document carrying the duplicated
/// `</release>` closing tag from F-RB-012 (`packaging/flatpak/...metainfo.xml`
/// lines 55-59 before the fix) must be detected and rejected by the strict
/// parser — no gate may turn green under structurally invalid metadata.
#[test]
fn appstream_metainfo_rejects_duplicate_release_close_tag() {
    const MALFORMED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<component type="desktop-application">
  <id>io.github.fabiohl.NAMAudioPipe.desktop</id>
  <releases>
    <release version="0.7.0" date="2026-08-24" type="stable">
      <description>
        <p>This release must be rejected.</p>
      </description>
    </release>
    </release>
  </releases>
</component>
"#;

    let errors = strict_xml_errors(MALFORMED);
    assert!(
        !errors.is_empty(),
        "duplicate </release> closing tag must be rejected by the strict XML parser"
    );
}

/// (a) Negative acceptance: truncating the document (unclosed root element)
/// is equally rejected — the parser must never accept a partial tree.
#[test]
fn appstream_metainfo_rejects_truncated_document() {
    let truncated =
        "<component type=\"desktop-application\"><id>io.github.fabiohl.NAMAudioPipe.desktop</id>";
    let errors = strict_xml_errors(truncated);
    assert!(
        !errors.is_empty(),
        "a truncated (unclosed) XML document must be rejected"
    );
}

// ---------------------------------------------------------------------------
// (b) Typed test-receipt validator (F-RB-015)
// ---------------------------------------------------------------------------

/// Extracts the `Running <target> ` banner names from a libtest log — the same
/// identity `utils/_lib.sh::assert_ran_target` greps for.
fn ran_targets(log: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in log.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("Running ") {
            let name = rest
                .split(" (")
                .next()
                .unwrap_or(rest)
                .trim_end()
                .to_string();
            if !name.is_empty() {
                out.push(name);
            }
        }
    }
    out
}

/// Parses the `N passed` / `N measured` counter from a libtest
/// `test result:` summary line.
fn parse_count(line: &str, label: &str) -> usize {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    tokens
        .windows(2)
        .find(|w| w[1].trim_end_matches([';', ',']) == label)
        .and_then(|w| w[0].parse().ok())
        .unwrap_or(0)
}

/// Executed test count (passed + measured) of the section belonging to
/// `target` in a libtest log. `None` when the target never ran.
fn target_executed_count(log: &str, target: &str) -> Option<usize> {
    let banner = format!("Running {target} ");
    let banner_line = log.lines().position(|l| l.contains(&banner))?;
    let result_line = log
        .lines()
        .skip(banner_line + 1)
        .find(|l| l.contains("test result:"))?;
    Some(parse_count(result_line, "passed") + parse_count(result_line, "measured"))
}

/// Fail-closed mandatory-target gate over a libtest log (F-RB-015): every
/// mandatory target must have executed at least one test. A target that is
/// missing entirely (removed/renamed), filtered out, or whose section ran
/// zero tests (e.g. a 100% `#[ignore]` selection) fails validation with a
/// typed reason.
fn validate_mandatory_targets(log: &str, mandatory: &[&str]) -> Result<(), String> {
    let ran = ran_targets(log);
    let mut defects = Vec::new();
    for target in mandatory {
        if !ran.iter().any(|t| t == target) {
            defects.push(format!("{target} (no 'Running {target} ' banner)"));
        } else if target_executed_count(log, target).unwrap_or(0) < 1 {
            defects.push(format!("{target} (section executed 0 tests)"));
        }
    }
    if defects.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "mandatory target gate failed: {}",
            defects.join("; ")
        ))
    }
}

/// Finds untyped skip text in a log: any `SKIP:` occurrence that is not part
/// of a typed `TEST_RESULT[...]=SKIP:reason` marker (the Phase 4 contract —
/// a skip must be structured and auditable, never free text).
fn untyped_skip_markers(log: &str) -> Vec<String> {
    log.lines()
        .filter(|l| l.contains("SKIP:") && !l.contains("TEST_RESULT["))
        .map(|l| l.trim().to_string())
        .collect()
}

/// Combined typed-receipt validator: mandatory-target presence AND skip
/// typing. A log that fails either side must never produce PASS.
fn validate_typed_receipt(log: &str, mandatory: &[&str]) -> Result<(), String> {
    validate_mandatory_targets(log, mandatory)?;
    let untyped = untyped_skip_markers(log);
    if !untyped.is_empty() {
        return Err(format!(
            "untyped skip marker(s) found (a skip must be typed TEST_RESULT[...]=SKIP:reason): {}",
            untyped.join(" | ")
        ));
    }
    Ok(())
}

/// (b) Negative: a removed/renamed mandatory target — even with other targets
/// keeping the aggregate positive — must fail the gate (F-RB-015: "remoção,
/// rename ... não pode produzir PASS").
#[test]
fn receipt_validator_detects_missing_mandatory_target() {
    let log = "\
Running unittests src/lib.rs (target/debug/deps/foo-abc123)
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
Running tests/service_resilience.rs (target/debug/deps/bar-def456)
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
";

    assert_eq!(
        validate_mandatory_targets(log, &["tests/service_resilience.rs"]),
        Ok(())
    );

    let err = validate_mandatory_targets(log, &["tests/distribution_qa.rs"]).unwrap_err();
    assert!(
        err.contains("tests/distribution_qa.rs") && err.contains("no 'Running"),
        "removed target must be reported as missing, got: {err}"
    );
}

/// (b) Negative: a target section that executed zero tests (100% `#[ignore]`
/// selection in the nominal pass) must fail the gate — presence of the banner
/// alone is not execution.
#[test]
fn receipt_validator_detects_zero_execution_target() {
    let log = "\
Running tests/recording.rs (target/debug/deps/rec-abc123)
test result: ok. 0 passed; 0 failed; 7 ignored; 0 measured; 0 filtered out
";
    let err = validate_mandatory_targets(log, &["tests/recording.rs"]).unwrap_err();
    assert!(
        err.contains("executed 0 tests"),
        "all-ignored target must fail, got: {err}"
    );
}

/// (b) Positive: every mandatory target executed and passed.
#[test]
fn receipt_validator_accepts_all_mandatory_targets() {
    let log = "\
Running tests/distribution_qa.rs (target/debug/deps/qa-abc123)
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
Running tests/e2e_cli.rs (target/debug/deps/e2e-def456)
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
";
    assert_eq!(
        validate_mandatory_targets(log, &["tests/distribution_qa.rs", "tests/e2e_cli.rs"]),
        Ok(())
    );
}

/// (b) Negative: incidental free-text `SKIP:` (the old `tests-quick.sh:152-163`
/// heuristic that greps for `SKIP:` text) must be rejected — an untyped skip
/// can never be certified as a documented skip.
#[test]
fn receipt_validator_rejects_untyped_skip() {
    let log = "\
Running tests/pw_integration.rs (target/debug/deps/pw-abc123)
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
WARN: SKIP: io_uring unsupported on this kernel
";
    let untyped = untyped_skip_markers(log);
    assert!(
        !untyped.is_empty(),
        "free-text SKIP: must be detected as an untyped skip"
    );
    let err = validate_typed_receipt(log, &["tests/pw_integration.rs"]).unwrap_err();
    assert!(
        err.contains("untyped skip marker(s)"),
        "typed receipt validation must fail on untyped skips, got: {err}"
    );
}

/// (b) Positive: a typed `TEST_RESULT[record_e2e]=SKIP:daemon_unavailable`
/// marker (the Phase 4 contract) is a *documented* skip and must not be
/// misclassified as untyped.
#[test]
fn receipt_validator_accepts_typed_skip_marker() {
    let log = "\
Running tests/recording.rs (target/debug/deps/rec-abc123)
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
TEST_RESULT[record_e2e]=SKIP:daemon_unavailable
";
    assert!(
        untyped_skip_markers(log).is_empty(),
        "typed TEST_RESULT[...]=SKIP: marker must not be flagged as untyped"
    );
    assert_eq!(validate_typed_receipt(log, &["tests/recording.rs"]), Ok(()));
}

// ---------------------------------------------------------------------------
// (c) Provenance integrity (F-RB-014 / T5.5)
// ---------------------------------------------------------------------------

fn sha256_hex(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Fail-closed validator for the release provenance receipt: every artifact
/// referenced in `artifacts` must exist on disk and its computed SHA-256 and
/// size must match the recorded values exactly. Relative paths are resolved
/// against the crate root (the receipt may be moved with the repo).
fn validate_provenance_receipt(path: &Path) -> Result<usize, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read provenance receipt {}: {e}", path.display()))?;
    let doc: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("provenance receipt is not valid JSON: {e}"))?;

    if doc.get("schema_version").and_then(|v| v.as_u64()) != Some(1) {
        return Err(format!(
            "unexpected schema_version: {:?}",
            doc.get("schema_version")
        ));
    }
    if doc.get("kind").and_then(|v| v.as_str()) != Some("release-provenance") {
        return Err(format!(
            "unexpected receipt kind: {:?}",
            doc.get("kind").and_then(|v| v.as_str())
        ));
    }

    let artifacts = doc
        .get("artifacts")
        .and_then(|v| v.as_object())
        .ok_or("'artifacts' object missing from the receipt")?;
    if artifacts.is_empty() {
        return Err("'artifacts' object is empty — a release receipt with no certified artifacts cannot be trusted".into());
    }

    let root = repo_root();
    let mut problems = Vec::new();
    for (name, art) in artifacts {
        let art = art
            .as_object()
            .ok_or_else(|| format!("artifact '{name}': expected an object"))?;
        let rec_path = art
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("artifact '{name}': missing 'path'"))?;
        let abs = PathBuf::from(rec_path);
        let abs = if abs.is_absolute() {
            abs
        } else {
            root.join(abs)
        };

        if !abs.is_file() {
            problems.push(format!(
                "'{name}': referenced file does not exist on disk: {}",
                abs.display()
            ));
            continue;
        }

        match art.get("sha256").and_then(|v| v.as_str()) {
            Some(recorded) => {
                let actual = sha256_hex(&abs)?;
                if !actual.eq_ignore_ascii_case(recorded) {
                    problems.push(format!(
                        "'{name}': SHA-256 mismatch (recorded {recorded}, computed {actual})"
                    ));
                }
            }
            None => problems.push(format!("'artifact {name}': missing 'sha256' field")),
        }

        match art.get("size_bytes").and_then(|v| v.as_u64()) {
            Some(recorded) => {
                let actual = std::fs::metadata(&abs)
                    .map_err(|e| format!("stat {}: {e}", abs.display()))?
                    .len();
                if actual != recorded {
                    problems.push(format!(
                        "'{name}': size mismatch (recorded {recorded}, actual {actual})"
                    ));
                }
            }
            None => problems.push(format!("'artifact {name}': missing 'size_bytes' field")),
        }
    }

    if !problems.is_empty() {
        return Err(format!(
            "provenance integrity violations ({}): {}",
            problems.len(),
            problems.join("; ")
        ));
    }
    Ok(artifacts.len())
}

/// Writes a minimal synthetic provenance receipt referencing `files` (as
/// `name -> path`) with the current on-disk hashes — used to exercise the
/// validator's positive/negative paths without depending on a release build.
fn write_synthetic_receipt(receipt_path: &Path, files: &[(&str, &Path)]) {
    let mut artifacts = serde_json::Map::new();
    for (name, path) in files {
        let mut art = serde_json::Map::new();
        art.insert("path".into(), path.display().to_string().into());
        art.insert(
            "sha256".into(),
            sha256_hex(path).expect("hash temp file").into(),
        );
        let size = std::fs::metadata(path).expect("stat temp file").len();
        art.insert("size_bytes".into(), size.into());
        artifacts.insert((*name).into(), art.into());
    }
    let doc = serde_json::json!({
        "schema_version": 1,
        "tool": "distribution_qa.rs",
        "kind": "release-provenance",
        "artifacts": artifacts,
    });
    std::fs::write(
        receipt_path,
        serde_json::to_vec_pretty(&doc).expect("serialize receipt"),
    )
    .expect("write synthetic receipt");
}

/// (c) Positive: a receipt whose recorded hashes match the referenced files
/// validates cleanly and reports every artifact.
#[test]
fn provenance_validator_accepts_matching_receipt() {
    let dir = temp_dir();
    let _guard = DirGuard::new(dir.clone());
    let file = dir.join("payload.bin");
    std::fs::write(&file, b"certified artifact payload").expect("write payload");
    let receipt = dir.join("release-provenance.json");
    write_synthetic_receipt(&receipt, &[("installed_binary", &file)]);

    let audited = validate_provenance_receipt(&receipt)
        .unwrap_or_else(|e| panic!("matching receipt must validate: {e}"));
    assert_eq!(audited, 1);
}

/// (c) Negative: a receipt referencing a file that does not exist on disk
/// must fail — the gate never certifies artifacts it cannot prove.
#[test]
fn provenance_validator_rejects_missing_referenced_file() {
    let dir = temp_dir();
    let _guard = DirGuard::new(dir.clone());
    let ghost = dir.join("does-not-exist.bin");
    std::fs::write(&ghost, b"present at receipt time").expect("write ghost file");
    let receipt = dir.join("release-provenance.json");
    write_synthetic_receipt(&receipt, &[("installed_binary", &ghost)]);
    std::fs::remove_file(&ghost).expect("remove ghost file to simulate a vanished artifact");

    let err = validate_provenance_receipt(&receipt).unwrap_err();
    assert!(
        err.contains("does not exist on disk") && err.contains("does-not-exist.bin"),
        "missing referenced artifact must be reported, got: {err}"
    );
}

/// (c) Negative: a tampered SHA-256 (corrupted artifact or forged receipt)
/// must fail — the recorded hash is binding.
#[test]
fn provenance_validator_rejects_tampered_hash() {
    let dir = temp_dir();
    let _guard = DirGuard::new(dir.clone());
    let file = dir.join("payload.bin");
    std::fs::write(&file, b"original bytes").expect("write payload");
    let receipt = dir.join("release-provenance.json");
    write_synthetic_receipt(&receipt, &[("installed_binary", &file)]);

    std::fs::write(&file, b"tampered bytes").expect("tamper payload");
    let err = validate_provenance_receipt(&receipt).unwrap_err();
    assert!(
        err.contains("SHA-256 mismatch"),
        "hash divergence must be reported, got: {err}"
    );
}

/// (c) Live acceptance: the release pipeline receipt (when present) must have
/// every artifact on disk with matching hashes. Absence of the receipt is a
/// *typed* skip (no release was built — nothing to audit); a present but
/// corrupt receipt is a hard failure (F-RB-014 rollback: divergence blocks
/// the release immediately).
#[test]
fn provenance_receipt_integrity() {
    let path = repo_root().join(PROVENANCE_REL);
    if !path.is_file() {
        eprintln!(
            "TEST_RESULT[provenance_integrity]=SKIP:receipt_not_found ({} absent; build a release with utils/build-release.sh or stage a simulated receipt)",
            path.display()
        );
        return;
    }
    let audited = validate_provenance_receipt(&path)
        .unwrap_or_else(|e| panic!("release provenance receipt is corrupt: {e}"));
    eprintln!(
        "TEST_RESULT[provenance_integrity]=PASS artifacts={audited} receipt={}",
        path.display()
    );
}

// ---------------------------------------------------------------------------
// (d) Distribution binary smoke (F-RB-014 / T5.4)
// ---------------------------------------------------------------------------

/// Candidate locations of the `--profile dist` (panic = "abort", stripped)
/// binary, in preference order: explicit override, cargo's standard dist
/// output, the atomically installed binary, then the PGO/BOLT build tree.
fn dist_bin_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(override_path) = std::env::var("NAM_DIST_BIN") {
        out.push(PathBuf::from(override_path));
    }
    let root = repo_root();
    out.push(root.join("target/dist/nam-audio-pipe"));
    if let Some(home) = std::env::var_os("HOME") {
        out.push(PathBuf::from(home).join(".local/bin/nam-audio-pipe"));
    }
    out.push(root.join("target/pgo-build/dist/nam-audio-pipe"));
    out
}

/// (d) Acceptance: the distribution binary — compiled with `--profile dist`
/// (`panic = "abort"`, `strip = true`, LTO fat) and therefore the exact
/// artifact shipped by `utils/build-release.sh` — must run stably as a
/// subprocess and exit cleanly with code 0: `--diagnose` must emit the
/// diagnostic bundle and neither stdout nor stderr may carry crash artifacts,
/// and `--help` must print usage. Absence of the artifact is a *typed* skip.
#[test]
fn dist_binary_smoke_under_panic_abort_profile() {
    let Some(bin) = dist_bin_candidates().into_iter().find(|p| p.is_file()) else {
        eprintln!(
            "TEST_RESULT[dist_bin_smoke]=SKIP:dist_binary_not_found (expected at target/dist/nam-audio-pipe, ~/.local/bin/nam-audio-pipe or $NAM_DIST_BIN; run cargo build --profile dist or ./utils/build-release.sh)"
        );
        return;
    };

    let diagnose = Command::new(&bin)
        .arg("--diagnose")
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn dist binary {}: {e}", bin.display()));
    assert!(
        diagnose.status.success(),
        "dist binary --diagnose must exit 0 under the panic=abort profile (got {:?})",
        diagnose.status.code()
    );

    let stdout = String::from_utf8_lossy(&diagnose.stdout);
    let stderr = String::from_utf8_lossy(&diagnose.stderr);
    assert!(
        stdout.contains("NeuralAmpModeler-rs Diagnostic")
            || stdout.contains("NAM-rs Diagnostic")
            || stdout.contains("System Information")
            || stdout.contains("Runtime State"),
        "--diagnose should emit the diagnostic bundle, got first lines: {}",
        stdout.lines().take(3).collect::<Vec<_>>().join(" | ")
    );

    let combined = format!("{stdout}\n{stderr}");
    for artifact in ["panicked", "Segmentation fault", "core dumped"] {
        assert!(
            !combined.contains(artifact),
            "dist binary --diagnose output carries a crash artifact: {artifact:?}"
        );
    }

    let help = Command::new(&bin)
        .arg("--help")
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn dist binary {}: {e}", bin.display()));
    assert!(
        help.status.success(),
        "dist binary --help must exit 0 (got {:?})",
        help.status.code()
    );
    let help_out = String::from_utf8_lossy(&help.stdout);
    assert!(
        help_out.contains("Usage:"),
        "dist binary --help should print usage"
    );

    eprintln!("TEST_RESULT[dist_bin_smoke]=PASS bin={}", bin.display());
}

// ---------------------------------------------------------------------------
// (e) ER-6 certification infrastructure audit (G-RB-002 / G-RB-003, T6.6)
// ---------------------------------------------------------------------------

/// (e) Acceptance: the long-audit runner `utils/tests-long.sh` must be a
/// real, executable, licensed and fully specified artifact. The ER-6 closing
/// gates depend on it being runnable by a human operator (and never silently
/// replaced by a non-executable stub), so this structural audit is mandatory:
/// `+x` permissions, `GPL-3.0-or-later` SPDX header, the AI-safety governance
/// warning (human-operator-only execution), all 5 canonical phases declared
/// verbatim, and the `--strict-pre-release` / `--simulate` argument parsing.
#[test]
fn long_suite_script_is_executable_and_fully_specified() {
    let path = repo_root().join(LONG_SUITE_REL);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let meta =
        std::fs::metadata(&path).unwrap_or_else(|e| panic!("cannot stat {}: {e}", path.display()));
    assert!(meta.is_file(), "{} must be a regular file", path.display());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "{} must carry the executable bit (+x) — the human-operator gate depends on it",
            path.display()
        );
    }

    let first = text.lines().next().unwrap_or("");
    assert!(
        first == "#!/bin/bash",
        "{} must start with a bash shebang, got {first:?}",
        path.display()
    );
    assert!(
        text.contains("SPDX-License-Identifier: GPL-3.0-or-later"),
        "{} must carry the GPL-3.0-or-later SPDX header",
        path.display()
    );
    assert!(
        text.contains("AI AGENTS MUST NEVER EXECUTE THIS SCRIPT DIRECTLY"),
        "{} must carry the mandatory AI-safety governance warning (execution reserved for the human operator)",
        path.display()
    );

    for id in LONG_PHASE_IDS {
        assert!(
            text.contains(&format!("finish_phase \"{id}\"")),
            "{} must close the canonical phase {id} through finish_phase",
            path.display()
        );
    }
    for name in LONG_PHASE_NAMES {
        assert!(
            text.contains(name),
            "{} must declare the canonical phase {name:?}",
            path.display()
        );
    }
    assert!(
        text.contains("--strict-pre-release") && text.contains("STRICT_PRE_RELEASE=1"),
        "{} must parse --strict-pre-release (promotes every GAP to a hard release-blocking failure)",
        path.display()
    );
    assert!(
        text.contains("--simulate") && text.contains("SIMULATE=1"),
        "{} must parse --simulate/--dry-run (the safe non-executing structural surface)",
        path.display()
    );
}

/// (e) Live structural surface: `utils/tests-long.sh --help` is the sanctioned
/// read-only surface for AI/CI structural validation (the runner itself is
/// human-operator-only). It must exit 0 and inventory all 5 canonical phases
/// plus the `--strict-pre-release` / `--simulate` options; an unknown option
/// must be rejected fail-closed with exit code 2 (never silently accepted).
#[test]
fn long_suite_help_surface_is_parseable_and_fail_closed() {
    let path = repo_root().join(LONG_SUITE_REL);
    let help = Command::new(&path)
        .arg("--help")
        .output()
        .unwrap_or_else(|e| panic!("failed to execute {} --help: {e}", path.display()));
    assert!(
        help.status.success(),
        "{} --help must exit 0 (got {:?})",
        path.display(),
        help.status.code()
    );
    let out = String::from_utf8_lossy(&help.stdout);
    assert!(
        out.contains("--strict-pre-release") && out.contains("--simulate"),
        "--help must document the --strict-pre-release and --simulate options"
    );
    for id in LONG_PHASE_IDS {
        assert!(
            out.contains(id),
            "--help must inventory the canonical phase {id}"
        );
    }

    let bad = Command::new(&path)
        .arg("--definitely-not-an-option")
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "failed to execute {} with an unknown option: {e}",
                path.display()
            )
        });
    assert_eq!(
        bad.status.code(),
        Some(2),
        "an unknown option must be rejected fail-closed with exit code 2 (got {:?})",
        bad.status.code()
    );
}

/// Phase verdict of a long-suite receipt line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LongPhaseStatus {
    Pass,
    Fail,
    Gap,
    Simulated,
}

/// One `PHASEn: <status> log=... duration_ms=...` line of the long receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LongPhaseResult {
    id: String,
    status: LongPhaseStatus,
    log: String,
    duration_ms: Option<u64>,
}

/// Fail-closed parsed representation of `target/logs/long-receipt.txt`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LongReceipt {
    suite: String,
    strict: bool,
    mode: String,
    phases: Vec<LongPhaseResult>,
    gaps: Vec<String>,
    overall: String,
}

/// Parses one `PHASEn: <status> log=... duration_ms=...` line. `None` when the
/// line is not a phase line or is structurally malformed (unknown status
/// token, missing `log=` attribute).
fn parse_long_phase(line: &str) -> Option<(String, LongPhaseStatus, String, Option<u64>)> {
    let rest = line.strip_prefix("PHASE")?;
    let (num, tail) = rest.split_once(':')?;
    let id = format!("PHASE{num}");
    let mut tokens = tail.split_whitespace();
    let status = match tokens.next()? {
        "PASS" => LongPhaseStatus::Pass,
        "FAIL" => LongPhaseStatus::Fail,
        "GAP" => LongPhaseStatus::Gap,
        "SIMULATED" => LongPhaseStatus::Simulated,
        _ => return None,
    };
    let mut log = String::new();
    let mut duration_ms = None;
    for tok in tokens {
        if let Some(v) = tok.strip_prefix("log=") {
            log = v.to_string();
        } else if let Some(v) = tok.strip_prefix("duration_ms=") {
            duration_ms = v.parse().ok();
        }
    }
    Some((id, status, log, duration_ms))
}

/// Fail-closed parser for the long-suite receipt format
/// (`target/logs/long-receipt.txt`, emitted by `utils/tests-long.sh`):
///
/// ```text
/// SUITE: tests-long
/// STRICT: 0
/// MODE: simulate|full
/// PHASE1: PASS|FAIL|GAP|SIMULATED log=target/logs/... [duration_ms=N]
/// ... (PHASE2..PHASE5)
/// GAP: <typed reason>            (zero or more)
/// OVERALL: PASSED|FAILED|COMPLETED_WITH_GAPS|SIMULATED
/// ```
///
/// Unknown line types, duplicated mandatory fields, invalid values, a missing
/// mandatory phase or a missing `OVERALL:` verdict are all rejected — a receipt
/// that cannot be parsed fail-closed must never certify anything.
fn parse_long_receipt(text: &str) -> Result<LongReceipt, String> {
    let mut suite: Option<String> = None;
    let mut strict: Option<bool> = None;
    let mut mode: Option<String> = None;
    let mut phases: Vec<LongPhaseResult> = Vec::new();
    let mut gaps: Vec<String> = Vec::new();
    let mut overall: Option<String> = None;

    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let lineno = idx + 1;
        if let Some(v) = line.strip_prefix("SUITE:") {
            let v = v.trim();
            if suite.replace(v.to_string()).is_some() {
                return Err(format!("line {lineno}: duplicate SUITE: line"));
            }
        } else if let Some(v) = line.strip_prefix("STRICT:") {
            let parsed = match v.trim() {
                "0" => false,
                "1" => true,
                other => {
                    return Err(format!(
                        "line {lineno}: invalid STRICT: value {other:?} (expected 0 or 1)"
                    ));
                }
            };
            if strict.replace(parsed).is_some() {
                return Err(format!("line {lineno}: duplicate STRICT: line"));
            }
        } else if let Some(v) = line.strip_prefix("MODE:") {
            let v = v.trim();
            if v != "simulate" && v != "full" {
                return Err(format!(
                    "line {lineno}: invalid MODE: value {v:?} (expected simulate or full)"
                ));
            }
            if mode.replace(v.to_string()).is_some() {
                return Err(format!("line {lineno}: duplicate MODE: line"));
            }
        } else if let Some(v) = line.strip_prefix("OVERALL:") {
            let v = v.trim();
            if !matches!(v, "PASSED" | "FAILED" | "COMPLETED_WITH_GAPS" | "SIMULATED") {
                return Err(format!("line {lineno}: invalid OVERALL: verdict {v:?}"));
            }
            if overall.replace(v.to_string()).is_some() {
                return Err(format!("line {lineno}: duplicate OVERALL: line"));
            }
        } else if let Some(v) = line.strip_prefix("GAP:") {
            gaps.push(v.trim().to_string());
        } else if line.starts_with("PHASE") {
            let (id, status, log, duration_ms) = parse_long_phase(line)
                .ok_or_else(|| format!("line {lineno}: malformed phase line: {line:?}"))?;
            phases.push(LongPhaseResult {
                id,
                status,
                log,
                duration_ms,
            });
        } else {
            return Err(format!(
                "line {lineno}: unrecognized receipt line: {line:?}"
            ));
        }
    }

    let suite = suite.ok_or("missing SUITE: line")?;
    let strict = strict.ok_or("missing STRICT: line")?;
    let mode = mode.ok_or("missing MODE: line")?;
    let overall = overall.ok_or("missing OVERALL: line")?;

    for n in 1..=5 {
        let id = format!("PHASE{n}");
        if !phases.iter().any(|p| p.id == id) {
            return Err(format!("missing mandatory phase {id} in receipt"));
        }
    }

    Ok(LongReceipt {
        suite,
        strict,
        mode,
        phases,
        gaps,
        overall,
    })
}

impl LongReceipt {
    /// Semantic consistency audit (fail-closed): the `OVERALL:` verdict must
    /// match the phase/gap evidence collected in the same receipt, mirroring
    /// the runner's verdict logic — a `PASSED` receipt may not hide a GAP
    /// phase, a `COMPLETED_WITH_GAPS` must be backed by gap evidence, and a
    /// simulate run must close as `SIMULATED` with only simulated phases.
    fn audit(&self) -> Result<(), String> {
        if self.mode == "simulate" {
            if self.overall != "SIMULATED" {
                return Err(format!(
                    "simulate receipt must close with OVERALL: SIMULATED, got {:?}",
                    self.overall
                ));
            }
            if !self
                .phases
                .iter()
                .all(|p| p.status == LongPhaseStatus::Simulated)
            {
                return Err("simulate receipt must carry only SIMULATED phase statuses".into());
            }
            return Ok(());
        }

        let has_fail = self
            .phases
            .iter()
            .any(|p| p.status == LongPhaseStatus::Fail);
        let has_gap =
            self.phases.iter().any(|p| p.status == LongPhaseStatus::Gap) || !self.gaps.is_empty();
        match self.overall.as_str() {
            "PASSED" if !has_fail && !has_gap => Ok(()),
            "FAILED" if has_fail => Ok(()),
            "COMPLETED_WITH_GAPS" if has_gap && !has_fail => Ok(()),
            other => Err(format!(
                "inconsistent receipt: OVERALL={other} (fail_phase={has_fail}, gap_evidence={has_gap})"
            )),
        }
    }
}

/// (e) Positive: the `--simulate` receipt format (the sanctioned CI structural
/// surface) parses and passes the semantic audit.
#[test]
fn long_receipt_parser_accepts_simulated_receipt() {
    const RECEIPT: &str = "\
SUITE: tests-long
STRICT: 0
MODE: simulate
PHASE1: SIMULATED log=target/logs/phase1-soak.log
PHASE2: SIMULATED log=target/logs/phase2-heap-audit.log
PHASE3: SIMULATED log=target/logs/phase3-rt-deadline.log
PHASE4: SIMULATED log=target/logs/phase4-rt-jitter.log
PHASE5: SIMULATED log=target/logs/phase5-concurrency.log
OVERALL: SIMULATED
";

    let receipt =
        parse_long_receipt(RECEIPT).unwrap_or_else(|e| panic!("simulated receipt must parse: {e}"));
    assert_eq!(receipt.suite, "tests-long");
    assert!(!receipt.strict);
    assert_eq!(receipt.mode, "simulate");
    assert_eq!(receipt.phases.len(), 5);
    assert!(
        receipt
            .phases
            .iter()
            .all(|p| p.status == LongPhaseStatus::Simulated)
    );
    assert!(receipt.gaps.is_empty());
    assert_eq!(receipt.overall, "SIMULATED");
    receipt
        .audit()
        .unwrap_or_else(|e| panic!("simulated receipt must audit cleanly: {e}"));
}

/// (e) Positive: GAPs in the receipt are detected structurally — both the
/// typed `GAP:` lines and the `PHASEn: GAP` phase statuses — and the
/// `COMPLETED_WITH_GAPS` verdict is accepted as consistent. A GAP must never
/// be silently dropped by the parser.
#[test]
fn long_receipt_parser_detects_gaps_fail_closed() {
    const RECEIPT: &str = "\
SUITE: tests-long
STRICT: 0
MODE: full
PHASE1: PASS log=target/logs/phase1-soak.log duration_ms=120000
PHASE2: PASS log=target/logs/phase2-heap-audit.log duration_ms=90000
PHASE3: GAP log=target/logs/phase3-rt-deadline.log duration_ms=1200
PHASE4: PASS log=target/logs/phase4-rt-jitter.log duration_ms=1500
PHASE5: PASS log=target/logs/phase5-concurrency.log duration_ms=3000
GAP: phase3:rt_metrics_harness_missing (T6.5 pending)
OVERALL: COMPLETED_WITH_GAPS
";

    let receipt =
        parse_long_receipt(RECEIPT).unwrap_or_else(|e| panic!("gap receipt must parse: {e}"));
    assert_eq!(receipt.gaps.len(), 1);
    assert!(receipt.gaps[0].contains("phase3:rt_metrics_harness_missing"));
    let phase3 = receipt
        .phases
        .iter()
        .find(|p| p.id == "PHASE3")
        .expect("PHASE3 must be present");
    assert_eq!(phase3.status, LongPhaseStatus::Gap);
    assert_eq!(receipt.overall, "COMPLETED_WITH_GAPS");
    receipt
        .audit()
        .unwrap_or_else(|e| panic!("gap receipt must audit cleanly: {e}"));
}

/// (e) Negative: a receipt claiming `OVERALL: PASSED` while carrying a GAP
/// phase and a `GAP:` line must fail the semantic audit — a green verdict can
/// never hide declared gaps.
#[test]
fn long_receipt_audit_rejects_passed_verdict_with_gap_evidence() {
    const RECEIPT: &str = "\
SUITE: tests-long
STRICT: 0
MODE: full
PHASE1: PASS log=target/logs/phase1-soak.log
PHASE2: PASS log=target/logs/phase2-heap-audit.log
PHASE3: GAP log=target/logs/phase3-rt-deadline.log
PHASE4: PASS log=target/logs/phase4-rt-jitter.log
PHASE5: PASS log=target/logs/phase5-concurrency.log
GAP: phase4:no_typed_jitter_result
OVERALL: PASSED
";

    let receipt = parse_long_receipt(RECEIPT).unwrap_or_else(|e| panic!("receipt must parse: {e}"));
    let err = receipt.audit().unwrap_err();
    assert!(
        err.contains("inconsistent receipt") && err.contains("gap_evidence=true"),
        "PASSED verdict with gap evidence must fail the audit, got: {err}"
    );
}

/// (e) Negative: an unknown line type must be rejected — the parser is
/// fail-closed and never tolerates drift in the receipt format.
#[test]
fn long_receipt_parser_rejects_unknown_lines() {
    const RECEIPT: &str = "\
SUITE: tests-long
STRICT: 0
MODE: simulate
PHASE1: SIMULATED log=target/logs/phase1-soak.log
PHASE2: SIMULATED log=target/logs/phase2-heap-audit.log
PHASE3: SIMULATED log=target/logs/phase3-rt-deadline.log
PHASE4: SIMULATED log=target/logs/phase4-rt-jitter.log
PHASE5: SIMULATED log=target/logs/phase5-concurrency.log
FOO: bar
OVERALL: SIMULATED
";
    let err = parse_long_receipt(RECEIPT).unwrap_err();
    assert!(
        err.contains("FOO") && err.contains("unrecognized receipt line"),
        "unknown line must be rejected, got: {err}"
    );
}

/// (e) Negative: a missing `OVERALL:` verdict must be rejected — a receipt
/// without a closing verdict can never certify anything.
#[test]
fn long_receipt_parser_rejects_missing_overall() {
    const RECEIPT: &str = "\
SUITE: tests-long
STRICT: 0
MODE: simulate
PHASE1: SIMULATED log=target/logs/phase1-soak.log
PHASE2: SIMULATED log=target/logs/phase2-heap-audit.log
PHASE3: SIMULATED log=target/logs/phase3-rt-deadline.log
PHASE4: SIMULATED log=target/logs/phase4-rt-jitter.log
PHASE5: SIMULATED log=target/logs/phase5-concurrency.log
";
    let err = parse_long_receipt(RECEIPT).unwrap_err();
    assert!(
        err.contains("missing OVERALL"),
        "missing OVERALL must be rejected, got: {err}"
    );
}

/// (e) Negative: an unknown phase status token and a missing mandatory phase
/// must both be rejected — a malformed or truncated receipt is fail-closed.
#[test]
fn long_receipt_parser_rejects_malformed_and_truncated_receipts() {
    const BAD_STATUS: &str = "\
SUITE: tests-long
STRICT: 0
MODE: simulate
PHASE1: MAYBE log=target/logs/phase1-soak.log
PHASE2: SIMULATED log=target/logs/phase2-heap-audit.log
PHASE3: SIMULATED log=target/logs/phase3-rt-deadline.log
PHASE4: SIMULATED log=target/logs/phase4-rt-jitter.log
PHASE5: SIMULATED log=target/logs/phase5-concurrency.log
OVERALL: SIMULATED
";
    let err = parse_long_receipt(BAD_STATUS).unwrap_err();
    assert!(
        err.contains("malformed phase line"),
        "unknown phase status must be rejected, got: {err}"
    );

    const TRUNCATED: &str = "\
SUITE: tests-long
STRICT: 0
MODE: simulate
PHASE1: SIMULATED log=target/logs/phase1-soak.log
PHASE2: SIMULATED log=target/logs/phase2-heap-audit.log
PHASE3: SIMULATED log=target/logs/phase3-rt-deadline.log
PHASE4: SIMULATED log=target/logs/phase4-rt-jitter.log
OVERALL: SIMULATED
";
    let err = parse_long_receipt(TRUNCATED).unwrap_err();
    assert!(
        err.contains("missing mandatory phase PHASE5"),
        "truncated receipt (missing PHASE5) must be rejected, got: {err}"
    );
}

/// (e) Live acceptance: when `target/logs/long-receipt.txt` exists, it must
/// parse fail-closed and pass the semantic audit — a present but corrupt
/// receipt is a hard failure. Absence is a *typed* skip (no audit run was
/// performed); under `NAM_QUICK_STRICT=1` the skip becomes a fatal GAP, so a
/// green ER-6 gate always implies a valid long-suite receipt was on disk.
#[test]
fn long_suite_receipt_audit() {
    let path = repo_root().join(LONG_RECEIPT_REL);
    if !path.is_file() {
        eprintln!(
            "TEST_RESULT[long_receipt_audit]=SKIP:receipt_not_found ({} absent; register the planned phases with ./utils/tests-long.sh --simulate or ask the human operator to execute the full audit)",
            path.display()
        );
        return;
    }
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read long receipt {}: {e}", path.display()));
    let receipt = parse_long_receipt(&text)
        .unwrap_or_else(|e| panic!("long receipt {} is not parseable: {e}", path.display()));
    receipt.audit().unwrap_or_else(|e| {
        panic!(
            "long receipt {} failed the semantic audit: {e}",
            path.display()
        )
    });
    let phases: Vec<&str> = receipt.phases.iter().map(|p| p.id.as_str()).collect();
    eprintln!(
        "TEST_RESULT[long_receipt_audit]=PASS suite={} mode={} overall={} phases={} gaps={} receipt={}",
        receipt.suite,
        receipt.mode,
        receipt.overall,
        phases.join(","),
        receipt.gaps.len(),
        path.display()
    );
}

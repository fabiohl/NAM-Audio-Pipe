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
use nam_audio_pipe::receipt::long::{
    ENDURANCE_PURPOSE_TOKEN, LongPhaseStatus, SOAK_PURPOSE_TOKEN, parse_long_receipt,
};
use quick_xml::Reader;
use quick_xml::events::Event;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

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
/// The six canonical long-audit phases (G-RB-002 / T6.3; PHASE6 = real
/// wall-clock endurance, T5.3 / G-PERF-004).
const LONG_PHASE_IDS: [&str; 6] = ["PHASE1", "PHASE2", "PHASE3", "PHASE4", "PHASE5", "PHASE6"];
/// Canonical `run_phase` names the runner must declare verbatim.
const LONG_PHASE_NAMES: [&str; 6] = [
    "Phase 1: Soak acelerado (timeline comprimida) & concorrência de swaps",
    "Phase 2: RT-Safety heap-audit (zero-alloc)",
    "Phase 3: RT Deadline gate (nanosecond budget)",
    "Phase 4: RT Jitter gate (inter-callback dispersion)",
    "Phase 5: Concurrency interleaving stress & state resilience",
    "Phase 6: Endurance real & state-machine throughput",
];

/// Serializes tests that replace `target/logs/long-receipt.txt` (T5.1 strict
/// propagation validation via `utils/tests-long.sh --simulate`) against the
/// ER-6 live audit that reads it — a concurrent read must never observe a
/// half-replaced receipt.
static LONG_RECEIPT_MUTEX: Mutex<()> = Mutex::new(());

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
            Ok(Event::Start(e)) if e.name().as_ref() == "release" => {
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
                        "version" => {
                            version = attr
                                .normalized_value(quick_xml::XmlVersion::Explicit1_0)
                                .map(|c| c.into_owned())
                                .unwrap_or_default();
                        }
                        "date" => {
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
fn get_git_head(dir: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|e| {
            format!(
                "failed to execute git rev-parse HEAD in {}: {e}",
                dir.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse HEAD in {} exited with status {:?}",
            dir.display(),
            output.status.code()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn get_git_tree(dir: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD^{tree}"])
        .output()
        .map_err(|e| {
            format!(
                "failed to execute git rev-parse HEAD^{{tree}} in {}: {e}",
                dir.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse HEAD^{{tree}} in {} exited with status {:?}",
            dir.display(),
            output.status.code()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

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

fn verify_single_artifact(
    name: &str,
    art: &serde_json::Value,
    root: &Path,
    seen_paths: &mut HashMap<PathBuf, (String, u64)>,
) -> Result<(), String> {
    let art_obj = art
        .as_object()
        .ok_or_else(|| format!("artifact '{name}': expected an object"))?;
    let rec_path = art_obj
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
        return Err(format!(
            "'{name}': referenced file does not exist on disk: {}",
            abs.display()
        ));
    }

    let recorded_sha = art_obj
        .get("sha256")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("artifact '{name}': missing 'sha256' field"))?;

    let actual_sha = sha256_hex(&abs)?;
    if !actual_sha.eq_ignore_ascii_case(recorded_sha) {
        return Err(format!(
            "'{name}': SHA-256 mismatch (recorded {recorded_sha}, computed {actual_sha})"
        ));
    }

    let recorded_size = art_obj
        .get("size_bytes")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| format!("artifact '{name}': missing 'size_bytes' field"))?;

    let actual_size = std::fs::metadata(&abs)
        .map_err(|e| format!("stat {}: {e}", abs.display()))?
        .len();
    if actual_size != recorded_size {
        return Err(format!(
            "'{name}': size mismatch (recorded {recorded_size}, actual {actual_size})"
        ));
    }

    if let Some((prev_sha, prev_size)) = seen_paths.get(&abs) {
        if !prev_sha.eq_ignore_ascii_case(recorded_sha) || *prev_size != recorded_size {
            return Err(format!(
                "'{name}': duplicate path collision with divergent content: {}",
                abs.display()
            ));
        }
    } else {
        seen_paths.insert(abs, (recorded_sha.to_string(), recorded_size));
    }

    Ok(())
}

/// Fail-closed validator for the release provenance receipt: every artifact,
/// receipt, log, source commit, git tree SHA, lockfile hash, coupled dependency
/// commit, and ceremony chain status must exist, match on-disk hashes/content,
/// and be semantically consistent (T8.2).
fn validate_provenance_receipt(path: &Path) -> Result<usize, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read provenance receipt {}: {e}", path.display()))?;
    let doc: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("provenance receipt is not valid JSON: {e}"))?;

    if doc.get("schema_version").and_then(|v| v.as_u64()) != Some(2) {
        return Err(format!(
            "unexpected schema_version: {:?} (T5.1 identity schema requires version 2)",
            doc.get("schema_version")
        ));
    }
    if doc.get("kind").and_then(|v| v.as_str()) != Some("release-provenance") {
        return Err(format!(
            "unexpected receipt kind: {:?}",
            doc.get("kind").and_then(|v| v.as_str())
        ));
    }

    let root = repo_root();

    // 1. Validate project commit and tree SHA
    let project = doc
        .get("project")
        .and_then(|v| v.as_object())
        .ok_or("'project' object missing from receipt")?;
    let rec_commit = project
        .get("commit")
        .and_then(|v| v.as_str())
        .ok_or("missing 'project.commit'")?;
    let actual_commit = get_git_head(&root)?;
    if rec_commit != actual_commit {
        return Err(format!(
            "project.commit mismatch: recorded {rec_commit:?}, current HEAD is {actual_commit:?}"
        ));
    }

    let rec_tree = project
        .get("git_tree_sha256")
        .and_then(|v| v.as_str())
        .ok_or("missing 'project.git_tree_sha256'")?;
    let actual_tree = get_git_tree(&root)?;
    if rec_tree != actual_tree {
        return Err(format!(
            "git_tree_sha256 mismatch: recorded {rec_tree:?}, current tree is {actual_tree:?}"
        ));
    }

    // 2. Validate dependencies (Cargo.lock hash and coupled NeuralAmpModeler-rs commit)
    let deps = doc
        .get("dependencies")
        .and_then(|v| v.as_object())
        .ok_or("'dependencies' object missing from receipt")?;
    let rec_lock_sha = deps
        .get("cargo_lock_sha256")
        .and_then(|v| v.as_str())
        .ok_or("missing 'dependencies.cargo_lock_sha256'")?;
    let lock_path = root.join("Cargo.lock");
    let actual_lock_sha = sha256_hex(&lock_path)?;
    if !rec_lock_sha.eq_ignore_ascii_case(&actual_lock_sha) {
        return Err(format!(
            "cargo_lock_sha256 mismatch: recorded {rec_lock_sha:?}, actual Cargo.lock is {actual_lock_sha:?}"
        ));
    }

    let rec_nam_commit = deps
        .get("neural_amp_modeler_rs_commit")
        .and_then(|v| v.as_str())
        .ok_or("missing 'dependencies.neural_amp_modeler_rs_commit'")?;
    let nam_dir = root.join("../NeuralAmpModeler-rs");
    let expected_nam_commit = if nam_dir.join(".git").exists() {
        get_git_head(&nam_dir)?
    } else {
        "not-a-git-repo".to_string()
    };
    if rec_nam_commit != expected_nam_commit {
        return Err(format!(
            "neural_amp_modeler_rs_commit mismatch: recorded {rec_nam_commit:?}, actual is {expected_nam_commit:?}"
        ));
    }

    // 3. Validate build identity (T5.1): the receipt must identify the exact
    //    artifact — build profile, active features, the explicit opt-out of
    //    harness-measured performance claims for the final ELF — and the build
    //    environment (kernel release + pw-cli version).
    let build = doc
        .get("build")
        .and_then(|v| v.as_object())
        .ok_or("'build' object missing from receipt")?;
    let build_profile = build
        .get("profile")
        .and_then(|v| v.as_str())
        .ok_or("missing 'build.profile'")?;
    if build_profile != "dist" && build_profile != "testing" {
        return Err(format!(
            "invalid build.profile: {build_profile:?} (expected 'dist' or 'testing')"
        ));
    }
    let features = build
        .get("features")
        .and_then(|v| v.as_array())
        .ok_or("missing 'build.features' (active feature list)")?;
    if features.is_empty() {
        return Err("'build.features' must not be empty — the ELF was built with features".into());
    }
    for f in features {
        f.as_str()
            .ok_or("'build.features' entries must be strings")?;
    }
    let measured_claims = build
        .get("optimizations")
        .and_then(|v| v.as_object())
        .and_then(|v| v.get("measured_performance_claims"))
        .and_then(|v| v.as_bool())
        .ok_or("missing 'build.optimizations.measured_performance_claims'")?;
    if measured_claims {
        return Err(
            "measured_performance_claims must be false: no harness metric may be attributed to the final PGO+BOLT ELF (T5.1)"
                .into(),
        );
    }

    let environment = doc
        .get("environment")
        .and_then(|v| v.as_object())
        .ok_or("'environment' object missing from receipt")?;
    let kernel_release = environment
        .get("kernel_release")
        .and_then(|v| v.as_str())
        .ok_or("missing 'environment.kernel_release' (uname -r)")?;
    if kernel_release.is_empty() {
        return Err("'environment.kernel_release' must not be empty".into());
    }
    match environment.get("pw_cli_version") {
        Some(v) if v.is_string() => {}
        Some(v) if v.is_null() => {}
        _ => {
            return Err(
                "missing 'environment.pw_cli_version' (a string or null — absent only when pw-cli is unavailable)".into(),
            );
        }
    }

    // 4. Validate ceremony_chain (Mandatory schema component, T8.2)
    let chain = doc
        .get("ceremony_chain")
        .and_then(|v| v.as_object())
        .ok_or("'ceremony_chain' object missing from receipt (old schema rejected)")?;
    let status = chain
        .get("certification_status")
        .and_then(|v| v.as_str())
        .ok_or("missing 'certification_status' in ceremony_chain")?;
    if status != "certified_release" && status != "uncertified" {
        return Err(format!("invalid certification_status: {status:?}"));
    }

    let artifacts = doc
        .get("artifacts")
        .and_then(|v| v.as_object())
        .ok_or("'artifacts' object missing from the receipt")?;
    if artifacts.is_empty() {
        return Err("'artifacts' object is empty — a release receipt with no certified artifacts cannot be trusted".into());
    }

    let mut seen_paths = HashMap::new();
    let mut total_audited = 0usize;

    for (name, art) in artifacts {
        verify_single_artifact(name, art, &root, &mut seen_paths)?;
        total_audited += 1;
    }

    // Validate receipts and phase logs in ceremony_chain
    if status == "certified_release" {
        for req_receipt in [
            "quick_receipt",
            "long_receipt",
            "pgo_receipt",
            "release_receipt",
        ] {
            let art = chain
                .get(req_receipt)
                .and_then(|v| v.as_object())
                .ok_or_else(|| {
                    format!(
                        "certified_release requires mandatory '{req_receipt}' in ceremony_chain"
                    )
                })?;
            verify_single_artifact(
                req_receipt,
                &serde_json::Value::Object(art.clone()),
                &root,
                &mut seen_paths,
            )?;
            total_audited += 1;
        }

        let phase_logs = chain
            .get("phase_logs")
            .and_then(|v| v.as_object())
            .ok_or("certified_release requires 'phase_logs' object in ceremony_chain")?;
        if phase_logs.is_empty() {
            return Err(
                "certified_release requires non-empty 'phase_logs' in ceremony_chain".into(),
            );
        }
        for (log_name, art) in phase_logs {
            verify_single_artifact(
                &format!("phase_log:{log_name}"),
                art,
                &root,
                &mut seen_paths,
            )?;
            total_audited += 1;
        }

        // Validate semantic content of quick_receipt & long_receipt
        let quick_art_obj = chain.get("quick_receipt").unwrap().as_object().unwrap();
        let quick_path_str = quick_art_obj.get("path").unwrap().as_str().unwrap();
        let quick_abs = if Path::new(quick_path_str).is_absolute() {
            PathBuf::from(quick_path_str)
        } else {
            root.join(quick_path_str)
        };
        let quick_text = std::fs::read_to_string(&quick_abs)
            .map_err(|e| format!("read quick_receipt {}: {e}", quick_abs.display()))?;
        if !quick_text.contains("STRICT: 1") || !quick_text.contains("OVERALL: PASSED") {
            return Err(
                "certified_release requires quick_receipt with STRICT: 1 and OVERALL: PASSED"
                    .into(),
            );
        }

        let long_art_obj = chain.get("long_receipt").unwrap().as_object().unwrap();
        let long_path_str = long_art_obj.get("path").unwrap().as_str().unwrap();
        let long_abs = if Path::new(long_path_str).is_absolute() {
            PathBuf::from(long_path_str)
        } else {
            root.join(long_path_str)
        };
        let long_text = std::fs::read_to_string(&long_abs)
            .map_err(|e| format!("read long_receipt {}: {e}", long_abs.display()))?;
        // Semantic strict certification (T5.1/T8.1) — never a substring search:
        // the shared parser must accept the receipt and certify it as a real
        // strict passed run (SUITE: tests-long, STRICT: 1, NAM_RT_STRICT: 1,
        // MODE: full, OVERALL: PASSED).
        let long_receipt = parse_long_receipt(&long_text).map_err(|e| {
            format!(
                "certified_release long_receipt {} is not parseable: {e}",
                long_abs.display()
            )
        })?;
        long_receipt.verify_release_certification().map_err(|e| {
            format!(
                "certified_release long_receipt {} failed strict certification: {e}",
                long_abs.display()
            )
        })?;
    } else {
        // Optional receipt verification when uncertified
        for key in [
            "quick_receipt",
            "long_receipt",
            "pgo_receipt",
            "release_receipt",
        ] {
            if let Some(art) = chain.get(key).filter(|v| !v.is_null()) {
                verify_single_artifact(key, art, &root, &mut seen_paths)?;
                total_audited += 1;
            }
        }
        if let Some(phase_logs) = chain.get("phase_logs").and_then(|v| v.as_object()) {
            for (log_name, art) in phase_logs {
                verify_single_artifact(
                    &format!("phase_log:{log_name}"),
                    art,
                    &root,
                    &mut seen_paths,
                )?;
                total_audited += 1;
            }
        }
    }

    Ok(total_audited)
}

fn write_synthetic_receipt_with_doc(receipt_path: &Path, doc: serde_json::Value) {
    std::fs::write(
        receipt_path,
        serde_json::to_vec_pretty(&doc).expect("serialize receipt"),
    )
    .expect("write synthetic receipt");
}

/// Writes a full synthetic provenance receipt referencing `files` (as `name -> path`)
/// with valid worktree commit, tree hash, lockfile SHA-256 and ceremony chain (T8.2).
fn write_synthetic_receipt(receipt_path: &Path, files: &[(&str, &Path)]) {
    let root = repo_root();
    let commit = get_git_head(&root)
        .unwrap_or_else(|_| "0000000000000000000000000000000000000000".to_string());
    let tree_sha = get_git_tree(&root)
        .unwrap_or_else(|_| "0000000000000000000000000000000000000000".to_string());
    let lock_sha = sha256_hex(&root.join("Cargo.lock")).unwrap_or_default();
    let nam_dir = root.join("../NeuralAmpModeler-rs");
    let nam_commit = if nam_dir.join(".git").exists() {
        get_git_head(&nam_dir).unwrap_or_else(|_| "not-a-git-repo".to_string())
    } else {
        "not-a-git-repo".to_string()
    };

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
        "schema_version": 2,
        "tool": "distribution_qa.rs",
        "kind": "release-provenance",
        "project": {
            "name": "nam-audio-pipe",
            "version": env!("CARGO_PKG_VERSION"),
            "commit": commit,
            "git_tree_sha256": tree_sha,
            "timestamp_utc": "2026-08-28T00:00:00Z"
        },
        "toolchain": {
            "rustc": "rustc 1.88.0",
            "cargo": "cargo 1.88.0"
        },
        "build": {
            "profile": "dist",
            "features": ["stereo"],
            "rustflags": "-C target-cpu=x86-64-v3",
            "optimizations": {
                "status": "PGO+BOLT",
                "cpu_baseline": "x86-64-v3",
                "pgo": true,
                "bolt": true,
                "measured_performance_claims": false
            }
        },
        "environment": {
            "kernel_release": "6.12-test-kernel",
            "pw_cli_version": null
        },
        "dependencies": {
            "cargo_lock_sha256": lock_sha,
            "neural_amp_modeler_rs_commit": nam_commit
        },
        "ceremony_chain": {
            "certification_status": "uncertified"
        },
        "artifacts": artifacts
    });
    write_synthetic_receipt_with_doc(receipt_path, doc);
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

/// (c) Negative: old schema missing `ceremony_chain` must be rejected (T8.2).
#[test]
fn provenance_validator_rejects_old_schema_without_ceremony_chain() {
    let dir = temp_dir();
    let _guard = DirGuard::new(dir.clone());
    let file = dir.join("payload.bin");
    std::fs::write(&file, b"certified artifact payload").expect("write payload");
    let receipt = dir.join("release-provenance.json");
    write_synthetic_receipt(&receipt, &[("installed_binary", &file)]);

    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
    doc.as_object_mut().unwrap().remove("ceremony_chain");
    write_synthetic_receipt_with_doc(&receipt, doc);

    let err = validate_provenance_receipt(&receipt).unwrap_err();
    assert!(
        err.contains("ceremony_chain") && err.contains("old schema rejected"),
        "receipt without ceremony_chain must be rejected, got: {err}"
    );
}

/// (c) Negative: the T5.1 identity schema bumps `schema_version` to 2 — a
/// stale v1 receipt must be rejected fail-closed.
#[test]
fn provenance_validator_rejects_schema_v1_receipt() {
    let dir = temp_dir();
    let _guard = DirGuard::new(dir.clone());
    let file = dir.join("payload.bin");
    std::fs::write(&file, b"certified artifact payload").expect("write payload");
    let receipt = dir.join("release-provenance.json");
    write_synthetic_receipt(&receipt, &[("installed_binary", &file)]);

    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
    doc["schema_version"] = 1.into();
    write_synthetic_receipt_with_doc(&receipt, doc);

    let err = validate_provenance_receipt(&receipt).unwrap_err();
    assert!(
        err.contains("unexpected schema_version") && err.contains("version 2"),
        "schema v1 receipt must be rejected, got: {err}"
    );
}

/// (c) Negative: `measured_performance_claims: true` must be rejected — no
/// harness metric may ever be attributed to the final PGO+BOLT ELF (T5.1).
#[test]
fn provenance_validator_rejects_measured_performance_claims() {
    let dir = temp_dir();
    let _guard = DirGuard::new(dir.clone());
    let file = dir.join("payload.bin");
    std::fs::write(&file, b"certified artifact payload").expect("write payload");
    let receipt = dir.join("release-provenance.json");
    write_synthetic_receipt(&receipt, &[("installed_binary", &file)]);

    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
    doc["build"]["optimizations"]["measured_performance_claims"] = true.into();
    write_synthetic_receipt_with_doc(&receipt, doc);

    let err = validate_provenance_receipt(&receipt).unwrap_err();
    assert!(
        err.contains("measured_performance_claims")
            && err.contains("no harness metric may be attributed"),
        "measured_performance_claims=true must be rejected, got: {err}"
    );
}

/// (c) Negative: the T5.1 identity fields are mandatory — a receipt without
/// `build.features` or without the `environment` block cannot identify the
/// exact artifact and build host.
#[test]
fn provenance_validator_rejects_missing_identity_fields() {
    for (field, expected) in [
        ("build.features", "missing 'build.features'"),
        ("environment", "'environment' object missing"),
    ] {
        let dir = temp_dir();
        let _guard = DirGuard::new(dir.clone());
        let file = dir.join("payload.bin");
        std::fs::write(&file, b"certified artifact payload").expect("write payload");
        let receipt = dir.join("release-provenance.json");
        write_synthetic_receipt(&receipt, &[("installed_binary", &file)]);

        let mut doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
        match field {
            "build.features" => {
                doc.as_object_mut()
                    .unwrap()
                    .get_mut("build")
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .remove("features");
            }
            "environment" => {
                doc.as_object_mut().unwrap().remove("environment");
            }
            _ => unreachable!(),
        }
        write_synthetic_receipt_with_doc(&receipt, doc);

        let err = validate_provenance_receipt(&receipt).unwrap_err();
        assert!(
            err.contains(expected),
            "receipt missing {field} must be rejected, got: {err}"
        );
    }
}

/// (c) Negative: mismatched `project.commit` must be rejected (T8.2).
#[test]
fn provenance_validator_rejects_mismatched_commit() {
    let dir = temp_dir();
    let _guard = DirGuard::new(dir.clone());
    let file = dir.join("payload.bin");
    std::fs::write(&file, b"certified artifact payload").expect("write payload");
    let receipt = dir.join("release-provenance.json");
    write_synthetic_receipt(&receipt, &[("installed_binary", &file)]);

    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
    doc["project"]["commit"] = "1111111111111111111111111111111111111111".into();
    write_synthetic_receipt_with_doc(&receipt, doc);

    let err = validate_provenance_receipt(&receipt).unwrap_err();
    assert!(
        err.contains("project.commit mismatch"),
        "mismatched project.commit must be rejected, got: {err}"
    );
}

/// (c) Negative: mismatched `dependencies.cargo_lock_sha256` must be rejected (T8.2).
#[test]
fn provenance_validator_rejects_mismatched_lock_hash() {
    let dir = temp_dir();
    let _guard = DirGuard::new(dir.clone());
    let file = dir.join("payload.bin");
    std::fs::write(&file, b"certified artifact payload").expect("write payload");
    let receipt = dir.join("release-provenance.json");
    write_synthetic_receipt(&receipt, &[("installed_binary", &file)]);

    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
    doc["dependencies"]["cargo_lock_sha256"] =
        "2222222222222222222222222222222222222222222222222222222222222222".into();
    write_synthetic_receipt_with_doc(&receipt, doc);

    let err = validate_provenance_receipt(&receipt).unwrap_err();
    assert!(
        err.contains("cargo_lock_sha256 mismatch"),
        "mismatched Cargo.lock hash must be rejected, got: {err}"
    );
}

/// (c) Negative: mismatched `dependencies.neural_amp_modeler_rs_commit` must be rejected (T8.2).
#[test]
fn provenance_validator_rejects_mismatched_nam_commit() {
    let dir = temp_dir();
    let _guard = DirGuard::new(dir.clone());
    let file = dir.join("payload.bin");
    std::fs::write(&file, b"certified artifact payload").expect("write payload");
    let receipt = dir.join("release-provenance.json");
    write_synthetic_receipt(&receipt, &[("installed_binary", &file)]);

    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
    doc["dependencies"]["neural_amp_modeler_rs_commit"] =
        "3333333333333333333333333333333333333333".into();
    write_synthetic_receipt_with_doc(&receipt, doc);

    let err = validate_provenance_receipt(&receipt).unwrap_err();
    assert!(
        err.contains("neural_amp_modeler_rs_commit mismatch"),
        "mismatched neural_amp_modeler_rs_commit must be rejected, got: {err}"
    );
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

/// (c) Negative: `certification_status == "certified_release"` without quick `STRICT: 1` or
/// long `STRICT: 1` + `MODE: full` + `OVERALL: PASSED` must be rejected (T8.2).
#[test]
fn provenance_validator_rejects_certified_release_without_strict_quick_or_long_receipt() {
    let dir = temp_dir();
    let _guard = DirGuard::new(dir.clone());
    let file = dir.join("payload.bin");
    std::fs::write(&file, b"certified artifact payload").expect("write payload");

    let quick_file = dir.join("quick-receipt.txt");
    std::fs::write(
        &quick_file,
        "SUITE: tests-quick\nSTRICT: 0\nOVERALL: PASSED\n",
    )
    .unwrap();
    let long_file = dir.join("long-receipt.txt");
    std::fs::write(
        &long_file,
        "SUITE: tests-long\nSTRICT: 1\nMODE: full\nPHASE1: PASS log=p1.log\nPHASE2: PASS log=p2.log\nPHASE3: PASS log=p3.log\nPHASE4: PASS log=p4.log\nPHASE5: PASS log=p5.log\nOVERALL: PASSED\n",
    )
    .unwrap();
    let pgo_file = dir.join("pgo-receipt.json");
    std::fs::write(&pgo_file, "{}").unwrap();
    let release_file = dir.join("release-receipt.json");
    std::fs::write(&release_file, "{}").unwrap();
    let log_file = dir.join("phase1.log");
    std::fs::write(&log_file, "log content").unwrap();

    let receipt = dir.join("release-provenance.json");
    write_synthetic_receipt(&receipt, &[("installed_binary", &file)]);

    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
    let make_art = |p: &Path| {
        serde_json::json!({
            "path": p.display().to_string(),
            "sha256": sha256_hex(p).unwrap(),
            "size_bytes": std::fs::metadata(p).unwrap().len()
        })
    };

    doc["ceremony_chain"] = serde_json::json!({
        "certification_status": "certified_release",
        "quick_receipt": make_art(&quick_file),
        "long_receipt": make_art(&long_file),
        "pgo_receipt": make_art(&pgo_file),
        "release_receipt": make_art(&release_file),
        "phase_logs": {
            "phase1.log": make_art(&log_file)
        }
    });
    write_synthetic_receipt_with_doc(&receipt, doc);

    let err = validate_provenance_receipt(&receipt).unwrap_err();
    assert!(
        err.contains("quick_receipt with STRICT: 1 and OVERALL: PASSED"),
        "non-strict quick receipt must be rejected in certified_release, got: {err}"
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
/// warning (human-operator-only execution), all 6 canonical phases declared
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

/// (e) Structural audit: `utils/build-release.sh` must enforce a real, strict
/// long-suite receipt during `--release-ceremony` through the *semantic*
/// verifier (`long_receipt_check` + `NAM_RT_STRICT: 1` propagation, T5.1/T8.1)
/// — never a substring search — and strictly reject `SIMULATED`, `STRICT: 0`,
/// `COMPLETED_WITH_GAPS`, `FAILED` or missing receipts.
#[test]
fn build_release_script_requires_real_strict_long_receipt() {
    let path = repo_root().join("utils/build-release.sh");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));

    assert!(
        text.contains("long_receipt_check"),
        "{} must verify the long receipt through the semantic long_receipt_check gate (T5.1), not a substring search",
        path.display()
    );
    assert!(
        text.contains("SUITE: tests-long")
            && text.contains("STRICT: 1")
            && text.contains("NAM_RT_STRICT: 1")
            && text.contains("MODE: full")
            && text.contains("OVERALL: PASSED"),
        "{} must name the strict certification fields (SUITE: tests-long, STRICT: 1, NAM_RT_STRICT: 1, MODE: full, OVERALL: PASSED)",
        path.display()
    );
    assert!(
        !text.contains("tests-long.sh\" --simulate"),
        "{} must NOT generate long receipt via --simulate in release ceremony",
        path.display()
    );
    assert!(
        text.contains("strictly rejected") || text.contains("tests-long.sh --strict-pre-release"),
        "{} must instruct operator to execute tests-long.sh --strict-pre-release when receipt is absent/invalid",
        path.display()
    );
}

/// (e) Live structural surface: `utils/tests-long.sh --help` is the sanctioned
/// read-only surface for AI/CI structural validation (the runner itself is
/// human-operator-only). It must exit 0 and inventory all 6 canonical phases
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

/// (e) Negative: `LongReceipt::verify_release_certification` must reject simulated receipts
/// (`MODE: simulate`, `OVERALL: SIMULATED`) when evaluating release certification (T8.1).
#[test]
fn long_receipt_certification_rejects_simulated_receipt() {
    const RECEIPT: &str = "\
SUITE: tests-long
STRICT: 0
NAM_RT_STRICT: 0
MODE: simulate
PHASE1: SIMULATED log=target/logs/phase1-soak.log
PHASE2: SIMULATED log=target/logs/phase2-heap-audit.log
PHASE3: SIMULATED log=target/logs/phase3-rt-deadline.log
PHASE4: SIMULATED log=target/logs/phase4-rt-jitter.log
PHASE5: SIMULATED log=target/logs/phase5-concurrency.log
OVERALL: SIMULATED
";
    let receipt = parse_long_receipt(RECEIPT).unwrap();
    let err = receipt.verify_release_certification().unwrap_err();
    assert!(
        err.contains("STRICT: 1") || err.contains("MODE: full") || err.contains("OVERALL: PASSED"),
        "simulated receipt must be rejected for release certification, got: {err}"
    );
}

/// (e) Negative: `LongReceipt::verify_release_certification` must reject `STRICT: 0` receipts
/// even if all phases passed (T8.1).
#[test]
fn long_receipt_certification_rejects_non_strict_receipt() {
    const RECEIPT: &str = "\
SUITE: tests-long
STRICT: 0
NAM_RT_STRICT: 0
MODE: full
PHASE1: PASS log=target/logs/phase1-soak.log
PHASE2: PASS log=target/logs/phase2-heap-audit.log
PHASE3: PASS log=target/logs/phase3-rt-deadline.log
PHASE4: PASS log=target/logs/phase4-rt-jitter.log
PHASE5: PASS log=target/logs/phase5-concurrency.log
OVERALL: PASSED
";
    let receipt = parse_long_receipt(RECEIPT).unwrap();
    let err = receipt.verify_release_certification().unwrap_err();
    assert!(
        err.contains("STRICT: 1"),
        "STRICT: 0 receipt must be rejected for release certification, got: {err}"
    );
}

/// (e) Positive: `LongReceipt::verify_release_certification` accepts real strict passed receipt (T8.1 + T5.1 + T5.3).
#[test]
fn long_receipt_certification_accepts_real_strict_passed_receipt() {
    const RECEIPT: &str = "\
SUITE: tests-long
STRICT: 1
NAM_RT_STRICT: 1
MODE: full
SOAK_PURPOSE: accelerated_timeline — timeline comprimida, janelas fail-closed
ENDURANCE_PURPOSE: real_wall_clock — parede, RSS/faults/threads/FDs periódicos
PHASE1: PASS log=target/logs/phase1-soak.log
PHASE2: PASS log=target/logs/phase2-heap-audit.log
PHASE3: PASS log=target/logs/phase3-rt-deadline.log
PHASE4: PASS log=target/logs/phase4-rt-jitter.log
PHASE5: PASS log=target/logs/phase5-concurrency.log
PHASE6: PASS log=target/logs/phase6-endurance.log duration_ms=30000
OVERALL: PASSED
";
    let receipt = parse_long_receipt(RECEIPT).unwrap();
    receipt.verify_release_certification().unwrap();
}

/// (e) Positive: the `--simulate` receipt format (the sanctioned CI structural
/// surface) parses and passes the semantic audit.
#[test]
fn long_receipt_parser_accepts_simulated_receipt() {
    const RECEIPT: &str = "\
SUITE: tests-long
STRICT: 0
NAM_RT_STRICT: 0
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
    assert_eq!(receipt.nam_rt_strict, Some(false));
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
NAM_RT_STRICT: 0
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
NAM_RT_STRICT: 0
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
NAM_RT_STRICT: 0
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
NAM_RT_STRICT: 0
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
NAM_RT_STRICT: 0
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
NAM_RT_STRICT: 0
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
    let _lock = LONG_RECEIPT_MUTEX.lock().expect("long receipt mutex");
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

// ---------------------------------------------------------------------------
// T5.1: NAM_RT_STRICT propagation validation — the strict flag must reach
// `tests/rt_metrics.rs` (via `utils/tests-long.sh --strict-pre-release`)
// without ever running the long suite (rules/testing.md §2: AI/CI uses only
// the non-executing `--simulate` surface).
// ---------------------------------------------------------------------------

/// RAII guard that restores the operator's long-audit receipt and phase logs
/// after a `--simulate` invocation replaced them (the sanctioned CI surface).
struct LongSimulateGuard {
    backups: Vec<(PathBuf, PathBuf)>,
}

impl LongSimulateGuard {
    fn new() -> Self {
        let root = repo_root();
        let mut files = vec![root.join(LONG_RECEIPT_REL)];
        for name in [
            "phase1-soak.log",
            "phase2-heap-audit.log",
            "phase3-rt-deadline.log",
            "phase4-rt-jitter.log",
            "phase5-concurrency.log",
            "phase6-endurance.log",
        ] {
            files.push(root.join(format!("target/logs/{name}")));
        }
        let mut backups = Vec::new();
        for f in files {
            if f.is_file() {
                let bk = f.with_extension("t5.1-test-backup");
                let _ = std::fs::copy(&f, &bk);
                backups.push((f, bk));
            }
        }
        Self { backups }
    }
}

impl Drop for LongSimulateGuard {
    fn drop(&mut self) {
        for (orig, bk) in &self.backups {
            let _ = std::fs::copy(bk, orig);
            let _ = std::fs::remove_file(bk);
        }
    }
}

/// Runs `utils/tests-long.sh` with `args` (the sanctioned non-executing
/// `--simulate` surface) with `NAM_RT_STRICT` removed from the child
/// environment, and returns the generated long-receipt text.
fn run_tests_long_simulate(args: &[&str]) -> String {
    let root = repo_root();
    let out = Command::new(root.join(LONG_SUITE_REL))
        .current_dir(&root)
        .args(args)
        .env_remove("NAM_RT_STRICT")
        .output()
        .unwrap_or_else(|e| panic!("failed to execute {}: {e}", LONG_SUITE_REL));
    assert!(
        out.status.success(),
        "{} {args:?} must exit 0 (got {:?}); stderr: {}",
        LONG_SUITE_REL,
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    let receipt_path = root.join(LONG_RECEIPT_REL);
    std::fs::read_to_string(&receipt_path).unwrap_or_else(|e| {
        panic!(
            "cannot read generated receipt {}: {e}",
            receipt_path.display()
        )
    })
}

/// (T5.1) Gate acceptance: `--strict-pre-release` must propagate
/// `NAM_RT_STRICT=1` — the simulate receipt records `STRICT: 1` AND
/// `NAM_RT_STRICT: 1` (the observable propagation evidence the release
/// ceremony requires). No long-suite test is executed.
#[test]
fn strict_pre_release_propagates_nam_rt_strict() {
    let _lock = LONG_RECEIPT_MUTEX.lock().expect("long receipt mutex");
    let _guard = LongSimulateGuard::new();

    let text = run_tests_long_simulate(&["--simulate", "--strict-pre-release"]);
    let receipt = parse_long_receipt(&text)
        .unwrap_or_else(|e| panic!("--simulate --strict-pre-release receipt must parse: {e}"));
    assert!(
        receipt.strict,
        "STRICT: 1 must be recorded for --strict-pre-release"
    );
    assert_eq!(
        receipt.nam_rt_strict,
        Some(true),
        "NAM_RT_STRICT=1 must be propagated and recorded (T5.1), receipt:\n{text}"
    );
    assert_eq!(
        receipt.mode, "simulate",
        "the --simulate surface stays non-executing"
    );
    receipt
        .audit()
        .unwrap_or_else(|e| panic!("strict simulate receipt must audit cleanly: {e}"));
}

/// (T5.1) Companion: a non-strict simulate run records `NAM_RT_STRICT: 0` —
/// the propagation is a strict-only behavior, never inherited by accident.
#[test]
fn non_strict_simulate_records_nam_rt_strict_zero() {
    let _lock = LONG_RECEIPT_MUTEX.lock().expect("long receipt mutex");
    let _guard = LongSimulateGuard::new();

    let text = run_tests_long_simulate(&["--simulate"]);
    let receipt =
        parse_long_receipt(&text).unwrap_or_else(|e| panic!("--simulate receipt must parse: {e}"));
    assert!(!receipt.strict);
    assert_eq!(
        receipt.nam_rt_strict,
        Some(false),
        "a non-strict run must record NAM_RT_STRICT: 0, receipt:\n{text}"
    );
    receipt
        .audit()
        .unwrap_or_else(|e| panic!("simulate receipt must audit cleanly: {e}"));
}

/// (T5.1) Negative: an invalid `NAM_RT_STRICT:` value must be rejected
/// fail-closed by the semantic parser.
#[test]
fn long_receipt_parser_rejects_invalid_nam_rt_strict_value() {
    const RECEIPT: &str = "\
SUITE: tests-long
STRICT: 1
NAM_RT_STRICT: 2
MODE: simulate
PHASE1: SIMULATED log=target/logs/phase1-soak.log
PHASE2: SIMULATED log=target/logs/phase2-heap-audit.log
PHASE3: SIMULATED log=target/logs/phase3-rt-deadline.log
PHASE4: SIMULATED log=target/logs/phase4-rt-jitter.log
PHASE5: SIMULATED log=target/logs/phase5-concurrency.log
OVERALL: SIMULATED
";
    let err = parse_long_receipt(RECEIPT).unwrap_err();
    assert!(
        err.contains("invalid NAM_RT_STRICT") && err.contains("expected 0 or 1"),
        "invalid NAM_RT_STRICT value must be rejected, got: {err}"
    );
}

/// (T5.1) Negative: a receipt claiming `STRICT: 1` while recording
/// `NAM_RT_STRICT: 0` is internally inconsistent and must fail the semantic
/// audit — a strict run without propagation evidence can never certify.
#[test]
fn long_receipt_audit_rejects_strict_without_propagation_evidence() {
    const RECEIPT: &str = "\
SUITE: tests-long
STRICT: 1
NAM_RT_STRICT: 0
MODE: full
PHASE1: PASS log=target/logs/phase1-soak.log
PHASE2: PASS log=target/logs/phase2-heap-audit.log
PHASE3: PASS log=target/logs/phase3-rt-deadline.log
PHASE4: PASS log=target/logs/phase4-rt-jitter.log
PHASE5: PASS log=target/logs/phase5-concurrency.log
OVERALL: PASSED
";
    let receipt = parse_long_receipt(RECEIPT).unwrap();
    let err = receipt.audit().unwrap_err();
    assert!(
        err.contains("STRICT: 1 but NAM_RT_STRICT: 0"),
        "strict receipt without propagation evidence must fail the audit, got: {err}"
    );
}

/// (T5.1) Negative: strict release certification requires the
/// `NAM_RT_STRICT: 1` propagation evidence — a receipt that predates the
/// field (or lacks the line) cannot certify a release.
#[test]
fn long_receipt_certification_requires_nam_rt_strict_propagation() {
    const RECEIPT: &str = "\
SUITE: tests-long
STRICT: 1
MODE: full
PHASE1: PASS log=target/logs/phase1-soak.log
PHASE2: PASS log=target/logs/phase2-heap-audit.log
PHASE3: PASS log=target/logs/phase3-rt-deadline.log
PHASE4: PASS log=target/logs/phase4-rt-jitter.log
PHASE5: PASS log=target/logs/phase5-concurrency.log
OVERALL: PASSED
";
    let receipt = parse_long_receipt(RECEIPT).unwrap();
    let err = receipt.verify_release_certification().unwrap_err();
    assert!(
        err.contains("NAM_RT_STRICT: 1"),
        "certification without NAM_RT_STRICT propagation evidence must be rejected, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// T5.3 (G-PERF-004): soak/endurance purpose declarations in the receipt.
// The accelerated timeline soak and the real wall-clock endurance are separate
// suites; the receipt must declare each purpose and the strict certification
// must require PHASE6 (real endurance) — never conflating harness throughput
// with RT audio throughput.
// ---------------------------------------------------------------------------

/// (T5.3) Gate acceptance: the `--simulate` receipt (the sanctioned
/// non-executing surface) declares both soak suite purposes — `SOAK_PURPOSE:
/// accelerated_timeline` and `ENDURANCE_PURPOSE: real_wall_clock` — and
/// closes the new PHASE6 (real endurance) as SIMULATED. No long-suite test is
/// executed.
#[test]
fn simulate_receipt_declares_suite_purposes_and_phase6() {
    let _lock = LONG_RECEIPT_MUTEX.lock().expect("long receipt mutex");
    let _guard = LongSimulateGuard::new();

    let text = run_tests_long_simulate(&["--simulate"]);
    let receipt =
        parse_long_receipt(&text).unwrap_or_else(|e| panic!("--simulate receipt must parse: {e}"));
    let soak = receipt.soak_purpose.as_deref().unwrap_or_default();
    assert!(
        soak.starts_with(SOAK_PURPOSE_TOKEN),
        "SOAK_PURPOSE must declare accelerated_timeline, receipt:\n{text}"
    );
    let endurance = receipt.endurance_purpose.as_deref().unwrap_or_default();
    assert!(
        endurance.starts_with(ENDURANCE_PURPOSE_TOKEN),
        "ENDURANCE_PURPOSE must declare real_wall_clock, receipt:\n{text}"
    );
    let phase6 = receipt
        .phases
        .iter()
        .find(|p| p.id == "PHASE6")
        .expect("PHASE6 must be declared by the runner");
    assert_eq!(
        phase6.status,
        LongPhaseStatus::Simulated,
        "simulate receipt must register PHASE6 as SIMULATED"
    );
    receipt
        .audit()
        .unwrap_or_else(|e| panic!("simulate receipt must audit cleanly: {e}"));
}

/// (T5.3) Negative: a `SOAK_PURPOSE:` / `ENDURANCE_PURPOSE:` line carrying a
/// non-canonical purpose token must be rejected fail-closed.
#[test]
fn long_receipt_parser_rejects_invalid_purpose_values() {
    const BAD_SOAK: &str = "\
SUITE: tests-long
STRICT: 0
NAM_RT_STRICT: 0
MODE: simulate
SOAK_PURPOSE: real_wall_clock
PHASE1: SIMULATED log=target/logs/phase1-soak.log
PHASE2: SIMULATED log=target/logs/phase2-heap-audit.log
PHASE3: SIMULATED log=target/logs/phase3-rt-deadline.log
PHASE4: SIMULATED log=target/logs/phase4-rt-jitter.log
PHASE5: SIMULATED log=target/logs/phase5-concurrency.log
PHASE6: SIMULATED log=target/logs/phase6-endurance.log
OVERALL: SIMULATED
";
    let err = parse_long_receipt(BAD_SOAK).unwrap_err();
    assert!(
        err.contains("invalid SOAK_PURPOSE"),
        "non-canonical SOAK_PURPOSE must be rejected, got: {err}"
    );

    const BAD_ENDURANCE: &str = "\
SUITE: tests-long
STRICT: 0
NAM_RT_STRICT: 0
MODE: simulate
SOAK_PURPOSE: accelerated_timeline — timeline comprimida
ENDURANCE_PURPOSE: accelerated_timeline
PHASE1: SIMULATED log=target/logs/phase1-soak.log
PHASE2: SIMULATED log=target/logs/phase2-heap-audit.log
PHASE3: SIMULATED log=target/logs/phase3-rt-deadline.log
PHASE4: SIMULATED log=target/logs/phase4-rt-jitter.log
PHASE5: SIMULATED log=target/logs/phase5-concurrency.log
PHASE6: SIMULATED log=target/logs/phase6-endurance.log
OVERALL: SIMULATED
";
    let err = parse_long_receipt(BAD_ENDURANCE).unwrap_err();
    assert!(
        err.contains("invalid ENDURANCE_PURPOSE"),
        "non-canonical ENDURANCE_PURPOSE must be rejected, got: {err}"
    );

    // A crafted value that merely *starts with* the canonical token (no
    // separator) must never certify — whole-token match is fail-closed.
    const PREFIX_BYPASS: &str = "\
SUITE: tests-long
STRICT: 1
NAM_RT_STRICT: 1
MODE: full
SOAK_PURPOSE: accelerated_timeline_fake_evidence
ENDURANCE_PURPOSE: real_wall_clock — parede
PHASE1: PASS log=target/logs/phase1-soak.log
PHASE2: PASS log=target/logs/phase2-heap-audit.log
PHASE3: PASS log=target/logs/phase3-rt-deadline.log
PHASE4: PASS log=target/logs/phase4-rt-jitter.log
PHASE5: PASS log=target/logs/phase5-concurrency.log
PHASE6: PASS log=target/logs/phase6-endurance.log duration_ms=30000
OVERALL: PASSED
";
    let err = parse_long_receipt(PREFIX_BYPASS).unwrap_err();
    assert!(
        err.contains("invalid SOAK_PURPOSE"),
        "token-prefix bypass must be rejected, got: {err}"
    );
}

/// (T5.3) Negative: strict release certification requires PHASE6 (real
/// wall-clock endurance) and both purpose declarations — a receipt that lacks
/// them can never certify a release.
#[test]
fn long_receipt_certification_requires_purposes_and_phase6() {
    const NO_PURPOSES: &str = "\
SUITE: tests-long
STRICT: 1
NAM_RT_STRICT: 1
MODE: full
PHASE1: PASS log=target/logs/phase1-soak.log
PHASE2: PASS log=target/logs/phase2-heap-audit.log
PHASE3: PASS log=target/logs/phase3-rt-deadline.log
PHASE4: PASS log=target/logs/phase4-rt-jitter.log
PHASE5: PASS log=target/logs/phase5-concurrency.log
OVERALL: PASSED
";
    let receipt = parse_long_receipt(NO_PURPOSES).unwrap();
    let err = receipt.verify_release_certification().unwrap_err();
    assert!(
        err.contains("PHASE6"),
        "certification without PHASE6 must be rejected, got: {err}"
    );

    const NO_ENDURANCE_PURPOSE: &str = "\
SUITE: tests-long
STRICT: 1
NAM_RT_STRICT: 1
MODE: full
SOAK_PURPOSE: accelerated_timeline — timeline comprimida
PHASE1: PASS log=target/logs/phase1-soak.log
PHASE2: PASS log=target/logs/phase2-heap-audit.log
PHASE3: PASS log=target/logs/phase3-rt-deadline.log
PHASE4: PASS log=target/logs/phase4-rt-jitter.log
PHASE5: PASS log=target/logs/phase5-concurrency.log
PHASE6: PASS log=target/logs/phase6-endurance.log duration_ms=30000
OVERALL: PASSED
";
    let receipt = parse_long_receipt(NO_ENDURANCE_PURPOSE).unwrap();
    let err = receipt.verify_release_certification().unwrap_err();
    assert!(
        err.contains("ENDURANCE_PURPOSE"),
        "certification without ENDURANCE_PURPOSE must be rejected, got: {err}"
    );
}

/// (T5.3) The semantic audit covers PHASE6 like any other phase: a GAP or FAIL
/// in the real-endurance phase can never hide behind a green `OVERALL` verdict.
#[test]
fn long_receipt_audit_accounts_phase6_gap_and_fail() {
    const PHASE6_GAP: &str = "\
SUITE: tests-long
STRICT: 0
NAM_RT_STRICT: 0
MODE: full
PHASE1: PASS log=target/logs/phase1-soak.log
PHASE2: PASS log=target/logs/phase2-heap-audit.log
PHASE3: PASS log=target/logs/phase3-rt-deadline.log
PHASE4: PASS log=target/logs/phase4-rt-jitter.log
PHASE5: PASS log=target/logs/phase5-concurrency.log
PHASE6: GAP log=target/logs/phase6-endurance.log
GAP: phase6:endurance_harness_missing (T5.3 pending)
OVERALL: COMPLETED_WITH_GAPS
";
    let receipt = parse_long_receipt(PHASE6_GAP).unwrap();
    receipt
        .audit()
        .unwrap_or_else(|e| panic!("PHASE6 GAP with gap evidence must audit cleanly: {e}"));

    const PHASE6_FAIL_HIDDEN: &str = "\
SUITE: tests-long
STRICT: 0
NAM_RT_STRICT: 0
MODE: full
PHASE1: PASS log=target/logs/phase1-soak.log
PHASE2: PASS log=target/logs/phase2-heap-audit.log
PHASE3: PASS log=target/logs/phase3-rt-deadline.log
PHASE4: PASS log=target/logs/phase4-rt-jitter.log
PHASE5: PASS log=target/logs/phase5-concurrency.log
PHASE6: FAIL log=target/logs/phase6-endurance.log
OVERALL: PASSED
";
    let receipt = parse_long_receipt(PHASE6_FAIL_HIDDEN).unwrap();
    let err = receipt.audit().unwrap_err();
    assert!(
        err.contains("inconsistent receipt"),
        "PASSED verdict hiding a PHASE6 FAIL must fail the audit, got: {err}"
    );
}

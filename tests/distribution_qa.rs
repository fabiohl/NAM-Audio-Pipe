// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Distribution QA for the shipped product artifacts.
//!
//! This harness covers the two product-facing acceptance groups:
//!
//! * **(a) AppStream structural validation** — the repository metainfo must parse
//!   under a strict XML parser (quick-xml, end-tag/comment checking enabled)
//!   and its `<release>` entry must carry a version matching the crate plus a
//!   mandatory date. Any malformed document — such as a duplicated or unmatched
//!   `</release>` closing tag — must be detected and rejected fail-closed.
//! * **(b) Distribution binary smoke** — the `--profile dist` (stripped, LTO
//!   fat; `panic = "unwind"` so the catch_unwind containment is effective in
//!   the shipped artifact) binary is exercised as a subprocess: `--diagnose`
//!   must exit 0, emit the diagnostic bundle and show no crash artifacts, and
//!   `--help` must exit 0. The artifact is located at `target/dist/nam-audio-pipe`,
//!   the installed `~/.local/bin/nam-audio-pipe`, or `$NAM_DIST_BIN`; its absence
//!   is a *typed* skip.
//!
//! Release-pipeline infrastructure (test receipts, provenance receipts, script
//! structure and long-audit certification) is deliberately not tested here:
//! those artifacts are certified by their own gates.

use quick_xml::Reader;
use quick_xml::events::Event;
use std::path::PathBuf;
use std::process::Command;

/// AppStream metainfo location relative to the crate root.
const METAINFO_REL: &str = "packaging/flatpak/io.github.fabiohl.NAMAudioPipe.metainfo.xml";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

// ---------------------------------------------------------------------------
// (a) AppStream structural validation
// ---------------------------------------------------------------------------

/// Runs a strict quick-xml parse over `xml` and returns every structural
/// error. quick-xml's defaults already enforce well-formedness: end-tag names
/// must match their open tag (`check_end_names`), comments are syntax-checked
/// (`check_comments`) and an unbalanced close — like a duplicate
/// `</release>` tag — is rejected with a typed error. The element
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
/// version and carry the mandatory `date` attribute (AppStream semantic gate).
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
        "<release> is missing the mandatory date attribute (semantic gate)"
    );
}

/// (a) Negative acceptance: a malformed document carrying the duplicated
/// `</release>` closing tag (`packaging/flatpak/...metainfo.xml`
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
// (b) Distribution binary smoke
// ---------------------------------------------------------------------------

/// Candidate locations of the `--profile dist` (stripped, LTO fat; panic =
/// "unwind" so the catch_unwind containment is effective in the
/// shipped artifact) binary, in preference order: explicit override, cargo's
/// standard dist output, the atomically installed binary, then the PGO/BOLT
/// build tree.
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

/// (b) Acceptance: the distribution binary — compiled with `--profile dist`
/// (`strip = true`, LTO fat; `panic = "unwind"` so the containment
/// is effective in the artifact users install) and therefore the exact
/// artifact shipped by `utils/build-release.sh` — must run stably as a
/// subprocess and exit cleanly with code 0: `--diagnose` must emit the
/// diagnostic bundle and neither stdout nor stderr may carry crash artifacts,
/// and `--help` must print usage. Absence of the artifact is a *typed* skip.
#[test]
fn dist_binary_smoke_under_dist_profile() {
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
        "dist binary --diagnose must exit 0 under the dist profile (got {:?})",
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

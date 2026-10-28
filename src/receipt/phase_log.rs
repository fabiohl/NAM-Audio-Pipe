// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Typed verdicts of a long-suite phase derived from its log.
//!
//! `utils/tests-long.sh` runs each phase and hands the log to
//! `pipe_long_receipt append --from-log`, which resolves the verdict here. Only
//! typed evidence decides it: the exit code, the `TEST_RESULT[<name>]=…` markers
//! emitted by the harnesses, cargo's `Running <target>` / `test result:` summaries
//! and the engine's canonical gap markers. Free text is only quoted as a
//! diagnostic fail reason, never as a decision input.

use std::fs;
use std::path::Path;

use neural_amp_modeler_rs::testing::receipt::{
    count_before, count_tests_executed_from_log, detect_gap_markers,
};

const MARKER_KEY: &str = "TEST_RESULT[";
const FAIL_REASON_MAX_CHARS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Passed,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseVerdict {
    pub verdict: Verdict,
    pub gaps: Vec<String>,
    pub fail_reason: Option<String>,
    pub tests_executed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MarkerState {
    Pass,
    Fail,
    Gap(String),
    Skip(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Marker {
    name: String,
    state: MarkerState,
    line: String,
}

/// A `--require` item: `name[|alternative…][:min]`. A trailing `*` on a name
/// matches every marker sharing that prefix; `min` counts `PASS` markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    alternatives: Vec<String>,
    min: usize,
}

impl Requirement {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (names, min) = match spec.rsplit_once(':') {
            Some((names, count)) => (
                names,
                count
                    .parse::<usize>()
                    .map_err(|_| format!("invalid minimum in --require {spec}"))?,
            ),
            None => (spec, 1),
        };
        let alternatives: Vec<String> = names
            .split('|')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect();
        if alternatives.is_empty() {
            return Err(format!("empty --require item: {spec}"));
        }
        Ok(Self { alternatives, min })
    }

    fn label(&self) -> &str {
        self.alternatives[0].trim_end_matches('*')
    }

    fn matches(&self, name: &str) -> bool {
        self.alternatives
            .iter()
            .any(|alt| match alt.strip_suffix('*') {
                Some(prefix) => name.starts_with(prefix),
                None => name == alt,
            })
    }
}

pub fn derive(
    log: &Path,
    rc: i32,
    requirements: &[Requirement],
    targets: &[String],
) -> Result<PhaseVerdict, String> {
    let raw = fs::read_to_string(log).map_err(|e| format!("cannot read {}: {e}", log.display()))?;
    let text = strip_ansi(&raw);
    let lines: Vec<&str> = text.lines().collect();
    let markers: Vec<Marker> = lines.iter().filter_map(|l| parse_marker(l)).collect();

    let mut gaps = detect_gap_markers(log);
    let mut failures: Vec<String> = Vec::new();
    if rc != 0 {
        failures.push(format!("rc={rc}"));
    }
    if lines
        .iter()
        .any(|l| l.trim_start().starts_with("test result: FAILED"))
    {
        failures.push("cargo_test_failed".to_string());
    }
    for marker in &markers {
        match &marker.state {
            MarkerState::Fail => failures.push(format!("marker_fail:{}", marker.name)),
            MarkerState::Gap(why) | MarkerState::Skip(why) => {
                push_unique(&mut gaps, format!("{}:{why}", marker.name));
            }
            MarkerState::Pass => {}
        }
    }
    let tests_executed = count_tests_executed_from_log(log);
    if tests_executed == 0 {
        failures.push("zero_tests_executed".to_string());
    }
    for target in targets {
        if !target_executed(&lines, target) {
            failures.push(format!("target_not_executed:{target}"));
        }
    }
    for requirement in requirements {
        let passes = markers
            .iter()
            .filter(|m| m.state == MarkerState::Pass && requirement.matches(&m.name))
            .count();
        if passes < requirement.min {
            push_unique(
                &mut gaps,
                format!("no_typed_{}_result", requirement.label()),
            );
        }
    }

    if !failures.is_empty() {
        return Ok(PhaseVerdict {
            verdict: Verdict::Failed,
            gaps,
            fail_reason: Some(failure_reason(&lines, &markers, &failures)),
            tests_executed,
        });
    }
    Ok(PhaseVerdict {
        verdict: if gaps.is_empty() {
            Verdict::Passed
        } else {
            Verdict::Skipped
        },
        gaps,
        fail_reason: None,
        tests_executed,
    })
}

fn failure_reason(lines: &[&str], markers: &[Marker], failures: &[String]) -> String {
    let detail = markers
        .iter()
        .find(|m| m.state == MarkerState::Fail)
        .map(|m| m.line.clone())
        .or_else(|| {
            lines
                .iter()
                .find(|l| l.contains("panicked at") || l.contains("assertion"))
                .map(|l| l.trim().to_string())
        })
        .unwrap_or_else(|| failures.join("; "));
    detail.chars().take(FAIL_REASON_MAX_CHARS).collect()
}

fn parse_marker(line: &str) -> Option<Marker> {
    let start = line.find(MARKER_KEY)? + MARKER_KEY.len();
    let close = start + line[start..].find(']')?;
    let name = &line[start..close];
    let rest = line[close + 1..].strip_prefix('=')?;
    let token: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/'))
        .collect();
    let state = match token.split_once(':') {
        None => match token.as_str() {
            "PASS" => MarkerState::Pass,
            "FAIL" => MarkerState::Fail,
            "GAP" => MarkerState::Gap("unspecified".to_string()),
            "SKIP" => MarkerState::Skip("unspecified".to_string()),
            _ => return None,
        },
        Some(("GAP", why)) => MarkerState::Gap(why.to_string()),
        Some(("SKIP", why)) => MarkerState::Skip(why.to_string()),
        Some(_) => return None,
    };
    Some(Marker {
        name: name.to_string(),
        state,
        line: line.trim().to_string(),
    })
}

fn target_executed(lines: &[&str], target: &str) -> bool {
    let header = format!("Running {target}");
    let Some(at) = lines.iter().position(|l| {
        let trimmed = l.trim();
        trimmed == header || trimmed.starts_with(&format!("{header} "))
    }) else {
        return false;
    };
    lines[at + 1..]
        .iter()
        .find(|l| l.trim_start().starts_with("test result:"))
        .is_some_and(|l| count_before(l, " passed") + count_before(l, " measured") >= 1)
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn push_unique(list: &mut Vec<String>, entry: String) {
    if !list.contains(&entry) {
        list.push(entry);
    }
}

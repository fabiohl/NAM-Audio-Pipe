// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

const SAMPLE_FULL_RECEIPT: &str = "\
SUITE: tests-long
STRICT: 1
NAM_RT_STRICT: 1
MODE: full
SOAK_PURPOSE: accelerated_timeline - automated soak verification
ENDURANCE_PURPOSE: real_wall_clock - automated endurance verification
PHASE1: PASS log=target/logs/1.log
PHASE2: PASS log=target/logs/2.log
PHASE3: PASS log=target/logs/3.log env=pinned:1,isolated:1,fifo:1
PHASE4: PASS log=target/logs/4.log env=pinned:1,isolated:1,fifo:1
PHASE5: PASS log=target/logs/5.log
PHASE6: PASS log=target/logs/6.log
OVERALL: PASSED
";

const SAMPLE_PARTIAL_RECEIPT: &str = "\
SUITE: tests-long
STRICT: 1
NAM_RT_STRICT: 1
MODE: partial
SOAK_PURPOSE: accelerated_timeline - automated soak verification
ENDURANCE_PURPOSE: real_wall_clock - automated endurance verification
PHASE1: PASS log=target/logs/1.log
OVERALL: PARTIAL (--only: recibo sem overall, não certificável)
";

#[test]
fn test_parse_full_receipt_passes_certification() {
    let parsed = parse_long_receipt(SAMPLE_FULL_RECEIPT).expect("parse full receipt");
    assert_eq!(parsed.mode, "full");
    assert!(parsed.audit().is_ok());
    assert!(parsed.verify_release_certification().is_ok());
}

#[test]
fn test_parse_partial_receipt_audits_cleanly_but_rejects_certification() {
    let parsed = parse_long_receipt(SAMPLE_PARTIAL_RECEIPT).expect("parse partial receipt");
    assert_eq!(parsed.mode, "partial");
    assert!(parsed.audit().is_ok());
    let cert_err = parsed.verify_release_certification();
    assert!(cert_err.is_err());
    assert!(cert_err.unwrap_err().contains("MODE: full"));
}

#[test]
fn test_parse_partial_with_failure() {
    let content = "\
SUITE: tests-long
STRICT: 0
NAM_RT_STRICT: 0
MODE: partial
PHASE1: FAIL log=target/logs/1.log
OVERALL: FAILED
";
    let parsed = parse_long_receipt(content).expect("parse partial failed receipt");
    assert!(parsed.audit().is_ok());
}

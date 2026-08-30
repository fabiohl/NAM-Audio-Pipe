// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Fail-closed semantic verifier for the long-suite receipt (T5.1 / T8.1).
//!
//! Invoked by `utils/build-release.sh` at the release-ceremony gate to verify
//! `target/logs/long-receipt.txt` with the shared semantic parser
//! ([`nam_audio_pipe::receipt::long`]) — never a substring search. The receipt
//! must parse fail-closed, pass the internal consistency audit and satisfy the
//! strict release certification (`SUITE: tests-long`, `STRICT: 1`,
//! `NAM_RT_STRICT: 1`, `MODE: full`, `OVERALL: PASSED`).
//!
//! Exit codes:
//!   * `0` — receipt verified (real strict passed receipt).
//!   * `1` — receipt missing, unparseable, inconsistent or not strictly certified.
//!   * `2` — usage error (no path argument).

use nam_audio_pipe::receipt::long::verify_release_certification_file;
use std::path::Path;

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: long_receipt_check <path-to-long-receipt.txt>");
        std::process::exit(2);
    };
    match verify_release_certification_file(Path::new(&path)) {
        Ok(receipt) => {
            println!(
                "long_receipt_check: strict release certification verified: SUITE={}, STRICT=1, NAM_RT_STRICT=1, MODE=full, OVERALL=PASSED ({})",
                receipt.suite, path,
            );
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("long_receipt_check: FAILED: {e}");
            std::process::exit(1);
        }
    }
}

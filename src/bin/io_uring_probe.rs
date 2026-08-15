// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Native `io_uring` capability probe — shell-orchestration entrypoint.
//!
//! Used by `utils/tests-quick.sh` Phase 4 to decide whether the recording
//! disk-writer tests can run, without depending on `python3`.
//!
//! Exit codes:
//!   * `0` — `io_uring` available.
//!   * `1` — kernel/security unsupported (`io_uring_disabled=2`, ENOSYS, EPERM).
//!   * `2` — probe failed unexpectedly.

use nam_audio_pipe::recording::probe::IoUringSupport;
use nam_audio_pipe::recording::probe::probe_io_uring;

fn main() {
    std::process::exit(match probe_io_uring() {
        IoUringSupport::Available => 0,
        IoUringSupport::KernelUnsupported => 1,
        IoUringSupport::ProbeFailed => 2,
    });
}

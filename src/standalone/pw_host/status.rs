// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Backend state machine and thread-safe shared status for the PipeWire host.
//!
//! Every fatal loss of backend connectivity (stream `StreamState::Error`,
//! post-streaming `StreamState::Unconnected`) transitions the shared
//! [`SharedBackendStatus`] to `Failed` through the stream-state observers
//! installed on the capture and playback streams. The main control loop in
//! `run.rs` polls [`SharedBackendStatus::is_failed`] every iteration and, on
//! failure, either enters the bounded reconnect cycle (via
//! [`SharedBackendStatus::begin_reconnect`]) or tears the host down
//! observably (RT loop stop, GC drain, recording teardown) and returns an
//! error — the process never survives as a functionally-dead zombie with no
//! audio, and never reconnects unboundedly.
//!
//! Exception: a post-streaming `Unconnected` observed while the process-global
//! `SHUTDOWN` flag is raised (SIGINT/SIGTERM) is the expected teardown of the
//! streams by `thread_loop.stop()`, not a daemon crash. It is logged at `info!`
//! and does **not** transition the backend to `Failed`, so a graceful termination
//! never raises a false "daemon crash" alarm.

mod machine;
mod observe;
mod snapshot;
mod teardown;
mod transitions;

pub use machine::{BackendState, SharedBackendStatus};
pub use observe::{observe_rt_panic, observe_stream_state};
pub use snapshot::BackendStatusSnapshot;
pub use teardown::TeardownGuard;

#[cfg(test)]
#[path = "status_test.rs"]
mod tests;

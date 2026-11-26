// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Disk I/O thread for WAV recording.

mod session;
mod startup;
mod write;

pub use crate::recording::writer::CHECKPOINT_INTERVAL_SECS;
#[cfg(test)]
use session::disk_writer_loop_inner;
pub use startup::{disk_writer_loop, spawn_recording_worker};
#[cfg(test)]
use startup::{publish_worker_result, validate_output_dir};

#[cfg(test)]
pub(super) use crate::recording::writer::{AsyncWavWriter, WavSink, create_new_capture};

#[cfg(test)]
#[path = "disk_test.rs"]
mod disk_test;

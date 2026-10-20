// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Asynchronous WAV recording subsystem for NAM-Audio-Pipe.
//!
//! Provides a real-time safe capture path connecting the audio DSP thread
//! to an asynchronous background disk-I/O worker powered by Linux `io_uring`:
//!
//! - **RT Producer**: Writes stereo audio frames into a lock-free preallocated
//!   ring/pool ([`pool::RecordingPool`] or inline [`buffer::AlignedBlock`]),
//!   guaranteeing zero heap allocations, zero blocking syscalls, and zero locks.
//! - **Async Disk Worker**: Consumes captured audio chunks off the RT path,
//!   formats PCM WAV headers, checkpoints file lengths periodically to prevent data loss
//!   on unexpected termination, and splits files at 4 GiB boundaries.

pub mod alert;
pub mod buffer;
pub mod disk;
pub mod guard;
mod io;
pub mod pool;
pub mod probe;
pub mod space;
pub mod status;
pub mod summary;
pub mod transport;
pub mod wav_header;

pub use alert::{ALERT_REMINDER_INTERVAL, Alert, AlertKind, Clock, RecordingAlert, SystemClock};

pub use buffer::{
    AlignedBlock, AudioMetadata, CONTROL_CAPACITY, ControlPayload, MAX_BLOCK_SIZE, OVERRUN_COUNT,
    OVERRUN_FRAMES_COUNT, RING_CAPACITY, RingPayload, create_audio_ring_buffer,
    create_control_ring_buffer,
};
pub use disk::{disk_writer_loop, spawn_recording_worker};
pub use guard::{
    RECORDING_IO_JOIN_TIMEOUT, RecordingWorkerGuard, RecordingWorkerOutcome,
    STREAM_STOP_RETRY_TIMEOUT,
};
pub use pool::{
    AcquiredSlot, Descriptor, InFlightBlock, POOL_CAPACITY, PoolConsumer, PoolProducer,
    RecordingPool,
};
pub use probe::{IoUringSupport, probe_io_uring};
pub use status::{
    RECORDING_INIT_TIMEOUT, RecordingInit, RecordingStartupError, RecordingStatus,
    SharedRecordingStatus, mark_active, publish_space_warning, record_failure,
    wait_for_recording_init,
};
pub use summary::{
    CaptureSegment, CaptureSummary, format_bytes, format_capture_size, format_duration,
};
pub use transport::{
    RECORDING_POOL_TRANSPORT, RecordingReceiver, RecordingSender, create_recording_transport,
};
pub use wav_header::{build_wav_header, capture_filename, current_capture_timestamp};

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

pub mod buffer;
pub mod disk;
pub mod guard;
mod io;
pub mod pool;
pub mod probe;
pub mod status;
pub mod transport;
pub mod wav_header;

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
    SharedRecordingStatus, record_failure, wait_for_recording_init,
};
pub use transport::{
    RECORDING_POOL_TRANSPORT, RecordingReceiver, RecordingSender, create_recording_transport,
};
pub use wav_header::{build_wav_header, capture_filename, current_capture_timestamp};

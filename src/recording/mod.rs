// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

pub mod buffer;
pub mod disk;
pub mod probe;
pub mod wav_header;

pub use buffer::{
    AlignedBlock, AudioMetadata, MAX_BLOCK_SIZE, OVERRUN_COUNT, RING_CAPACITY, RingPayload,
    create_audio_ring_buffer,
};
pub use disk::disk_writer_loop;
pub use probe::{IoUringSupport, probe_io_uring};
pub use wav_header::{build_wav_header, resolve_available_filename};

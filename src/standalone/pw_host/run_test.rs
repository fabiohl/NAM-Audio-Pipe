// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

#[test]
fn join_recording_io_returns_promptly_when_thread_finishes() {
    let handle = std::thread::spawn(|| {});
    let start = std::time::Instant::now();
    join_recording_io(handle, std::time::Duration::from_secs(5));
    assert!(start.elapsed() < std::time::Duration::from_secs(1));
}

#[test]
fn join_recording_io_detaches_after_timeout() {
    let handle = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(500));
    });
    let start = std::time::Instant::now();
    join_recording_io(handle, std::time::Duration::from_millis(50));
    let elapsed = start.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_millis(50),
        "join returned before the timeout ({elapsed:?})"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(450),
        "join blocked past the bounded timeout ({elapsed:?})"
    );
}

fn dummy_meta() -> RingPayload<MAX_BLOCK_SIZE> {
    RingPayload::Metadata(crate::recording::buffer::AudioMetadata {
        sample_rate: 48000.0,
        bit_depth: 32,
        channels: 2,
    })
}

#[test]
fn push_stream_stop_succeeds_when_capacity_frees() {
    let (mut prod, mut cons) = rtrb::RingBuffer::new(1);
    prod.push(dummy_meta()).unwrap();

    let handle = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _ = cons.pop();
    });

    push_stream_stop(&mut prod, std::time::Duration::from_millis(200));
    handle.join().unwrap();
}

#[test]
fn push_stream_stop_times_out_when_ring_stays_full() {
    let (mut prod, _cons) = rtrb::RingBuffer::new(1);
    prod.push(dummy_meta()).unwrap();

    let start = std::time::Instant::now();
    push_stream_stop(&mut prod, std::time::Duration::from_millis(30));
    let elapsed = start.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_millis(30),
        "retry returned before the timeout ({elapsed:?})"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(200),
        "retry blocked past the bounded timeout ({elapsed:?})"
    );
}

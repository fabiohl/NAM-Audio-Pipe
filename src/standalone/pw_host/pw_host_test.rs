// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

#[test]
fn test_dsp_bridge_concurrent_access() {
    // 1. DspBridge setup:
    // We use Box::leak to obtain a 'static reference, simulating the runtime
    // behavior where the object lives for the entire duration of the PipeWire host.
    // This allows safely converting the reference into raw pointers (*const/*mut).
    let bridge: &'static DspBridge = Box::leak(Box::new(DspBridge {
        buffers: [BridgeBuffer::new(), BridgeBuffer::new()],
        active_read_idx: std::sync::atomic::AtomicUsize::new(0),
        generation: std::sync::atomic::AtomicU64::new(0),
        consumed_gen: std::sync::atomic::AtomicU64::new(0),
        dropped_frames: std::sync::atomic::AtomicU32::new(0),
    }));

    // Raw pointers for the threads (writer/reader)
    let bridge_ptr_writer = bridge as *const DspBridge as *mut DspBridge as usize;
    let bridge_ptr_reader = bridge as *const DspBridge;

    // 2. Writer Thread (Simulates RT Capture Callback):
    // This thread fills the "back buffer" (the buffer not being read)
    // and then toggles the atomic index to make it the new "front buffer".
    let writer_handle = std::thread::spawn(move || {
        let mut counter = 0.0f32;
        let bridge_ptr_writer = bridge_ptr_writer as *mut DspBridge;
        for _ in 0..1000 {
            let bridge_ref = unsafe { &mut *bridge_ptr_writer };

            // Prevents the writer from overwriting the buffer being read
            while bridge_ref.generation.load(Ordering::Acquire)
                > bridge_ref.consumed_gen.load(Ordering::Acquire)
            {
                std::thread::yield_now();
            }

            // Locates the inactive buffer (back-buffer) for writing.
            let back_idx = 1 - bridge_ref.active_read_idx.load(Ordering::Relaxed);
            let back_buf = &mut bridge_ref.buffers[back_idx];

            // Fills with sequential data to verify integrity in the reader.
            for i in 0..64 {
                back_buf.buf_l[i] = counter;
                back_buf.buf_r[i] = counter;
                counter += 1.0;
            }
            back_buf.n_samples = 64;

            bridge_ref
                .active_read_idx
                .store(back_idx, Ordering::Release);
            bridge_ref.generation.fetch_add(1, Ordering::Release);

            // Small delay to simulate DSP processing time and allow interleaving.
            std::thread::sleep(Duration::from_micros(10));
        }
    });

    // 3. Reader Thread (Simulates RT Playback Callback):
    // This thread monitors the 'generation' counter. When it changes, it
    // consumes the new "front buffer".
    let start = Instant::now();
    let mut last_gen = 0;
    let mut reads = 0;
    let mut last_val_read = -1.0;

    while reads < 1000 && start.elapsed() < Duration::from_millis(500) {
        let bridge_ref = unsafe { &*bridge_ptr_reader };
        let current_gen = bridge_ref.generation.load(Ordering::Acquire);

        if current_gen != last_gen {
            let read_idx = bridge_ref.active_read_idx.load(Ordering::Acquire);
            let front_buf = &bridge_ref.buffers[read_idx];

            assert_eq!(front_buf.n_samples, 64);

            // Integrity Check: data in a buffer must be contiguous.
            let first_val = front_buf.buf_l[0];
            for i in 0..64 {
                assert_eq!(
                    front_buf.buf_l[i],
                    first_val + i as f32,
                    "Buffer mixing detected in channel L"
                );
                assert_eq!(
                    front_buf.buf_r[i],
                    first_val + i as f32,
                    "Buffer mixing detected in channel R"
                );
            }

            // Monotonicity Check:
            // Even if we skip frames (which may occur in tests under load),
            // we must never read data older than previously read.
            assert!(
                first_val > last_val_read,
                "Read older data than previously seen! (Stale read)"
            );
            last_val_read = front_buf.buf_l[63];

            last_gen = current_gen;
            bridge_ref
                .consumed_gen
                .store(current_gen, Ordering::Release);
            reads += 1;
        }

        if last_gen == 1000 {
            break;
        }
    }

    bridge.consumed_gen.store(u64::MAX, Ordering::Release);
    writer_handle.join().unwrap();

    // 4. Performance Check:
    // The test should complete quickly. If it takes too long, it indicates deadlocks
    // or severe starvation (even though the design is lock-free).
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "Test took too long to execute"
    );
}

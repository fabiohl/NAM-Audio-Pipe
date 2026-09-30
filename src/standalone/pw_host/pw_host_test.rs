// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[test]
#[cfg_attr(
    miri,
    ignore = "Upstream DspBridge in NeuralAmpModeler-rs 0.8.0 lacks UnsafeCell on buffers; concurrent &DspBridge retag produces UB under Miri (Finding F-MIRI-BRIDGE-01)"
)]
fn test_dsp_bridge_concurrent_access() {
    // 1. DspBridge setup:
    // We allocate the bridge on the heap and leak it to obtain a static reference
    // for the lifetime of this test, matching the standalone PipeWire host lifecycle.
    // We then obtain DspBridgeWriter and DspBridgeReader via BridgeRef.
    //
    // Soundness & Aliasing Contract (Finding F-RB-112):
    // Under Stacked Borrows / Tree Borrows, concurrently creating whole-struct
    // references `&mut DspBridge` and `&DspBridge` between threads is undefined behavior.
    // By using `DspBridgeWriter` and `DspBridgeReader`, no thread ever forms `&mut DspBridge`.
    // The writer narrows its borrow strictly to the inactive back-buffer (`buffers[back_idx]`),
    // while the reader narrows its borrow strictly to the active front-buffer (`buffers[read_idx]`),
    // ensuring the two borrowed regions are completely disjoint at all times.
    let bridge_ptr: *mut DspBridge = Box::into_raw(DspBridge::new_boxed());

    // SAFETY: `bridge_ptr` points to the heap-immortal `DspBridge` allocated above.
    let bridge_ref = unsafe { BridgeRef::new(bridge_ptr) };
    let writer = DspBridgeWriter::from_ref(bridge_ref).expect("valid DspBridgeWriter");
    let reader = DspBridgeReader::from_ref(bridge_ref).expect("valid DspBridgeReader");

    // SAFETY: We borrow strictly the atomic counters for spin-wait synchronization in the test.
    // This avoids creating a whole-struct `&DspBridge` reference, preserving unique provenance
    // over the buffer payload memory under Stacked Borrows / Tree Borrows.
    let generation: &'static AtomicU64 = unsafe { &(*bridge_ptr).generation };
    let consumed_gen: &'static AtomicU64 = unsafe { &(*bridge_ptr).consumed_gen };

    let stop = Arc::new(AtomicBool::new(false));
    let stop_writer = Arc::clone(&stop);

    let total_blocks = if cfg!(miri) { 50 } else { 1000 };
    let sleep_delay = if cfg!(miri) {
        Duration::ZERO
    } else {
        Duration::from_micros(10)
    };

    // 2. Writer Thread (Simulates RT Capture Callback):
    // This thread fills sequential audio blocks into the back-buffer using DspBridgeWriter,
    // which synchronizes publication via atomic release stores.
    let writer_handle = std::thread::spawn(move || {
        let mut counter = 0.0f32;
        let mut chunk_l = [0.0f32; 64];
        let mut chunk_r = [0.0f32; 64];

        for _ in 0..total_blocks {
            if stop_writer.load(Ordering::Relaxed) {
                break;
            }

            // Prevents the writer from overwriting the buffer before the reader consumes it.
            // Atomic load via field reference is sound and race-free.
            while generation.load(Ordering::Acquire) > consumed_gen.load(Ordering::Acquire) {
                if stop_writer.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::yield_now();
            }

            // Fills with sequential data to verify integrity in the reader.
            for i in 0..64 {
                chunk_l[i] = counter;
                chunk_r[i] = counter;
                counter += 1.0;
            }

            // Publishes the block. DspBridgeWriter mutates only the back-buffer
            // (1 - active_read_idx), never forming a whole-struct `&mut DspBridge`.
            writer.write_block(&chunk_l, &chunk_r, 64, false);

            // Small delay to simulate DSP processing time and allow interleaving.
            if sleep_delay > Duration::ZERO {
                std::thread::sleep(sleep_delay);
            }
        }
    });

    // 3. Reader Thread (Simulates RT Playback Callback):
    // Consumes published blocks via `DspBridgeReader::read_block`, which validates
    // generation consistency and acquires the front-buffer.
    let start = Instant::now();
    let mut last_gen = 0u64;
    let mut reads = 0;
    let mut last_val_read = -1.0f32;

    let deadline = if cfg!(miri) {
        Duration::from_secs(60)
    } else {
        Duration::from_millis(2000)
    };

    while reads < total_blocks && start.elapsed() < deadline {
        let read_result = reader.read_block(&mut last_gen, |buf_l, buf_r| {
            assert_eq!(buf_l.len(), 64);
            assert_eq!(buf_r.len(), 64);

            // Integrity Check: data in a buffer must be contiguous.
            let first_val = buf_l[0];
            for i in 0..64 {
                assert_eq!(
                    buf_l[i],
                    first_val + i as f32,
                    "Buffer mixing detected in channel L"
                );
                assert_eq!(
                    buf_r[i],
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

            buf_l[63]
        });

        if let Some(last_val) = read_result {
            last_val_read = last_val;
            reads += 1;
        } else {
            std::thread::yield_now();
        }

        if last_gen == total_blocks as u64 {
            break;
        }
    }

    // Terminate writer thread if early break/timeout occurred and join.
    stop.store(true, Ordering::Release);
    writer_handle.join().unwrap();

    // 4. Performance & Liveness Check:
    // The test should complete quickly. If it takes too long, it indicates deadlocks
    // or severe starvation (even though the design is lock-free).
    //
    // Measured: nominal completion is ~0.07 s on an idle 16-core desktop (writer
    // cadence = 1000 × 10 µs sleeps). The 2000 ms window keeps the deadlock/livelock
    // guard (a real deadlock blocks forever; the writer's spin-wait never
    // completes) while tolerating scheduler contention on non-isolated hosts.
    // Under Miri interpretation, the deadline is extended and iteration count reduced.
    assert!(
        start.elapsed() < deadline,
        "Test took too long to execute ({:?})",
        start.elapsed()
    );
    assert_eq!(reads, total_blocks, "Expected all 1000 blocks to be read");

    // Clean up leaked bridge memory now that both threads are finished.
    // SAFETY: `writer_handle` has joined and neither thread accesses `bridge` anymore.
    unsafe {
        drop(Box::from_raw(bridge_ptr));
    }
}

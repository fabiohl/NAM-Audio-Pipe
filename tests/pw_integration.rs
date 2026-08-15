// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire Pipeline Integration (End-to-End) Test
//!
//! Validates the full lifecycle of the PipeWire host: context initialization,
//! SPSC channel setup, gain parameter injection, and graceful shutdown.
//!
//! Requires a running PipeWire daemon (session or system). Without it, the test
//! is skipped by the `#[ignore]` attribute; `utils/tests-quick.sh` Phase 3
//! auto-detects the daemon via `pw-cli info`.

use nam_audio_pipe::standalone::pw_host::{self, PipewireHostConfig};
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{self, GcOverflowBuffer, RtStatusFlags};
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use rtrb::RingBuffer;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

mod common;

/// R-17: the script (`utils/tests-quick.sh` Phase 3) only executes
/// this test after `pw-cli info 0` succeeded. A `pw-cli` failure INSIDE the
/// test therefore means the daemon vanished mid-run or the script↔test
/// probes diverged — a silent `return` would let the script print
/// `LIVE_PW=RAN` with zero DSP executed. Fail closed instead: panic.
fn assert_daemon_probe_consistent() {
    if !common::probe_pipewire_daemon() {
        panic!(
            "R-17: pw-cli info 0 failed inside the test after the script probe \
             passed — daemon vanished or probes diverged. Refusing to emit \
             LIVE_PW=RAN without real DSP execution."
        );
    }
}

/// Tests the basic initialization and communication of the PipeWire pipeline.
///
/// This test simulates the full lifecycle of the engine:
/// 1. Creation of SPSC RingBuffers for commands and telemetry.
/// 2. Spawning the audio thread (host).
/// 3. Sending gain parameters via the control channel.
/// 4. Shutdown signaled via atomic flag.
#[test]
#[ignore = "requires a running PipeWire daemon (session or system); auto-detected by utils/tests-quick.sh Phase 3"]
fn test_pipewire_integration() {
    // R-17: fail-closed divergence check — never a silent skip here (the
    // script probe already gated on the daemon).
    assert_daemon_probe_consistent();

    pipewire::init();
    println!("PipeWire initialized successfully.");

    let (mut param_prod, param_cons) = RingBuffer::new(4);
    let (gc_prod, gc_cons) = RingBuffer::new(4);
    let (res_prod, res_cons) = RingBuffer::new(2);
    let (cs_prod, cs_cons) = RingBuffer::new(2);
    let (sl_prod, sl_cons) = RingBuffer::new(2);
    let (os_prod, os_cons) = RingBuffer::new(2);

    let gc_overflow = Arc::new(GcOverflowBuffer::new(64));
    let rt_status = Arc::new(RtStatusFlags::default());

    let rt_clone = rt_status.clone();
    let gc_overflow_clone = gc_overflow.clone();
    let sys = SystemSnapshot::capture();

    let pw_thread = thread::spawn(move || {
        pw_host::run_pipewire_host(
            param_cons,
            gc_prod,
            gc_overflow_clone,
            res_cons,
            res_prod,
            cs_cons,
            cs_prod,
            rt_clone,
            PipewireHostConfig {
                buffer_size: 0,
                sys,
                ir_raw_samples: None,
                full_wavenet_model: None,
                slimmable_producer: sl_prod,
                os_producer: os_prod,
                oversample: OversampleFactor::Off,
            },
            gc_cons,
            sl_cons,
            os_cons,
            None,
            None,
        )
    });

    thread::sleep(Duration::from_millis(50));
    let _ = param_prod.push(neural_amp_modeler_rs::common::spsc::ParamPayload::InputGain(2.5));
    let _ = param_prod.push(neural_amp_modeler_rs::common::spsc::ParamPayload::OutputGain(-1.0));

    // Wait (bounded) for the RT callback to observe at least one audio quantum.
    // `last_n_samples > 0` proves the capture stream actually processed a buffer
    // — the daemon probe alone is not evidence of DSP execution.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut last_n_samples = 0u32;
    while std::time::Instant::now() < deadline {
        last_n_samples = rt_status.last_n_samples.load(Ordering::Relaxed);
        if last_n_samples > 0 {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    spsc::SHUTDOWN.store(true, Ordering::Relaxed);

    let host_result = match pw_thread.join() {
        Ok(result) => result,
        Err(_) => panic!("The PipeWire thread suffered a fatal panic!"),
    };

    // Fail-closed: the daemon probe confirmed PipeWire is up, so an `Err` from
    // the host is a defect — never a benign "possible daemon absence".
    if let Err(e) = host_result {
        panic!("run_pipewire_host failed while the PipeWire daemon is up: {e:#}");
    }

    assert!(
        last_n_samples > 0,
        "no audio quantum was processed (last_n_samples == 0); \
         LIVE_PW must reflect real DSP execution, not merely daemon presence"
    );

    println!(
        "Integration test completed: host ran, {} samples processed in the last quantum.",
        last_n_samples
    )
}

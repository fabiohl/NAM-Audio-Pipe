// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#![warn(missing_docs)]

//! Main entry point for the NAM-Audio-Pipe standalone PipeWire binary.
//!
//! This file is the "reception" of our virtual studio. It is responsible for:
//! 1. Reading what the user types in the terminal (which amp to load and the input/output volumes).
//! 2. Opening the audio connection to the system (PipeWire), connecting the audio signal to the sound engine.
//! 3. Ensuring that when the user presses CTRL+C, everything shuts down safely, without leaving noise.
//!
//! # Architecture Rules for Developers
//! - **ZERO LOCKS** in the Audio thread (`pw_host` module): Audio does not "wait" for the visual interface. If there is no new instruction, it continues using the previous one. This avoids sound "glitching".
//! - **ZERO ALLOCATIONS** in the Audio thread: The audio channel memory (`process()`) is always prepared 100% in advance. Audio never "requests more RAM" out of nowhere.

use nam_audio_pipe::recording::{self, buffer};
use nam_audio_pipe::standalone::{cli, colors::Colorize, pw_host, rt_setup, setup};

use neural_amp_modeler_rs::SystemSnapshot;
use neural_amp_modeler_rs::common::diagnostics::logger::{LoggerConfig, NamLogger};
use neural_amp_modeler_rs::math::activations::set_activation_tls;
use neural_amp_modeler_rs::{common::spsc, common::spsc::ParamPayload};

use std::ffi::CStr;
use std::sync::atomic::Ordering;

/// Entry point for NAM-rs.
fn main() -> anyhow::Result<()> {
    // Install panic hook to capture crash diagnostics
    neural_amp_modeler_rs::common::panic_hook::install_panic_hook("standalone");

    // Initialize the NamLogger backend (respects RUST_LOG/NAM_LOG_LEVEL; default: info)
    let level_filter = std::env::var("RUST_LOG")
        .or_else(|_| std::env::var("NAM_LOG_LEVEL"))
        .unwrap_or_else(|_| "info".to_string())
        .parse::<log::LevelFilter>()
        .unwrap_or(log::LevelFilter::Info);
    let _logger = NamLogger::init(LoggerConfig {
        level_filter,
        emit_stderr: true,
    })
    .expect("Failed to initialize NamLogger backend");

    #[cfg(feature = "testing")]
    if std::env::var("NAM_DISABLE_GATE").is_ok() {
        neural_amp_modeler_rs::dsp::pipeline::DISABLE_GATE.store(true, Ordering::Relaxed);
        log::info!("⚡ Noise gate disabled via NAM_DISABLE_GATE environment variable.");
    }

    // 1. READ CONFIGURATIONS: The system starts by reading what you typed in the terminal.
    // It figures out which "amplifier" file (.nam) you want to use and the initial volumes.
    let args = cli::parse_args();

    let model_path = args.model_path;
    let initial_in_gain = args.input_gain;
    let initial_out_gain = args.output_gain;
    let buffer_size = args.buffer_size;

    // 2. PREPARE THE AUDIO: Initialize PipeWire (the Linux sound system)
    // and calibrate internal "clocks" to ensure sound output without delays (latency).
    pipewire::init();
    neural_amp_modeler_rs::common::diagnostics::set_host_library_version(pw_library_version());

    // 2.1. IMMEDIATE DIAGNOSTIC EXITS: If the user requested an immediate diagnostic dump,
    // we print it to stdout and exit immediately with code 0 (without starting audio processing).
    if args.diagnose || args.diagnose_full {
        let bundle =
            neural_amp_modeler_rs::DiagnosticBundle::capture().with_full(args.diagnose_full);
        println!("{}", bundle.render());
        unsafe {
            pipewire::deinit();
        }
        std::process::exit(0);
    }

    rt_setup::calibrate_tsc();

    // 3. KNOW THE COMPUTER: Captures a "snapshot" of your processor's capabilities.
    // This helps NAM-rs choose the fastest way to process the audio math.
    let sys = SystemSnapshot::capture();
    log::info!(
        "🎸 {}",
        format!(
            "NAM-Audio-Pipe v{} [x86-64-v3] — Neural Amp Modeler",
            sys.version
        )
        .bright_green()
        .bold()
    );

    // 4. EMERGENCY BUTTON: Configures "Ctrl+C".
    // If you want to close the program, it ensures the audio stops smoothly, without clicks.
    extern "C" fn sigint_handler(_sig: libc::c_int) {
        if spsc::SHUTDOWN.load(Ordering::Acquire) {
            // Second Ctrl-C: graceful shutdown did not respond in time.
            // `_exit(1)` terminates the process immediately without running destructors.
            // The kernel reclaims ALL open resources (file descriptors, mmap, PM QoS,
            // THP advice) — nothing persists after process exit.
            // Prefer `_exit` over `abort` to avoid unneeded core dumps;
            // use `abort()` only if core dumps are explicitly required for debugging.
            unsafe { libc::_exit(1) };
        }
        spsc::SHUTDOWN.store(true, Ordering::Release); // Pairs with Acquire loads in panic_hook and main loop
    }
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        // SAFETY: sigint_handler has the 1-arg signature expected by the kernel
        // when SA_SIGINFO is not set; SA_RESTART alone triggers the 1-arg handler
        // path. The cast to sighandler_t (usize) is a no-op ABI-compatible
        // conversion — both `extern "C" fn(i32)` and `sighandler_t` (usize) are
        // pointer-sized integers on this target.
        sa.sa_sigaction = sigint_handler as *const () as libc::sighandler_t;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }

    // 5. COMMUNICATION CHANNELS: Creates ultra-fast "pipes" for communication.
    let channels = setup::setup_communication_channels();
    let mut producer = channels.param_producer;
    let consumer = channels.param_consumer;
    let gc_producer = channels.gc_producer;
    let gc_consumer = channels.gc_consumer;
    let gc_overflow = channels.gc_overflow;
    let resampler_producer = channels.resampler_producer;
    let resampler_consumer = channels.resampler_consumer;
    let mut cabsim_producer = channels.cabsim_producer;
    let cabsim_consumer = channels.cabsim_consumer;
    let slimmable_producer = channels.slimmable_producer;
    let slimmable_consumer = channels.slimmable_consumer;
    let os_producer = channels.os_producer;
    let os_consumer = channels.os_consumer;
    let rt_status = channels.rt_status;

    // 6. LOAD THE MODEL: If a model path was provided, open the .nam file,
    // parse architecture & metadata, and push it to the audio thread.
    let model_setup = setup::load_initial_model(model_path.as_deref(), &sys, &mut producer);
    let full_wavenet_model = model_setup.full_wavenet_model;
    let model_architecture = model_setup.architecture;

    // 7. LOAD THE CAB-SIM IR: If you said "use cabinet X",
    // this is where the computer opens that WAV file and builds the convolution engine.
    let ir_raw_samples =
        setup::load_initial_cabsim(args.cab_path.as_deref(), buffer_size, &mut cabsim_producer)?;

    // Initial gains
    if initial_in_gain != 0.0 {
        let _ = producer.push(ParamPayload::InputGain(
            neural_amp_modeler_rs::math::dsp::gain_lut::get_gain_lut()
                .db_to_linear(initial_in_gain),
        ));
    }
    if initial_out_gain != 0.0 {
        let _ = producer.push(ParamPayload::OutputGain(
            neural_amp_modeler_rs::math::dsp::gain_lut::get_gain_lut()
                .db_to_linear(initial_out_gain),
        ));
    }

    let _ = producer.push(ParamPayload::SlimOverride(args.slim_override));
    let _ = producer.push(ParamPayload::SetOversample(args.oversample));

    // Apply activation precision mode before any audio processing if explicitly overridden by the user
    if let Some(activation) = args.activation {
        set_activation_tls(activation);
        log::info!(
            "{} Activation precision explicitly set to {:?}",
            "⚡".yellow(),
            activation
        );

        if activation == neural_amp_modeler_rs::math::activations::ActivationPrecision::Fast
            && model_architecture.eq_ignore_ascii_case("LSTM")
        {
            log::warn!(
                "{} Fast activation (Padé) with LSTM architecture is NOT recommended — \
                 measured degradation is ~−13 dB ESR (clearly audible). \
                 Standard (exact-grade) activation is the universal default and costs only \
                 +10–15% CPU for LSTM models. See docs/audio_fidelity_map.md §2.",
                "⚠️".yellow(),
            );
        }
    }

    // Process-wide settings (THP disable + mlockall) before starting PipeWire.
    // Executed here (outside the cold-path of the first DSP frame) to avoid
    // syscalls that would cause jitter at the critical moment of the first audio delivery.
    rt_setup::configure_process_wide();

    // Create the recording ring buffer and spawn the disk I/O thread (opt-in via --record)
    let (recording_producer, recording_io_handle) = if args.record {
        let (producer, consumer) = recording::create_audio_ring_buffer::<{ buffer::MAX_BLOCK_SIZE }>(
            buffer::RING_CAPACITY,
        );
        // The JoinHandle is retained and awaited (bounded) during shutdown so the
        // WAV header is finalized before PipeWire is deinitialized.
        let io_handle = std::thread::Builder::new()
            .name("nam-recording-io".into())
            .spawn(move || {
                tokio_uring::start(async {
                    if let Err(e) = recording::disk_writer_loop(consumer).await {
                        log::error!("Disk writer error: {e}");
                    }
                });
            })
            .expect("Failed to spawn recording I/O thread");
        (Some(producer), Some(io_handle))
    } else {
        (None, None)
    };

    // Run the PipeWire host (blocking)
    let res = pw_host::run_pipewire_host(
        consumer,
        gc_producer,
        gc_overflow,
        resampler_consumer,
        resampler_producer,
        cabsim_consumer,
        cabsim_producer,
        rt_status,
        pw_host::PipewireHostConfig {
            buffer_size,
            sys,
            ir_raw_samples,
            full_wavenet_model,
            slimmable_producer,
            os_producer,
            oversample: args.oversample,
        },
        gc_consumer,
        slimmable_consumer,
        os_consumer,
        recording_producer,
        recording_io_handle,
    );

    log::info!("{} Encerrando NAM-Audio-Pipe...", "🔌".yellow());

    // Signal shutdown to bypass panic hook during cleanup
    neural_amp_modeler_rs::common::panic_hook::set_shutdown_in_progress();

    unsafe {
        pipewire::deinit();
    }
    res?;
    log::info!("{} NAM-Audio-Pipe encerrado. 🎸", "✅".green());
    Ok(())
}

fn pw_library_version() -> String {
    unsafe {
        let ptr = pw_get_library_version();
        if ptr.is_null() {
            return "unknown".to_string();
        }
        CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }
}

unsafe extern "C" {
    fn pw_get_library_version() -> *const std::ffi::c_char;
}

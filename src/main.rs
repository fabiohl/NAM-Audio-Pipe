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

use nam_audio_pipe::recording;
use nam_audio_pipe::standalone::banner::{BannerRequest, CabStatus};
use nam_audio_pipe::standalone::cabinet::InitialCabSetup;
use nam_audio_pipe::standalone::colors::{self, Colorize};
use nam_audio_pipe::standalone::{cli, exit, pw_host, rt_setup, setup, signals};

use neural_amp_modeler_rs::SystemSnapshot;
use neural_amp_modeler_rs::common::diagnostics::logger::{LoggerConfig, NamLogger};
use neural_amp_modeler_rs::common::spsc::{CabSimSwapPayload, ParamPayload};
use neural_amp_modeler_rs::math::activations::set_activation_tls;

use std::path::PathBuf;
#[cfg(feature = "testing")]
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

struct RecordingSetup {
    data_available: Option<Arc<std::sync::atomic::AtomicBool>>,
    status: Option<recording::SharedRecordingStatus>,
    worker: Option<recording::RecordingWorkerGuard>,
    banner_dir: Option<PathBuf>,
}

struct LoadedCabinet {
    ir_raw_samples: Option<Vec<f32>>,
    ir_source_rate: u32,
    initial_nam_cab: Option<InitialCabSetup>,
    status: CabStatus,
}

fn init_logging() -> (log::LevelFilter, &'static NamLogger) {
    let level_filter = std::env::var("RUST_LOG")
        .or_else(|_| std::env::var("NAM_LOG_LEVEL"))
        .unwrap_or_else(|_| "info".to_string())
        .parse::<log::LevelFilter>()
        .unwrap_or(log::LevelFilter::Info);
    let logger = match NamLogger::init(LoggerConfig::new(level_filter).with_stderr(true)) {
        Ok(logger) => logger,
        Err(e) => exit::exit_runtime_error(
            exit::ExitKind::Backend,
            format!("Failed to initialize the logging backend: {e}"),
            "Set RUST_LOG or NAM_LOG_LEVEL to a valid level \
             (error, warn, info, debug, trace).",
        ),
    };
    (level_filter, logger)
}

fn run_diagnose_and_exit(diagnose_full: bool, pause: bool) -> ! {
    pipewire::init();
    // PipeWire logging is deliberately NOT installed here: `--diagnose`
    // must emit a clean structured bundle without asynchronous libpipewire logs.
    neural_amp_modeler_rs::common::diagnostics::set_host_library_version(format!(
        "nam-audio-pipe {} / {}",
        env!("CARGO_PKG_VERSION"),
        pw_host::identity::pw_library_version()
    ));
    let bundle = neural_amp_modeler_rs::DiagnosticBundle::capture().with_full(diagnose_full);
    cli::write_stdout(&format!("{}\n", bundle.render()));
    // SAFETY: `pipewire::init()` was called immediately before capturing diagnostics
    // on this early-exit branch (`diagnose_bundle`). This branch terminates the process via
    // `exit(0)` and is mutually exclusive with the main audio host run loop below.
    unsafe {
        pipewire::deinit();
    }
    if pause {
        nam_audio_pipe::standalone::launcher::pause_if_interactive();
    }
    std::process::exit(0);
}

fn load_cabinet_or_exit(
    args: &cli::CliArgs,
    cabsim_producer: &mut rtrb::Producer<Box<CabSimSwapPayload>>,
    cab_nam_producer: &mut rtrb::Producer<nam_audio_pipe::standalone::cabinet::PipeCabNamPayload>,
    sys: &SystemSnapshot,
) -> LoadedCabinet {
    let loaded = match setup::load_initial_cabinet(
        args.cab_path.as_deref(),
        args.cabsim_partition,
        args.cab_optional,
        args.ir_level_policy,
        cabsim_producer,
        cab_nam_producer,
        sys,
    ) {
        Ok(Some(InitialCabSetup::WavIr {
            raw_samples,
            source_rate,
            ..
        })) => LoadedCabinet {
            ir_raw_samples: Some(raw_samples),
            ir_source_rate: source_rate,
            initial_nam_cab: None,
            status: CabStatus::Asset {
                kind: "WAV",
                bypassed: args.cab_bypass,
            },
        },
        Ok(Some(setup @ InitialCabSetup::Nam { kind, .. })) => LoadedCabinet {
            ir_raw_samples: None,
            ir_source_rate: 0,
            initial_nam_cab: Some(setup),
            status: CabStatus::Asset {
                kind: kind.badge(),
                bypassed: args.cab_bypass,
            },
        },
        Ok(None) => LoadedCabinet {
            ir_raw_samples: None,
            ir_source_rate: 0,
            initial_nam_cab: None,
            status: if args.cab_path.is_some() {
                CabStatus::SkippedOptional
            } else {
                CabStatus::Absent
            },
        },
        Err(e) => exit::exit_runtime_error(
            exit::ExitKind::CabIr,
            format!("{e:#}"),
            "The cabinet asset could not be loaded. Verify it is a valid .wav IR or a \
             1-in/1-out .nam/.namb cabinet model without post-stack head, or pass \
             --cab-optional to continue without cabinet.",
        ),
    };
    if args.cab_bypass {
        log::info!(
            "{} Cabinet stage starts BYPASSED (--cab-bypass; loaded asset & constant latency preserved)",
            "⏸".yellow()
        );
    }
    loaded
}

fn push_initial_parameters(
    producer: &mut rtrb::Producer<ParamPayload>,
    args: &cli::CliArgs,
    model_architecture: &str,
) {
    if args.input_gain != 0.0 {
        let _ = producer.push(ParamPayload::InputGain(
            neural_amp_modeler_rs::math::dsp::gain_lut::get_gain_lut()
                .db_to_linear(args.input_gain),
        ));
    }
    if args.output_gain != 0.0 {
        let _ = producer.push(ParamPayload::OutputGain(
            neural_amp_modeler_rs::math::dsp::gain_lut::get_gain_lut()
                .db_to_linear(args.output_gain),
        ));
    }

    let _ = producer.push(ParamPayload::SlimOverride(args.slim_override));
    let _ = producer.push(ParamPayload::SetOversample(args.oversample));

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
                 +10–15% CPU for LSTM models. See NeuralAmpModeler-rs/docs/audio_fidelity_map.md §2.",
                "⚠️".yellow(),
            );
        }
    }
}

fn setup_recording(args: &cli::CliArgs, housekeeping_cpus: &[usize]) -> RecordingSetup {
    if !args.record {
        return RecordingSetup {
            data_available: None,
            status: None,
            worker: None,
            banner_dir: None,
        };
    }

    let (sender, receiver) = recording::create_recording_transport();
    let data_available = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag_for_thread = Arc::clone(&data_available);

    let recording_status: recording::SharedRecordingStatus =
        Arc::new(Mutex::new(recording::RecordingStatus::Starting));
    let failed_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let failed_for_thread = Arc::clone(&failed_flag);
    let (init_tx, init_rx) = tokio::sync::oneshot::channel();

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (base_dir, cwd_fallback) = cli::resolve_record_dir(
        args.record_dir.as_deref(),
        std::env::var_os("XDG_MUSIC_DIR").as_deref(),
        std::env::var_os("HOME").as_deref(),
        &cwd,
    );
    if cwd_fallback {
        log::warn!(
            "Neither $XDG_MUSIC_DIR nor $HOME is set; recording into the \
             current directory: {} (use --record-dir to override).",
            base_dir.display()
        );
    }
    if let Err(e) = std::fs::create_dir_all(&base_dir) {
        exit::exit_runtime_error(
            exit::ExitKind::Recording,
            format!(
                "Failed to create the recording directory {}: {e}",
                base_dir.display()
            ),
            "Check that the path is writable (read-only mount? permissions?) \
             and that its parent directory exists, or pass --record-dir <DIR>.",
        );
    }
    log::info!("💾 Recording directory: {}", base_dir.display());
    let banner_dir = Some(base_dir.clone());
    let init = recording::RecordingInit::new(
        recording_status.clone(),
        init_tx,
        failed_for_thread,
        base_dir,
    );

    let io_handle = match recording::spawn_recording_worker(
        receiver,
        Some(flag_for_thread),
        init,
        housekeeping_cpus.to_vec(),
    ) {
        Ok(h) => h,
        Err(e) => {
            log::error!("🛑 Failed to spawn recording worker: {e}");
            exit::exit_runtime_error(
                exit::ExitKind::Recording,
                format!("Failed to spawn recording worker: {e}"),
                "The recording disk-I/O thread could not be created. Check the \
                 process/thread limit (`ulimit -u`) and available memory, then retry.",
            );
        }
    };

    match recording::wait_for_recording_init(init_rx, recording::RECORDING_INIT_TIMEOUT) {
        Ok(dir) => {
            log::info!(
                "🎙️  Recording worker ready — capture directory: {}",
                dir.display()
            );
            signals::arm_recording_grace_window();
        }
        Err(e) => {
            log::error!("🛑 Recording startup FAILED: {e}");
            exit::exit_runtime_error(
                exit::ExitKind::Recording,
                format!("Recording startup FAILED: {e}"),
                "Verify the capture directory is writable (pass --record-dir <DIR>), \
                 that it has free space, and that the kernel allows io_uring \
                 (`/proc/sys/kernel/io_uring_disabled`).",
            );
        }
    }

    RecordingSetup {
        data_available: Some(data_available),
        status: Some(recording_status),
        worker: Some(recording::RecordingWorkerGuard::new(
            io_handle,
            Some(sender),
            Some(failed_flag),
        )),
        banner_dir,
    }
}

fn exit_with_recording_outcome(outcome: Option<recording::RecordingWorkerOutcome>) -> ! {
    match outcome {
        None | Some(recording::RecordingWorkerOutcome::Success) => {
            log::info!("{} NAM-Audio-Pipe terminated. 🎸", "✅".green());
            std::process::exit(0);
        }
        Some(outcome @ recording::RecordingWorkerOutcome::SuccessWithLoss { blocks, frames }) => {
            log::error!(
                "🛑 Recording completed with lost audio: {blocks} block(s) / \
                 {frames} frame(s) dropped by overruns — exit {}.",
                outcome.exit_code()
            );
            eprintln!(
                "{}",
                format!(
                    "Recording completed with lost audio: {blocks} block(s) / \
                     {frames} frame(s) dropped by overruns — the capture is \
                     incomplete (exit {}).",
                    outcome.exit_code()
                )
                .bright_red()
                .bold()
            );
            std::process::exit(outcome.exit_code());
        }
        Some(outcome) => {
            log::error!("🛑 Recording worker did not complete cleanly: {outcome}");
            eprintln!(
                "{}",
                format!("Recording failed: {outcome}").bright_red().bold()
            );
            std::process::exit(outcome.exit_code());
        }
    }
}

/// Entry point for NAM-Audio-Pipe.
fn main() -> anyhow::Result<()> {
    neural_amp_modeler_rs::common::panic_hook::install_panic_hook("standalone");
    let (level_filter, _logger) = init_logging();

    #[cfg(feature = "testing")]
    if std::env::var("NAM_DISABLE_GATE").as_deref() == Ok("1") {
        neural_amp_modeler_rs::dsp::pipeline::DISABLE_GATE.store(true, Ordering::Relaxed);
        log::info!("⚡ Noise gate disabled via NAM_DISABLE_GATE environment variable.");
    }

    colors::init_from_env();
    let args = cli::parse_args();

    if args.diagnose || args.diagnose_full {
        run_diagnose_and_exit(args.diagnose_full, args.pause);
    }

    pipewire::init();
    pw_host::init_pipewire_logging(level_filter);
    neural_amp_modeler_rs::common::diagnostics::set_host_library_version(format!(
        "nam-audio-pipe {} / {}",
        env!("CARGO_PKG_VERSION"),
        pw_host::identity::pw_library_version()
    ));

    rt_setup::calibrate_tsc();

    let Some(cpu_receipt) = rt_setup::select_optimal_cpu_with_receipt(args.cpu) else {
        exit::exit_runtime_error(
            exit::ExitKind::Backend,
            "CPU selection did not produce a receipt.",
            "Re-run with --diagnose to inspect the CPU topology; the audio host \
             cannot start without an RT-core decision.",
        );
    };
    let _ = rt_setup::apply_housekeeping_affinity(&cpu_receipt.housekeeping_cpus);

    let sys = SystemSnapshot::capture();
    log::info!(
        "🎸 {}",
        format!(
            "NAM-Audio-Pipe v{} (engine v{}) [x86-64-v3] — Neural Amp Modeler",
            env!("CARGO_PKG_VERSION"),
            sys.version
        )
        .bright_green()
        .bold()
    );

    signals::install_termination_signal_handlers()?;

    let channels = setup::setup_communication_channels()?;
    let mut producer = channels.param_producer;
    let mut cabsim_producer = channels.cabsim_producer;

    let model_setup = setup::load_initial_model(
        args.model_path.as_deref(),
        &sys,
        args.calibration_config(),
        args.output_mode_explicit,
        &mut producer,
    );

    let (mut cab_nam_producer, cab_nam_consumer) =
        rtrb::RingBuffer::<nam_audio_pipe::standalone::cabinet::PipeCabNamPayload>::new(8);
    let cabinet = load_cabinet_or_exit(&args, &mut cabsim_producer, &mut cab_nam_producer, &sys);

    push_initial_parameters(&mut producer, &args, &model_setup.architecture);

    rt_setup::configure_process_wide();

    let rec = setup_recording(&args, &cpu_receipt.housekeeping_cpus);

    let banner_request = BannerRequest {
        model_arch: if model_setup.architecture.is_empty() {
            None
        } else {
            Some(model_setup.architecture)
        },
        cab: cabinet.status,
        recording_dir: rec.banner_dir,
    };
    let res = pw_host::run_pipewire_host_with_cabinet(
        channels.param_consumer,
        channels.gc_producer,
        channels.gc_overflow,
        channels.resampler_consumer,
        channels.resampler_producer,
        channels.cabsim_consumer,
        cabsim_producer,
        channels.rt_status,
        pw_host::PipewireHostConfig {
            buffer_size: args.buffer_size,
            cabsim_partition: args.cabsim_partition,
            sys,
            ir_raw_samples: cabinet.ir_raw_samples,
            ir_source_rate: cabinet.ir_source_rate,
            full_wavenet_model_l: model_setup.full_wavenet_model_l,
            full_wavenet_model_r: model_setup.full_wavenet_model_r,
            has_model_r: model_setup.has_model_r,
            slimmable_producer: channels.slimmable_producer,
            os_producer: channels.os_producer,
            oversample: args.oversample,
            cpu_receipt,
            fail_fast: args.fail_fast,
            reconnect_timeout: args.reconnect_timeout,
            gate_config: args.gate,
            gate_mode: args.gate_mode,
            stream_status: None,
            recording_status: rec.status.clone(),
            banner: banner_request,
            activation: args
                .activation
                .unwrap_or(neural_amp_modeler_rs::math::activations::ActivationPrecision::Standard),
            ir_level_policy: args.ir_level_policy,
            output_limit: args.output_limit,
        },
        channels.gc_consumer,
        channels.slimmable_consumer,
        channels.os_consumer,
        rec.data_available,
        rec.worker,
        pw_host::HostCabinetOptions {
            initial_nam_cab: cabinet.initial_nam_cab,
            cab_nam_channels: Some((cab_nam_producer, cab_nam_consumer)),
            cab_bypass: args.cab_bypass,
        },
    );

    log::info!("{} Shutting down NAM-Audio-Pipe...", "🔌".yellow());

    if let Some(status) = &rec.status
        && let Ok(guard) = status.lock()
    {
        log::info!("Recording status at exit: {guard:?}");
    }

    neural_amp_modeler_rs::common::panic_hook::set_shutdown_in_progress();

    pw_host::restore_pipewire_logging();
    // SAFETY: `pipewire::init()` was called during startup for the audio host run loop.
    // The host loop, worker threads, and streams have completed teardown before this point.
    // This `deinit` executes exactly once on the normal host shutdown path (mutually exclusive
    // with the early-exit `diagnose_bundle` branch above).
    unsafe {
        pipewire::deinit();
    }
    let recording_outcome = match res {
        Ok(outcome) => outcome,
        Err(e) => {
            log::error!("🛑 PipeWire backend terminated with an error: {e:#}");
            exit::exit_runtime_error(
                exit::ExitKind::Backend,
                format!("{e:#}"),
                exit::BACKEND_UNREACHABLE_HINT,
            );
        }
    };

    exit_with_recording_outcome(recording_outcome);
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Handlers for SPSC non-RT communication and dynamic parameter rebuilds.
//!
//! Handles resampler, buffer quantum logging, CabSim IR rebuild,
//! slimmable WaveNet slicing, and oversampling engine reconfiguration.

use crate::standalone::colors::Colorize;
use neural_amp_modeler_rs::common::diagnostics::{NamErrorCode, SystemSnapshot};
use neural_amp_modeler_rs::common::spsc::{
    self, CabSimSwapPayload, ResamplerSwapPayload, RtStatusFlags, SlimModelPair,
};
use neural_amp_modeler_rs::dsp::cabsim::adapter::{CabSimAdapter, CabSimPair};
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
use neural_amp_modeler_rs::dsp::cabsim::loader::CabSimIr;
use neural_amp_modeler_rs::dsp::oversample::{OsEnginePair, OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
use neural_amp_modeler_rs::models::StaticModel;
use neural_amp_modeler_rs::models::slimmable::SlicingError;
use neural_amp_modeler_rs::models::slimmable::slice_wavenet_model;
use neural_amp_modeler_rs::models::wavenet::WaveNetModelDyn;
use std::sync::atomic::Ordering;

/// F-RB-015 test-only fault injection for the resampler rebuild error path.
///
/// Under `feature = "testing"`, a test can arm a one-shot build failure for a
/// specific [`RtStatusFlags`] instance and request generation. When
/// `handle_resampler_rebuild` captures that instance and generation, the build
/// fails deterministically (no allocator is touched) and — in the pause variant
/// — the handler blocks on a channel until the test releases it, reproducing
/// the F-RB-015 window where the RT thread publishes a newer generation while
/// the main thread handles the failed rebuild of an older one. The arm is
/// scoped to the exact flags instance so parallel tests never trigger it.
///
/// Compiled only under `feature = "testing"`; absent from default/release builds.
#[cfg(feature = "testing")]
mod resampler_fault {
    use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::{Mutex, OnceLock};

    struct ArmedFault {
        flags: usize,
        generation: u64,
        reached_tx: Option<Sender<()>>,
        release_rx: Option<Receiver<()>>,
    }

    static ARMED: OnceLock<Mutex<Option<ArmedFault>>> = OnceLock::new();

    fn flags_id(rt_status: &RtStatusFlags) -> usize {
        std::ptr::from_ref(rt_status) as usize
    }

    /// Arms a one-shot failure for `generation` that fails the next build without
    /// pausing.
    #[cfg(test)]
    pub(super) fn arm_fail_once(rt_status: &RtStatusFlags, generation: u64) {
        *ARMED.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(ArmedFault {
            flags: flags_id(rt_status),
            generation,
            reached_tx: None,
            release_rx: None,
        });
    }

    /// Arms a one-shot failure for `generation` that pauses the handler at the
    /// injection point. Returns the `reached` receiver the test waits on until
    /// the handler is paused, and the `release` sender the test uses to unblock
    /// it.
    #[cfg(test)]
    pub(super) fn arm_fail_and_pause(
        rt_status: &RtStatusFlags,
        generation: u64,
    ) -> (Receiver<()>, Sender<()>) {
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *ARMED.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(ArmedFault {
            flags: flags_id(rt_status),
            generation,
            reached_tx: Some(reached_tx),
            release_rx: Some(release_rx),
        });
        (reached_rx, release_tx)
    }

    /// Injects the fault when armed for this flags instance and the captured
    /// generation.
    ///
    /// Consumes the one-shot arm. When it matches, returns `true` (and, in the
    /// pause variant, blocks until the test sends `release`) so the caller
    /// treats the build as failed. Returns `false` when not armed, leaving a
    /// non-matching arm intact.
    pub(super) fn inject(rt_status: &RtStatusFlags, generation: u64) -> bool {
        let mut guard = match ARMED.get() {
            Some(slot) => slot.lock().unwrap(),
            None => return false,
        };
        let Some(armed) = guard.as_ref() else {
            return false;
        };
        if armed.flags != flags_id(rt_status) || armed.generation != generation {
            return false;
        }
        let armed = guard.take().expect("occupied: checked above");
        drop(guard);
        if let (Some(tx), Some(rx)) = (armed.reached_tx, armed.release_rx) {
            let _ = tx.send(());
            let _ = rx.recv();
        }
        true
    }
}

/// F-RB-017 test-only fault injection for the oversample engine rebuild error
/// path.
///
/// Under `feature = "testing"`, a test can arm a **persistent** build failure
/// for a specific [`RtStatusFlags`] instance and request generation. Every
/// build attempt for that generation fails deterministically (no allocator is
/// touched) and is counted, so a test can assert that N control-loop ticks
/// produce exactly one build attempt for a failed generation. Unlike the
/// one-shot F-RB-015 hook, the arm is never consumed — the handler's
/// failed-generation latch (F-RB-017) is what stops the retry storm. The arm
/// is scoped to the exact flags instance so parallel tests never trigger it.
///
/// Compiled only under `feature = "testing"`; absent from default/release builds.
#[cfg(feature = "testing")]
mod os_fault {
    use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
    use std::sync::{Mutex, OnceLock};

    struct ArmedFault {
        flags: usize,
        generation: u64,
        attempts: usize,
    }

    static ARMED: OnceLock<Mutex<Option<ArmedFault>>> = OnceLock::new();

    fn flags_id(rt_status: &RtStatusFlags) -> usize {
        std::ptr::from_ref(rt_status) as usize
    }

    /// Arms a persistent build failure for `generation`.
    #[cfg(test)]
    pub(super) fn arm_fail(rt_status: &RtStatusFlags, generation: u64) {
        *ARMED.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(ArmedFault {
            flags: flags_id(rt_status),
            generation,
            attempts: 0,
        });
    }

    /// Number of build attempts injected so far for the armed (flags,
    /// generation) — how many times the handler actually entered engine
    /// construction for that generation.
    #[cfg(test)]
    pub(super) fn attempts(rt_status: &RtStatusFlags, generation: u64) -> usize {
        let guard = match ARMED.get() {
            Some(slot) => slot.lock().unwrap(),
            None => return 0,
        };
        let Some(armed) = guard.as_ref() else {
            return 0;
        };
        if armed.flags != flags_id(rt_status) || armed.generation != generation {
            return 0;
        }
        armed.attempts
    }

    /// Injects the fault when armed for this flags instance and the captured
    /// generation. The fault persists (it never consumes the arm), so the test
    /// can drive many control-loop ticks and count every build attempt.
    pub(super) fn inject(rt_status: &RtStatusFlags, generation: u64) -> bool {
        let mut guard = match ARMED.get() {
            Some(slot) => slot.lock().unwrap(),
            None => return false,
        };
        let Some(armed) = guard.as_mut() else {
            return false;
        };
        if armed.flags != flags_id(rt_status) || armed.generation != generation {
            return false;
        }
        armed.attempts += 1;
        true
    }
}

/// F-RB-018 test-only fault injection for the slimmable slice error path.
///
/// Under `feature = "testing"`, a test can arm a **persistent** slice failure
/// for a specific [`RtStatusFlags`] instance and request generation. Every
/// `slice_wavenet_model` attempt for that generation fails deterministically
/// (no allocator is touched) and is counted, so a test can assert that N
/// control-loop ticks produce exactly one slice attempt for a failed
/// generation. The arm is scoped to the exact flags instance so parallel tests
/// never trigger it.
///
/// Compiled only under `feature = "testing"`; absent from default/release builds.
#[cfg(feature = "testing")]
mod slimmable_fault {
    use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
    use std::sync::{Mutex, OnceLock};

    struct ArmedFault {
        flags: usize,
        generation: u64,
        attempts: usize,
    }

    static ARMED: OnceLock<Mutex<Option<ArmedFault>>> = OnceLock::new();

    fn flags_id(rt_status: &RtStatusFlags) -> usize {
        std::ptr::from_ref(rt_status) as usize
    }

    /// Arms a persistent slice failure for `generation`.
    #[cfg(test)]
    pub(super) fn arm_fail(rt_status: &RtStatusFlags, generation: u64) {
        *ARMED.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(ArmedFault {
            flags: flags_id(rt_status),
            generation,
            attempts: 0,
        });
    }

    /// Number of slice attempts injected so far for the armed (flags,
    /// generation) — how many times the handler actually entered
    /// `slice_wavenet_model` + `prewarm()` for that generation.
    #[cfg(test)]
    pub(super) fn attempts(rt_status: &RtStatusFlags, generation: u64) -> usize {
        let guard = match ARMED.get() {
            Some(slot) => slot.lock().unwrap(),
            None => return 0,
        };
        let Some(armed) = guard.as_ref() else {
            return 0;
        };
        if armed.flags != flags_id(rt_status) || armed.generation != generation {
            return 0;
        }
        armed.attempts
    }

    /// Injects the fault when armed for this flags instance and the captured
    /// generation. The fault persists (it never consumes the arm).
    pub(super) fn inject(rt_status: &RtStatusFlags, generation: u64) -> bool {
        let mut guard = match ARMED.get() {
            Some(slot) => slot.lock().unwrap(),
            None => return false,
        };
        let Some(armed) = guard.as_mut() else {
            return false;
        };
        if armed.flags != flags_id(rt_status) || armed.generation != generation {
            return false;
        }
        armed.attempts += 1;
        true
    }
}

/// Main-thread failure latches for off-RT rebuilds (F-RB-017 / F-RB-018).
///
/// `RtStatusFlags` (owned by the `NeuralAmpModeler-rs` crate) already tracks
/// `resampler_failed_generation` because the RT callback reads it for the
/// fail-open unmute guard. Oversampling and slimmable rebuilds have no RT-side
/// reader, so their failed generations live here — plain main-thread state
/// owned by the control loop. It mirrors the resampler semantics exactly: a
/// build failure for generation N suppresses further attempts until a newer
/// request (higher generation) arrives, capping the retry storm at one
/// allocation + one `log::error!` per generation.
#[derive(Debug, Default)]
pub(super) struct RebuildFailureTracker {
    /// Last oversample build generation that failed (`0` = none).
    pub(super) os_failed_generation: u64,
    /// Last slimmable slice generation that failed (`0` = none).
    pub(super) slimmable_failed_generation: u64,
}

/// Builds the replacement resampler and streaming buffer for a rate change
/// (off-RT main thread; allocates).
///
/// Under `feature = "testing"` the F-RB-015 fault hook can force a
/// deterministic one-shot build failure and pause the handler exactly in the
/// lost-wakeup window (see [`resampler_fault`]).
fn build_resampler_pair(
    host_rate: u32,
    nam_rate: u32,
    _generation: u64,
    _rt_status: &RtStatusFlags,
) -> (
    Result<NamResampler, anyhow::Error>,
    Result<StreamingResampleBuffer, NamErrorCode>,
) {
    #[cfg(feature = "testing")]
    if resampler_fault::inject(_rt_status, _generation) {
        return (
            Err(anyhow::anyhow!("injected F-RB-015 resampler build fault")),
            Err(NamErrorCode::OutOfMemory),
        );
    }
    (
        NamResampler::new(host_rate, nam_rate, 2048),
        StreamingResampleBuffer::new(host_rate, nam_rate, MAX_RESAMP_BUF),
    )
}

/// Handles dynamic resampler rebuild requested by the audio thread.
///
/// Builds a consistent "photograph" of the pending request: the generation is
/// captured with Acquire (which orders the rate stores published before the
/// Release increment in `sync_rate`), then the resampler is built and pushed
/// inside a [`ResamplerSwapPayload`] stamped with that generation. The rebuild
/// request is only cleared if no newer generation was published while the build
/// was in flight — otherwise `NEEDS_RESAMPLER_REBUILD` is re-armed so the next
/// control-loop iteration rebuilds for the most recent request (F-RB-004). The
/// same lost-wakeup guard applies to the failure arms (F-RB-015): a failed
/// build for generation N never erases a newer request published while the
/// failure was being handled.
pub(super) fn handle_resampler_rebuild(
    rt_status: &RtStatusFlags,
    _sys: &SystemSnapshot,
    resampler_producer: &mut rtrb::Producer<Box<ResamplerSwapPayload>>,
) {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD) {
        return;
    }
    let generation = rt_status.requested_rate_generation.load(Ordering::Acquire);
    let target_host_rate = rt_status.requested_host_rate.load(Ordering::Relaxed);
    let target_nam_rate = rt_status.requested_nam_rate.load(Ordering::Relaxed);

    if target_host_rate != 0 && target_nam_rate != 0 {
        match build_resampler_pair(target_host_rate, target_nam_rate, generation, rt_status) {
            (Ok(new_rs), Ok(new_stream)) => {
                rt_status
                    .resampler_failed_generation
                    .store(0, Ordering::Release);

                log::info!(
                    "{} Sample rate updated: PW={} Hz, NAM={} Hz (bypass={})",
                    "🔄".cyan(),
                    target_host_rate,
                    target_nam_rate,
                    new_rs.is_bypass()
                );

                let payload = Box::new(ResamplerSwapPayload {
                    generation,
                    resampler: Box::new(new_rs),
                    stream: Box::new(new_stream),
                });
                if resampler_producer.push(payload).is_err() {
                    // Fail-closed: the replacement was built but could not reach
                    // the RT callback. Keep NEEDS_RESAMPLER_REBUILD set so the
                    // next main-loop iteration retries the delivery. Clearing
                    // NEEDS here (or setting REBUILD_FAILED) would either strand
                    // RESAMP_SWAP_PENDING (permanent mute) or unmute with the
                    // stale resampler (wrong rate).
                    // Sprint 6 / T6.1: concise runtime warning, no support block.
                    log::warn!(
                        "[E2201 | RESAMPLER_CHANNEL_FULL] Resampler channel full — rebuild will \
                         be retried; the audio engine is overloaded (PW={} Hz, NAM={} Hz). \
                         The swap is retried automatically until delivery succeeds.",
                        target_host_rate,
                        target_nam_rate
                    );
                    return;
                }
                rearm_rebuild_if_superseded(rt_status, generation);
            }
            (Err(e), _) => {
                log::error!(
                    "[E2200 | RESAMPLER_BUILD_FAILED] Failed to rebuild resampler for PW={} Hz \
                     and NAM={} Hz ({e}) — audio will continue with the previous resampler; if \
                     the sample rate is incorrect, restart NAM-Audio-Pipe.",
                    target_host_rate,
                    target_nam_rate
                );

                rt_status
                    .resampler_failed_generation
                    .store(generation, Ordering::Release);
                // F-RB-015: the failure must not erase a newer request published
                // while the build was in flight — same lost-wakeup guard as the
                // success arm. A failure without a newer generation clears the
                // request (no spurious retry).
                rearm_rebuild_if_superseded(rt_status, generation);
            }
            (_, Err(e)) => {
                log::error!(
                    "[E2200 | RESAMPLER_BUILD_FAILED] Failed to create streaming resample buffer \
                     for PW={} Hz and NAM={} Hz ({e:?}) — audio will continue with the previous \
                     resampler; if the sample rate is incorrect, restart NAM-Audio-Pipe.",
                    target_host_rate,
                    target_nam_rate
                );

                rt_status
                    .resampler_failed_generation
                    .store(generation, Ordering::Release);
                // F-RB-015: same lost-wakeup guard as the resampler failure arm.
                rearm_rebuild_if_superseded(rt_status, generation);
            }
        }
    }
}

/// Lost-wakeup guard (F-RB-004) for the main-thread side of a resampler rebuild.
///
/// Clears `NEEDS_RESAMPLER_REBUILD` and re-arms it if the request generation
/// advanced past the generation the just-completed build was stamped with. The
/// clear runs *first* and the check *after* it: if the RT thread publishes a
/// new request between the clear and the load, the load observes the advanced
/// generation and the re-arm below restores the bit; if the publish happens
/// after the load, the RT's own `set_flag` lands on the already-cleared bit and
/// sticks. Either interleaving leaves the request visible — the request can
/// never be erased by a stale build completion.
#[inline(always)]
fn rearm_rebuild_if_superseded(rt_status: &RtStatusFlags, generation: u64) {
    rt_status.clear_flag_relaxed(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD);
    if rt_status.requested_rate_generation.load(Ordering::Acquire) != generation {
        rt_status.set_flag(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD);
    }
}

/// Logs PipeWire quantum renegotiation updates.
pub(super) fn handle_quantum_log(rt_status: &RtStatusFlags) {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_QUANTUM_LOG) {
        return;
    }
    let new_quantum = rt_status.requested_buffer_frames.load(Ordering::Relaxed);
    let old_quantum = rt_status.previous_buffer_frames.load(Ordering::Relaxed);
    if new_quantum != 0 && new_quantum != old_quantum {
        log::info!(
            "{} PipeWire quantum renegotiated: {} -> {} samples ({}->{} ms @48kHz)",
            "🔄".cyan(),
            old_quantum,
            new_quantum,
            old_quantum as f64 * 1000.0 / 48_000.0,
            new_quantum as f64 * 1000.0 / 48_000.0,
        );
        rt_status
            .previous_buffer_frames
            .store(new_quantum, Ordering::Relaxed);
    }
    rt_status.clear_flag_relaxed(spsc::RT_STATUS_NEEDS_QUANTUM_LOG);
}

/// Handles CabSim IR dynamic rebuild (quantum and rate calibration, F-RB-006).
///
/// The cab-sim stage runs at the applied host output rate, so the preserved
/// original IR (`ir_raw_samples` at `ir_source_rate`) is resampled
/// specifically for the requested host rate before building a
/// stereo-decoupled [`CabSimPair`] (independent L/R adapters, identical IR).
/// The pair is stamped with the host rate it was calibrated for so the RT
/// can detect drift again.
///
/// Lost-wakeup guard (F-RB-004 pattern): the request generation is captured
/// with Acquire before building; the flag is only cleared via
/// [`rearm_cabsim_if_superseded`] if no newer generation was published while
/// the build was in flight.
///
/// Rollback: on build failure the handler delivers `None` — safe cab-sim
/// bypass — instead of letting the RT run an IR calibrated for a divergent
/// rate. The RT re-requests while `active == None`, so transient failures
/// recover automatically.
pub(super) fn handle_cabsim_rebuild(
    rt_status: &RtStatusFlags,
    ir_raw_samples: Option<&[f32]>,
    ir_source_rate: u32,
    _sys: &SystemSnapshot,
    cabsim_producer: &mut rtrb::Producer<Box<CabSimSwapPayload>>,
) {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD) {
        return;
    }
    let generation = rt_status
        .requested_cabsim_generation
        .load(Ordering::Acquire);
    let requested_partition = rt_status
        .requested_cabsim_partition_size
        .load(Ordering::Relaxed) as usize;
    let target_host_rate = rt_status.requested_cabsim_host_rate.load(Ordering::Relaxed);
    if requested_partition == 0 || target_host_rate == 0 {
        return;
    }
    // Fail-closed partition bound (G-RB-003 / T6.2): the RT-requested partition
    // must lie in [16, MAX_RESAMP_BUF]. A spurious quantum outside that domain
    // is clamped before any `ConvEngine` is instantiated, so no oversized FFT
    // structure is ever allocated off-RT.
    let partition_size = requested_partition.clamp(16, MAX_RESAMP_BUF);
    if partition_size != requested_partition {
        log::warn!(
            "Requested cabsim partition_size {} clamped to {}",
            requested_partition,
            partition_size
        );
    }
    let Some(raw_samples) = ir_raw_samples else {
        rt_status.clear_flag_relaxed(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD);
        return;
    };

    match build_cabsim_pair(
        raw_samples,
        ir_source_rate,
        target_host_rate,
        partition_size,
    ) {
        Ok(pair) => {
            log::info!(
                "{} Cab-sim IR rebuilt: rate={} Hz, partition_size={} ({} partitions, FFT={})",
                "🔄".cyan(),
                target_host_rate,
                partition_size,
                pair.l.num_partitions(),
                pair.l.engine().fft_size(),
            );
            // Box::new runs exclusively on this (non-RT) main thread: the RT
            // swap then moves the same allocation into the GC (F-RB-007).
            let payload = Box::new(CabSimSwapPayload {
                generation,
                pair: Some(Box::new(pair)),
            });
            if cabsim_producer.push(payload).is_err() {
                // Fail-closed: keep NEEDS_CABSIM_REBUILD so the next
                // main-loop iteration retries. Clearing NEEDS here
                // would lock the RT on the stale partition/rate.
                // Sprint 6 / T6.1: concise runtime warning, no support block.
                log::warn!(
                    "[E3100 | PARAM_CHANNEL_FULL] Cab-sim rebuild channel full — rebuild will \
                     be retried; the audio engine is overloaded (partition={}, PW={} Hz). \
                     The swap is retried automatically until delivery succeeds.",
                    partition_size,
                    target_host_rate
                );
                return;
            }
            rearm_cabsim_if_superseded(rt_status, generation);
        }
        Err(e) => {
            log::error!(
                "Failed to rebuild Cab-sim IR ({} -> {} Hz, partition={}): {e:#} — bypassing cab-sim",
                ir_source_rate,
                target_host_rate,
                partition_size,
            );
            let payload = Box::new(CabSimSwapPayload {
                generation,
                pair: None,
            });
            if cabsim_producer.push(payload).is_err() {
                log::warn!(
                    "[E3100 | PARAM_CHANNEL_FULL] Cab-sim bypass channel full — rebuild will \
                     be retried; the audio engine is overloaded. The swap is retried \
                     automatically until delivery succeeds."
                );
                return;
            }
            rearm_cabsim_if_superseded(rt_status, generation);
        }
    }
}

/// Builds a stereo-decoupled [`CabSimPair`] from the preserved original IR,
/// resampled for the applied host output rate. Off-RT only (allocates).
fn build_cabsim_pair(
    raw_samples: &[f32],
    ir_source_rate: u32,
    target_host_rate: u32,
    partition_size: usize,
) -> anyhow::Result<CabSimPair> {
    if raw_samples.is_empty() {
        anyhow::bail!("IR has no samples");
    }
    let resampled: Option<Vec<f32>> = if ir_source_rate != 0 && ir_source_rate != target_host_rate {
        Some(
            CabSimIr::resample(raw_samples, ir_source_rate, target_host_rate).map_err(|e| {
                anyhow::anyhow!("IR resample ({ir_source_rate} -> {target_host_rate} Hz): {e}")
            })?,
        )
    } else {
        None
    };
    let samples: &[f32] = resampled.as_deref().unwrap_or(raw_samples);

    let build_adapter = || {
        ConvEngine::new(samples, partition_size)
            .map_err(|e| anyhow::anyhow!("Cab-sim engine: {e}"))
            .and_then(|engine| {
                CabSimAdapter::new(Box::new(engine))
                    .map_err(|e| anyhow::anyhow!("Cab-sim adapter: {e:?}"))
            })
    };
    let l = build_adapter()?;
    let r = build_adapter()?;
    Ok(CabSimPair {
        l: Box::new(l),
        r: Box::new(r),
        sample_rate: target_host_rate,
    })
}

/// Lost-wakeup guard (F-RB-004 pattern) for the main-thread side of a
/// cab-sim rebuild.
///
/// Clears `NEEDS_CABSIM_REBUILD` and re-arms it if the cabsim generation
/// advanced past the generation the just-completed build was stamped with.
/// The clear runs *first* and the check *after* it, so a rebuild request
/// published during the resample/build cannot be erased by the stale
/// completion.
#[inline(always)]
fn rearm_cabsim_if_superseded(rt_status: &RtStatusFlags, generation: u64) {
    rt_status.clear_flag_relaxed(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD);
    if rt_status
        .requested_cabsim_generation
        .load(Ordering::Acquire)
        != generation
    {
        rt_status.set_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD);
    }
}

/// Handles WaveNet slimmable channel slicing rebuild (F-RB-005).
///
/// Slices and prewarms the L and (if stereo) R channel models **before** any
/// delivery, then pushes both in a single [`SlimModelPair`] envelope over the
/// SPSC channel. The RT drain consumes the pair with one `pop()` and swaps L
/// and R in the same logical block — an all-or-nothing transaction. If the
/// channel is full, neither channel is delivered and
/// `RT_STATUS_NEEDS_SLIMMABLE_REBUILD` stays armed for a full retry in the next
/// main-loop iteration.
///
/// F-RB-018: deterministic rejections (target channel < 4, absent or
/// non-WaveNet model) are **terminal** — the request is cleared immediately, so
/// an incompatible model never repeats `slice_wavenet_model` + `prewarm()`
/// (both allocators) on every control-loop tick. Only genuine slice failures
/// are **transient**: they are latched in
/// [`RebuildFailureTracker::slimmable_failed_generation`] and retried only
/// when a newer request (higher generation) arrives.
pub(super) fn handle_slimmable_rebuild(
    rt_status: &RtStatusFlags,
    full_wavenet_model_l: Option<&StaticModel>,
    full_wavenet_model_r: Option<&StaticModel>,
    has_model_r: bool,
    _sys: &SystemSnapshot,
    slimmable_producer: &mut rtrb::Producer<Box<SlimModelPair>>,
    failures: &mut RebuildFailureTracker,
) {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD) {
        return;
    }
    let target_ch = rt_status.requested_slimmable_ch.load(Ordering::Relaxed) as usize;
    let generation = rt_status
        .requested_slimmable_generation
        .load(Ordering::Acquire);

    // F-RB-018 — terminal rejections: deterministic conditions that an
    // identical retry can never fix. The request is cleared (lost-wakeup guard
    // still applies: a newer generation published while the rejection was
    // handled survives), so the slice/prewarm allocators are never re-entered
    // per control-loop tick.
    if target_ch < 4 {
        log::warn!(
            "Slimmable rebuild rejected: target channel {target_ch} < 4 — keeping the full \
             model (no retry)."
        );
        rearm_slimmable_if_superseded(rt_status, generation);
        return;
    }
    let Some(m_l) = full_wavenet_model_l else {
        log::warn!(
            "Slimmable rebuild rejected: no full WaveNet model is loaded — keeping the full \
             model (no retry)."
        );
        rearm_slimmable_if_superseded(rt_status, generation);
        return;
    };
    let StaticModel::WavenetDyn(w_l) = m_l else {
        log::warn!(
            "Slimmable rebuild rejected: the loaded model is not a sliceable WaveNetDyn — \
             keeping the full model (no retry)."
        );
        rearm_slimmable_if_superseded(rt_status, generation);
        return;
    };

    // F-RB-018 — transient-failure gate: a slice failure for this exact
    // generation was already recorded (same generation-latch pattern as T2.1);
    // retry only when a newer request bumps the generation.
    if failures.slimmable_failed_generation == generation {
        return;
    }

    // Build L channel model from full_wavenet_model_l.
    let model_l = match slice_slimmable_model(w_l.as_ref(), target_ch, rt_status, generation) {
        Ok(mut slimmed) => {
            slimmed.prewarm();
            Box::new(StaticModel::WavenetDyn(Box::new(slimmed)))
        }
        Err(_) => {
            failures.slimmable_failed_generation = generation;
            rt_status.set_flag(spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED);
            rearm_slimmable_if_superseded(rt_status, generation);
            return;
        }
    };

    // Build R channel model from full_wavenet_model_r for stereo configurations.
    let model_r = if has_model_r {
        let Some(m_r) = full_wavenet_model_r else {
            log::warn!(
                "Slimmable rebuild rejected: stereo configuration requested but the R model \
                 is absent — keeping the full model (no retry)."
            );
            rearm_slimmable_if_superseded(rt_status, generation);
            return;
        };
        let StaticModel::WavenetDyn(w_r) = m_r else {
            log::warn!(
                "Slimmable rebuild rejected: the R model is not a sliceable WaveNetDyn — \
                 keeping the full model (no retry)."
            );
            rearm_slimmable_if_superseded(rt_status, generation);
            return;
        };
        match slice_slimmable_model(w_r.as_ref(), target_ch, rt_status, generation) {
            Ok(mut slimmed) => {
                slimmed.prewarm();
                Some(Box::new(StaticModel::WavenetDyn(Box::new(slimmed))))
            }
            Err(_) => {
                failures.slimmable_failed_generation = generation;
                rt_status.set_flag(spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED);
                rearm_slimmable_if_superseded(rt_status, generation);
                return;
            }
        }
    } else {
        None
    };

    failures.slimmable_failed_generation = 0;

    let pair = Box::new(SlimModelPair {
        generation,
        channels: target_ch,
        l: Some(model_l),
        r: model_r,
    });
    if slimmable_producer.push(pair).is_err() {
        // Fail-closed (F-RB-005): neither channel is delivered; keep NEEDS so
        // the next cycle retries the whole pair instead of delivering a
        // half-swap that would desynchronize L/R generations.
        // Sprint 6 / T6.1: concise runtime warning, no support block.
        log::warn!(
            "[E3100 | PARAM_CHANNEL_FULL] Slimmable model channel full — rebuild will be \
             retried; the audio engine is overloaded (target_ch={target_ch}). The swap is \
             retried automatically until delivery succeeds."
        );
        return;
    }
    rearm_slimmable_if_superseded(rt_status, generation);
}

/// Slices a WaveNet model for the slimmable rebuild (off-RT; allocates).
///
/// Under `feature = "testing"` the F-RB-018 fault hook can force a
/// deterministic slice failure for a specific [`RtStatusFlags`] instance and
/// generation (see [`slimmable_fault`]).
fn slice_slimmable_model(
    model: &WaveNetModelDyn,
    target_ch: usize,
    _rt_status: &RtStatusFlags,
    _generation: u64,
) -> Result<WaveNetModelDyn, SlicingError> {
    #[cfg(feature = "testing")]
    if slimmable_fault::inject(_rt_status, _generation) {
        return Err(SlicingError::ZeroChannelCount);
    }
    slice_wavenet_model(model, target_ch)
}

/// Lost-wakeup guard (F-RB-004 pattern) for the main-thread side of a
/// slimmable rebuild.
///
/// Clears `NEEDS_SLIMMABLE_REBUILD` and re-arms it if the slimmable generation
/// advanced past the generation the just-completed pair was stamped with. The
/// clear runs *first* and the check *after* it, so a rebuild request published
/// during the slice/prewarm cannot be erased by the stale completion.
#[inline(always)]
fn rearm_slimmable_if_superseded(rt_status: &RtStatusFlags, generation: u64) {
    rt_status.clear_flag_relaxed(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD);
    if rt_status
        .requested_slimmable_generation
        .load(Ordering::Acquire)
        != generation
    {
        rt_status.set_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD);
    }
}

/// Builds the replacement L/R oversample engines for a factor change (off-RT
/// main thread; allocates).
///
/// Under `feature = "testing"` the F-RB-017 fault hook can force a
/// deterministic build failure for a specific [`RtStatusFlags`] instance and
/// generation (see [`os_fault`]).
fn build_os_pair(
    factor: OversampleFactor,
    _generation: u64,
    _rt_status: &RtStatusFlags,
) -> (
    Result<OversampleEngine, NamErrorCode>,
    Result<OversampleEngine, NamErrorCode>,
) {
    #[cfg(feature = "testing")]
    if os_fault::inject(_rt_status, _generation) {
        return (
            Err(NamErrorCode::OutOfMemory),
            Err(NamErrorCode::OutOfMemory),
        );
    }
    (
        OversampleEngine::new(factor, MAX_RESAMP_BUF),
        OversampleEngine::new(factor, MAX_RESAMP_BUF),
    )
}

/// Handles oversampling engine dynamic rebuild.
///
/// F-RB-017: a persistent build failure (e.g. real OOM) must not be retried on
/// every control-loop tick (≤ 100 ms). The failure is recorded in
/// [`RebuildFailureTracker::os_failed_generation`] and the request is cleared
/// through the same lost-wakeup guard as the success arm — at most one
/// allocation attempt and one `log::error!` per request generation, while a
/// newer request (higher generation) always survives.
pub(super) fn handle_oversample_rebuild(
    rt_status: &RtStatusFlags,
    _sys: &SystemSnapshot,
    os_producer: &mut rtrb::Producer<Box<OsEnginePair>>,
    failures: &mut RebuildFailureTracker,
) {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_OS_REBUILD) {
        return;
    }
    let generation = rt_status.requested_os_generation.load(Ordering::Acquire);
    // F-RB-017: this exact generation already failed to build — do not retry
    // until the RT publishes a newer request (which always bumps the
    // generation before re-arming the flag).
    if failures.os_failed_generation == generation {
        return;
    }
    let factor_val = rt_status.requested_os_factor.load(Ordering::Relaxed);
    let factor = OversampleFactor::from_f32(factor_val as f32);
    match build_os_pair(factor, generation, rt_status) {
        (Ok(os_l), Ok(os_r)) => {
            failures.os_failed_generation = 0;
            let pair = Box::new(OsEnginePair {
                generation,
                l: Box::new(os_l),
                r: Box::new(os_r),
            });
            log::info!(
                "{} Oversampling factor changed to {:?}",
                "🔄".cyan(),
                factor,
            );
            if os_producer.push(pair).is_err() {
                // Sprint 6 / T6.1: concise runtime warning, no support block.
                log::warn!(
                    "[E3100 | PARAM_CHANNEL_FULL] OS engine channel full — rebuild will be \
                     retried; the audio engine is overloaded. The oversampling swap is \
                     retried automatically until delivery succeeds."
                );
                return;
            }
            rearm_os_if_superseded(rt_status, generation);
        }
        (Err(e), _) | (_, Err(e)) => {
            log::error!(
                "[E5000 | OUT_OF_MEMORY] Failed to rebuild oversample engine ({e}) — audio \
                 will continue with the previous oversampling state; retried only when a \
                 newer oversampling request arrives."
            );
            // F-RB-017: record the failed generation and clear the request via
            // the same lost-wakeup guard as the success arm — a failure for N
            // never erases a newer request published while it was handled, and
            // with no newer request the flag is cleared (no per-tick retry).
            failures.os_failed_generation = generation;
            rearm_os_if_superseded(rt_status, generation);
        }
    }
}

/// Lost-wakeup guard (F-RB-004 pattern) for the main-thread side of an
/// oversampling rebuild.
///
/// Clears `NEEDS_OS_REBUILD` and re-arms it if the oversample generation
/// advanced past the generation the just-completed pair was stamped with. The
/// clear runs *first* and the check *after* it, so an oversample request published
/// during engine construction cannot be erased by the stale completion.
#[inline(always)]
fn rearm_os_if_superseded(rt_status: &RtStatusFlags, generation: u64) {
    rt_status.clear_flag_relaxed(spsc::RT_STATUS_NEEDS_OS_REBUILD);
    if rt_status.requested_os_generation.load(Ordering::Acquire) != generation {
        rt_status.set_flag(spsc::RT_STATUS_NEEDS_OS_REBUILD);
    }
}

#[cfg(test)]
#[path = "handlers_test.rs"]
mod tests;

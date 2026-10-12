// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire graph latency calculation and publication.
//!
//! Provides algorithmic DSP processing latency accounting and publication to
//! the PipeWire graph via `SPA_PARAM_ProcessLatency` and `SPA_PARAM_Latency`
//! SPA PODs. All operations in this module run strictly outside the audio
//! callback (off-RT).

use super::output_pw::SpaPodStorage;
use pipewire as pw;

/// Computes total algorithmic DSP latency in samples and equivalent nanoseconds.
///
/// Total algorithmic latency is composed of:
/// 1. Cab-sim partitioned convolution latency (exactly `partition_size` samples
///    from `CabSimAdapter::latency_samples` when active, 0 when bypassed).
/// 2. Resampler filter chain group delay (from
///    `StreamingResampleBuffer::latency_samples`, 0 when host rate matches model rate).
/// 3. Intra-cycle bridge latency (0 samples in intra-cycle lockstep execution).
///
/// Returns `(total_samples, latency_ns)`.
#[inline]
pub fn compute_dsp_latency(
    cabsim_samples: u32,
    resampler_samples: u32,
    host_rate: u32,
) -> (u32, i64) {
    let total_samples = cabsim_samples.saturating_add(resampler_samples);
    let latency_ns = if host_rate > 0 {
        ((i64::from(total_samples)) * 1_000_000_000) / i64::from(host_rate)
    } else {
        0
    };
    (total_samples, latency_ns)
}

/// Builds a SPA `ProcessLatency` POD for PipeWire graph latency publication.
///
/// The returned pod borrows `storage`; the caller must keep `storage` alive
/// and unmodified for as long as the pod is in use.
///
/// # Safety
/// The caller must not use the returned pod beyond the lifetime of `storage`.
pub unsafe fn build_process_latency_pod(
    latency_samples: u32,
    latency_ns: i64,
    storage: &mut SpaPodStorage<1024>,
) -> anyhow::Result<&pw::spa::pod::Pod> {
    // SAFETY: zeroed builder struct is valid and initialized by `spa_pod_builder_init`.
    let mut builder: pw::spa::sys::spa_pod_builder = unsafe { std::mem::zeroed() };
    let info = pw::spa::sys::spa_process_latency_info {
        quantum: 0.0,
        rate: latency_samples as i32,
        ns: latency_ns,
    };
    unsafe {
        pw::spa::sys::spa_pod_builder_init(
            &mut builder,
            storage.as_mut_slice().as_mut_ptr().cast(),
            storage.as_mut_slice().len() as u32,
        );
        let pod_ptr = pw::spa::sys::spa_process_latency_build(
            &mut builder,
            pw::spa::sys::SPA_PARAM_ProcessLatency,
            &info,
        );
        super::output_pw::validate_built_pod(storage, pod_ptr)
            .ok_or_else(|| anyhow::anyhow!("Failed to build SPA ProcessLatency POD"))
    }
}

/// Builds a SPA `Latency` POD for PipeWire graph latency publication.
///
/// The returned pod borrows `storage`; the caller must keep `storage` alive
/// and unmodified for as long as the pod is in use.
///
/// # Safety
/// The caller must not use the returned pod beyond the lifetime of `storage`.
pub unsafe fn build_latency_pod(
    direction: pw::spa::sys::spa_direction,
    latency_samples: u32,
    latency_ns: i64,
    storage: &mut SpaPodStorage<1024>,
) -> anyhow::Result<&pw::spa::pod::Pod> {
    // SAFETY: zeroed builder struct is valid and initialized by `spa_pod_builder_init`.
    let mut builder: pw::spa::sys::spa_pod_builder = unsafe { std::mem::zeroed() };
    let rate_i32 = latency_samples as i32;
    let info = pw::spa::sys::spa_latency_info {
        direction,
        min_quantum: 0.0,
        max_quantum: 0.0,
        min_rate: rate_i32,
        max_rate: rate_i32,
        min_ns: latency_ns,
        max_ns: latency_ns,
    };
    unsafe {
        pw::spa::sys::spa_pod_builder_init(
            &mut builder,
            storage.as_mut_slice().as_mut_ptr().cast(),
            storage.as_mut_slice().len() as u32,
        );
        let pod_ptr =
            pw::spa::sys::spa_latency_build(&mut builder, pw::spa::sys::SPA_PARAM_Latency, &info);
        super::output_pw::validate_built_pod(storage, pod_ptr)
            .ok_or_else(|| anyhow::anyhow!("Failed to build SPA Latency POD"))
    }
}

/// Updates latency parameters on a PipeWire stream outside the audio callback.
///
/// Builds `ProcessLatency` and `Latency` SPA PODs using stack-local aligned
/// [`SpaPodStorage`] instances and updates the stream via `update_params`.
///
/// Must be invoked while holding the PipeWire ThreadLoop lock when called
/// from the main thread.
pub fn update_stream_latency(
    stream: &pw::stream::Stream,
    direction: pw::spa::sys::spa_direction,
    latency_samples: u32,
    latency_ns: i64,
) -> anyhow::Result<()> {
    let mut storage_proc = SpaPodStorage::<1024>::new();
    let mut storage_lat = SpaPodStorage::<1024>::new();
    // SAFETY: storage_proc and storage_lat are stack-local and outlive the
    // synchronous `update_params` invocation.
    unsafe {
        let proc_pod = build_process_latency_pod(latency_samples, latency_ns, &mut storage_proc)?;
        let lat_pod = build_latency_pod(direction, latency_samples, latency_ns, &mut storage_lat)?;
        stream.update_params(&mut [proc_pod, lat_pod])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_dsp_latency_zero_rate() {
        let (samples, ns) = compute_dsp_latency(128, 16, 0);
        assert_eq!(samples, 144);
        assert_eq!(ns, 0);
    }

    #[test]
    fn compute_dsp_latency_standard_partitions() {
        // 32 samples @ 48kHz: 32 * 1e9 / 48000 = 666_666 ns
        let (samples, ns) = compute_dsp_latency(32, 0, 48_000);
        assert_eq!(samples, 32);
        assert_eq!(ns, 666_666);

        // 64 samples @ 48kHz: 64 * 1e9 / 48000 = 1_333_333 ns
        let (samples, ns) = compute_dsp_latency(64, 0, 48_000);
        assert_eq!(samples, 64);
        assert_eq!(ns, 1_333_333);

        // 128 samples @ 48kHz: 128 * 1e9 / 48000 = 2_666_666 ns
        let (samples, ns) = compute_dsp_latency(128, 0, 48_000);
        assert_eq!(samples, 128);
        assert_eq!(ns, 2_666_666);

        // 256 samples @ 48kHz: 256 * 1e9 / 48000 = 5_333_333 ns
        let (samples, ns) = compute_dsp_latency(256, 0, 48_000);
        assert_eq!(samples, 256);
        assert_eq!(ns, 5_333_333);
    }

    #[test]
    fn compute_dsp_latency_combined_cabsim_and_resampler() {
        // 128 samples cabsim + 32 samples resampler @ 96kHz
        // 160 * 1e9 / 96000 = 1_666_666 ns
        let (samples, ns) = compute_dsp_latency(128, 32, 96_000);
        assert_eq!(samples, 160);
        assert_eq!(ns, 1_666_666);
    }

    #[test]
    fn build_process_latency_pod_creates_valid_pod() {
        pw::init();
        let mut storage = SpaPodStorage::new();
        let (samples, ns) = compute_dsp_latency(128, 0, 48_000);
        let pod = unsafe { build_process_latency_pod(samples, ns, &mut storage) }
            .expect("process latency pod build");

        assert_eq!(pod.type_(), pw::spa::utils::SpaTypes::Object);
        let addr = pod.as_raw_ptr() as usize;
        assert!(addr.is_multiple_of(8));

        // Parse back using libspa FFI to verify field accuracy
        let mut parsed: pw::spa::sys::spa_process_latency_info = unsafe { std::mem::zeroed() };
        let ret = unsafe { pw::spa::sys::spa_process_latency_parse(pod.as_raw_ptr(), &mut parsed) };
        assert_eq!(ret, 0);
        assert_eq!(parsed.rate, 128);
        assert_eq!(parsed.ns, ns);
    }

    #[test]
    fn build_latency_pod_creates_valid_pod() {
        pw::init();
        let mut storage = SpaPodStorage::new();
        let (samples, ns) = compute_dsp_latency(64, 16, 48_000);
        let pod = unsafe {
            build_latency_pod(
                pw::spa::sys::SPA_DIRECTION_OUTPUT,
                samples,
                ns,
                &mut storage,
            )
        }
        .expect("latency pod build");

        assert_eq!(pod.type_(), pw::spa::utils::SpaTypes::Object);
        let addr = pod.as_raw_ptr() as usize;
        assert!(addr.is_multiple_of(8));

        // Parse back using libspa FFI
        let mut parsed: pw::spa::sys::spa_latency_info = unsafe { std::mem::zeroed() };
        let ret = unsafe { pw::spa::sys::spa_latency_parse(pod.as_raw_ptr(), &mut parsed) };
        assert_eq!(ret, 0);
        assert_eq!(parsed.direction, pw::spa::sys::SPA_DIRECTION_OUTPUT);
        assert_eq!(parsed.min_rate, 80);
        assert_eq!(parsed.max_rate, 80);
        assert_eq!(parsed.min_ns, ns);
        assert_eq!(parsed.max_ns, ns);
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

#[test]
fn compute_dsp_latency_zero_rate() {
    let (samples, ns) = compute_dsp_latency(128, 16, 0, 0);
    assert_eq!(samples, 144);
    assert_eq!(ns, 0);
}

#[test]
fn compute_dsp_latency_standard_partitions() {
    // 32 samples @ 48kHz: 32 * 1e9 / 48000 = 666_666 ns
    let (samples, ns) = compute_dsp_latency(32, 0, 0, 48_000);
    assert_eq!(samples, 32);
    assert_eq!(ns, 666_666);

    // 64 samples @ 48kHz: 64 * 1e9 / 48000 = 1_333_333 ns
    let (samples, ns) = compute_dsp_latency(64, 0, 0, 48_000);
    assert_eq!(samples, 64);
    assert_eq!(ns, 1_333_333);

    // 128 samples @ 48kHz: 128 * 1e9 / 48000 = 2_666_666 ns
    let (samples, ns) = compute_dsp_latency(128, 0, 0, 48_000);
    assert_eq!(samples, 128);
    assert_eq!(ns, 2_666_666);

    // 256 samples @ 48kHz: 256 * 1e9 / 48000 = 5_333_333 ns
    let (samples, ns) = compute_dsp_latency(256, 0, 0, 48_000);
    assert_eq!(samples, 256);
    assert_eq!(ns, 5_333_333);
}

#[test]
fn compute_dsp_latency_combined_cabsim_and_resampler() {
    // 128 samples cabsim + 32 samples resampler @ 96kHz
    // 160 * 1e9 / 96000 = 1_666_666 ns
    let (samples, ns) = compute_dsp_latency(128, 32, 0, 96_000);
    assert_eq!(samples, 160);
    assert_eq!(ns, 1_666_666);
}

#[test]
fn compute_dsp_latency_includes_oversampling() {
    // 64 samples cabsim + 16 samples resampler + 12 samples oversampling (X2 @ 48kHz)
    // 92 * 1e9 / 48000 = 1_916_666 ns
    let (samples, ns) = compute_dsp_latency(64, 16, 12, 48_000);
    assert_eq!(samples, 92);
    assert_eq!(ns, 1_916_666);

    // Converted oversampling latency (X4 @ 96kHz -> 36 host samples)
    // 64 cabsim + 32 resampler + 36 oversampling = 132 samples @ 96kHz
    let os_host = convert_os_latency_to_host(18, 96_000, 48_000);
    assert_eq!(os_host, 36);
    let (samples, ns) = compute_dsp_latency(64, 32, os_host, 96_000);
    assert_eq!(samples, 132);
    assert_eq!(ns, 1_375_000);
}

#[test]
fn convert_os_latency_to_host_ratios() {
    assert_eq!(convert_os_latency_to_host(0, 48_000, 48_000), 0);
    assert_eq!(convert_os_latency_to_host(12, 48_000, 48_000), 12);
    assert_eq!(convert_os_latency_to_host(18, 48_000, 48_000), 18);

    // 96kHz host (2x)
    assert_eq!(convert_os_latency_to_host(12, 96_000, 48_000), 24);
    assert_eq!(convert_os_latency_to_host(18, 96_000, 48_000), 36);

    // 44.1kHz host: ceil(12 * 44100 / 48000) = ceil(11.025) = 12
    assert_eq!(convert_os_latency_to_host(12, 44_100, 48_000), 12);
    // ceil(18 * 44100 / 48000) = ceil(16.5375) = 17
    assert_eq!(convert_os_latency_to_host(18, 44_100, 48_000), 17);

    // Edge case: 0 rate
    assert_eq!(convert_os_latency_to_host(12, 0, 48_000), 12);
}

#[test]
fn build_process_latency_pod_creates_valid_pod() {
    pw::init();
    for expected_samples in [0u32, 64u32] {
        let mut storage = SpaPodStorage::new();
        // SAFETY: `storage` is a live stack-local buffer kept alive and unmodified
        // for the lifetime of the returned pod.
        let pod = unsafe { build_process_latency_pod(expected_samples, &mut storage) }
            .expect("process latency pod build");

        assert_eq!(pod.type_(), pw::spa::utils::SpaTypes::Object);
        let addr = pod.as_raw_ptr() as usize;
        assert!(addr.is_multiple_of(8));

        // Parse back using libspa FFI to verify synchronous SPA contract (quantum=0.0, ns=0)
        // SAFETY: `parsed` is overwritten by the parse call and `pod` is the live pod.
        let mut parsed: pw::spa::sys::spa_process_latency_info = unsafe { std::mem::zeroed() };
        // SAFETY: `pod.as_raw_ptr()` is a valid built POD and `parsed` is writable.
        let ret = unsafe { pw::spa::sys::spa_process_latency_parse(pod.as_raw_ptr(), &mut parsed) };
        assert_eq!(ret, 0);
        assert_eq!(parsed.quantum, 0.0);
        assert_eq!(parsed.rate, expected_samples as i32);
        assert_eq!(
            parsed.ns, 0,
            "synchronous ProcessLatency POD must publish ns=0"
        );
    }
}

#[test]
fn build_latency_pod_creates_valid_pod_both_directions() {
    pw::init();
    for expected_samples in [0u32, 64u32] {
        for direction in [
            pw::spa::sys::SPA_DIRECTION_INPUT,
            pw::spa::sys::SPA_DIRECTION_OUTPUT,
        ] {
            let mut storage = SpaPodStorage::new();
            // SAFETY: `storage` is a live stack-local buffer kept alive and unmodified
            // for the lifetime of the returned pod.
            let pod = unsafe { build_latency_pod(direction, expected_samples, &mut storage) }
                .expect("latency pod build");

            assert_eq!(pod.type_(), pw::spa::utils::SpaTypes::Object);
            let addr = pod.as_raw_ptr() as usize;
            assert!(addr.is_multiple_of(8));

            // Parse back using libspa FFI to verify synchronous SPA contract (ns=0)
            // SAFETY: `parsed` is overwritten by the parse call and `pod` is the live pod.
            let mut parsed: pw::spa::sys::spa_latency_info = unsafe { std::mem::zeroed() };
            // SAFETY: `pod.as_raw_ptr()` is a valid built POD and `parsed` is writable.
            let ret = unsafe { pw::spa::sys::spa_latency_parse(pod.as_raw_ptr(), &mut parsed) };
            assert_eq!(ret, 0);
            assert_eq!(parsed.direction, direction);
            assert_eq!(parsed.min_quantum, 0.0);
            assert_eq!(parsed.max_quantum, 0.0);
            assert_eq!(parsed.min_rate, expected_samples as i32);
            assert_eq!(parsed.max_rate, expected_samples as i32);
            assert_eq!(
                parsed.min_ns, 0,
                "synchronous Latency POD must publish min_ns=0"
            );
            assert_eq!(
                parsed.max_ns, 0,
                "synchronous Latency POD must publish max_ns=0"
            );
        }
    }
}

struct StubLatencyPublisher {
    fail_capture: bool,
    fail_playback: bool,
    calls: Vec<(LatencyStreamRole, u32)>,
}

impl StubLatencyPublisher {
    fn new(fail_capture: bool, fail_playback: bool) -> Self {
        Self {
            fail_capture,
            fail_playback,
            calls: Vec::new(),
        }
    }
}

impl LatencyPublisher for StubLatencyPublisher {
    fn publish_latency(
        &mut self,
        role: LatencyStreamRole,
        latency_samples: u32,
    ) -> anyhow::Result<()> {
        self.calls.push((role, latency_samples));
        let should_fail = match role {
            LatencyStreamRole::Capture => self.fail_capture,
            LatencyStreamRole::Playback => self.fail_playback,
        };
        if should_fail {
            anyhow::bail!("injected {} update_params failure", role.label());
        }
        Ok(())
    }
}

#[test]
fn tracker_capture_only_failure_retries_without_premature_acceptance() {
    let t0 = Instant::now();
    let initial = LatencyTarget {
        samples: 0,
        host_rate: 48_000,
    };
    let next = LatencyTarget {
        samples: 128,
        host_rate: 48_000,
    };
    let mut tracker = LatencyPublicationTracker::new(0, 48_000);
    let mut pub_stub = StubLatencyPublisher::new(true, false);

    let outcome1 = tracker.sync_at(&mut pub_stub, next, 128, 0, 0, t0);
    assert!(outcome1.any_newly_accepted);
    assert!(!outcome1.all_accepted);
    assert_eq!(tracker.capture().accepted(), Some(initial));
    assert_eq!(tracker.capture().desired(), next);
    assert_eq!(tracker.capture().consecutive_failures(), 1);
    assert_eq!(tracker.playback().accepted(), Some(next));
    assert_eq!(tracker.playback().consecutive_failures(), 0);
    assert_eq!(
        pub_stub.calls,
        vec![
            (LatencyStreamRole::Capture, 128),
            (LatencyStreamRole::Playback, 128),
        ]
    );

    // Within retry cooldown (200 ms), steady-state loop does not spam PipeWire.
    assert!(!tracker.needs_sync(next, t0 + Duration::from_millis(100)));

    // After cooldown elapses, retry only the failed capture stream and recover.
    pub_stub.fail_capture = false;
    pub_stub.calls.clear();
    let t1 = t0 + Duration::from_millis(200);
    assert!(tracker.needs_sync(next, t1));
    let outcome2 = tracker.sync_at(&mut pub_stub, next, 128, 0, 0, t1);
    assert!(outcome2.any_newly_accepted);
    assert!(outcome2.all_accepted);
    assert_eq!(tracker.capture().accepted(), Some(next));
    assert_eq!(tracker.capture().consecutive_failures(), 0);
    assert_eq!(pub_stub.calls, vec![(LatencyStreamRole::Capture, 128)]);
    assert!(!tracker.needs_sync(next, t1 + Duration::from_millis(500)));
}

#[test]
fn tracker_playback_only_failure_retries_and_recovers() {
    let t0 = Instant::now();
    let initial = LatencyTarget {
        samples: 0,
        host_rate: 48_000,
    };
    let next = LatencyTarget {
        samples: 64,
        host_rate: 48_000,
    };
    let mut tracker = LatencyPublicationTracker::new(0, 48_000);
    let mut pub_stub = StubLatencyPublisher::new(false, true);

    let outcome1 = tracker.sync_at(&mut pub_stub, next, 64, 0, 0, t0);
    assert!(outcome1.any_newly_accepted);
    assert!(!outcome1.all_accepted);
    assert_eq!(tracker.capture().accepted(), Some(next));
    assert_eq!(tracker.playback().accepted(), Some(initial));
    assert_eq!(tracker.playback().consecutive_failures(), 1);

    pub_stub.fail_playback = false;
    pub_stub.calls.clear();
    let t1 = t0 + Duration::from_millis(200);
    let outcome2 = tracker.sync_at(&mut pub_stub, next, 64, 0, 0, t1);
    assert!(outcome2.any_newly_accepted);
    assert!(outcome2.all_accepted);
    assert_eq!(tracker.playback().accepted(), Some(next));
    assert_eq!(tracker.playback().consecutive_failures(), 0);
    assert_eq!(pub_stub.calls, vec![(LatencyStreamRole::Playback, 64)]);
}

#[test]
fn tracker_both_fail_then_succeed_with_rate_limited_backoff() {
    let t0 = Instant::now();
    let initial = LatencyTarget {
        samples: 0,
        host_rate: 48_000,
    };
    let next = LatencyTarget {
        samples: 146,
        host_rate: 48_000,
    };
    let mut tracker = LatencyPublicationTracker::new(0, 48_000);
    let mut pub_stub = StubLatencyPublisher::new(true, true);

    let outcome1 = tracker.sync_at(&mut pub_stub, next, 128, 0, 18, t0);
    assert!(!outcome1.any_newly_accepted);
    assert!(!outcome1.all_accepted);
    assert_eq!(tracker.capture().accepted(), Some(initial));
    assert_eq!(tracker.playback().accepted(), Some(initial));
    assert_eq!(tracker.capture().consecutive_failures(), 1);
    assert_eq!(tracker.playback().consecutive_failures(), 1);

    // Attempt #2 at +200ms fails and pushes next backoff to 400ms.
    let t1 = t0 + Duration::from_millis(200);
    let outcome2 = tracker.sync_at(&mut pub_stub, next, 128, 0, 18, t1);
    assert!(!outcome2.all_accepted);
    assert_eq!(tracker.capture().consecutive_failures(), 2);
    assert_eq!(tracker.playback().consecutive_failures(), 2);
    assert!(!tracker.needs_sync(next, t1 + Duration::from_millis(300)));

    // Attempt #3 at +400ms succeeds for both streams.
    pub_stub.fail_capture = false;
    pub_stub.fail_playback = false;
    pub_stub.calls.clear();
    let t2 = t1 + Duration::from_millis(400);
    assert!(tracker.needs_sync(next, t2));
    let outcome3 = tracker.sync_at(&mut pub_stub, next, 128, 0, 18, t2);
    assert!(outcome3.any_newly_accepted);
    assert!(outcome3.all_accepted);
    assert_eq!(tracker.capture().accepted(), Some(next));
    assert_eq!(tracker.playback().accepted(), Some(next));
    assert_eq!(
        pub_stub.calls,
        vec![
            (LatencyStreamRole::Capture, 146),
            (LatencyStreamRole::Playback, 146),
        ]
    );
}

#[test]
fn tracker_desired_change_during_retry_supersedes_backoff() {
    let t0 = Instant::now();
    let first = LatencyTarget {
        samples: 128,
        host_rate: 48_000,
    };
    let second = LatencyTarget {
        samples: 146,
        host_rate: 48_000,
    };
    let mut tracker = LatencyPublicationTracker::new(0, 48_000);
    let mut pub_stub = StubLatencyPublisher::new(true, false);

    let _ = tracker.sync_at(&mut pub_stub, first, 128, 0, 0, t0);
    assert!(!tracker.needs_sync(first, t0 + Duration::from_millis(50)));

    // Desired changes while capture is still in backoff: must attempt immediately.
    pub_stub.fail_capture = false;
    pub_stub.calls.clear();
    let t_change = t0 + Duration::from_millis(50);
    assert!(tracker.needs_sync(second, t_change));
    let outcome = tracker.sync_at(&mut pub_stub, second, 128, 0, 18, t_change);
    assert!(outcome.all_accepted);
    assert_eq!(tracker.capture().accepted(), Some(second));
    assert_eq!(tracker.playback().accepted(), Some(second));
    assert_eq!(
        pub_stub.calls,
        vec![
            (LatencyStreamRole::Capture, 146),
            (LatencyStreamRole::Playback, 146),
        ]
    );
}

#[test]
fn tracker_active_zero_fallback_rate_change_triggers_update() {
    let t0 = Instant::now();
    let fallback_48k = resolve_effective_host_rate(0, 0);
    assert_eq!(fallback_48k, 48_000);
    let fallback_96k = resolve_effective_host_rate(0, 96_000);
    assert_eq!(fallback_96k, 96_000);
    assert_eq!(resolve_effective_host_rate(44_100, 96_000), 44_100);

    let mut tracker = LatencyPublicationTracker::new(0, fallback_48k);
    let mut pub_stub = StubLatencyPublisher::new(false, false);

    // Even when active_rate == 0 and samples stay 0, a change in fallback param rate triggers sync.
    let target = LatencyTarget {
        samples: 0,
        host_rate: fallback_96k,
    };
    assert!(tracker.needs_sync(target, t0));
    let outcome = tracker.sync_at(&mut pub_stub, target, 0, 0, 0, t0);
    assert!(outcome.all_accepted);
    assert_eq!(tracker.capture().accepted(), Some(target));
    assert_eq!(tracker.playback().accepted(), Some(target));
    assert_eq!(
        pub_stub.calls,
        vec![
            (LatencyStreamRole::Capture, 0),
            (LatencyStreamRole::Playback, 0),
        ]
    );
}

#[test]
fn tracker_reconnect_instance_starts_with_clean_state() {
    let t0 = Instant::now();
    let mut instance1 = LatencyPublicationTracker::new(128, 48_000);
    let mut pub_stub = StubLatencyPublisher::new(true, true);
    let target = LatencyTarget {
        samples: 146,
        host_rate: 48_000,
    };
    let _ = instance1.sync_at(&mut pub_stub, target, 128, 0, 18, t0);
    assert_eq!(instance1.capture().consecutive_failures(), 1);

    // Simulated reconnect creates a fresh tracker for the newly connected streams.
    let instance2 = LatencyPublicationTracker::new(146, 48_000);
    assert_eq!(instance2.capture().consecutive_failures(), 0);
    assert_eq!(instance2.playback().consecutive_failures(), 0);
    assert_eq!(instance2.capture().accepted(), Some(target));
    assert_eq!(instance2.playback().accepted(), Some(target));
    assert!(!instance2.needs_sync(target, t0));
}

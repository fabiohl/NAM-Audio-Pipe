// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

#[test]
fn e2e_exact_reservoir_resolves_distinct_percentiles() {
    // The log-linear histogram can collapse p50/p99/p999 onto the same bucket
    // edge; the exact reservoir must keep distinct samples distinct.
    let status = StreamStatusFlags::new();
    for v in 1..=1000u64 {
        status.e2e_exact.record(v);
    }
    let p50 = status.e2e_exact.percentile_exact(0.50);
    let p99 = status.e2e_exact.percentile_exact(0.99);
    let p999 = status.e2e_exact.percentile_exact(0.999);
    assert!(
        p50 < p99 && p99 < p999,
        "exact percentiles must be distinct when the samples differ: {p50} < {p99} < {p999}"
    );
}

#[test]
fn e2e_exact_reservoir_resets_to_empty() {
    let status = StreamStatusFlags::new();
    status.e2e_exact.record(1_000);
    assert_eq!(status.e2e_exact.len(), 1);
    status.e2e_exact.reset();
    assert_eq!(status.e2e_exact.len(), 0);
}

#[test]
fn applied_latency_pair_coherent_snapshot() {
    let status = StreamStatusFlags::new();
    assert_eq!(status.read_applied_latency_pair(), (0, 0));

    status.update_resampler_latency(32);
    assert_eq!(status.read_applied_latency_pair(), (32, 0));
    assert_eq!(status.applied_resampler_latency.load(Ordering::Acquire), 32);

    status.update_cabsim_latency(128);
    assert_eq!(status.read_applied_latency_pair(), (32, 128));
    assert_eq!(status.applied_cabsim_latency.load(Ordering::Acquire), 128);

    status.update_resampler_latency(0);
    assert_eq!(status.read_applied_latency_pair(), (0, 128));

    status.update_cabsim_latency(0);
    assert_eq!(status.read_applied_latency_pair(), (0, 0));
}

#[test]
fn applied_latencies_coherent_seqlock_snapshot() {
    let status = StreamStatusFlags::new();
    assert_eq!(status.read_applied_latencies(), (0, 0, 0));

    status.update_resampler_latency(32);
    assert_eq!(status.read_applied_latencies(), (32, 0, 0));

    status.update_cabsim_latency(128);
    assert_eq!(status.read_applied_latencies(), (32, 128, 0));

    status.update_oversample_latency(12);
    assert_eq!(status.read_applied_latencies(), (32, 128, 12));

    status.update_oversample_latency(18);
    assert_eq!(status.read_applied_latencies(), (32, 128, 18));

    status.update_resampler_latency(0);
    assert_eq!(status.read_applied_latencies(), (0, 128, 18));

    status.update_oversample_latency(0);
    assert_eq!(status.read_applied_latencies(), (0, 128, 0));

    status.update_cabsim_latency(0);
    assert_eq!(status.read_applied_latencies(), (0, 0, 0));
}

<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# PipeWire SPA Latency Specification & Graph Delay Accounting

This document defines the technical specification and architectural rules governing PipeWire Simple Plugin API (SPA) latency publication and algorithmic delay accounting in **NAM-Audio-Pipe**.

For the complete system topology and threading architecture, consult the main [NAM-Audio-Pipe Architecture Bible](architecture.md).

---

## 1. Scope & Normative References

- **Implementation**: [`src/standalone/pw_host/latency.rs`](../src/standalone/pw_host/latency.rs), [`stream_status.rs`](../src/standalone/pw_host/stream_status.rs), and [`run.rs`](../src/standalone/pw_host/run.rs).
- **PipeWire Headers**: `/usr/include/spa-0.2/spa/param/latency.h`, `/usr/include/spa-0.2/spa/param/latency-utils.h`.
- **Upstream Documentation**: [PipeWire Latency Documentation](https://docs.pipewire.org/page_latency.html).

---

## 2. Core Principle: Additive Evaluation of `rate` and `ns`

PipeWire multimedia graphs schedule nodes and compute latency compensation through the SPA POD interface (`SPA_PARAM_Latency` and `SPA_PARAM_ProcessLatency`).

The calculation in PipeWire (`src/pipewire/stream.c`, function `pw_stream_get_time_n`) evaluates delay as follows:

```c
/* PipeWire upstream stream timing calculation */
time->delay += (latency.min_rate + latency.max_rate) / 2;
time->delay += ((latency.min_ns + latency.max_ns) / 2) * (int64_t)time->rate.denom
             / ((int64_t)SPA_NSEC_PER_SEC * time->rate.num);
```

### Architectural Implications

1. **Additive Evaluation**: PipeWire does **not** treat sample rate (`rate`) and nanosecond (`ns`) fields as mutual fallbacks; it evaluates both terms and adds them to `time->delay`.
2. **Double Compensation Risk**: When an identical time interval is populated across both `rate` (in sample units) and `ns` (in nanoseconds), PipeWire sums the two, doubling the latency compensation reported to the graph scheduler.
3. **Canonical DSP Node Contract**:
   - For sample-synchronous DSP nodes running at graph rate, publish:
     $$\text{min\_rate} = \text{max\_rate} = \text{latency\_samples}, \quad \text{min\_ns} = \text{max\_ns} = 0$$
   - The `ns` properties are reserved for asynchronous physical boundaries (such as USB packet batching or Bluetooth packetization intervals) that operate on fixed wall-clock schedules independent of sample rate.

---

## 3. Algorithmic Delay Synthesis & Unit Conversion

Total algorithmic latency published to PipeWire is the arithmetic sum of active DSP stages:

$$\text{total\_latency} = \text{latency}_{\text{cabsim}} + \text{latency}_{\text{resampler}} + \text{latency}_{\text{oversample}}$$

### 3.1 Oversampling Rate Conversion

The oversampling engine calculates filter group delay at the native model rate (`model_rate`). Before combining with host-rate delays, it is scaled to host sample units using integer ceiling division:

$$\text{os\_host} = \left\lceil \frac{\text{os\_native} \times \text{host\_rate}}{\text{model\_rate}} \right\rceil$$

Implemented via `div_ceil` in `convert_os_latency_to_host`:

```rust
pub fn convert_os_latency_to_host(os_native: u32, host_rate: u32, model_rate: u32) -> u32 {
    if host_rate == 0 || model_rate == 0 || os_native == 0 {
        os_native
    } else {
        ((os_native as u64 * host_rate as u64).div_ceil(model_rate as u64)) as u32
    }
}
```

### 3.2 Lock-Free Seqlock Snapshot

To expose multi-field latency updates across thread boundaries without allocator or mutex contention:

1. **Audio Writer**: Increments `latency_sequence: AtomicU64` with `Ordering::Release` before and after updating latency counters in `StreamStatusFlags`.
2. **Control Plane Reader**: `read_applied_latencies` performs an optimistic read, verifying that `latency_sequence` is even and unchanged across the snapshot. If concurrent mutation occurs, the reader retries until a coherent snapshot is acquired.
3. **PipeWire Notification**: Whenever the synthesized snapshot changes, the main loop calls `pw_stream_set_parameter` to notify the PipeWire scheduler.

---

## 4. Operational Classification: Starvation vs. Validation Failure

The audio host strictly differentiates buffer starvation from SPA protocol corruption:

| Condition                     | Classification      | Callback Action                                                                      | Published Output Chunk Metadata                    |
|:----------------------------- |:------------------- |:------------------------------------------------------------------------------------ |:-------------------------------------------------- |
| **Bridge Starvation**         | Normal Success Path | Delivers analytical silence (`0.0f32`). No error flags raised.                       | `offset = 0`, `size = silence_bytes`, `stride = 4` |
| **Malformed SPA Descriptors** | Validation Failure  | Zeroes provably valid memory; latches `RT_STATUS_HOST_CONTRACT_VIOLATION` (`E2304`). | `offset = 0`, `size = 0`, `stride = 4`             |

---

## 5. Live Graph Inspection Commands

To verify active latency values published by NAM-Audio-Pipe on a live PipeWire system:

```bash
# Query published latency properties on the playback node:
pw-dump | jq '.[] | select(.info.props["node.name"] == "NAM-Audio-Pipe-playback") | .info.params.Latency'

# Monitor real-time driver scheduling and graph delay:
pw-top
```

# ADR: PipeWire SPA Latency Semantics (`rate` vs `ns`) and Oversampling Delay Accounting

- **Status:** Proposed
- **Scope:** `NAM-Audio-Pipe` (`src/standalone/pw_host/latency.rs`, `stream_status.rs`, `run.rs`)
- **References:**
  - PipeWire 1.6.2 source: `src/pipewire/stream.c` (`pw_stream_get_time_n`)
  - SPA headers: `/usr/include/spa-0.2/spa/param/latency.h`, `/usr/include/spa-0.2/spa/param/latency-utils.h`
  - PipeWire Latency Documentation: <https://docs.pipewire.org/page_latency.html>

---

## 1. Context and Problem Statement

`NAM-Audio-Pipe` publishes latency parameters to the PipeWire graph using the SPA POD API (`SPA_PARAM_Latency`).
In `src/standalone/pw_host/latency.rs`:

```rust
builder.add_property(SPA_LATENCY_min_rate, latency_samples);
builder.add_property(SPA_LATENCY_max_rate, latency_samples);
builder.add_property(SPA_LATENCY_min_ns, latency_ns);
builder.add_property(SPA_LATENCY_max_ns, latency_ns);
```

Two questions required empirical and architectural investigation:

1. **SPA `rate` vs `ns` Semantics:** Do PipeWire graph calculations treat `rate` (samples) and `ns` (nanoseconds) as mutually exclusive alternatives, or do they sum them additively?
2. **Oversampling Latency Publication:** How should native oversampling filter delay (`OversampleEngine::latency_samples`: 0 for Off, 12 for X2, 18 for X4 at model rate) be converted to host units and exposed atomically alongside resampler and CabSim latencies?

---

## 2. Source Code Proof (PipeWire 1.6.2)

Inspection of the installed system headers (`/usr/include/spa-0.2/spa/param/latency.h`) and FreeDesktop PipeWire 1.6.2 source code (`src/pipewire/stream.c`, function `pw_stream_get_time_n`) proves conclusively:

```c
/* PipeWire 1.6.2 src/pipewire/stream.c lines 980-990 */
time->delay += (latency.min_rate + latency.max_rate) / 2;
time->delay += ((latency.min_ns + latency.max_ns) / 2) * (int64_t)time->rate.denom
             / ((int64_t)SPA_NSEC_PER_SEC * time->rate.num);
```

### Analysis

1. **Additive Behavior:** PipeWire does **not** treat `rate` and `ns` as fallback alternatives; it evaluates both terms and adds them to `time->delay`.
2. **Double Compensation Risk:** When `latency_samples` is written to `min_rate`/`max_rate` and the exact same duration is converted to nanoseconds and written to `min_ns`/`max_ns`, PipeWire sums the two, doubling the reported delay in graph time calculations.
3. **Canonical PipeWire Recommendation:** The official PipeWire documentation (<https://docs.pipewire.org/page_latency.html>) specifies:
   - For sample-based synchronous DSP nodes running at graph rate, set `min_rate = max_rate = samples`, and set `min_ns = max_ns = 0`.
   - `ns` is intended for hardware or fixed-time asynchronous components (such as USB packet intervals or Bluetooth frame packetization) that operate independently of graph sample rates.

---

## 3. Decision and Constraints

### 3.1 Current Contract Preservation (Fail-Closed)

To preserve existing compatibility contracts until an explicit breaking change to SPA parameter emission is scheduled, `latency.rs` retains its existing dual-field (`rate` and `ns`) emission.

### 3.2 Recommended Target Emission

When updating `update_stream_latency` in `latency.rs` to align strictly with PipeWire's additive `pw_stream_get_time_n` semantics, publish:

- `min_rate = latency_samples`
- `max_rate = latency_samples`
- `min_ns = 0`
- `max_ns = 0`

### 3.3 Oversampling Latency Accounting

To account for oversampling delay accurately without RT locking:

1. **Unit Conversion:** `OversampleEngine::latency_samples()` reports delay in native model samples (at the installed model rate, not an assumed 48 kHz). The function `convert_os_latency_to_host(os_native, host_rate, model_rate)` scales this delay to host samples with integer ceiling:
   $$\text{os\_host} = \left\lceil \frac{\text{os\_native} \times \text{host\_rate}}{\text{model\_rate}} \right\rceil$$
   The implementation is `div_ceil` (`latency.rs`); the previous round-to-nearest text did not match the code. `model_rate` is the rate of the model actually installed (captured during setup/reconnection and on every resampler install), never a hardcoded 48 kHz.
2. **Seqlock Snapshot:** Expanding the 64-bit pair (`resampler`, `cabsim`) to a 3-component snapshot (`resampler`, `cabsim`, `oversample`) is implemented via a lock-free, wait-free sequence lock (`latency_sequence: AtomicU64` in `StreamStatusFlags`).
   - Single RT writer: `latency_sequence` is bumped with `Ordering::Release` before and after the atomic latency stores. The closing bump publishes the field stores to readers.
   - Non-RT readers (`read_applied_latencies`) read the sequence with `Ordering::Acquire`, read the three fields, execute an `Acquire` fence, then re-read the sequence. The fence orders the field loads before the closing validation; retry if the sequence was odd (write in flight) or changed during the read.
3. **Runtime Updates:** Oversampling swaps update `StreamStatusFlags::applied_oversample_latency` inside `drain_os_engines_with_status`, and the main thread latency watcher (`run.rs`) detects the change and notifies PipeWire via `pw_stream_set_parameter`.

---

## 4. Operator Verification Protocol (Physical PipeWire Graph)

For live verification on a physical PipeWire system:

1. **Test Matrix:**
   - Quanta: `16`, `64`, `256` frames (`pw-metadata -n settings 0 clock.force-quantum <Q>`). Quanta below `16` are rejected by the installation gate (the CLI minimum is also `16`).
   - Rates: `44100`, `48000`, `96000` Hz (`pw-metadata -n settings 0 clock.force-rate <R>`)
   - Oversampling: `--oversample off`, `--oversample 2x`, `--oversample 4x`
   - CabSim: Partition `32`, `64`, `128` (the partition is independent of the quantum; allowed values are `{32, 64, 128, 256}`)
2. **Commands:**

   ```bash
   # Inspect node latency published to PipeWire:
   pw-dump | jq '.[] | select(.info.props["node.name"] == "NAM-Audio-Pipe-playback") | .info.params.Latency'

   # Verify graph delay calculation:
   pw-top
   ```

3. **Validation Criteria:**
   - At quantum 16, CabSim installs without underrun or contract violation flags (`RT_STATUS_CABSIM_CONTRACT_VIOLATION == 0`).
   - Dynamic oversample switching (via CLI/param) updates published latency without audio dropouts or graph errors.

### 4.1 Starvation success versus validation failure

Bridge starvation (the capture side produced no new block for this quantum: paused input, pending rebuild, clock drift or quantum miss) is a **success path**: the playback callback still dequeues the buffer, validates the SPA descriptors, and delivers coherent analytical silence with `offset = 0`, `size = silence_bytes`, `stride = 4` on both chunks. It must not be reported as an SPA contract violation.

A malformed SPA descriptor (null/misaligned data or chunk, non-cardinal size, out-of-bounds window, asymmetric frames or overlapping channels) is a **validation failure**: it raises `RT_STATUS_HOST_CONTRACT_VIOLATION` (`E2304`), zeroes the regions it can prove writable, and stamps `size = 0` where the chunk descriptor itself is valid. It never writes through a pointer whose validity cannot be established and never leaves stale chunk metadata that could replay old audio.

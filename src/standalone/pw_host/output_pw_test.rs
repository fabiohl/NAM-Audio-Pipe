// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use neural_amp_modeler_rs::dsp::pipeline::MAX_BRIDGE_BUF;

#[test]
fn spa_pod_storage_is_8_byte_aligned() {
    assert!(std::mem::align_of::<SpaPodStorage<1024>>().is_multiple_of(8));
    assert!(std::mem::align_of::<SpaPodStorage<8>>().is_multiple_of(8));
    assert_eq!(SpaPodStorage::<8>::new().as_slice().len(), 8);
    let storage = SpaPodStorage::<8>::default();
    assert!(storage.as_slice().iter().all(|&b| b == 0));
}

#[test]
fn build_spa_format_pod_returns_aligned_pod_within_storage() {
    pw::init();
    let mut audio_info = pw::spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(pw::spa::param::audio::AudioFormat::F32P);
    audio_info.set_channels(2);
    let mut storage = SpaPodStorage::new();
    let base = storage.as_slice().as_ptr() as usize;
    let len = storage.as_slice().len();
    // SAFETY: storage outlives the returned pod for the duration of the test.
    let pod = unsafe { build_spa_format_pod(&audio_info, &mut storage) }.expect("pod build");
    let pod_addr = pod.as_raw_ptr() as usize;
    assert!(pod_addr.is_multiple_of(8));
    assert!(pod_addr >= base);
    assert!(pod_addr + std::mem::size_of::<pw::spa::sys::spa_pod>() <= base + len);
    assert_eq!(pod.type_(), pw::spa::utils::SpaTypes::Object);
}

#[test]
fn validate_built_pod_rejects_null_and_out_of_bounds_pointers() {
    let storage = SpaPodStorage::new();
    assert!(validate_built_pod(&storage, std::ptr::null()).is_none());

    // One-past-the-end is outside the storage -> rejected.
    let base = storage.as_slice().as_ptr();
    let one_past_end = unsafe { base.add(storage.as_slice().len()) }.cast();
    assert!(validate_built_pod(&storage, one_past_end).is_none());
}

#[test]
fn validate_built_pod_rejects_misaligned_pointer() {
    let storage = SpaPodStorage::new();
    let base = storage.as_slice().as_ptr() as usize;
    let misaligned = (base + 4) as *const pw::spa::sys::spa_pod;
    assert!(validate_built_pod(&storage, misaligned).is_none());
}

#[test]
fn validate_built_pod_rejects_header_claiming_oversized_body() {
    let mut storage = SpaPodStorage::new();
    {
        let bytes = storage.as_mut_slice();
        bytes[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[4..8].copy_from_slice(&0u32.to_le_bytes());
    }
    let pod_ptr = storage.as_slice().as_ptr().cast();
    assert!(validate_built_pod(&storage, pod_ptr).is_none());
}

#[test]
fn validate_built_pod_accepts_valid_header_within_storage() {
    let mut storage = SpaPodStorage::new();
    {
        let bytes = storage.as_mut_slice();
        bytes[0..4].copy_from_slice(&16u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&0u32.to_le_bytes());
    }
    let pod_ptr = storage.as_slice().as_ptr().cast();
    let pod = validate_built_pod(&storage, pod_ptr).expect("valid pod");
    assert_eq!(
        pod.as_raw_ptr() as usize,
        storage.as_slice().as_ptr() as usize
    );
    assert_eq!(pod.size(), 16);
}

// ── Malformed FFI/SPA playback harness (F-RB-003 Part 3 / T1.5) ──────────
//
// `playback_dsp_cycle` funnels every dequeued output buffer through
// `handle_spa_pair_fail_closed` with the playback window `(0, n_bytes)`.
// These tests feed that exact function raw pointer values (no live PipeWire
// stream required) and assert the malformed descriptors are rejected
// fail-closed: `RT_STATUS_HOST_CONTRACT_VIOLATION` raised, no panic, and
// the output channels silenced.

/// Creates an SPA chunk descriptor with the given valid-data window.
fn chunk_of(offset: u32, size: u32) -> pw::spa::sys::spa_chunk {
    pw::spa::sys::spa_chunk {
        offset,
        size,
        stride: std::mem::size_of::<f32>() as i32,
        flags: 0,
    }
}

fn fill_bytes(buf: &mut [f32], byte: u8) {
    let bytes: &mut [u8] =
        unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, buf.len() * 4) };
    bytes.fill(byte);
}

#[test]
fn playback_harness_accepts_valid_disjoint_output_buffers() {
    let l = [0.0f32; 16];
    let r = [0.0f32; 16];
    let chunk = chunk_of(0, 64);
    let rt = RtStatusFlags::default();
    let got = handle_spa_pair_fail_closed(
        l.as_ptr() as usize,
        l.len() * 4,
        &chunk,
        0,
        64,
        r.as_ptr() as usize,
        r.len() * 4,
        &chunk,
        0,
        64,
        &rt,
    );
    assert_eq!(got, Some((64, 16)));
    assert!(!rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn playback_harness_rejects_identical_output_buffers() {
    let buf = [0.0f32; 32];
    let chunk = chunk_of(0, 128);
    let rt = RtStatusFlags::default();
    let p = buf.as_ptr() as usize;
    let m = buf.len() * 4;
    // A malformed host handing the same output buffer to both channels must
    // be rejected fail-closed (identical intervals -> aliasing).
    assert_eq!(
        handle_spa_pair_fail_closed(p, m, &chunk, 0, 128, p, m, &chunk, 0, 128, &rt),
        None
    );
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn playback_harness_rejects_partial_overlap() {
    let buf = [0.0f32; 32];
    let chunk = chunk_of(0, 128);
    let rt = RtStatusFlags::default();
    let p = buf.as_ptr() as usize;
    let m = buf.len() * 4;
    // L: [0, 64); R: [32, 96) inside the same buffer -> partial overlap.
    assert_eq!(
        handle_spa_pair_fail_closed(p, m, &chunk, 0, 64, p, m, &chunk, 32, 64, &rt),
        None
    );
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn playback_harness_rejects_misaligned_output_base_1_2_3() {
    let buf = [0.0f32; 32];
    let chunk = chunk_of(0, 64);
    let base = buf.as_ptr() as usize;
    let m = buf.len() * 4;
    for delta in [1usize, 2, 3] {
        let rt = RtStatusFlags::default();
        assert_eq!(
            handle_spa_pair_fail_closed(
                base + delta,
                m.saturating_sub(delta),
                &chunk,
                0,
                64,
                base,
                m,
                &chunk,
                0,
                64,
                &rt,
            ),
            None,
            "delta={delta}"
        );
        assert!(
            rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION),
            "delta={delta}"
        );
    }
}

#[test]
fn playback_harness_rejects_null_output_chunk() {
    let buf = [0.0f32; 32];
    let chunk = chunk_of(0, 128);
    let rt = RtStatusFlags::default();
    let p = buf.as_ptr() as usize;
    let m = buf.len() * 4;
    assert_eq!(
        handle_spa_pair_fail_closed(p, m, &chunk, 0, 128, p, m, std::ptr::null(), 0, 128, &rt),
        None
    );
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn playback_harness_rejects_odd_output_size() {
    let l = [0.0f32; 32];
    let r = [0.0f32; 32];
    let rt = RtStatusFlags::default();
    let chunk = chunk_of(0, 0);
    assert_eq!(
        handle_spa_pair_fail_closed(
            l.as_ptr() as usize,
            l.len() * 4,
            &chunk,
            0,
            6,
            r.as_ptr() as usize,
            r.len() * 4,
            &chunk,
            0,
            6,
            &rt,
        ),
        None
    );
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn playback_harness_rejects_quantum_exceeding_max_bridge_buf() {
    // G-RB-003 / T6.2: a playback window of (MAX_BRIDGE_BUF + 1) frames
    // (32,772 bytes) must be rejected fail-closed — flag raised and both
    // output channels silenced — before any copy into the DSP buffers.
    let mut l = [0.0f32; MAX_BRIDGE_BUF + 1];
    let mut r = [0.0f32; MAX_BRIDGE_BUF + 1];
    fill_bytes(&mut l, 0x5A);
    fill_bytes(&mut r, 0xA5);
    let rt = RtStatusFlags::default();
    let size = (MAX_BRIDGE_BUF + 1) * 4;
    let chunk = chunk_of(0, size as u32);

    assert_eq!(
        handle_spa_pair_fail_closed(
            l.as_ptr() as usize,
            l.len() * 4,
            &chunk,
            0,
            size,
            r.as_ptr() as usize,
            r.len() * 4,
            &chunk,
            0,
            size,
            &rt,
        ),
        None
    );
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    assert!(
        l[..MAX_BRIDGE_BUF].iter().all(|&s| s == 0.0)
            && r[..MAX_BRIDGE_BUF].iter().all(|&s| s == 0.0),
        "oversized playback window must silence both output channels fail-closed up to MAX_BRIDGE_BUF"
    );
}

#[test]
fn playback_harness_accepts_max_bridge_buf_quantum() {
    // Boundary: exactly MAX_BRIDGE_BUF frames (32,768 bytes) is the largest
    // legal playback window and must be accepted.
    let l = [0.0f32; MAX_BRIDGE_BUF];
    let r = [0.0f32; MAX_BRIDGE_BUF];
    let rt = RtStatusFlags::default();
    let size = MAX_BRIDGE_BUF * 4;
    let chunk = chunk_of(0, size as u32);

    assert_eq!(
        handle_spa_pair_fail_closed(
            l.as_ptr() as usize,
            l.len() * 4,
            &chunk,
            0,
            size,
            r.as_ptr() as usize,
            r.len() * 4,
            &chunk,
            0,
            size,
            &rt,
        ),
        Some((size, MAX_BRIDGE_BUF))
    );
    assert!(!rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn playback_harness_rejects_size_overflow_beyond_maxsize() {
    let l = [0.0f32; 16];
    let r = [0.0f32; 16];
    let rt = RtStatusFlags::default();
    let chunk = chunk_of(0, 64);
    // offset 48 + size 32 = 80 > 64 -> out of bounds (no silent clamp).
    assert_eq!(
        handle_spa_pair_fail_closed(
            l.as_ptr() as usize,
            l.len() * 4,
            &chunk,
            0,
            64,
            r.as_ptr() as usize,
            r.len() * 4,
            &chunk,
            48,
            32,
            &rt,
        ),
        None
    );
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn playback_harness_rejects_asymmetric_frame_counts() {
    let l = [0.0f32; 64];
    let r = [0.0f32; 32];
    let rt = RtStatusFlags::default();
    let chunk = chunk_of(0, 0);
    assert_eq!(
        handle_spa_pair_fail_closed(
            l.as_ptr() as usize,
            l.len() * 4,
            &chunk,
            0,
            l.len() * 4,
            r.as_ptr() as usize,
            r.len() * 4,
            &chunk,
            0,
            r.len() * 4,
            &rt,
        ),
        None
    );
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn playback_harness_silences_channels_on_violation() {
    let mut buf = [0.0f32; 32];
    fill_bytes(&mut buf, 0x5A);
    let chunk = chunk_of(0, 128);
    let rt = RtStatusFlags::default();
    let p = buf.as_ptr() as usize;
    let m = buf.len() * 4;
    // Malformed partial-overlap pair: flag raised, both channels silenced,
    // no panic (reaching the end of this test proves it).
    let res = handle_spa_pair_fail_closed(p, m, &chunk, 0, 64, p, m, &chunk, 32, 64, &rt);
    assert_eq!(res, None);
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    assert!(
        buf.iter().all(|&s| s == 0.0),
        "playback fail-closed path must silence the output channels"
    );
}

// ── Bridge-starvation silence policy (G-RB-001 / T4.2) ────────────────────
//
// When `bridge.read_block` yields no new DSP block, `playback_dsp_cycle`
// must still dequeue, validate, silence-fill and recycle the output buffer.
// The pure kernel `deliver_silence_pair_fail_closed` is exercised with raw
// SPA descriptors (the mock stream) and proves: 100% of the output
// extension is zeroed (zero residual/stale audio), the chunk metadata is
// stamped consistently, no buffer miss is counted and the starvation
// telemetry counter advances.

#[test]
fn playback_bridge_starvation_zeroes_full_extension_and_stamps_chunks() {
    let mut l = [1.0f32; 32];
    let mut r = [1.0f32; 32];
    fill_bytes(&mut l, 0x5A);
    fill_bytes(&mut r, 0xA5);
    // Stale chunk metadata from a previous cycle — the silent recycling
    // path must overwrite it deterministically.
    let mut chunk_l = pw::spa::sys::spa_chunk {
        offset: 7,
        size: 4,
        stride: 0,
        flags: 0,
    };
    let mut chunk_r = pw::spa::sys::spa_chunk {
        offset: 3,
        size: 8,
        stride: 2,
        flags: 0,
    };
    let rt = RtStatusFlags::default();

    // SAFETY: `l`/`r` are disjoint `[f32; 32]` arrays (aligned, writable,
    // non-overlapping) and `chunk_l`/`chunk_r` are local, non-null structs
    // that outlive the call.
    let frames = unsafe {
        deliver_silence_pair_fail_closed(
            l.as_ptr() as usize,
            l.len() * 4,
            &mut chunk_l,
            r.as_ptr() as usize,
            r.len() * 4,
            &mut chunk_r,
            l.len() * 4,
            &rt,
        )
    };

    assert_eq!(frames, Some(32));
    assert!(
        l.iter().all(|&s| s == 0.0),
        "L must be fully silenced (no stale audio residue)"
    );
    assert!(
        r.iter().all(|&s| s == 0.0),
        "R must be fully silenced (no stale audio residue)"
    );
    assert_eq!(chunk_l.offset, 0);
    assert_eq!(chunk_l.size, 32 * 4);
    assert_eq!(chunk_l.stride, 4);
    assert_eq!(chunk_r.offset, 0);
    assert_eq!(chunk_r.size, 32 * 4);
    assert_eq!(chunk_r.stride, 4);
    assert_eq!(
        rt.playback_bridge_starvation.load(Ordering::Relaxed),
        1,
        "the starvation occurrence must be registered on rt_status"
    );
    assert_eq!(
        rt.output_buffer_miss.load(Ordering::Relaxed),
        0,
        "a dequeued silence buffer is not a buffer miss"
    );
    assert!(!rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn playback_bridge_starvation_rejects_aliased_channels_fail_closed() {
    let mut buf = [0.0f32; 32];
    fill_bytes(&mut buf, 0x5A);
    let mut chunk = chunk_of(0, 128);
    let rt = RtStatusFlags::default();
    let p = buf.as_ptr() as usize;
    let m = buf.len() * 4;

    // SAFETY: `buf` is a local, aligned, writable `[f32; 32]` and `chunk`
    // is a local non-null struct; the kernel rejects the aliasing fail-closed.
    let frames =
        unsafe { deliver_silence_pair_fail_closed(p, m, &mut chunk, p, m, &mut chunk, m, &rt) };

    assert_eq!(frames, None);
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    assert!(
        buf.iter().all(|&s| s == 0.0),
        "aliased channels must be silenced fail-closed"
    );
    assert_eq!(
        rt.playback_bridge_starvation.load(Ordering::Relaxed),
        0,
        "a host contract violation is not a starvation event"
    );
}

#[test]
fn playback_bridge_starvation_rejects_asymmetric_extensions() {
    let mut l = [1.0f32; 32];
    let mut r = [1.0f32; 16];
    fill_bytes(&mut l, 0x5A);
    fill_bytes(&mut r, 0xA5);
    let mut chunk = chunk_of(0, 0);
    let rt = RtStatusFlags::default();

    // SAFETY: `l`/`r` are local aligned writable arrays and `chunk` is a
    // local non-null struct; a silence window exceeding the smaller
    // channel's capacity must be rejected fail-closed.
    let frames = unsafe {
        deliver_silence_pair_fail_closed(
            l.as_ptr() as usize,
            l.len() * 4,
            &mut chunk,
            r.as_ptr() as usize,
            r.len() * 4,
            &mut chunk,
            l.len() * 4,
            &rt,
        )
    };

    assert_eq!(frames, None);
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    assert!(
        l.iter().all(|&s| s == 0.0) && r.iter().all(|&s| s == 0.0),
        "asymmetric extensions must silence both channels fail-closed"
    );
    assert_eq!(rt.playback_bridge_starvation.load(Ordering::Relaxed), 0);
}

#[test]
fn playback_bridge_starvation_with_huge_maxsize_bounds_to_max_bridge_buf() {
    // S5 / E2304 + F-RES-001 / T6.1: when host supplies a huge buffer
    // (e.g. 1 MiB or > MAX_BRIDGE_BUF), the *caller* quantizes the silence
    // window to MAX_BRIDGE_BUF frames (32 KiB), so the kernel delivers
    // exactly that bounded interval: no false E2304 on pause, chunk.size
    // matches the zeroed interval and trailing memory stays untouched.
    let total_samples = MAX_BRIDGE_BUF + 1024;
    let mut l = vec![0.5f32; total_samples];
    let mut r = vec![0.5f32; total_samples];
    fill_bytes(&mut l, 0x5A);
    fill_bytes(&mut r, 0xA5);
    let mut chunk_l = chunk_of(0, 0);
    let mut chunk_r = chunk_of(0, 0);
    let rt = RtStatusFlags::default();

    let silence_bytes = MAX_BRIDGE_BUF * std::mem::size_of::<f32>();
    let frames = unsafe {
        deliver_silence_pair_fail_closed(
            l.as_ptr() as usize,
            l.len() * 4,
            &mut chunk_l,
            r.as_ptr() as usize,
            r.len() * 4,
            &mut chunk_r,
            silence_bytes,
            &rt,
        )
    };

    assert_eq!(frames, Some(MAX_BRIDGE_BUF));
    assert!(!rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    assert_eq!(
        rt.playback_bridge_starvation.load(Ordering::Relaxed),
        1,
        "bounded silence delivery is a starvation event"
    );
    assert_eq!(chunk_l.size, silence_bytes as u32);
    assert_eq!(chunk_l.stride, 4);
    assert_eq!(chunk_r.size, silence_bytes as u32);

    // Exactly MAX_BRIDGE_BUF samples must be zeroed
    assert!(
        l[..MAX_BRIDGE_BUF].iter().all(|&s| s == 0.0),
        "bounded range must be zeroed"
    );
    assert!(
        r[..MAX_BRIDGE_BUF].iter().all(|&s| s == 0.0),
        "bounded range must be zeroed"
    );

    // Samples past MAX_BRIDGE_BUF must remain untouched
    let trailing_l = unsafe {
        std::slice::from_raw_parts(
            (l.as_ptr() as usize + MAX_BRIDGE_BUF * 4) as *const u8,
            1024 * 4,
        )
    };
    assert!(
        trailing_l.iter().all(|&b| b == 0x5A),
        "trailing memory past MAX_BRIDGE_BUF must not be touched"
    );

    let trailing_r = unsafe {
        std::slice::from_raw_parts(
            (r.as_ptr() as usize + MAX_BRIDGE_BUF * 4) as *const u8,
            1024 * 4,
        )
    };
    assert!(
        trailing_r.iter().all(|&b| b == 0xA5),
        "trailing memory past MAX_BRIDGE_BUF must not be touched"
    );
}

#[test]
fn playback_bridge_starvation_rejects_oversized_silence_window() {
    // S5 / E2304: a caller-requested silence window larger than
    // MAX_BRIDGE_BUF × 4 is still rejected fail-closed — the kernel never
    // zeroes beyond the safety cap even when the host buffer could hold it.
    let total_samples = MAX_BRIDGE_BUF + 1;
    let mut l = vec![0.5f32; total_samples];
    let mut r = vec![0.5f32; total_samples];
    fill_bytes(&mut l, 0x5A);
    fill_bytes(&mut r, 0xA5);
    let mut chunk_l = chunk_of(0, 0);
    let mut chunk_r = chunk_of(0, 0);
    let rt = RtStatusFlags::default();

    let oversized = (MAX_BRIDGE_BUF + 1) * std::mem::size_of::<f32>();
    let frames = unsafe {
        deliver_silence_pair_fail_closed(
            l.as_ptr() as usize,
            l.len() * 4,
            &mut chunk_l,
            r.as_ptr() as usize,
            r.len() * 4,
            &mut chunk_r,
            oversized,
            &rt,
        )
    };

    assert_eq!(frames, None);
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    assert_eq!(rt.playback_bridge_starvation.load(Ordering::Relaxed), 0);
    assert!(
        l[..MAX_BRIDGE_BUF].iter().all(|&s| s == 0.0)
            && r[..MAX_BRIDGE_BUF].iter().all(|&s| s == 0.0),
        "oversized window must silence both channels up to MAX_BRIDGE_BUF"
    );
}

// ── SPA format negotiation validator (G-RB-001 / T4.3) ───────────────────
//
// `validate_audio_raw_format` is the canonical fail-closed gate applied by
// both the capture and playback `param_changed` listeners. These tests
// build real SPA PODs (via `spa_format_audio_raw_build`) and prove that
// only `F32P` planar stereo passes, while mono, interleaved, S16, surround
// and non-format PODs are rejected with a typed `ContractViolation`.

/// Builds a real SPA format POD for the given audio info into `storage`.
fn build_raw_format_pod<'a>(
    audio_info: &pw::spa::param::audio::AudioInfoRaw,
    storage: &'a mut SpaPodStorage<1024>,
) -> &'a pw::spa::pod::Pod {
    // SAFETY: the returned pod borrows `storage`, which outlives the call.
    unsafe { build_spa_format_pod(audio_info, storage) }.expect("pod build")
}

fn raw_audio_info(
    format: pw::spa::param::audio::AudioFormat,
    channels: u32,
) -> pw::spa::param::audio::AudioInfoRaw {
    let mut info = pw::spa::param::audio::AudioInfoRaw::new();
    info.set_format(format);
    info.set_channels(channels);
    info.set_rate(48_000);
    let mut pos = [0u32; 64];
    pos[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
    pos[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(pos);
    info
}

#[test]
fn validate_audio_raw_format_accepts_f32p_planar_stereo() {
    pw::init();
    let info = raw_audio_info(pw::spa::param::audio::AudioFormat::F32P, 2);
    let mut storage = SpaPodStorage::new();
    let pod = build_raw_format_pod(&info, &mut storage);
    assert_eq!(validate_audio_raw_format(pod), Ok(48_000));
}

#[test]
fn validate_audio_raw_format_rejects_swapped_positions_fr_fl() {
    pw::init();
    let mut info = raw_audio_info(pw::spa::param::audio::AudioFormat::F32P, 2);
    let mut pos = [0u32; 64];
    pos[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
    pos[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
    info.set_position(pos);
    let mut storage = SpaPodStorage::new();
    let pod = build_raw_format_pod(&info, &mut storage);
    assert_eq!(
        validate_audio_raw_format(pod),
        Err(ContractViolation::InvalidChannelPositions {
            ch0: pw::spa::sys::SPA_AUDIO_CHANNEL_FR,
            ch1: pw::spa::sys::SPA_AUDIO_CHANNEL_FL,
        })
    );
}

#[test]
fn validate_audio_raw_format_rejects_fc_lfe_positions() {
    pw::init();
    let mut info = raw_audio_info(pw::spa::param::audio::AudioFormat::F32P, 2);
    let mut pos = [0u32; 64];
    pos[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FC;
    pos[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_LFE;
    info.set_position(pos);
    let mut storage = SpaPodStorage::new();
    let pod = build_raw_format_pod(&info, &mut storage);
    assert_eq!(
        validate_audio_raw_format(pod),
        Err(ContractViolation::InvalidChannelPositions {
            ch0: pw::spa::sys::SPA_AUDIO_CHANNEL_FC,
            ch1: pw::spa::sys::SPA_AUDIO_CHANNEL_LFE,
        })
    );
}

#[test]
fn validate_audio_raw_format_rejects_missing_positions() {
    pw::init();
    let mut info = raw_audio_info(pw::spa::param::audio::AudioFormat::F32P, 2);
    let pos = [0u32; 64];
    info.set_position(pos);
    let mut storage = SpaPodStorage::new();
    let pod = build_raw_format_pod(&info, &mut storage);
    assert_eq!(
        validate_audio_raw_format(pod),
        Err(ContractViolation::InvalidChannelPositions { ch0: 0, ch1: 0 })
    );
}

#[test]
fn validate_audio_raw_format_rejects_mono() {
    pw::init();
    let info = raw_audio_info(pw::spa::param::audio::AudioFormat::F32P, 1);
    let mut storage = SpaPodStorage::new();
    let pod = build_raw_format_pod(&info, &mut storage);
    assert_eq!(
        validate_audio_raw_format(pod),
        Err(ContractViolation::NotStereo(1))
    );
}

#[test]
fn validate_audio_raw_format_rejects_surround_5_1() {
    pw::init();
    let info = raw_audio_info(pw::spa::param::audio::AudioFormat::F32P, 6);
    let mut storage = SpaPodStorage::new();
    let pod = build_raw_format_pod(&info, &mut storage);
    assert_eq!(
        validate_audio_raw_format(pod),
        Err(ContractViolation::NotStereo(6))
    );
}

#[test]
fn validate_audio_raw_format_rejects_interleaved_f32() {
    pw::init();
    let info = raw_audio_info(pw::spa::param::audio::AudioFormat::F32LE, 2);
    let mut storage = SpaPodStorage::new();
    let pod = build_raw_format_pod(&info, &mut storage);
    assert_eq!(
        validate_audio_raw_format(pod),
        Err(ContractViolation::NotF32Planar(
            pw::spa::param::audio::AudioFormat::F32LE
        ))
    );
}

#[test]
fn validate_audio_raw_format_rejects_s16() {
    pw::init();
    let info = raw_audio_info(pw::spa::param::audio::AudioFormat::S16, 2);
    let mut storage = SpaPodStorage::new();
    let pod = build_raw_format_pod(&info, &mut storage);
    assert_eq!(
        validate_audio_raw_format(pod),
        Err(ContractViolation::NotF32Planar(
            pw::spa::param::audio::AudioFormat::S16
        ))
    );
}

#[test]
fn validate_audio_raw_format_rejects_non_format_pod() {
    // A well-formed SPA Struct POD (not an Object/Format) must fail
    // `spa_format_parse` fail-closed instead of panicking. The backing
    // buffer is `SpaPodStorage` (8-byte aligned) so `Pod::from_bytes`
    // receives a properly aligned slice (its safety contract requires a
    // well-aligned pod).
    let mut storage = SpaPodStorage::<8>::new();
    {
        let bytes = storage.as_mut_slice();
        bytes[4..8].copy_from_slice(&pw::spa::utils::SpaTypes::Struct.as_raw().to_le_bytes());
    }
    let pod = pw::spa::pod::Pod::from_bytes(storage.as_slice()).expect("struct pod");
    assert_eq!(
        validate_audio_raw_format(pod),
        Err(ContractViolation::NotAudioRaw)
    );
}

#[test]
fn reject_negotiated_format_violation_raises_host_contract_flag_and_latches() {
    let rt = RtStatusFlags::default();
    assert_eq!(
        rt.format_contract_ok.load(Ordering::Relaxed),
        1,
        "the latch defaults to contract-ok"
    );
    reject_negotiated_format_violation(&rt, "capture", ContractViolation::NotStereo(1));
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    assert_eq!(
        rt.format_contract_ok.load(Ordering::Relaxed),
        0,
        "a rejected negotiation must latch the RT mute guard"
    );
    assert_eq!(
        rt.flags_seen.load(Ordering::Relaxed),
        0,
        "the flag is telemetry for the main loop; it is not consumed here"
    );
}

#[test]
fn mark_format_contract_ok_restores_the_rt_mute_guard() {
    let rt = RtStatusFlags::default();
    reject_negotiated_format_violation(
        &rt,
        "playback",
        ContractViolation::NotF32Planar(pw::spa::param::audio::AudioFormat::S16),
    );
    assert_eq!(rt.format_contract_ok.load(Ordering::Relaxed), 0);

    mark_format_contract_ok(&rt, "playback");
    assert_eq!(
        rt.format_contract_ok.load(Ordering::Relaxed),
        1,
        "a subsequent valid F32P stereo negotiation must re-arm audio processing"
    );
}

#[test]
fn negotiated_rate_mismatch_detects_discrepant_streams() {
    let rt = RtStatusFlags::default();
    assert_eq!(negotiated_rate_mismatch(&rt), None);

    rt.capture_negotiated_rate.store(48_000, Ordering::Release);
    assert_eq!(
        negotiated_rate_mismatch(&rt),
        None,
        "single negotiated stream is not a mismatch"
    );

    rt.playback_negotiated_rate.store(48_000, Ordering::Release);
    assert_eq!(
        negotiated_rate_mismatch(&rt),
        None,
        "equal negotiated rates are not a mismatch"
    );

    rt.playback_negotiated_rate.store(44_100, Ordering::Release);
    assert_eq!(
        negotiated_rate_mismatch(&rt),
        Some((48_000, 44_100)),
        "discrepant negotiated rates must be reported"
    );
}

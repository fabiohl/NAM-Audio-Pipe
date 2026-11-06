// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

#[test]
fn test_create_capture_properties_with_latency() {
    pw::init();
    let props = create_capture_properties(256);
    assert_eq!(props.get("node.latency"), Some("256/48000"));
    assert_eq!(props.get(&pw::keys::MEDIA_TYPE), Some("Audio"));
    assert_eq!(props.get(&pw::keys::MEDIA_CATEGORY), Some("Duplex"));
    assert_eq!(props.get(&pw::keys::MEDIA_ROLE), Some("DSP"));
    assert_eq!(props.get(&pw::keys::MEDIA_CLASS), Some("Audio/Sink"));
}

#[test]
fn test_create_capture_properties_zero_buffer_size() {
    pw::init();
    let props = create_capture_properties(0);
    assert_eq!(props.get("node.latency"), None);
}

#[test]
fn test_build_capture_format_pod() {
    pw::init();
    let mut storage = SpaPodStorage::new();
    let res = build_capture_format_pod(&mut storage);
    assert!(res.is_ok());
}

// ── cabsim_rebuild_needed ───────────────────────────────────────────────────

fn make_pair(
    partition: usize,
    rate: u32,
) -> neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair {
    use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
    use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
    let ir = [1.0f32, 0.5, 0.25];
    let make = || {
        let engine = ConvEngine::new(&ir, partition).expect("test engine");
        CabSimAdapter::new(Box::new(engine)).expect("test adapter")
    };
    neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair {
        l: Box::new(make()),
        r: Box::new(make()),
        sample_rate: rate,
    }
}

#[test]
fn rebuild_not_requested_without_ir_or_samples() {
    let pair = make_pair(64, 48000);
    assert!(!cabsim_rebuild_needed(
        Some(&pair),
        false,
        64,
        48000,
        64,
        None,
        false
    ));
    assert!(!cabsim_rebuild_needed(
        None, false, 64, 48000, 64, None, false
    ));
    assert!(!cabsim_rebuild_needed(
        None, true, 0, 48000, 64, None, false
    ));
}

#[test]
fn rebuild_requested_when_no_pair_active_and_not_pending() {
    // First install: no pair active and no latch.
    assert!(cabsim_rebuild_needed(
        None, true, 64, 48000, 64, None, false
    ));
    // A request already in flight must not be re-requested.
    assert!(!cabsim_rebuild_needed(
        None, true, 64, 48000, 64, None, true
    ));
}

#[test]
fn rebuild_suppressed_when_failed_key_matches() {
    // Recovery after a failed rebuild pushed None.
    // In NAM-Audio-Pipe, IR is configured at startup and is immutable per process.
    // The latch keys strictly on (host_rate, partition), suppressing retries across
    // all subsequent callbacks until a renegotiated rate or partition policy changes the key.
    assert!(!cabsim_rebuild_needed(
        None,
        true,
        64,
        48000,
        64,
        Some((48000, 64)),
        false
    ));
    // Even if n_samples changes within domain, the same (rate, partition) stays latched.
    assert!(!cabsim_rebuild_needed(
        None,
        true,
        128,
        48000,
        64,
        Some((48000, 64)),
        false
    ));
}

#[test]
fn rebuild_resumed_when_host_rate_or_partition_differs_from_failed_key() {
    // Legitimate new rate: PipeWire renegotiates rate from 48000 to 44100 or 96000.
    assert!(cabsim_rebuild_needed(
        None,
        true,
        64,
        44100,
        64,
        Some((48000, 64)),
        false
    ));
    assert!(cabsim_rebuild_needed(
        None,
        true,
        64,
        96000,
        64,
        Some((48000, 64)),
        false
    ));
    // Legitimate new partition policy:
    assert!(cabsim_rebuild_needed(
        None,
        true,
        64,
        48000,
        128,
        Some((48000, 64)),
        false
    ));
}

#[test]
fn rebuild_not_requested_on_partition_mismatch_quantum_decoupled() {
    // The host quantum does not drive rebuild — the block-agnostic driver
    // (`process_block`) fragments any block internally against the fixed
    // partition. A quantum renegotiation (e.g.: 64 → 128 samples at constant
    // rate) keeps the active pair without disruption or heavy FFT.
    let pair = make_pair(64, 48000);
    assert!(!cabsim_rebuild_needed(
        Some(&pair),
        true,
        128,
        48000,
        64,
        None,
        false
    ));
    assert!(!cabsim_rebuild_needed(
        Some(&pair),
        true,
        256,
        48000,
        64,
        None,
        false
    ));
    assert!(!cabsim_rebuild_needed(
        Some(&pair),
        true,
        64,
        48000,
        64,
        None,
        false
    ));
}

#[test]
fn rebuild_requested_on_rate_mismatch() {
    let pair = make_pair(64, 48000);
    // IR calibrated for 48k while the host output runs at another rate.
    assert!(cabsim_rebuild_needed(
        Some(&pair),
        true,
        64,
        44100,
        64,
        None,
        false
    ));
    assert!(cabsim_rebuild_needed(
        Some(&pair),
        true,
        64,
        96000,
        64,
        None,
        false
    ));
    assert!(!cabsim_rebuild_needed(
        Some(&pair),
        true,
        64,
        48000,
        64,
        None,
        false
    ));
}

#[test]
fn rebuild_suppressed_while_pending_with_active_pair() {
    // Invariante: enquanto um rebuild está em voo, um par ativo ainda
    // divergente na taxa NÃO deve re-armar a requisição, independente do
    // quantum atual. Re-armar a cada callback incrementa
    // `requested_cabsim_generation`, de modo que o envelope produzido pelo
    // rebuild em voo chega obsoleto e é descartado pelo swap RT, travando o
    // par em livelock. (O quantum é irrelevante — só a taxa é comparada.)
    let pair = make_pair(64, 48000);

    // Quantum divergente sozinho nunca rebuilda — com ou sem pendência.
    assert!(!cabsim_rebuild_needed(
        Some(&pair),
        true,
        128,
        48000,
        64,
        None,
        false
    ));
    assert!(!cabsim_rebuild_needed(
        Some(&pair),
        true,
        128,
        48000,
        64,
        None,
        true
    ));
    // Taxa divergente isolada.
    assert!(!cabsim_rebuild_needed(
        Some(&pair),
        true,
        64,
        44100,
        64,
        None,
        true
    ));
    assert!(!cabsim_rebuild_needed(
        Some(&pair),
        true,
        64,
        96000,
        64,
        None,
        true
    ));
    // Ambos divergentes (quantum + taxa).
    assert!(!cabsim_rebuild_needed(
        Some(&pair),
        true,
        128,
        44100,
        64,
        None,
        true
    ));
    // Par correspondente com pendência segue no-op.
    assert!(!cabsim_rebuild_needed(
        Some(&pair),
        true,
        64,
        48000,
        64,
        None,
        true
    ));

    // Sem pendência, só a divergência de taxa requisita.
    assert!(!cabsim_rebuild_needed(
        Some(&pair),
        true,
        128,
        48000,
        64,
        None,
        false
    ));
    assert!(cabsim_rebuild_needed(
        Some(&pair),
        true,
        64,
        44100,
        64,
        None,
        false
    ));
}

#[test]
fn rebuild_first_install_gated_by_partition_domain() {
    // O quantum só condiciona a primeira instalação (um quantum real prova que
    // a taxa do host foi negociada) — fora do domínio
    // [BUFFER_SIZE_MIN, MAX_RESAMP_BUF] o par ausente nunca requisita. A
    // partição em si vem da política `--cabsim-partition`, nunca do quantum.
    // Um par ativo saudável nunca rebuilda por quantum, mesmo anômalo: o
    // driver agnóstico de bloco processa qualquer tamanho.
    assert!(!cabsim_rebuild_needed(
        None,
        true,
        MAX_RESAMP_BUF + 1,
        48000,
        64,
        None,
        false
    ));
    // Quantum zero is not a valid audio block.
    assert!(!cabsim_rebuild_needed(
        None, true, 0, 48000, 64, None, false
    ));
    // Quanta from 1 to 15 are explicitly rejected by the installation gate.
    assert!(!cabsim_rebuild_needed(
        None, true, 1, 48000, 64, None, false
    ));
    assert!(!cabsim_rebuild_needed(
        None, true, 8, 48000, 64, None, false
    ));
    assert!(!cabsim_rebuild_needed(
        None, true, 15, 48000, 64, None, false
    ));
    // Boundary and typical domain values still install when the pair is missing.
    assert!(cabsim_rebuild_needed(
        None, true, 16, 48000, 64, None, false
    ));
    assert!(cabsim_rebuild_needed(
        None,
        true,
        MAX_RESAMP_BUF,
        48000,
        64,
        None,
        false
    ));
    let pair = make_pair(16, 48000);
    // Active pair ignores the quantum — even anomalous values — at constant rate.
    assert!(!cabsim_rebuild_needed(
        Some(&pair),
        true,
        MAX_RESAMP_BUF,
        48000,
        16,
        None,
        false
    ));
    assert!(!cabsim_rebuild_needed(
        Some(&pair),
        true,
        MAX_RESAMP_BUF + 1,
        48000,
        16,
        None,
        false
    ));
    assert!(!cabsim_rebuild_needed(
        Some(&pair),
        true,
        8,
        48000,
        16,
        None,
        false
    ));
}

/// Simulated quantum renegotiation: 128 → 256 samples at constant rate
/// does NOT issue a rebuild command — the active pair remains active and no
/// `RT_STATUS_NEEDS_CABSIM_REBUILD` is set.
#[test]
fn quantum_renegotiation_at_constant_rate_never_rebuilds() {
    for partition in [64usize, 128, 256] {
        let pair = make_pair(partition, 48000);
        for quantum in [16usize, 32, 64, 128, 256, 512, 1024] {
            assert!(
                !cabsim_rebuild_needed(Some(&pair), true, quantum, 48000, partition, None, false),
                "quantum {quantum} at 48 kHz with partition {partition} must not rebuild"
            );
        }
    }
}

// ── cabsim_partition_to_request (--cabsim-partition policy) ───────────────

#[test]
fn partition_request_preserves_active_pair_partition() {
    // Rate recalibration: active pair preserves installed partition — neither
    // quantum nor new rate alter published latency.
    let pair = make_pair(32, 48000);
    assert_eq!(cabsim_partition_to_request(Some(&pair), 128), 32);
    let pair = make_pair(256, 48000);
    assert_eq!(cabsim_partition_to_request(Some(&pair), 32), 256);
}

#[test]
fn partition_request_first_install_uses_policy_not_quantum() {
    // Initial install: partition comes from `--cabsim-partition` policy
    // (domain {32, 64, 128, 256}, default 128) — host quantum never sizes the pair.
    assert_eq!(cabsim_partition_to_request(None, 32), 32);
    assert_eq!(cabsim_partition_to_request(None, 64), 64);
    assert_eq!(cabsim_partition_to_request(None, 128), 128);
    assert_eq!(cabsim_partition_to_request(None, 256), 256);
    // Host quantum does not enter any branch.
    assert_eq!(cabsim_partition_to_request(None, 64), 64);
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unit tests for standalone initialization: SPSC communication channel setup,
//! initial model/cabsim loading behaviors, error hints, and partition clamping.

use super::*;

#[test]
fn test_setup_communication_channels() {
    let channels = setup_communication_channels().expect("test SPSC setup");
    assert!(channels.param_consumer.slots() <= spsc::SPSC_CAPACITY);
}

#[test]
fn test_load_initial_model_none() {
    let mut channels = setup_communication_channels().expect("test SPSC setup");
    let sys = SystemSnapshot::capture();
    let result = load_initial_model(None, &sys, &mut channels.param_producer);
    assert!(result.full_wavenet_model_l.is_none());
    assert!(result.full_wavenet_model_r.is_none());
    assert!(result.architecture.is_empty());
}

#[test]
fn test_load_initial_cabsim_none() {
    let mut channels = setup_communication_channels().expect("test SPSC setup");
    let result = load_initial_cabsim(None, 128, false, &mut channels.cabsim_producer).unwrap();
    assert!(result.is_none());
}

// F-UX-07: an explicitly requested IR that cannot be loaded is fatal by
// default (same contract as `--model`); `--cab-optional` restores the
// graceful `Ok(None)` path.
#[test]
fn load_initial_cabsim_missing_path_is_fatal_by_default() {
    let mut channels = setup_communication_channels().expect("test SPSC setup");
    let missing = std::path::Path::new("/nonexistent/definitely/missing_ir.wav");
    let err = load_initial_cabsim(Some(missing), 128, false, &mut channels.cabsim_producer)
        .err()
        .expect("a missing --cab must be fatal by default");
    assert!(
        err.to_string().contains("Cab-sim IR load failed"),
        "fatal error must name the failure, got: {err}"
    );
}

#[test]
fn load_initial_cabsim_missing_path_is_optional_with_flag() {
    let mut channels = setup_communication_channels().expect("test SPSC setup");
    let missing = std::path::Path::new("/nonexistent/definitely/missing_ir.wav");
    let result = load_initial_cabsim(Some(missing), 128, true, &mut channels.cabsim_producer)
        .expect("--cab-optional must keep going without the IR");
    assert!(result.is_none());
}

#[test]
fn model_load_hint_is_specific_per_root_cause() {
    use neural_amp_modeler_rs::loader::{JsonError, LoadError};
    // Unsupported version → re-export/upgrade hint.
    let version = LoadError::Json(JsonError::UnsupportedVersion {
        raw: "9.9.9".to_string(),
        reason: "future schema".to_string(),
    });
    assert!(model_load_hint(&version).contains("newer NAM version"));
    // Unsupported architecture → update/architecture hint.
    let arch = LoadError::UnsupportedArchitecture("SomeFutureNet".to_string());
    assert!(model_load_hint(&arch).contains("architecture"));
    // Read failures → permissions/path hint.
    let io = LoadError::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    assert!(model_load_hint(&io).contains("permissions"));
}

#[test]
fn initial_cabsim_partition_size_follows_policy_and_clamps_domain() {
    // The policy set {32, 64, 128, 256} maps identically — the latency is
    // exactly the chosen partition, never derived from the quantum.
    assert_eq!(initial_cabsim_partition_size(32), 32);
    assert_eq!(initial_cabsim_partition_size(64), 64);
    assert_eq!(initial_cabsim_partition_size(128), 128);
    assert_eq!(initial_cabsim_partition_size(256), 256);
    // Defense-in-depth clamp (the CLI already rejects out-of-set values):
    // the partition can never leave the engine's safe domain.
    assert_eq!(initial_cabsim_partition_size(0), 16);
    assert_eq!(initial_cabsim_partition_size(1), 16);
    assert_eq!(initial_cabsim_partition_size(16384), MAX_RESAMP_BUF);
    assert_eq!(initial_cabsim_partition_size(u32::MAX), MAX_RESAMP_BUF);
}

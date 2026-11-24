// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Single-slot cabinet simulation (`.wav` IR or 1-in/1-out `.nam`/`.namb`) and lock-free bypass.

#[path = "cabinet/asset.rs"]
mod asset;
#[path = "cabinet/bypass.rs"]
mod bypass;
#[path = "cabinet/drain.rs"]
mod drain;
#[path = "cabinet/slot.rs"]
mod slot;

pub use asset::{
    CabAssetKind, CabNamMeta, InitialCabSetup, disambiguate_cab_path, parse_and_validate_cab_nam,
    validate_cab_nam_data,
};
pub use bypass::{
    CAB_BYPASS_CROSSFADE_SAMPLES, CAB_DRY_DELAY_MAX_EXTRA, CabBypassState, CabDryDelay,
};
pub use drain::{CabNamDrainStats, drain_cab_nam_payloads};
pub use slot::{CabNamSlot, PipeCabNamPayload, active_cabinet_latency, build_cab_nam_slot};

#[cfg(test)]
#[path = "cabinet_test.rs"]
mod cabinet_test;

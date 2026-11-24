// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Test-only fault-injection hooks for off-RT rebuild error paths.
//!
//! Compiled only under `feature = "testing"`; absent from default/release builds.

use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use std::sync::atomic::{AtomicU64, Ordering};

/// Lost-wakeup guard shared by every off-RT rebuild handler.
///
/// Clears `flag` and re-arms it if `gen_counter` advanced past `generation`
/// while the build was in flight, so a newer request can never be erased by a
/// stale build completion.
#[inline(always)]
pub(super) fn rearm_if_superseded(
    rt_status: &RtStatusFlags,
    flag: u64,
    gen_counter: &AtomicU64,
    generation: u64,
) {
    rt_status.clear_flag_relaxed(flag);
    if gen_counter.load(Ordering::Acquire) != generation {
        rt_status.set_flag(flag);
    }
}

#[cfg(feature = "testing")]
pub(super) mod testing {
    use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::{Mutex, OnceLock};

    pub(super) fn flags_id(rt_status: &RtStatusFlags) -> usize {
        std::ptr::from_ref(rt_status) as usize
    }

    pub(super) struct PersistentFault {
        flags: usize,
        generation: u64,
        attempts: usize,
    }

    pub(super) struct PersistentFaultSlot {
        slot: OnceLock<Mutex<Option<PersistentFault>>>,
    }

    impl PersistentFaultSlot {
        pub(super) const fn new() -> Self {
            Self {
                slot: OnceLock::new(),
            }
        }

        #[cfg(test)]
        pub(super) fn arm_fail(&self, rt_status: &RtStatusFlags, generation: u64) {
            *self.slot.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(PersistentFault {
                flags: flags_id(rt_status),
                generation,
                attempts: 0,
            });
        }

        #[cfg(test)]
        pub(super) fn attempts(&self, rt_status: &RtStatusFlags, generation: u64) -> usize {
            let guard = match self.slot.get() {
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

        pub(super) fn inject(&self, rt_status: &RtStatusFlags, generation: u64) -> bool {
            let mut guard = match self.slot.get() {
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

    /// Test-only fault injection for the resampler rebuild error path.
    pub(crate) mod resampler_fault {
        use super::*;

        struct ArmedFault {
            flags: usize,
            generation: u64,
            reached_tx: Option<Sender<()>>,
            release_rx: Option<Receiver<()>>,
        }

        static ARMED: OnceLock<Mutex<Option<ArmedFault>>> = OnceLock::new();

        /// Arms a one-shot failure for `generation` that fails the next build without
        /// pausing.
        #[cfg(test)]
        pub(crate) fn arm_fail_once(rt_status: &RtStatusFlags, generation: u64) {
            *ARMED.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(ArmedFault {
                flags: flags_id(rt_status),
                generation,
                reached_tx: None,
                release_rx: None,
            });
        }

        /// Arms a one-shot failure for `generation` that pauses the handler at the
        /// injection point.
        #[cfg(test)]
        pub(crate) fn arm_fail_and_pause(
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

        /// Injects the fault when armed for this flags instance and generation.
        pub(crate) fn inject(rt_status: &RtStatusFlags, generation: u64) -> bool {
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

    /// Test-only persistent fault injection for the oversample rebuild error path.
    pub(crate) mod os_fault {
        use super::*;

        static SLOT: PersistentFaultSlot = PersistentFaultSlot::new();

        #[cfg(test)]
        pub(crate) fn arm_fail(rt_status: &RtStatusFlags, generation: u64) {
            SLOT.arm_fail(rt_status, generation);
        }

        #[cfg(test)]
        pub(crate) fn attempts(rt_status: &RtStatusFlags, generation: u64) -> usize {
            SLOT.attempts(rt_status, generation)
        }

        pub(crate) fn inject(rt_status: &RtStatusFlags, generation: u64) -> bool {
            SLOT.inject(rt_status, generation)
        }
    }

    /// Test-only persistent fault injection for the slimmable slice error path.
    pub(crate) mod slimmable_fault {
        use super::*;

        static SLOT: PersistentFaultSlot = PersistentFaultSlot::new();

        #[cfg(test)]
        pub(crate) fn arm_fail(rt_status: &RtStatusFlags, generation: u64) {
            SLOT.arm_fail(rt_status, generation);
        }

        #[cfg(test)]
        pub(crate) fn attempts(rt_status: &RtStatusFlags, generation: u64) -> usize {
            SLOT.attempts(rt_status, generation)
        }

        pub(crate) fn inject(rt_status: &RtStatusFlags, generation: u64) -> bool {
            SLOT.inject(rt_status, generation)
        }
    }
}

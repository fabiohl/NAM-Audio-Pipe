// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Raw Linux `perf_event_open` PMU counters for `ab_opt_bench`.

use std::collections::BTreeMap;

const PERF_EVENT_IOC_ENABLE: libc::c_ulong = 0x2400;
const PERF_EVENT_IOC_DISABLE: libc::c_ulong = 0x2401;
const PERF_EVENT_IOC_RESET: libc::c_ulong = 0x2403;
const PERF_FLAG_FD_CLOEXEC: u64 = 0x8;
const PERF_TYPE_HARDWARE: u32 = 0;
const PERF_TYPE_HW_CACHE: u32 = 3;
const PERF_COUNT_HW_CPU_CYCLES: u64 = 0;
const PERF_COUNT_HW_INSTRUCTIONS: u64 = 1;
pub(super) const PERF_COUNT_HW_CACHE_ITLB: u64 = 4;
pub(super) const PERF_COUNT_HW_CACHE_ICACHE: u64 = 1;
pub(super) const PERF_COUNT_HW_CACHE_OP_READ: u64 = 0;
pub(super) const PERF_COUNT_HW_CACHE_RESULT_MISS: u64 = 1;
// __NR_perf_event_open on x86-64 Linux (not exported by libc).
const NR_PERF_EVENT_OPEN: libc::c_long = 298;

#[repr(C)]
struct PerfEventAttr {
    type_: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
    config2: u64,
    branch_sample_type: u64,
    sample_regs_user: u64,
    sample_stack_user: u32,
    clockid: i32,
}

impl PerfEventAttr {
    fn hardware(config: u64) -> Self {
        Self {
            type_: PERF_TYPE_HARDWARE,
            size: std::mem::size_of::<Self>() as u32,
            config,
            sample_period: 0,
            sample_type: 0,
            read_format: 0,
            // disabled=1 (bit 0), exclude_kernel=1 (bit 5), exclude_hv=1 (bit 6).
            flags: 1 | (1 << 5) | (1 << 6),
            wakeup_events: 0,
            bp_type: 0,
            config1: 0,
            config2: 0,
            branch_sample_type: 0,
            sample_regs_user: 0,
            sample_stack_user: 0,
            clockid: 0,
        }
    }

    fn cache(config: u64) -> Self {
        Self {
            type_: PERF_TYPE_HW_CACHE,
            ..Self::hardware(config)
        }
    }
}

/// One best-effort PMU counter. The fd is `None` when the event could not be
/// opened (typed: `unavailable` + reason).
pub(super) struct Pmc {
    name: &'static str,
    fd: Option<i32>,
    reason: Option<String>,
}

impl Pmc {
    fn open(name: &'static str, attr: PerfEventAttr) -> Self {
        // SAFETY: `attr` is a fully initialized value of the correct layout;
        // the syscall only reads it and returns an fd (or -1). No memory is
        // retained beyond the copy the kernel makes.
        let fd = unsafe {
            libc::syscall(
                NR_PERF_EVENT_OPEN,
                &raw const attr,
                0,
                -1,
                -1,
                PERF_FLAG_FD_CLOEXEC,
            )
        };
        if fd >= 0 {
            Self {
                name,
                fd: Some(fd as i32),
                reason: None,
            }
        } else {
            Self {
                name,
                fd: None,
                reason: Some(std::io::Error::last_os_error().to_string()),
            }
        }
    }

    fn ioctl(&self, request: libc::c_ulong) {
        if let Some(fd) = self.fd {
            // SAFETY: `fd` is a valid perf fd owned by this counter.
            unsafe {
                libc::ioctl(fd, request, 0);
            }
        }
    }

    fn read(&self) -> u64 {
        let Some(fd) = self.fd else {
            return 0;
        };
        let mut val: u64 = 0;
        // SAFETY: `fd` is a valid perf fd and `val` is a writable u64 large
        // enough for a single counter value.
        let n = unsafe {
            libc::read(
                fd,
                &raw mut val as *mut libc::c_void,
                std::mem::size_of::<u64>(),
            )
        };
        if n != std::mem::size_of::<u64>() as isize {
            return 0;
        }
        val
    }
}

impl Drop for Pmc {
    fn drop(&mut self) {
        if let Some(fd) = self.fd {
            // SAFETY: `fd` is a valid fd owned by this counter.
            unsafe {
                libc::close(fd);
            }
        }
    }
}

/// The PMU counter set retained in the receipt: cycles, instructions, iTLB
/// misses and I-cache misses, each with typed availability.
pub(super) struct PmcSet {
    cycles: Pmc,
    instructions: Pmc,
    itlb_misses: Pmc,
    icache_misses: Pmc,
}

impl PmcSet {
    pub(super) fn open() -> Self {
        Self {
            cycles: Pmc::open("cycles", PerfEventAttr::hardware(PERF_COUNT_HW_CPU_CYCLES)),
            instructions: Pmc::open(
                "instructions",
                PerfEventAttr::hardware(PERF_COUNT_HW_INSTRUCTIONS),
            ),
            itlb_misses: Pmc::open(
                "itlb_misses",
                PerfEventAttr::cache(
                    PERF_COUNT_HW_CACHE_ITLB
                        | (PERF_COUNT_HW_CACHE_OP_READ << 8)
                        | (PERF_COUNT_HW_CACHE_RESULT_MISS << 16),
                ),
            ),
            icache_misses: Pmc::open(
                "icache_misses",
                PerfEventAttr::cache(
                    PERF_COUNT_HW_CACHE_ICACHE
                        | (PERF_COUNT_HW_CACHE_OP_READ << 8)
                        | (PERF_COUNT_HW_CACHE_RESULT_MISS << 16),
                ),
            ),
        }
    }

    fn all(&self) -> [&Pmc; 4] {
        [
            &self.cycles,
            &self.instructions,
            &self.itlb_misses,
            &self.icache_misses,
        ]
    }

    pub(super) fn reset(&self) {
        for pmc in self.all() {
            pmc.ioctl(PERF_EVENT_IOC_RESET);
        }
    }

    pub(super) fn enable(&self) {
        for pmc in self.all() {
            pmc.ioctl(PERF_EVENT_IOC_ENABLE);
        }
    }

    pub(super) fn disable(&self) {
        for pmc in self.all() {
            pmc.ioctl(PERF_EVENT_IOC_DISABLE);
        }
    }

    pub(super) fn counts(&self) -> BTreeMap<&'static str, u64> {
        self.all()
            .into_iter()
            .map(|pmc| (pmc.name, pmc.read()))
            .collect()
    }

    pub(super) fn availability(&self) -> Vec<(String, Option<String>)> {
        self.all()
            .into_iter()
            .map(|pmc| (pmc.name.to_string(), pmc.reason.clone()))
            .collect()
    }
}

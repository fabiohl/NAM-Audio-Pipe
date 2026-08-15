// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Native `io_uring` capability probe.
//!
//! Replaces the previous `python3` + `ctypes` syscall probe in
//! `utils/tests-quick.sh` with a self-contained Rust check that does not
//! depend on any optional interpreter being present.

/// Outcome of the `io_uring_setup(2)` capability probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoUringSupport {
    /// The syscall succeeded — `io_uring` is available.
    Available,
    /// The kernel (or security policy) rejects or lacks `io_uring`.
    KernelUnsupported,
    /// The probe itself failed for an unexpected reason (not a kernel verdict).
    ProbeFailed,
}

/// Probes whether `io_uring` is usable by attempting `io_uring_setup(2)`.
///
/// Fast-path: `/proc/sys/kernel/io_uring_disabled == 2` means the subsystem is
/// fully disabled. Otherwise the syscall is attempted directly; a returned fd
/// proves availability. `ENOSYS`/`EPERM`/`EACCES` are interpreted as a
/// kernel/security restriction rather than a probe bug.
pub fn probe_io_uring() -> IoUringSupport {
    if let Ok(raw) = std::fs::read_to_string("/proc/sys/kernel/io_uring_disabled")
        && raw.trim() == "2"
    {
        return IoUringSupport::KernelUnsupported;
    }

    // Zeroed `io_uring_params` (128 bytes): flags/reserved fields default to 0,
    // which the kernel accepts as "no features requested".
    let mut params = [0u8; 128];
    // SAFETY: `params` is a valid, writable 128-byte buffer; `entries` = 2.
    let fd = unsafe { libc::syscall(libc::SYS_io_uring_setup, 2u32, params.as_mut_ptr()) };
    if fd >= 0 {
        // SAFETY: `fd` is a valid file descriptor returned by the syscall.
        unsafe { libc::close(fd as i32) };
        return IoUringSupport::Available;
    }

    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ENOSYS) | Some(libc::EPERM) | Some(libc::EACCES) => {
            IoUringSupport::KernelUnsupported
        }
        _ => IoUringSupport::ProbeFailed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_returns_a_well_formed_verdict() {
        // The probe must never panic and must return exactly one of the three
        // variants; which one depends on the host kernel/security policy.
        let verdict = probe_io_uring();
        assert!(matches!(
            verdict,
            IoUringSupport::Available
                | IoUringSupport::KernelUnsupported
                | IoUringSupport::ProbeFailed
        ));
    }
}

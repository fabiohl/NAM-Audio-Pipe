// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//
// Fix + permanent regression guard for an ELF symbol-interposition hang —
// see .cargo/hide-libm-shadow.map for the full, GDB-verified root-cause
// analysis. Some part of the dependency graph pulls in libm-shaped
// fallback symbols (`log10f`, `atan2f`, `acosf`, ...) that end up compiled
// into the final binary with GLOBAL (exported) visibility and the same C
// name as the real functions in the system's `libm.so.6`. Under standard
// ELF symbol interposition rules, `ld.so` then resolves calls to those
// names back to our own binary instead of the real dynamic library,
// forming a self-referential `trampoline -> PLT -> GOT -> trampoline`
// infinite loop (zero computation, zero syscalls — exactly the observed
// hang).
//
// The fix: force every standard libm C symbol name to `local` binding via
// a linker version script, applied only to this crate's own link targets
// (not to dependency build-script helper binaries, which is why this is
// done here via `cargo:rustc-link-arg` rather than as a blanket
// `[build] rustflags` entry in `.cargo/config.toml` — see the comment
// there for why that approach failed).
fn main() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    // `rust-lld` (the linker used by default in this toolchain, rustc 1.97.1)
    // errors by default on any version-script entry naming a symbol that
    // isn't actually defined in a given binary (unlike GNU `bfd`, which just
    // ignores it). Our map intentionally lists the *entire* standard libm
    // surface so it keeps working regardless of which specific subset a
    // future dependency change ends up shadowing — most binaries will only
    // ever define a few of these locally, and that's fine, so restore the
    // lenient behavior.
    println!("cargo:rustc-link-arg=-Wl,--undefined-version");
    println!(
        "cargo:rustc-link-arg=-Wl,--version-script={manifest_dir}/.cargo/hide-libm-shadow.map"
    );
    println!("cargo:rerun-if-changed=.cargo/hide-libm-shadow.map");
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Typed CPU topology helper for the NAM-Audio-Pipe QA scripts.
//!
//! Replaces shell CPU discovery and literal `taskset`: `pick-core` resolves the
//! benchmark/RT core (explicit `NAM_BENCH_CORE` → first online isolated CPU →
//! first permitted CPU → `GAP`) and `pin` applies that affinity to the current
//! process before exec'ing the command.
//!
//! Usage:
//!   pipe_perf_gate pick-core [--bench-core `<N>`]
//!   pipe_perf_gate pin [--bench-core `<N>`] -- `<command>` [args...]

#[cfg(target_os = "linux")]
fn main() {
    use std::fs;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Command, exit};

    use neural_amp_modeler_rs::rt_hardening::{
        get_allowed_cpus, parse_cpu_list, pin_current_thread,
    };

    fn print_usage() {
        eprintln!(
            "usage: pipe_perf_gate pick-core [--bench-core <N>]\n       \
             pipe_perf_gate pin [--bench-core <N>] -- <command> [args...]"
        );
    }

    fn usage() -> ! {
        print_usage();
        exit(2);
    }

    fn is_online(cpu: usize) -> bool {
        let online = Path::new("/sys/devices/system/cpu").join(format!("cpu{cpu}/online"));
        match fs::read_to_string(online) {
            Ok(value) => value.trim() == "1",
            Err(_) => Path::new("/sys/devices/system/cpu")
                .join(format!("cpu{cpu}"))
                .is_dir(),
        }
    }

    /// §2.4 policy: explicit → first online isolated → first permitted → GAP.
    fn resolve(explicit: Option<&str>) -> Option<u32> {
        if let Some(raw) = explicit
            .map(str::trim)
            .filter(|raw| !raw.is_empty() && *raw != "auto")
        {
            return raw.parse::<u32>().ok();
        }
        let isolated = fs::read_to_string("/sys/devices/system/cpu/isolated")
            .map(|list| parse_cpu_list(&list))
            .unwrap_or_default();
        if let Some(cpu) = isolated.into_iter().find(|&cpu| is_online(cpu)) {
            return Some(cpu as u32);
        }
        get_allowed_cpus().into_iter().next().map(|cpu| cpu as u32)
    }

    fn explicit_core(args: &[String], stop: usize) -> Option<String> {
        args[..stop]
            .windows(2)
            .find(|window| window[0] == "--bench-core")
            .map(|window| window[1].clone())
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(action) = args.first().map(String::as_str) else {
        usage();
    };
    let env_core = std::env::var("NAM_BENCH_CORE").ok();

    match action {
        "-h" | "--help" => {
            print_usage();
            exit(0);
        }
        "pick-core" => {
            let explicit = explicit_core(&args, args.len());
            match resolve(explicit.as_deref().or(env_core.as_deref())) {
                Some(core) => println!("{core}"),
                None => {
                    eprintln!("GAP: no isolated or permitted bench core");
                    exit(2);
                }
            }
        }
        "pin" => {
            let sep = args
                .iter()
                .position(|arg| arg == "--")
                .unwrap_or_else(|| usage());
            if sep + 1 >= args.len() {
                usage();
            }
            let explicit = explicit_core(&args, sep);
            match resolve(explicit.as_deref().or(env_core.as_deref())) {
                Some(core) => {
                    if let Err(e) = pin_current_thread(Some(core as usize)) {
                        eprintln!("pipe_perf_gate: cannot pin to CPU {core}: {e:?}");
                        exit(2);
                    }
                }
                None => eprintln!("GAP: no isolated or permitted bench core — running unpinned"),
            }
            let command = &args[sep + 1];
            let err = Command::new(command).args(&args[sep + 2..]).exec();
            eprintln!("pipe_perf_gate: cannot exec {command}: {err}");
            exit(127);
        }
        _ => usage(),
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("pipe_perf_gate: CPU topology pinning is only supported on Linux");
    std::process::exit(2);
}

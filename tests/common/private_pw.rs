// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Private, isolated PipeWire daemon harness for integration testing.
//!
//! Spawns a lightweight, sandboxed `pipewire` daemon in a dedicated temporary
//! directory with minimal modules (`libpipewire-module-protocol-native`,
//! `libpipewire-module-client-node`, `libpipewire-module-access`,
//! `libpipewire-module-adapter`, `libpipewire-module-link-factory`).
//!
//! Ensures 100% isolation from the host user's PipeWire session:
//! - Uses `PIPEWIRE_RUNTIME_DIR`, `XDG_RUNTIME_DIR` and `PIPEWIRE_REMOTE`.
//! - Never touches systemd units (`systemctl --user`).
//! - Subprocess under test inherits private env vars explicitly (no `std::env::set_var`).
//! - RAII Drop unconditionally issues `SIGKILL`, reaps child and removes tempdir.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Errors encountered when managing the private PipeWire daemon.
#[derive(Debug)]
pub enum PrivatePwError {
    /// The `pipewire` binary was not found on the system (typed GAP).
    BinaryMissing,
    /// Timed out waiting for the daemon socket to appear or respond.
    Timeout(String),
    /// Standard I/O error during daemon management.
    Io(std::io::Error),
}

impl From<std::io::Error> for PrivatePwError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// An isolated, sandboxed PipeWire daemon instance running in a temporary directory.
pub struct PrivatePipewire {
    child: Option<Child>,
    dir: PathBuf,
}

impl PrivatePipewire {
    /// Starts a private, isolated PipeWire daemon in a fresh temp directory.
    pub fn start() -> Result<Self, PrivatePwError> {
        let which = Command::new("pipewire")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if which.is_err() || !which.unwrap().success() {
            return Err(PrivatePwError::BinaryMissing);
        }

        let dir = crate::common::temp_dir();
        let conf_path = dir.join("pipewire.conf");
        let minimal_conf = r#"
context.properties = {
    core.daemon = true
    core.name   = pipewire-0
    support.dbus = false
    log.level = 0
}
context.spa-libs = {
    support.* = support/libspa-support
    audio.convert.* = audioconvert/libspa-audioconvert
}
context.modules = [
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-metadata }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-access }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-link-factory }
]
"#;
        fs::write(&conf_path, minimal_conf)?;

        let mut instance = Self { child: None, dir };
        instance.spawn_daemon(&conf_path)?;
        Ok(instance)
    }

    fn spawn_daemon(&mut self, conf_path: &Path) -> Result<(), PrivatePwError> {
        let child = Command::new("pipewire")
            .arg("-c")
            .arg(conf_path)
            .env("PIPEWIRE_RUNTIME_DIR", &self.dir)
            .env("XDG_RUNTIME_DIR", &self.dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        self.child = Some(child);

        // Wait for socket to become available within 5 seconds
        let socket = self.socket_path();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if socket.exists() {
                let mut cmd = Command::new("pw-cli");
                self.apply_envs(&mut cmd);
                cmd.args(["info", "0"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if let Ok(st) = cmd.status() {
                    if st.success() {
                        return Ok(());
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(50));
                    return Ok(());
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        Err(PrivatePwError::Timeout(
            "private pipewire socket pipewire-0 did not appear within 5s".into(),
        ))
    }

    /// Path to the isolated temporary runtime directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// PipeWire remote identifier string.
    pub fn socket_name(&self) -> String {
        "pipewire-0".into()
    }

    /// Path to the `pipewire-0` UNIX socket file.
    pub fn socket_path(&self) -> PathBuf {
        self.dir.join("pipewire-0")
    }

    /// Applies the isolation environment variables to the provided command.
    pub fn apply_envs(&self, cmd: &mut Command) {
        cmd.env("PIPEWIRE_RUNTIME_DIR", &self.dir)
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("PIPEWIRE_REMOTE", "pipewire-0");
    }

    /// Kills the daemon child process immediately with `SIGKILL` and reaps it.
    pub fn kill_hard(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pid = child.id() as libc::pid_t;
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
            let _ = child.wait();
        }
    }

    /// Freezes the daemon child process with `SIGSTOP`.
    pub fn freeze(&self) {
        if let Some(child) = &self.child {
            let pid = child.id() as libc::pid_t;
            unsafe {
                libc::kill(pid, libc::SIGSTOP);
            }
        }
    }

    /// Thaws the frozen daemon child process with `SIGCONT`.
    pub fn thaw(&self) {
        if let Some(child) = &self.child {
            let pid = child.id() as libc::pid_t;
            unsafe {
                libc::kill(pid, libc::SIGCONT);
            }
        }
    }

    /// Restarts the private PipeWire daemon with the existing configuration.
    pub fn restart(&mut self) -> Result<(), PrivatePwError> {
        self.kill_hard();
        let socket = self.socket_path();
        let _ = fs::remove_file(&socket);
        let conf_path = self.dir.join("pipewire.conf");
        self.spawn_daemon(&conf_path)
    }

    /// Runs `pw-dump` against the private daemon and checks if the NAM capture sink node
    /// is registered in the graph (optionally matching PID).
    pub fn wait_for_nam_sink_matching(&self, timeout: Duration, target_pid: Option<u32>) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let mut cmd = Command::new("pw-dump");
            self.apply_envs(&mut cmd);
            if cmd
                .output()
                .ok()
                .is_some_and(|out| crate::common::parse_nam_sink_in_dump(&out.stdout, target_pid))
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Waits for NAM capture sink matching `pid` within `timeout`.
    pub fn wait_for_nam_sink_pid(&self, pid: u32, timeout: Duration) -> bool {
        self.wait_for_nam_sink_matching(timeout, Some(pid))
    }

    /// Waits for NAM capture sink within `timeout`.
    pub fn wait_for_nam_sink(&self, timeout: Duration) -> bool {
        self.wait_for_nam_sink_matching(timeout, None)
    }

    /// Spawns `pw-play` connected to the private daemon playing `wav` into
    /// `NAM-Audio-Pipe-input` sink with volume 0 (silent stream).
    pub fn spawn_tone(&self, wav: &Path) -> Option<Child> {
        let mut cmd = Command::new("pw-play");
        self.apply_envs(&mut cmd);
        cmd.args(["--target", "NAM-Audio-Pipe-input", "--volume", "0"])
            .arg(wav)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
    }
}

impl Drop for PrivatePipewire {
    fn drop(&mut self) {
        self.kill_hard();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

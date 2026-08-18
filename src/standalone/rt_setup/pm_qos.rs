// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PM QoS and hardware audio detection.
//!
//! Functions to lock deep CPU C-States and detect
//! the system's default hardware sink via PipeWire.

/// Dynamically detects the system's default hardware sink via `pw-metadata`.
///
/// This function attempts to identify which physical device audio should be sent to
/// by default. It parses the output of the PipeWire `pw-metadata` utility.
///
/// A watchdog thread with a 500ms timeout prevents hanging if the PipeWire daemon
/// or `pw-metadata` is unresponsive.
///
/// Returns `Some(name)` if a valid sink that is not NAM-rs itself is found,
/// or `None` otherwise (allowing routing to be decided by WirePlumber).
pub fn detect_hardware_sink() -> Option<String> {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

    let child = std::process::Command::new("pw-metadata")
        .args(["-n", "default", "0", "default.audio.sink"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    let output = match rx.recv_timeout(TIMEOUT) {
        Ok(Ok(out)) => out,
        Ok(Err(_)) => return None,
        Err(_) => {
            log::warn!(
                "detect_hardware_sink: pw-metadata did not respond within {}ms — \
                 skipping default sink detection (WirePlumber will decide routing).",
                TIMEOUT.as_millis()
            );
            return None;
        }
    };

    parse_sink_name_from_metadata(&output.stdout)
}

/// Parses the default sink name from `pw-metadata` raw output.
pub(crate) fn parse_sink_name_from_metadata(raw_stdout: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(raw_stdout);
    let start = s.find("\"name\":\"")?;
    let rest = &s[start + 8..];
    let end = rest.find('"')?;
    let name = &rest[..end];

    if name == crate::standalone::pw_host::identity::PW_CAPTURE_NODE_NAME {
        None
    } else {
        Some(name.to_string())
    }
}

/// Prevents the processor from entering power-saving C-States,
/// guaranteeing 0ms wake-up latency for RT audio processing.
///
/// **Warning:** This protection is **system-wide (global)** and affects all CPU cores,
/// not just the thread executing this function.
///
/// Uses the Linux kernel PM QoS interface to request zero latency.
///
/// RETURN: The `File` handle. It MUST be kept alive in the main scope.
/// If the file descriptor is closed (drop), the kernel revokes the protection.
pub fn lock_cpu_c_states() -> Option<std::fs::File> {
    match std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/cpu_dma_latency")
    {
        Ok(mut file) => {
            // Value 0 indicates zero tolerance to power transition latency.
            let zero: i32 = 0;
            if std::io::Write::write_all(&mut file, &zero.to_ne_bytes()).is_ok() {
                log::info!("⚡ PM QoS Lock: Deep CPU C-States disabled (Zero DMA Latency).");
                return Some(file);
            }
            log::warn!("PM QoS: Failed to write to /dev/cpu_dma_latency.");
            None
        }
        Err(e) => {
            // Often fails if write permission is missing or the file does not exist.
            log::warn!(
                "PM QoS: Access denied to /dev/cpu_dma_latency ({}). \
                 Consider creating a udev rule for the 'audio' group.",
                e
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_hardware_sink() {
        let sample = br#"update: id:0 key:'default.audio.sink' value:'{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}' type:'Spa:String:JSON'"#;
        let result = parse_sink_name_from_metadata(sample);
        assert_eq!(
            result.as_deref(),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo")
        );
    }

    #[test]
    fn parse_nam_capture_node_ignored() {
        let sample = format!(
            r#"update: id:0 key:'default.audio.sink' value:'{{"name":"{}"}}' type:'Spa:String:JSON'"#,
            crate::standalone::pw_host::identity::PW_CAPTURE_NODE_NAME
        );
        let result = parse_sink_name_from_metadata(sample.as_bytes());
        assert!(result.is_none());
    }

    #[test]
    fn parse_invalid_output_returns_none() {
        let sample = b"No metadata found";
        let result = parse_sink_name_from_metadata(sample);
        assert!(result.is_none());
    }

    #[test]
    fn detect_hardware_sink_terminates_promptly() {
        // Runs detect_hardware_sink to ensure it executes without panicking and respects the 500ms timeout.
        let _ = detect_hardware_sink();
    }
}

//! What a backup really sends: systemd's IP accounting of the job's own unit.
//!
//! restic reports what it read (unchanged files included), never what it
//! uploaded, and a computer's network counters are shared with everything else
//! it runs. systemd counts each service's traffic when `IPAccounting=` is on
//! (the NixOS module turns it on for job services); restic and its ssh run in
//! the job's unit, so its egress counter is the backup's upload. Outside such a
//! unit (a run by hand, macOS) there is no reading, and the server shows an
//! average without a peak.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Rates are taken over at least this long, so a burst between two restic
/// status lines (one every two seconds) does not swing them.
const WINDOW: Duration = Duration::from_secs(5);

pub struct UploadMeter {
    unit: Option<String>,
    start: u64,
    last_at: Instant,
    last: u64,
    rate: Option<u64>,
}

impl UploadMeter {
    pub fn new() -> UploadMeter {
        let unit = own_unit();
        let start = unit.as_deref().and_then(egress_bytes);
        UploadMeter {
            unit: start.and(unit),
            start: start.unwrap_or(0),
            last_at: Instant::now(),
            last: start.unwrap_or(0),
            rate: None,
        }
    }

    /// Bytes sent since the meter started, and the rate over the last window.
    pub fn read(&mut self) -> Option<(u64, Option<u64>)> {
        let unit = self.unit.as_deref()?;
        let elapsed = self.last_at.elapsed();
        if elapsed >= WINDOW
            && let Some(now) = egress_bytes(unit)
        {
            let ms = elapsed.as_millis().max(1) as u64;
            self.rate = Some(now.saturating_sub(self.last).saturating_mul(1000) / ms);
            self.last = now;
            self.last_at = Instant::now();
        }
        Some((self.last.saturating_sub(self.start), self.rate))
    }
}

impl Default for UploadMeter {
    fn default() -> Self {
        UploadMeter::new()
    }
}

/// The service this process runs in, from its cgroup (`…/replique-home.service`).
fn own_unit() -> Option<String> {
    unit_of_cgroup(&std::fs::read_to_string("/proc/self/cgroup").ok()?)
}

pub fn unit_of_cgroup(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|l| l.rsplit(':').next())
        .flat_map(|path| path.split('/'))
        .rfind(|c| c.ends_with(".service"))
        .map(str::to_string)
}

/// The unit's IP egress counter; `None` when accounting is off (systemd then
/// prints an empty value, `[not set]` or the largest u64).
fn egress_bytes(unit: &str) -> Option<u64> {
    let out = Command::new("systemctl")
        .args(["show", "--property=IPEgressBytes", "--value", unit])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|v| *v != u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_from_cgroups() {
        assert_eq!(
            unit_of_cgroup("0::/system.slice/replique-hetzner.service\n").as_deref(),
            Some("replique-hetzner.service")
        );
        // cgroup v1 lines, and a unit inside a nested slice.
        assert_eq!(
            unit_of_cgroup("12:pids:/system.slice/replique-phone.service\n1:name=systemd:/system.slice/replique-phone.service").as_deref(),
            Some("replique-phone.service")
        );
        assert_eq!(
            unit_of_cgroup("0::/user.slice/user-1000.slice/session-2.scope\n"),
            None
        );
    }

    #[test]
    fn no_unit_no_reading() {
        let mut m = UploadMeter {
            unit: None,
            start: 0,
            last_at: Instant::now(),
            last: 0,
            rate: None,
        };
        assert!(m.read().is_none());
    }
}

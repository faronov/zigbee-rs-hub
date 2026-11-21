//! What the application needs from the machine it runs on (the S3 firmware,
//! `hubctl` on a desktop): facts for `hub/system`, Wi-Fi credentials, the
//! API token's storage, reboot, boot mode, random bytes and a log tail.
//!
//! [`LogRing`] is a fixed-size byte ring of recent log lines (no per-line
//! allocation once full) that a platform's logger can feed.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::Value as J;

/// The platform (see the module docs). Every method has a default that
/// reports the feature as unsupported.
pub trait Platform: Send + Sync {
    /// Facts for `hub/system`: `version`, `uptime_s`, `heap` {`free`,
    /// `min_free`, `largest_block`}, `mode`, `wifi` {`ssid`, `state`, `ip`,
    /// `rssi`, `channel`}, `reset_reason` … (absent when unknown).
    fn system(&self) -> J {
        J::Object(Default::default())
    }

    /// The 2.4 GHz Wi-Fi channel the hub's own radio uses (1–14): the
    /// energy scan steers the Zigbee channel away from it.
    fn wifi_channel(&self) -> Option<u8> {
        None
    }

    /// Store Wi-Fi credentials and reconnect with them.
    fn set_wifi(&self, _ssid: &str, _password: &str) -> Result<(), String> {
        Err("Wi-Fi settings are not supported here".into())
    }

    /// Persist a new API token.
    fn save_token(&self, _token: &str) -> Result<(), String> {
        Err("token storage is not supported here".into())
    }

    /// Reboot the hub shortly (after the reply went out).
    fn reboot(&self) -> Result<(), String> {
        Err("reboot is not supported here".into())
    }

    /// Persist the boot mode (`hub`, `bridge`) and reboot into it.
    fn set_mode(&self, _mode: &str) -> Result<(), String> {
        Err("boot modes are not supported here".into())
    }

    /// Fill `buf` with random bytes (tokens).
    fn random(&self, buf: &mut [u8]);

    /// The last `max` log lines, oldest first.
    fn logs(&self, _max: usize) -> Vec<String> {
        Vec::new()
    }

    /// Feature names for `hub/info` `maintenance` (`wifi`, `reboot`,
    /// `mode`, `logs`, `token_rotate`).
    fn features(&self) -> Vec<&'static str> {
        Vec::new()
    }
}

impl std::fmt::Debug for dyn Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Platform")
    }
}

/// Recent log lines in at most `capacity` bytes.
pub struct LogRing {
    capacity: usize,
    inner: Mutex<(VecDeque<u8>, u64)>,
}

/// Longest line kept (longer ones are cut).
pub const MAX_LINE: usize = 240;

impl LogRing {
    /// An empty ring of `capacity` bytes (allocated on the first line).
    pub const fn new(capacity: usize) -> Self {
        Self { capacity, inner: Mutex::new((VecDeque::new(), 0)) }
    }

    /// Append one line (cut at [`MAX_LINE`] bytes on a character boundary;
    /// the oldest lines make room).
    pub fn push(&self, line: &str) {
        let mut end = line.len().min(MAX_LINE).min(self.capacity.saturating_sub(1));
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        let line = line[..end].trim_end_matches(['\r', '\n']);
        let Ok(mut g) = self.inner.lock() else { return };
        let (buf, lines) = &mut *g;
        if buf.capacity() < self.capacity {
            buf.reserve_exact(self.capacity - buf.len());
        }
        while buf.len() + line.len() + 1 > self.capacity {
            match buf.iter().position(|b| *b == b'\n') {
                Some(i) => {
                    buf.drain(..=i);
                }
                None => buf.clear(),
            }
        }
        buf.extend(line.bytes().map(|b| if b == b'\n' { b' ' } else { b }));
        buf.push_back(b'\n');
        *lines += 1;
    }

    /// The last `max` lines, oldest first.
    pub fn lines(&self, max: usize) -> Vec<String> {
        let Ok(g) = self.inner.lock() else { return Vec::new() };
        let (a, b) = g.0.as_slices();
        let mut all = Vec::with_capacity(a.len() + b.len());
        all.extend_from_slice(a);
        all.extend_from_slice(b);
        drop(g);
        let text = String::from_utf8_lossy(&all);
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(max)..].iter().map(|s| s.to_string()).collect()
    }

    /// Lines pushed since start (including those dropped).
    pub fn total(&self) -> u64 {
        self.inner.lock().map_or(0, |g| g.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_the_newest_whole_lines() {
        let r = LogRing::new(64);
        for i in 0..20 {
            r.push(&format!("line {i}\n"));
        }
        let lines = r.lines(100);
        assert_eq!(lines.last().unwrap(), "line 19");
        assert!(lines.iter().all(|l| l.starts_with("line ")));
        assert!(lines.iter().map(|l| l.len() + 1).sum::<usize>() <= 64);
        assert_eq!(r.lines(2), vec!["line 18", "line 19"]);
        assert_eq!(r.total(), 20);
        r.push(&"é".repeat(300));
        let last = r.lines(1).pop().unwrap();
        assert!(last.len() <= 63 && last.chars().all(|c| c == 'é'));
    }
}

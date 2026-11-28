//! The NCP's log in the hub's (spec 7.10).
//!
//! The hub grants the LOG endpoint credit in every session
//! ([`crate::driver::LOG_CREDITS`]), so the NCP sends its log records: first the
//! ones it kept since its reset (the zigbee-rs NCP: crystal tuning, IEEE
//! source, NV state), then new ones. Each goes into the hub's log under the
//! target `ncp` (ESP-IDF's log, the console, `hub/logs`) at its own level,
//! debug and trace records at info with `D:` or `T:` before their text
//! (the firmware logs nothing below info), up to the hub's NCP log level:
//! 0 off, 1 error … 5 trace, by default 3 (info, the NCP's own default). A level the operator set
//! ([`Hub::set_ncp_log`]) is kept in the host store and sent in every
//! session, as `SYS_SET_CONFIG` `log_level` (`LOG_CONFIG` travels on the
//! LOG endpoint, to which an NCP grants the host no credit: spec 3.3, and
//! the zigbee-rs NCP). At most [`BURST`] records go out at once
//! and [`PER_SEC`] a second after that; the others are counted
//! ([`NcpLogStats`]) and their number logged with the next one.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use log::{Level, warn};
use zigbee_zrhp::codec::{Bytes8, List};
use zigbee_zrhp::msg::log::{EvtLogDropped, EvtLogRecord};
use zigbee_zrhp::msg::system::{ConfigValue, SysSetConfig, SysSetConfigRsp, config};
use zigbee_zrhp::types::Status;

use super::Hub;
use crate::driver::{CallOpts, Error, SessionState};

/// Host store key of the NCP log level the operator set (one byte).
pub const LOG_KEY: &str = "cfg/ncp_log";
/// The level without one set: info, as the NCP's own default.
pub const DEFAULT_LEVEL: u8 = 3;
/// Records forwarded at once.
pub const BURST: u32 = 16;
/// Records a second after a burst.
pub const PER_SEC: u32 = 2;

/// The NCP log's level and counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NcpLogStats {
    /// 0 off, 1 error … 5 trace.
    pub level: u8,
    /// Records written to the hub's log.
    pub forwarded: u32,
    /// Records over the rate limit, not shown.
    pub suppressed: u32,
    /// Records the NCP dropped (`EVT_LOG_DROPPED`: its ring was full).
    pub dropped: u32,
}

/// [`BURST`] tokens, one more every `1 / PER_SEC` s.
#[derive(Debug)]
struct Bucket {
    tokens: u32,
    at: Instant,
    held: u32,
}

impl Bucket {
    fn new(now: Instant) -> Self {
        Bucket { tokens: BURST, at: now, held: 0 }
    }

    /// A record may go out at `now`: `Some(n)`, `n` held back before it.
    fn take(&mut self, now: Instant) -> Option<u32> {
        let step = Duration::from_millis(1000 / u64::from(PER_SEC));
        let n = (now.saturating_duration_since(self.at).as_millis() / step.as_millis()) as u32;
        if n > 0 {
            self.tokens = self.tokens.saturating_add(n).min(BURST);
            self.at = if self.tokens == BURST { now } else { self.at + step * n };
        }
        if self.tokens == 0 {
            self.held = self.held.saturating_add(1);
            return None;
        }
        self.tokens -= 1;
        Some(std::mem::take(&mut self.held))
    }
}

/// The lifecycle's NCP log state.
pub(super) struct NcpLog {
    level: AtomicU8,
    bucket: Mutex<Bucket>,
    forwarded: AtomicU32,
    suppressed: AtomicU32,
    dropped: AtomicU32,
}

impl NcpLog {
    pub(super) fn new(level: u8) -> Self {
        NcpLog {
            level: AtomicU8::new(level.min(5)),
            bucket: Mutex::new(Bucket::new(Instant::now())),
            forwarded: AtomicU32::new(0),
            suppressed: AtomicU32::new(0),
            dropped: AtomicU32::new(0),
        }
    }

    /// `EVT_LOG_RECORD`.
    pub(super) fn record(&self, e: &EvtLogRecord<'_>) {
        if e.level == 0 || e.level > self.level.load(Ordering::Relaxed) {
            return;
        }
        let Some(held) = self.bucket.lock().unwrap().take(Instant::now()) else {
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if held > 0 {
            warn!(target: "ncp", "{held} NCP log records not shown (rate limit)");
        }
        self.forwarded.fetch_add(1, Ordering::Relaxed);
        let (level, tag) = match e.level {
            1 => (Level::Error, ""),
            2 => (Level::Warn, ""),
            3 => (Level::Info, ""),
            4 => (Level::Info, "D: "),
            _ => (Level::Info, "T: "),
        };
        log::log!(target: "ncp", level, "[{} ms] {tag}{}", e.time_ms, String::from_utf8_lossy(e.text.0));
    }

    /// `EVT_LOG_DROPPED`.
    pub(super) fn dropped(&self, e: &EvtLogDropped) {
        self.dropped.fetch_add(e.count, Ordering::Relaxed);
        if self.level.load(Ordering::Relaxed) > 0 {
            warn!(target: "ncp", "the NCP dropped {} log records (from #{})", e.count, e.first_seq);
        }
    }
}

impl Hub {
    /// The NCP log level: 0 off, 1 error … 5 trace (above 5: 5). Records
    /// up to it go into the hub's log; sent to the NCP now (with a session
    /// up) and in every session, and kept in the host store.
    pub fn set_ncp_log(&self, level: u8) -> Result<(), Error> {
        let level = level.min(5);
        self.with_store(|s| s.kv_put(LOG_KEY, &[level])).map_err(|e| Error::Other(e.to_string()))?;
        self.inner.log.level.store(level, Ordering::Relaxed);
        if self.inner.client.state() != SessionState::Ready {
            return Ok(());
        }
        self.log_config(level, false)
    }

    /// The NCP log's level and counters.
    pub fn ncp_log(&self) -> NcpLogStats {
        let l = &self.inner.log;
        NcpLogStats {
            level: l.level.load(Ordering::Relaxed),
            forwarded: l.forwarded.load(Ordering::Relaxed),
            suppressed: l.suppressed.load(Ordering::Relaxed),
            dropped: l.dropped.load(Ordering::Relaxed),
        }
    }

    /// The level the operator set.
    pub(super) fn stored_log_level(store: &crate::store::Store) -> Option<u8> {
        store.kv_get(LOG_KEY).and_then(|v| v.first().copied())
    }

    /// `SYS_SET_CONFIG` `log_level`.
    pub(super) fn log_config(&self, level: u8, during_resume: bool) -> Result<(), Error> {
        let value = [level];
        let item = [ConfigValue { id: config::LOG_LEVEL, value: Bytes8(&value) }];
        let opts = CallOpts { during_resume, ..CallOpts::timeout(Duration::from_secs(5)) };
        let rsp = self.inner.client.call(&SysSetConfig { values: List::new(&item) }, opts)?;
        match rsp.body::<SysSetConfigRsp>().ok().and_then(|r| r.results.iter().next()) {
            Some(r) if r.status != Status::Success => Err(Error::Status(r.status)),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The firmware's logger: nothing below info is written.
    struct Firmware;
    static WRITTEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

    impl log::Log for Firmware {
        fn enabled(&self, m: &log::Metadata<'_>) -> bool {
            m.level() <= Level::Info
        }
        fn log(&self, r: &log::Record<'_>) {
            if r.target() == "ncp" && self.enabled(r.metadata()) {
                WRITTEN.lock().unwrap().push(format!("{} {}", r.level(), r.args()));
            }
        }
        fn flush(&self) {}
    }

    /// At level 5 the NCP's debug and trace records are written at info,
    /// their level in the text; every record counted is written.
    #[test]
    fn debug_and_trace_records_are_written_at_info() {
        let _ = log::set_logger(&Firmware);
        log::set_max_level(log::LevelFilter::Info);
        let l = NcpLog::new(5);
        for (level, text) in [(4, "ncplog debug"), (5, "ncplog trace"), (2, "ncplog warning")] {
            let text = zigbee_zrhp::codec::Rest(text.as_bytes());
            l.record(&EvtLogRecord { time_ms: 7, level, module: 0, seq: 1, text });
        }
        let written: Vec<String> = WRITTEN.lock().unwrap().iter().filter(|w| w.contains("ncplog")).cloned().collect();
        assert_eq!(written, ["INFO [7 ms] D: ncplog debug", "INFO [7 ms] T: ncplog trace", "WARN [7 ms] ncplog warning"]);
        assert_eq!(l.forwarded.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn a_burst_then_the_rate() {
        let t0 = Instant::now();
        let mut b = Bucket::new(t0);
        for _ in 0..BURST {
            assert_eq!(b.take(t0), Some(0));
        }
        assert_eq!(b.take(t0), None);
        assert_eq!(b.take(t0 + Duration::from_millis(100)), None);
        assert_eq!(b.take(t0 + Duration::from_millis(500)), Some(2), "one token after 1/PER_SEC s; two held back");
        assert_eq!(b.take(t0 + Duration::from_millis(700)), None);
        assert_eq!(b.take(t0 + Duration::from_millis(1000)), Some(1), "the fraction kept");
        // Idle: full again, not more.
        let later = t0 + Duration::from_secs(60);
        for _ in 0..BURST {
            assert!(b.take(later).is_some());
        }
        assert_eq!(b.take(later), None);
    }
}

//! Background maintenance jobs: the board of running jobs per lane, their
//! progress (estimated from time, refined by passes and by the NCP's
//! optional `EVT_SCAN_PROGRESS`), cancellation, and the last result of each
//! kind for clients that (re)connect while or after a job ran.
//!
//! Two lanes: `heavy` (anything that uses the radio or rewrites the
//! network: scans, map, channel change, backup/restore/reset) and `light`
//! (diagnostics, TX power), one job each, so a long scan never makes the
//! diagnostics `busy`. Jobs run on their own threads; the app thread only
//! reads this board (progress events once a second).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hub_devices::api::SessionId;
use hub_devices::hub::ApiError;
use serde_json::Value as J;

use crate::admin::{JobOut, Out};
use crate::jw::{Jw, oom};

/// Message ID of the optional `EVT_SCAN_PROGRESS` (NETWORK event, proposed:
/// `u16 req_tid, u8 scan (0 energy, 1 active), u8 channel, u8 done, u8
/// total, u8 networks_found`).
pub const EVT_SCAN_PROGRESS: u16 = 0x018B;

/// Results kept for `network/job/result`, all kinds together (the oldest
/// go first; larger ones only go to the session that started the job).
pub const MAX_KEPT_RESULT: usize = 8 * 1024;

/// A lane: one job at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// Radio or network-changing work.
    Heavy,
    /// Short reads and settings.
    Light,
}

/// A running job.
pub struct Running {
    /// Job ID (`network/jobs`, events).
    pub job: u64,
    /// `energy_scan`, `active_scan`, `topology`, or a request type.
    pub kind: &'static str,
    lane: Lane,
    /// Reports progress events and a result (else a request/response job).
    background: bool,
    started: Instant,
    passes: u32,
    done: u32,
    pass_started: Instant,
    /// Milliseconds.
    pass_estimate: u32,
    /// Within the current pass, ‰, as the NCP or the job reports it.
    fraction: Option<u32>,
    cancel: Arc<AtomicBool>,
}

impl Running {
    /// `(progress ‰, elapsed ms, ETA ms)`, integer arithmetic only (no
    /// float code in the firmware for this).
    fn progress(&self) -> (u32, u64, u64) {
        let in_pass = self.pass_started.elapsed().as_millis() as u64;
        let est = u64::from(self.pass_estimate.max(100));
        let frac = u64::from(self.fraction.unwrap_or_else(|| (in_pass * 1000 / est).min(950) as u32).min(1000));
        let total = ((u64::from(self.done) * 1000 + frac) / u64::from(self.passes.max(1))).min(1000);
        let left = (1000 - frac) * est / 1000 + u64::from(self.passes.saturating_sub(self.done + 1)) * est;
        (total as u32, self.started.elapsed().as_millis() as u64, left)
    }

    fn write(&self, w: &mut Jw, topic: &str) {
        let (p, elapsed, eta) = self.progress();
        w.obj().key("topic").str(topic).key("job").num(self.job).key("kind").str(self.kind);
        w.key("progress").num(format_args!("{}.{:03}", p / 1000, p % 1000)).key("pass").num(self.done).key("passes").num(self.passes);
        w.key("elapsed_ms").num(elapsed).key("eta_ms").num(eta).key("cancelling").num(self.cancel.load(Ordering::Relaxed)).end_obj();
    }

    fn event(&self) -> J {
        let mut w = match Jw::value(200) {
            Ok(w) => w,
            Err(_) => return J::Null,
        };
        self.write(&mut w, "job_progress");
        w.finish_value().map_or(J::Null, |t| jv(&t))
    }
}

/// An event body from its text (the engine takes values; small events
/// are cheaper to write as text than to build with `json!`).
fn jv(text: &str) -> J {
    serde_json::from_str(text).unwrap_or(J::Null)
}

/// The board.
#[derive(Default)]
pub struct Board {
    next: u64,
    running: Vec<Running>,
    /// `(job, kind, result text)`, the last one of each kind.
    results: Vec<(u64, &'static str, String)>,
}

/// Shared between the app thread and the job threads.
pub type Shared = Arc<Mutex<Board>>;

impl Board {
    /// Register a job on `lane`, or the job already running there.
    pub fn start(
        &mut self,
        lane: Lane,
        kind: &'static str,
        background: bool,
        passes: u32,
        pass_estimate: Duration,
    ) -> Result<(u64, Arc<AtomicBool>), ApiError> {
        if let Some(r) = self.running.iter().find(|r| r.lane == lane) {
            return Err(ApiError::new("busy", format!("{} (job {}) is running", r.kind, r.job)));
        }
        self.next += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        let now = Instant::now();
        self.running.push(Running {
            job: self.next,
            kind,
            lane,
            background,
            started: now,
            passes: passes.max(1),
            done: 0,
            pass_started: now,
            pass_estimate: pass_estimate.as_millis().min(u32::MAX as u128) as u32,
            fraction: None,
            cancel: cancel.clone(),
        });
        Ok((self.next, cancel))
    }

    fn get(&mut self, job: u64) -> Option<&mut Running> {
        self.running.iter_mut().find(|r| r.job == job)
    }

    /// The job ended.
    pub fn finish(&mut self, job: u64) {
        self.running.retain(|r| r.job != job);
    }

    /// Progress events of the background jobs (the app sends them once a
    /// second).
    #[inline(never)]
    pub fn progress_events(&self) -> Vec<J> {
        self.running.iter().filter(|r| r.background).map(Running::event).collect()
    }

    /// `EVT_SCAN_PROGRESS` from the NCP: the fraction of the current pass
    /// of the running scan.
    pub fn ncp_progress(&mut self, payload: &[u8]) {
        let (Some(&done), Some(&total)) = (payload.get(4), payload.get(5)) else {
            return;
        };
        if total == 0 {
            return;
        }
        if let Some(r) = self.running.iter_mut().find(|r| r.lane == Lane::Heavy && r.kind.ends_with("_scan")) {
            r.fraction = Some(u32::from(done) * 1000 / u32::from(total));
        }
    }

    /// Ask a job to stop (scans stop after the current pass).
    pub fn cancel(&mut self, job: u64) -> bool {
        self.get(job).filter(|r| r.background).map(|r| r.cancel.store(true, Ordering::Relaxed)).is_some()
    }

    /// The `network/jobs` result frame.
    #[inline(never)]
    pub fn list_frame(&self, id: u64) -> Result<String, ApiError> {
        let mut w = Jw::result(id, 120 + 220 * self.running.len() + 40 * self.results.len())?;
        w.obj().key("running").arr();
        for r in &self.running {
            r.write(&mut w, "job");
        }
        w.end_arr().key("results").arr();
        for (job, kind, _) in &self.results {
            w.obj().key("job").num(job).key("kind").str(kind).end_obj();
        }
        w.end_arr().end_obj();
        w.finish()
    }

    /// The `network/job/result` frame: `{job, kind, partial, result}`.
    #[inline(never)]
    pub fn result_reply(&self, id: u64, job: Option<u64>, kind: Option<&str>) -> Result<String, ApiError> {
        let (j, k, t) = self.result(job, kind).ok_or_else(|| ApiError::new("not_found", "no such result (kept: the last of each kind, up to 16 KB)"))?;
        let mut w = Jw::result(id, t.len() + 120)?;
        w.obj().key("job").num(j).key("kind").str(k).key("partial").num(false).key("result").raw(t).end_obj();
        w.finish()
    }

    /// A kept result: by job, or the last one of `kind`.
    pub fn result(&self, job: Option<u64>, kind: Option<&str>) -> Option<(u64, &'static str, &str)> {
        self.results.iter().find(|(j, k, _)| job.is_none_or(|x| x == *j) && kind.is_none_or(|x| x == *k)).map(|(j, k, t)| (*j, *k, t.as_str()))
    }

    fn keep(&mut self, job: u64, kind: &'static str, text: String) {
        self.results.retain(|(_, k, _)| *k != kind);
        if text.len() > MAX_KEPT_RESULT {
            return;
        }
        while self.results.iter().map(|r| r.2.len()).sum::<usize>() + text.len() > MAX_KEPT_RESULT {
            self.results.remove(0);
        }
        self.results.push((job, kind, text));
    }
}

/// What a background job body gets.
pub struct Bg {
    /// Job ID.
    pub job: u64,
    /// Kind.
    pub kind: &'static str,
    board: Shared,
    cancel: Arc<AtomicBool>,
    out: Out,
    sid: SessionId,
}

/// `{"type": "job_result", "job": …, "kind": …, "partial": …, "result": …}`:
/// a job's result for the session that started it.
pub fn result_frame(job: u64, kind: &str, partial: bool, result: &str) -> Result<String, ApiError> {
    let head = format!("{{\"type\":\"job_result\",\"job\":{job},\"kind\":\"{kind}\",\"partial\":{partial},\"result\":");
    let mut s = String::new();
    s.try_reserve_exact(head.len() + result.len() + 1).map_err(|_| oom())?;
    s.push_str(&head);
    s.push_str(result);
    s.push('}');
    Ok(s)
}

impl Bg {
    /// The client asked to stop.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// The cancel flag (for loops that check it themselves).
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    /// Pass `done` finished after `took`: later passes are estimated from it.
    pub fn pass_done(&self, done: u32, took: Duration) {
        let ev = {
            let mut b = self.board.lock().unwrap();
            let Some(r) = b.get(self.job) else { return };
            r.done = done;
            r.pass_started = Instant::now();
            r.pass_estimate = took.as_millis().min(u32::MAX as u128) as u32;
            r.fraction = None;
            r.event()
        };
        (self.out)(JobOut::Event(ev));
    }

    /// Progress within the (only) pass, ‰.
    pub fn fraction(&self, f: u32) {
        if let Some(r) = self.board.lock().unwrap().get(self.job) {
            r.fraction = Some(f);
        }
    }

    /// The output channel (request/response jobs).
    pub fn out(&self) -> &Out {
        &self.out
    }

    /// An intermediate result for the session that started the job.
    pub fn partial(&self, result: &str) {
        if let Ok(f) = result_frame(self.job, self.kind, true, result) {
            (self.out)(JobOut::Reply(self.sid, f));
        }
    }
}

/// `job_done`/`job_failed` event bodies, or the `job_result` frame of a
/// failed job: `{topic|type, job, kind, error | cancelled}`.
fn end_text(name: &str, job: u64, kind: &str, err: Option<&ApiError>, cancelled: bool) -> Result<String, ApiError> {
    let mut w = Jw::value(160)?;
    w.obj().key(if name == "job_result" { "type" } else { "topic" }).str(name).key("job").num(job).key("kind").str(kind);
    match err {
        Some(e) => w.key("error").obj().key("code").str(&e.code).key("message").str(&e.message).end_obj(),
        None => w.key("cancelled").num(cancelled),
    };
    if name == "job_result" {
        w.key("partial").num(false);
    }
    w.end_obj();
    w.finish_value()
}

/// Body of a background job: the result value's text.
pub type BgBody = Box<dyn FnOnce(&Bg) -> Result<String, ApiError> + Send>;

/// Ends the job on the board even if its body panics.
struct Done(Shared, u64);

impl Drop for Done {
    fn drop(&mut self) {
        if let Ok(mut b) = self.0.lock() {
            b.finish(self.1);
        }
    }
}

type Task = Box<dyn FnOnce() + Send>;

/// The long-lived job thread, started with the app while the heap is
/// still in one piece. A thread per job needs its whole stack in one free
/// block every time: after hours of uptime the S3's heap had 51 KB free
/// but no block of 12 KiB, and every job failed with `os error 12`; each
/// thread also left about 300 B behind. Jobs run here; only a job that
/// starts while another one runs gets a thread of its own.
pub struct Worker {
    /// The next task and the worker's wake-up (no channel: its generic
    /// code would cost more flash than this).
    slot: Arc<(Mutex<Option<Task>>, std::sync::Condvar)>,
    busy: Arc<AtomicBool>,
}

impl Worker {
    /// Start the worker thread (None when even that fails).
    pub fn start(stack: usize) -> Option<Worker> {
        let slot: Arc<(Mutex<Option<Task>>, std::sync::Condvar)> = Arc::default();
        let busy = Arc::new(AtomicBool::new(false));
        let (s, b) = (slot.clone(), busy.clone());
        let main: Task = Box::new(move || {
            loop {
                let task = {
                    let mut g = s.0.lock().unwrap();
                    loop {
                        if let Some(t) = g.take() {
                            break t;
                        }
                        g = s.1.wait(g).unwrap();
                    }
                };
                task();
                b.store(false, Ordering::Release);
            }
        });
        spawn_task("hub-job", stack, main).ok()?;
        Some(Worker { slot, busy })
    }

    /// Hand `task` to the worker if it is idle; else give it back.
    fn try_run(&self, task: Task) -> Result<(), Task> {
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(task);
        }
        *self.slot.0.lock().unwrap() = Some(task);
        self.slot.1.notify_one();
        Ok(())
    }

    /// A job is running on the worker.
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }
}

/// Where and how a job runs.
pub struct Exec<'a> {
    /// The long-lived worker, when there is one.
    pub worker: Option<&'a Worker>,
    /// Stack of a job thread of its own.
    pub stack: usize,
    /// Logs the stack high water of finished jobs.
    pub platform: Option<Arc<dyn crate::Platform>>,
}

/// Run `body` on the worker (or its own thread); the caller has registered
/// the job. With `reply` (a request ID) the body's text is the whole result
/// frame for that request; otherwise it is a background job's result value.
#[allow(clippy::too_many_arguments)]
pub fn run(
    board: Shared,
    job: u64,
    kind: &'static str,
    cancel: Arc<AtomicBool>,
    out: Out,
    sid: SessionId,
    exec: Exec<'_>,
    reply: Option<u64>,
    body: BgBody,
) -> Result<(), ApiError> {
    let bg = Bg { job, kind, board: board.clone(), cancel, out: out.clone(), sid };
    let done = Done(board.clone(), job);
    let platform = exec.platform;
    let task: Task = Box::new(move || {
        let r = body(&bg);
        if let Some(hw) = platform.as_ref().and_then(|p| p.stack_high_water()) {
            log::info!("admin: {kind} done, stack high water {hw} B free");
        }
        if let Err(e) = &r {
            log::warn!("admin: {kind}: {}", e.message);
        }
        if let Some(id) = reply {
            drop(done);
            out(JobOut::Reply(sid, r.unwrap_or_else(|e| crate::admin::frame(id, Err(e)))));
            return;
        }
        let cancelled = bg.cancelled();
        // The session that started the job gets the result (or the error)
        // whether or not it subscribed to `network`; the text is then kept
        // (moved, not copied) for `network/job/result`.
        let frame = match &r {
            Ok(text) => result_frame(job, kind, false, text),
            Err(e) => end_text("job_result", job, kind, Some(e), cancelled),
        };
        if let Ok(f) = frame {
            out(JobOut::Reply(sid, f));
        }
        let ev = end_text(if r.is_ok() { "job_done" } else { "job_failed" }, job, kind, r.as_ref().err(), cancelled);
        if let Ok(text) = r {
            board.lock().unwrap().keep(job, kind, text);
        }
        drop(done);
        out(JobOut::Event(ev.map_or(J::Null, |t| jv(&t))));
    });
    let task = match exec.worker {
        Some(w) => match w.try_run(task) {
            Ok(()) => return Ok(()),
            Err(t) => t,
        },
        None => task,
    };
    spawn_task("hub-job2", exec.stack, task).map_err(|e| ApiError::new("failed", format!("job thread: {e}")))
}

/// The one thread entry for jobs (one generic instance in the firmware).
fn spawn_task(name: &str, stack: usize, task: Task) -> std::io::Result<()> {
    std::thread::Builder::new().name(name.into()).stack_size(stack).spawn(task).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_from_passes_and_ncp_events() {
        let mut b = Board::default();
        let (job, _) = b.start(Lane::Heavy, "energy_scan", true, 4, Duration::from_secs(100)).unwrap();
        assert_eq!(b.start(Lane::Heavy, "topology", true, 1, Duration::ZERO).unwrap_err().code, "busy");
        assert!(b.start(Lane::Light, "diagnostics", false, 1, Duration::ZERO).is_ok());
        b.get(job).unwrap().done = 2;
        // EVT_SCAN_PROGRESS: tid, scan 0, channel 18, 8 of 16 channels.
        b.ncp_progress(&[1, 0, 0, 18, 8, 16, 0]);
        let ev = &b.progress_events()[0];
        assert_eq!((ev["job"].as_u64(), ev["pass"].as_u64(), ev["progress"].as_f64()), (Some(job), Some(2), Some(0.625)));
        let list: J = serde_json::from_str(&b.list_frame(7).unwrap()).unwrap();
        assert_eq!(list["result"]["running"].as_array().map(Vec::len), Some(2), "{list}");
        // Half a pass and one more pass left.
        assert!((140_000..=150_000).contains(&ev["eta_ms"].as_u64().unwrap()), "{ev}");
        assert!(b.cancel(job));
        assert_eq!(b.progress_events()[0]["cancelling"], true);
        b.finish(job);
        assert!(b.progress_events().is_empty(), "non-background jobs have no events");
    }
}

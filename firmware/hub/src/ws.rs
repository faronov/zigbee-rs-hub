//! The `/api` WebSocket endpoint on raw esp_http_server calls.
//!
//! `EspHttpServer::ws_handler` (esp-idf-svc 0.53) registers its handlers
//! without `handle_ws_control_frames`. esp_http_server then answers PING and
//! CLOSE itself but neither passes a PONG to the handler nor reads its
//! header remainder, so the next frame is parsed from the middle of the PONG
//! and esp-idf-svc panics on the reserved opcode it finds there. Any client
//! answering the hub's keepalive PING crashed the hub. Here control frames
//! come to our handler, which reads every frame completely and closes the
//! socket on a protocol error.
//!
//! Frames to a client go into one outbox drained by the httpd thread,
//! without waiting, so a slow client never holds up the app; the httpd
//! thread also runs the handshakes: a send to a socket number that has been
//! closed and handed to a new connection meanwhile is dropped there, by the
//! connection generation. `httpd_queue_work` is called only when the outbox
//! was idle: every call is a UDP datagram to esp_http_server's loopback
//! control socket, whose mailbox holds 6 (`CONFIG_LWIP_UDP_RECVMBOX_SIZE`),
//! so a call per frame lost frames, or failed and dropped sessions, as soon
//! as a few clients had events queued at once. lwIP drops a datagram to a
//! full mailbox without an error, so a drain queued too long ago is queued
//! again.
//!
//! What is queued is bounded in bytes, per client and for all clients
//! ([`Resources`](hub_core::resources::Resources)); a client behind its
//! budget is closed and subscribes again. A subscription snapshot goes as
//! the fragments of one message, each made once the earlier ones are sent
//! (`hub_app::Pieces`).

use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use esp_idf_svc::handle::RawHandle;
use esp_idf_svc::http::server::EspHttpServer;
use esp_idf_svc::sys::{
    ESP_ERR_INVALID_SIZE, ESP_FAIL, ESP_OK, EspError, IPPROTO_TCP, SO_SNDTIMEO, SOL_SOCKET,
    TCP_NODELAY, esp, esp_err_t, http_method_HTTP_GET, httpd_handle_t, httpd_queue_work,
    httpd_register_uri_handler, httpd_req_t, httpd_req_to_sockfd, httpd_sess_trigger_close,
    httpd_uri_t, httpd_ws_client_info_t_HTTPD_WS_CLIENT_WEBSOCKET, httpd_ws_frame_t,
    httpd_ws_get_fd_info, httpd_ws_recv_frame, httpd_ws_send_frame, httpd_ws_send_frame_async,
    httpd_ws_type_t, httpd_ws_type_t_HTTPD_WS_TYPE_BINARY, httpd_ws_type_t_HTTPD_WS_TYPE_CLOSE,
    httpd_ws_type_t_HTTPD_WS_TYPE_CONTINUE, httpd_ws_type_t_HTTPD_WS_TYPE_PING,
    httpd_ws_type_t_HTTPD_WS_TYPE_PONG, httpd_ws_type_t_HTTPD_WS_TYPE_TEXT, lwip_setsockopt,
    timeval,
};
use hub_app::{App, Pieces, SessionSink};
use log::{info, warn};

/// Largest API frame accepted from a client.
const MAX_FRAME: usize = 16 * 1024;

/// Bytes a queued frame costs besides its payload (job, box, header).
const FRAME_OVERHEAD: usize = 64;

/// Frames one outbox run sends before it yields to the httpd thread's
/// other work (incoming frames, handshakes); it queues itself again.
const PUMP_BATCH: usize = 16;

/// A drain queued this long ago without running was lost.
const PUMP_LOST_MS: u32 = 1000;

/// Frames for every client, in order, and whether (since when) a drain is
/// queued.
static OUTBOX: Mutex<VecDeque<Box<Job>>> = Mutex::new(VecDeque::new());
static PUMP_QUEUED: AtomicBool = AtomicBool::new(false);
static PUMP_SINCE_MS: AtomicU32 = AtomicU32::new(0);
/// Bytes queued to all clients.
static QUEUED_BYTES: AtomicUsize = AtomicUsize::new(0);

fn now_ms() -> u32 {
    (unsafe { esp_idf_svc::sys::esp_timer_get_time() } / 1000) as u32
}

/// Socket → generation of the WebSocket connection on it; only the httpd
/// thread changes it.
static GENERATIONS: Mutex<BTreeMap<i32, u32>> = Mutex::new(BTreeMap::new());
static NEXT_GENERATION: AtomicU32 = AtomicU32::new(1);

struct Endpoint {
    app: App,
    /// Socket → API session; httpd thread only.
    sessions: Mutex<BTreeMap<i32, u64>>,
}

/// Registers `/api` on `server`.
pub fn register(server: &EspHttpServer<'static>, app: App) -> Result<(), EspError> {
    let endpoint: &'static Endpoint = Box::leak(Box::new(Endpoint {
        app,
        sessions: Mutex::default(),
    }));
    let uri = httpd_uri_t {
        uri: c"/api".as_ptr(),
        method: http_method_HTTP_GET as _,
        handler: Some(handle),
        user_ctx: endpoint as *const Endpoint as *mut c_void,
        is_websocket: true,
        handle_ws_control_frames: true,
        ..Default::default()
    };
    esp!(unsafe { httpd_register_uri_handler(server.handle(), &uri) })
}

unsafe extern "C" fn handle(req: *mut httpd_req_t) -> esp_err_t {
    // SAFETY: esp_http_server passes a valid request; user_ctx is the leaked
    // `Endpoint` from `register`.
    let (endpoint, hd, fd, method) = unsafe {
        (
            &*((*req).user_ctx as *const Endpoint),
            (*req).handle,
            httpd_req_to_sockfd(req),
            (*req).method,
        )
    };
    if method == http_method_HTTP_GET as i32 {
        // Through the open setup AP the client would send its token in the
        // clear: closed before `auth_required`.
        if crate::setup::via_ap(req) {
            warn!("api: socket {fd} through the setup access point, closing");
            return ESP_FAIL;
        }
        endpoint.open(hd, fd);
        return ESP_OK;
    }
    match endpoint.frame(req, fd) {
        Ok(()) => ESP_OK,
        Err(e) => {
            // esp_http_server closes the socket when the handler fails.
            warn!("api: socket {fd}: {e}, closing");
            endpoint.close(fd);
            e.code()
        }
    }
}

impl Endpoint {
    /// A completed handshake on `fd`.
    fn open(&self, hd: httpd_handle_t, fd: i32) {
        // esp_http_server writes a frame's header and payload separately:
        // with Nagle the payload waited for the client's delayed ACK.
        let on: i32 = 1;
        unsafe {
            lwip_setsockopt(
                fd,
                IPPROTO_TCP as i32,
                TCP_NODELAY as i32,
                (&raw const on).cast(),
                size_of::<i32>() as u32,
            )
        };
        // The httpd thread sends for every client: a full TCP window may
        // hold it up for 2 s at most (esp-idf-svc sets 5 s), then the
        // send fails and the connection is closed.
        let timeout = timeval {
            tv_sec: 2,
            tv_usec: 0,
        };
        unsafe {
            lwip_setsockopt(
                fd,
                SOL_SOCKET as i32,
                SO_SNDTIMEO as i32,
                (&raw const timeout).cast(),
                size_of::<timeval>() as u32,
            )
        };
        let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
        GENERATIONS.lock().unwrap().insert(fd, generation);
        let mut sessions = self.sessions.lock().unwrap();
        // The socket number of a connection that went away silently.
        if let Some(old) = sessions.remove(&fd) {
            self.app.disconnect(old);
        }
        let sink = WsSink {
            hd: hd as usize,
            fd,
            generation,
            state: Arc::default(),
        };
        if let Some(sid) = self.app.connect(Box::new(sink)) {
            sessions.insert(fd, sid);
            info!("api: socket {fd} session {sid}");
        }
    }

    fn close(&self, fd: i32) {
        GENERATIONS.lock().unwrap().remove(&fd);
        if let Some(sid) = self.sessions.lock().unwrap().remove(&fd) {
            self.app.disconnect(sid);
        }
    }

    /// Reads one whole frame and acts on it.
    fn frame(&self, req: *mut httpd_req_t, fd: i32) -> Result<(), EspError> {
        let mut frame = httpd_ws_frame_t::default();
        esp!(unsafe { httpd_ws_recv_frame(req, &mut frame, 0) })?;
        if frame.len > MAX_FRAME {
            return Err(EspError::from_infallible::<ESP_ERR_INVALID_SIZE>());
        }
        let mut payload = vec![0u8; frame.len];
        if frame.len > 0 {
            frame.payload = payload.as_mut_ptr();
            esp!(unsafe { httpd_ws_recv_frame(req, &mut frame, payload.len()) })?;
        }
        #[allow(non_upper_case_globals)]
        match frame.type_ {
            httpd_ws_type_t_HTTPD_WS_TYPE_TEXT if frame.final_ => {
                let session = self.sessions.lock().unwrap().get(&fd).copied();
                if let (Some(sid), Ok(text)) = (session, String::from_utf8(payload)) {
                    self.app.text(sid, text);
                }
            }
            httpd_ws_type_t_HTTPD_WS_TYPE_PING => {
                reply(req, httpd_ws_type_t_HTTPD_WS_TYPE_PONG, &mut payload)?;
            }
            httpd_ws_type_t_HTTPD_WS_TYPE_CLOSE => {
                // A browser closing the page; esp_http_server closes the
                // socket after this handler.
                let _ = reply(req, httpd_ws_type_t_HTTPD_WS_TYPE_CLOSE, &mut []);
                self.close(fd);
            }
            // Fragmented and binary messages are not part of the API.
            httpd_ws_type_t_HTTPD_WS_TYPE_PONG
            | httpd_ws_type_t_HTTPD_WS_TYPE_TEXT
            | httpd_ws_type_t_HTTPD_WS_TYPE_BINARY
            | httpd_ws_type_t_HTTPD_WS_TYPE_CONTINUE => {}
            _ => return Err(EspError::from_infallible::<ESP_FAIL>()),
        }
        Ok(())
    }
}

fn reply(req: *mut httpd_req_t, kind: httpd_ws_type_t, payload: &mut [u8]) -> Result<(), EspError> {
    let mut frame = httpd_ws_frame_t {
        type_: kind,
        final_: true,
        payload: payload.as_mut_ptr(),
        len: payload.len(),
        ..Default::default()
    };
    esp!(unsafe { httpd_ws_send_frame(req, &mut frame) })
}

fn is_current(hd: httpd_handle_t, fd: i32, generation: u32) -> bool {
    GENERATIONS.lock().unwrap().get(&fd) == Some(&generation)
        && unsafe { httpd_ws_get_fd_info(hd, fd) }
            == httpd_ws_client_info_t_HTTPD_WS_CLIENT_WEBSOCKET
}

/// One API session's connection.
struct WsSink {
    /// `httpd_handle_t` (a pointer, kept as an address to be `Send`).
    hd: usize,
    fd: i32,
    generation: u32,
    state: Arc<SinkState>,
}

#[derive(Default)]
struct SinkState {
    /// A send failed or the client fell behind.
    dead: AtomicBool,
    /// Bytes queued to the httpd thread and not sent yet.
    queued: AtomicUsize,
}

/// A frame for the httpd thread.
struct Job {
    hd: usize,
    fd: i32,
    generation: u32,
    kind: httpd_ws_type_t,
    /// A fragment of a message; `last` is its final one.
    fragment: bool,
    last: bool,
    payload: Vec<u8>,
    state: Arc<SinkState>,
}

impl Job {
    fn cost(&self) -> usize {
        self.payload.len() + FRAME_OVERHEAD
    }
}

/// Queues a drain of [`OUTBOX`] on the httpd thread unless one is queued.
fn schedule(hd: httpd_handle_t) {
    let now = now_ms();
    let queued = PUMP_QUEUED.swap(true, Ordering::AcqRel);
    if queued && now.wrapping_sub(PUMP_SINCE_MS.load(Ordering::Acquire)) < PUMP_LOST_MS {
        return;
    }
    if queued {
        warn!("api: outbox drain lost, queuing it again");
    }
    PUMP_SINCE_MS.store(now, Ordering::Release);
    if unsafe { httpd_queue_work(hd, Some(pump), hd.cast()) } != ESP_OK {
        // Retried with the next frame (at the latest the 30 s keepalive).
        PUMP_QUEUED.store(false, Ordering::Release);
    }
}

/// Drains [`OUTBOX`] on the httpd thread, [`PUMP_BATCH`] frames at a time.
unsafe extern "C" fn pump(arg: *mut c_void) {
    for _ in 0..PUMP_BATCH {
        let job = OUTBOX.lock().unwrap().pop_front();
        match job {
            Some(job) => run_job(job),
            None => {
                PUMP_QUEUED.store(false, Ordering::Release);
                // A frame queued between the pop and the store.
                if !OUTBOX.lock().unwrap().is_empty() {
                    schedule(arg.cast());
                }
                return;
            }
        }
    }
    PUMP_QUEUED.store(false, Ordering::Release);
    schedule(arg.cast());
}

fn run_job(mut job: Box<Job>) {
    let hd = job.hd as httpd_handle_t;
    let current = is_current(hd, job.fd, job.generation);
    let sent = current && !job.state.dead.load(Ordering::Relaxed) && {
        let mut frame = httpd_ws_frame_t {
            type_: job.kind,
            final_: !job.fragment || job.last,
            fragmented: job.fragment,
            payload: job.payload.as_mut_ptr(),
            len: job.payload.len(),
            ..Default::default()
        };
        let err = unsafe { httpd_ws_send_frame_async(hd, job.fd, &mut frame) };
        err == ESP_OK
    };
    let cost = job.cost();
    job.state.queued.fetch_sub(cost, Ordering::Relaxed);
    QUEUED_BYTES.fetch_sub(cost, Ordering::Relaxed);
    if !sent && !job.state.dead.swap(true, Ordering::Relaxed) && current {
        unsafe { httpd_sess_trigger_close(hd, job.fd) };
    }
}

impl WsSink {
    /// Queues one frame to the httpd thread without waiting for it, so a
    /// slow or vanished client never holds up the app (and through it the
    /// other clients). A failed send, or a client with frames queued that
    /// would go over its or the total byte budget, ends the connection.
    fn queue(&self, kind: httpd_ws_type_t, fragment: bool, last: bool, payload: Vec<u8>) -> bool {
        if !self.alive() {
            return false;
        }
        let r = crate::resources::get();
        let job = Box::new(Job {
            hd: self.hd,
            fd: self.fd,
            generation: self.generation,
            kind,
            fragment,
            last,
            payload,
            state: self.state.clone(),
        });
        let cost = job.cost();
        let mine = self.state.queued.fetch_add(cost, Ordering::Relaxed);
        let total = QUEUED_BYTES.fetch_add(cost, Ordering::Relaxed);
        // A message to a client with nothing queued always goes.
        if mine > 0 && (mine + cost > r.ws_client_bytes || total + cost > r.ws_total_bytes) {
            self.state.queued.fetch_sub(cost, Ordering::Relaxed);
            QUEUED_BYTES.fetch_sub(cost, Ordering::Relaxed);
            warn!(
                "api: socket {} is {mine} B behind ({total} B queued in all), closing",
                self.fd
            );
            self.state.dead.store(true, Ordering::Relaxed);
            unsafe { httpd_sess_trigger_close(self.hd as httpd_handle_t, self.fd) };
            return false;
        }
        OUTBOX.lock().unwrap().push_back(job);
        schedule(self.hd as httpd_handle_t);
        true
    }

    fn text(&self, payload: Vec<u8>) -> bool {
        self.queue(httpd_ws_type_t_HTTPD_WS_TYPE_TEXT, false, true, payload)
    }
}

impl SessionSink for WsSink {
    fn send(&mut self, text: &str) -> bool {
        self.text(text.as_bytes().to_vec())
    }
    fn send_owned(&mut self, text: String) -> bool {
        self.text(text.into_bytes())
    }
    fn alive(&self) -> bool {
        !self.state.dead.load(Ordering::Relaxed)
            && is_current(self.hd as httpd_handle_t, self.fd, self.generation)
    }
    fn keepalive(&mut self) -> bool {
        // A control frame may go between the fragments of a message.
        self.queue(httpd_ws_type_t_HTTPD_WS_TYPE_PING, false, true, Vec::new())
    }
    fn pieces(&self) -> Option<Pieces> {
        let r = crate::resources::get();
        Some(Pieces {
            size: r.snapshot_chunk_bytes,
            backlog: r.ws_client_bytes / 2 - r.snapshot_chunk_bytes,
            held: r.ws_client_bytes,
        })
    }
    fn backlog(&self) -> usize {
        // Polled while a snapshot waits: a lost drain is queued again here
        // instead of with the next frame (which waits for the snapshot).
        if !OUTBOX.lock().unwrap().is_empty() {
            schedule(self.hd as httpd_handle_t);
        }
        self.state.queued.load(Ordering::Relaxed)
    }
    fn close(&mut self) {
        let hd = self.hd as httpd_handle_t;
        if !self.state.dead.swap(true, Ordering::Relaxed) && is_current(hd, self.fd, self.generation) {
            unsafe { httpd_sess_trigger_close(hd, self.fd) };
        }
    }
    fn send_piece(&mut self, text: String, first: bool, last: bool) -> bool {
        let kind = if first { httpd_ws_type_t_HTTPD_WS_TYPE_TEXT } else { httpd_ws_type_t_HTTPD_WS_TYPE_CONTINUE };
        self.queue(kind, true, last, text.into_bytes())
    }
}

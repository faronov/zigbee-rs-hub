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
//! Frames to a client are queued to the httpd thread (`httpd_queue_work`)
//! without waiting, so a slow client never holds up the app; the httpd
//! thread also runs the handshakes: a send to a socket number that has been
//! closed and handed to a new connection meanwhile is dropped there, by the
//! connection generation.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

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
use hub_app::{App, SessionSink};
use log::{info, warn};

/// Largest API frame accepted from a client.
const MAX_FRAME: usize = 16 * 1024;

/// Frames a client may be behind before its connection is closed.
const MAX_INFLIGHT: u32 = 32;

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
    /// Frames queued to the httpd thread and not sent yet.
    inflight: AtomicU32,
}

/// A frame for the httpd thread.
struct Job {
    hd: usize,
    fd: i32,
    generation: u32,
    kind: httpd_ws_type_t,
    payload: Vec<u8>,
    state: Arc<SinkState>,
}

unsafe extern "C" fn run_job(arg: *mut c_void) {
    // SAFETY: `arg` is the `Box<Job>` leaked by `WsSink::queue`.
    let mut job = unsafe { Box::from_raw(arg as *mut Job) };
    let hd = job.hd as httpd_handle_t;
    let current = is_current(hd, job.fd, job.generation);
    let sent = current && !job.state.dead.load(Ordering::Relaxed) && {
        let mut frame = httpd_ws_frame_t {
            type_: job.kind,
            final_: true,
            payload: job.payload.as_mut_ptr(),
            len: job.payload.len(),
            ..Default::default()
        };
        let err = unsafe { httpd_ws_send_frame_async(hd, job.fd, &mut frame) };
        err == ESP_OK
    };
    job.state.inflight.fetch_sub(1, Ordering::Relaxed);
    if !sent && !job.state.dead.swap(true, Ordering::Relaxed) && current {
        unsafe { httpd_sess_trigger_close(hd, job.fd) };
    }
}

impl WsSink {
    /// Queues one frame to the httpd thread without waiting for it, so a
    /// slow or vanished client never holds up the app (and through it the
    /// other clients). A failed send, or a client [`MAX_INFLIGHT`] frames
    /// behind, ends the connection.
    fn queue(&self, kind: httpd_ws_type_t, payload: Vec<u8>) -> bool {
        if !self.alive() {
            return false;
        }
        if self.state.inflight.fetch_add(1, Ordering::Relaxed) >= MAX_INFLIGHT {
            self.state.inflight.fetch_sub(1, Ordering::Relaxed);
            warn!(
                "api: socket {} is {MAX_INFLIGHT} frames behind, closing",
                self.fd
            );
            self.state.dead.store(true, Ordering::Relaxed);
            unsafe { httpd_sess_trigger_close(self.hd as httpd_handle_t, self.fd) };
            return false;
        }
        let job = Box::new(Job {
            hd: self.hd,
            fd: self.fd,
            generation: self.generation,
            kind,
            payload,
            state: self.state.clone(),
        });
        let arg = Box::into_raw(job) as *mut c_void;
        if unsafe { httpd_queue_work(self.hd as httpd_handle_t, Some(run_job), arg) } != ESP_OK {
            // SAFETY: not queued, so still ours.
            drop(unsafe { Box::from_raw(arg as *mut Job) });
            self.state.inflight.fetch_sub(1, Ordering::Relaxed);
            return false;
        }
        true
    }
}

impl SessionSink for WsSink {
    fn send(&mut self, text: &str) -> bool {
        self.queue(httpd_ws_type_t_HTTPD_WS_TYPE_TEXT, text.as_bytes().to_vec())
    }
    fn alive(&self) -> bool {
        !self.state.dead.load(Ordering::Relaxed)
            && is_current(self.hd as httpd_handle_t, self.fd, self.generation)
    }
    fn keepalive(&mut self) -> bool {
        self.queue(httpd_ws_type_t_HTTPD_WS_TYPE_PING, Vec::new())
    }
}

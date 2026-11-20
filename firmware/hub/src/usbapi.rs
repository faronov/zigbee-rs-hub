//! Hub-mode API over USB: the WebSocket JSON API's frames as JSON lines on
//! CDC 0 (one session while the host holds the port open, DTR asserted).
//! A test path that needs no network; the frames are the same.

use std::time::Duration;

use hub_app::{App, SessionSink};
use tbr_bridge::usb;

struct UsbSink;

impl SessionSink for UsbSink {
    fn send(&mut self, text: &str) -> bool {
        if !usb::bridge_open() {
            return false;
        }
        let mut line = Vec::with_capacity(text.len() + 1);
        line.extend_from_slice(text.as_bytes());
        line.push(b'\n');
        usb::write(usb::BRIDGE, &line, 200) == line.len()
    }
}

/// Runs the CDC 0 line API on its own thread.
pub fn start(app: App) {
    let _ = std::thread::Builder::new().stack_size(6144).spawn(move || {
        let mut session = None;
        let mut line = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            let open = usb::bridge_open();
            match (open, session) {
                (true, None) => {
                    line.clear();
                    session = app.connect(Box::new(UsbSink));
                }
                (false, Some(sid)) => {
                    app.disconnect(sid);
                    session = None;
                }
                _ => {}
            }
            let n = usb::read_wait(usb::BRIDGE, &mut buf, 100);
            let Some(sid) = session else {
                if n == 0 {
                    std::thread::sleep(Duration::from_millis(100));
                }
                continue;
            };
            for &b in &buf[..n] {
                match b {
                    b'\n' | b'\r' => {
                        if !line.is_empty() {
                            app.text(sid, String::from_utf8_lossy(&line).into_owned());
                            line.clear();
                        }
                    }
                    _ if line.len() < 16 * 1024 => line.push(b),
                    _ => line.clear(),
                }
            }
        }
    });
}

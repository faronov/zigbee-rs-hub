//! The hub's HTTP server (esp_http_server): the web UI (a pre-compressed
//! page and the tab modules it loads on demand, `crates/hub-web`), the web
//! tools from the catalog store (`/zdefc/…`) and the WebSocket JSON API on
//! `/api` ([`crate::ws`]). One server for every interface (it listens on
//! all addresses).

use embedded_svc::http::Method;
use embedded_svc::io::Write;
use esp_idf_svc::handle::RawHandle;
use esp_idf_svc::http::server::{self, EspHttpServer};
use esp_idf_svc::sys::EspError;
use hub_app::App;

/// Closes the request's socket once the response is out: idle keep-alive
/// sockets of page loads (often never closed by the browser) would make
/// the server purge the least recently used socket — an API client's.
pub fn close_after(req: *mut esp_idf_svc::sys::httpd_req_t) {
    unsafe {
        let fd = esp_idf_svc::sys::httpd_req_to_sockfd(req);
        esp_idf_svc::sys::httpd_sess_trigger_close((*req).handle, fd);
    }
}

/// The server of a running app: page, modules, web tools, `/api`.
pub fn api_server(app: App) -> Result<EspHttpServer<'static>, EspError> {
    let r = crate::resources::get();
    let mut server = EspHttpServer::new(&server::Configuration {
        stack_size: 8192,
        // The web UI (page + WebSocket) and integrations at once; page
        // requests close their socket (`close_after`). With PSRAM the
        // least recently used socket is purged when they run out;
        // without it a new connection is refused instead, so Home
        // Assistant keeps its session. 7 + the server's 3 =
        // CONFIG_LWIP_MAX_SOCKETS.
        max_open_sockets: r.http_sockets,
        lru_purge_enable: r.http_lru_purge,
        // Peers that vanished without a FIN.
        keep_alive: Some(server::KeepAlive { idle_secs: 30, interval_secs: 5, probe_count: 3 }),
        ..Default::default()
    })
    .map_err(|e| e.0)?;
    // The web UI: one pre-compressed page that talks to /api (crates/hub-web).
    server.fn_handler("/", Method::Get, |mut req| {
        close_after(req.connection().handle());
        if req.header("If-None-Match").is_some_and(hub_web::not_modified) {
            let headers = [("ETag", hub_web::ETAG), ("Cache-Control", "no-cache"), ("Connection", "close")];
            return req.into_response(304, Some("Not Modified"), &headers).map(|_| ());
        }
        let headers = [hub_web::HEADERS.as_slice(), &[("Connection", "close")]].concat();
        req.into_response(200, None, &headers)?.write_all(hub_web::INDEX_HTML_GZ).map(|_| ())
    })?;
    // The tabs the page loads when first opened, `/ui/<build>/<name>.js`: of
    // this build only (another build's path is not found, and a page left
    // open over an update reloads), cached for good; the developer tools
    // only while the hub's developer mode is on (asked again every time).
    for m in hub_web::MODULES {
        let app = app.clone();
        server.fn_handler(m.path, Method::Get, move |mut req| {
            close_after(req.connection().handle());
            if !m.served(app.developer_mode()) {
                let headers = [("Cache-Control", "no-store"), ("Connection", "close")];
                return req.into_response(403, Some("Forbidden"), &headers).map(|_| ());
            }
            if req.header("If-None-Match").is_some_and(|v| hub_web::matches(v, m.etag)) {
                let headers = [("ETag", m.etag), ("Cache-Control", m.cache_control()), ("Connection", "close")];
                return req.into_response(304, Some("Not Modified"), &headers).map(|_| ());
            }
            let headers = [m.headers().as_slice(), &[("Connection", "close")]].concat();
            let mut resp = req.into_response(200, None, &headers)?;
            for c in m.gz.chunks(4096) {
                resp.write_all(c)?;
            }
            Ok(())
        })?;
    }
    // Web tools from hubdata (the definition compiler of the web UI).
    for (uri, name) in [("/zdefc/zdefc_wasm.js", "zdefc_wasm.js"), ("/zdefc/zdefc_wasm_bg.wasm", "zdefc_wasm_bg.wasm")] {
        let web = app.web().clone();
        server.fn_handler(uri, Method::Get, move |mut req| {
            close_after(req.connection().handle());
            let fresh = req.header("If-None-Match").map(str::to_string);
            web.serve(name, |f| match f {
                None => req.into_status_response(404).map(|_| ()),
                Some(f) if fresh.as_deref() == Some(f.etag) => {
                    req.into_response(304, None, &[("ETag", f.etag), ("Connection", "close")]).map(|_| ())
                }
                Some(f) => {
                    let headers = [
                        ("Content-Type", f.content_type),
                        ("Content-Encoding", "gzip"),
                        ("Cache-Control", "no-cache"),
                        ("ETag", f.etag),
                        ("Connection", "close"),
                    ];
                    let mut resp = req.into_response(200, None, &headers)?;
                    for c in f.data.chunks(4096) {
                        resp.write_all(c)?;
                    }
                    Ok(())
                }
            })
        })?;
    }
    crate::ws::register(&server, app)?;
    Ok(server)
}

/// The server of a hub that did not start its app: the page says why
/// (`message`), the API answers 503 with `error`.
pub fn wrong_image_server(error: &'static str, message: String) -> Result<EspHttpServer<'static>, EspError> {
    let mut server = EspHttpServer::new(&server::Configuration { stack_size: 6144, max_open_sockets: 2, ..Default::default() })
        .map_err(|e| e.0)?;
    let page = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><title>zigbee-rs hub</title>\
         <h1>zigbee-rs hub: wrong firmware image</h1><p>{message}</p>"
    );
    server.fn_handler("/", Method::Get, move |mut req| {
        close_after(req.connection().handle());
        let headers = [("Content-Type", "text/html; charset=utf-8"), ("Cache-Control", "no-store"), ("Connection", "close")];
        req.into_response(200, None, &headers)?.write_all(page.as_bytes()).map(|_| ())
    })?;
    let body = serde_json::json!({"error": error, "message": message}).to_string();
    server.fn_handler("/api", Method::Get, move |mut req| {
        close_after(req.connection().handle());
        let headers = [("Content-Type", "application/json"), ("Connection", "close")];
        req.into_response(503, Some("Service Unavailable"), &headers)?.write_all(body.as_bytes()).map(|_| ())
    })?;
    Ok(server)
}

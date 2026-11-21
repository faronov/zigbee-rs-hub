//! The hub's embedded web UI (`web/index.html`): a single static page
//! (HTML, CSS and JS, no framework, no CDN) that talks only to the
//! WebSocket JSON API on `/api` (device-support 14). Served at `/` by the
//! firmware's esp_http_server and by `hubctl serve`, pre-compressed.
#![no_std]

/// The page, gzip-compressed at build time (`Content-Encoding: gzip`).
pub const INDEX_HTML_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/index.html.gz"));

/// Strong ETag of [`INDEX_HTML_GZ`], with its quotes.
pub const ETAG: &str = include_str!(concat!(env!("OUT_DIR"), "/etag"));

/// Response headers of the page (besides the status line).
pub const HEADERS: [(&str, &str); 4] = [
    ("Content-Type", "text/html; charset=utf-8"),
    ("Content-Encoding", "gzip"),
    ("Cache-Control", "no-cache"),
    ("ETag", ETAG),
];

/// Whether an `If-None-Match` header value matches the page (answer 304).
pub fn not_modified(if_none_match: &str) -> bool {
    if_none_match.trim() == "*" || if_none_match.split(',').any(|t| t.trim().trim_start_matches("W/") == ETAG)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::string::String;

    fn page() -> std::vec::Vec<u8> {
        let gz = INDEX_HTML_GZ;
        assert_eq!(&gz[..4], &[0x1f, 0x8b, 8, 0], "gzip, deflate, no optional fields");
        let body = &gz[10..gz.len() - 8];
        let html = miniz_oxide::inflate::decompress_to_vec(body).expect("deflate stream");
        let isize = u32::from_le_bytes(gz[gz.len() - 4..].try_into().unwrap());
        assert_eq!(isize as usize, html.len());
        html
    }

    #[test]
    fn embedded_page_is_the_source_and_small() {
        assert_eq!(page(), include_bytes!("../web/index.html"));
        assert!(INDEX_HTML_GZ.len() <= 25 * 1024, "{} B compressed", INDEX_HTML_GZ.len());
    }

    #[test]
    fn page_is_self_contained_and_uses_only_the_api() {
        let html = String::from_utf8(page()).unwrap();
        for external in ["http://", "https://", "src=\"//", "<link", "<script src", "import("] {
            assert!(!html.contains(external), "external reference {external:?}");
        }
        assert!(html.contains("+\"/api\""));
    }

    #[test]
    fn etag_and_conditional_requests() {
        assert!(ETAG.starts_with('"') && ETAG.ends_with('"') && ETAG.len() == 18);
        assert!(not_modified(ETAG));
        assert!(not_modified(&std::format!("\"x\", W/{ETAG}")));
        assert!(!not_modified("\"0000000000000000\""));
    }
}

//! The hub's embedded web UI (`web/index.html`): a single static page
//! (HTML, CSS and JS, no framework, no CDN) that talks only to the
//! WebSocket JSON API on `/api` (device-support 14). Served at `/` by the
//! firmware's esp_http_server and by `hubctl serve`, pre-compressed.
#![no_std]

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod minify;
#[cfg(test)]
mod variant;

/// The page, gzip-compressed at build time (`Content-Encoding: gzip`).
pub const INDEX_HTML_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/index.html.gz"));

/// The setup access point's page (`web/setup.html`, Wi-Fi credentials;
/// talks to `/setup/*`), gzip-compressed (`Content-Encoding: gzip`).
pub const SETUP_HTML_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/setup.html.gz"));

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
    use super::*;
    use std::string::{String, ToString};

    fn page() -> std::vec::Vec<u8> {
        gunzip(INDEX_HTML_GZ)
    }

    fn gunzip(gz: &[u8]) -> std::vec::Vec<u8> {
        assert_eq!(&gz[..4], &[0x1f, 0x8b, 8, 0], "gzip, deflate, no optional fields");
        let body = &gz[10..gz.len() - 8];
        let html = miniz_oxide::inflate::decompress_to_vec(body).expect("deflate stream");
        let isize = u32::from_le_bytes(gz[gz.len() - 4..].try_into().unwrap());
        assert_eq!(isize as usize, html.len());
        html
    }

    #[test]
    fn embedded_page_is_the_minified_source_and_small() {
        let src = include_str!("../web/index.html");
        let selected = super::variant::select(src, cfg!(feature = "advanced")).unwrap();
        assert_eq!(page(), super::minify::minify(&selected).unwrap().into_bytes());
        // Budget (approved for hub 0.2.0): 32 KiB; it counts in the firmware image.
        assert!(INDEX_HTML_GZ.len() <= 32 * 1024, "{} B compressed", INDEX_HTML_GZ.len());
    }

    /// The page's scripts, minified and not, parse (when Node is installed).
    #[test]
    fn minified_script_parses() {
        let src = include_str!("../web/index.html");
        let script = |html: &str| {
            let a = html.find("<script>").unwrap() + "<script>".len();
            html[a..a + html[a..].find("</script>").unwrap()].to_string()
        };
        let plain = super::variant::select(src, false).unwrap();
        let advanced = super::variant::select(src, true).unwrap();
        let min_plain = super::minify::minify(&plain).unwrap();
        let min_advanced = super::minify::minify(&advanced).unwrap();
        for js in [script(&plain), script(&advanced), script(&min_plain), script(&min_advanced)] {
            use std::io::Write;
            let Ok(mut node) = std::process::Command::new("node")
                .args(["-e", "new Function(require('fs').readFileSync(0,'utf8'))"])
                .stdin(std::process::Stdio::piped())
                .spawn()
            else {
                return;
            };
            node.stdin.take().unwrap().write_all(js.as_bytes()).unwrap();
            assert!(node.wait().unwrap().success(), "script does not parse");
        }
    }

    #[test]
    fn basic_page_omits_manual_console_but_keeps_normal_device_controls() {
        let src = include_str!("../web/index.html");
        let plain = super::variant::select(src, false).unwrap();
        let advanced = super::variant::select(src, true).unwrap();
        for item in ["data-tab=\"zcl\"", "zcl/read", "tuya/send", "zFrame(", "onTab.zcl"] {
            assert!(!plain.contains(item), "{item} remains in the basic page");
            assert!(advanced.contains(item), "{item} missing from the advanced page");
        }
        for item in ["entities/command", "data-tab=\"groups\"", "data-tab=\"conv\""] {
            assert!(plain.contains(item), "normal functionality lost: {item}");
        }
        assert!(super::variant::select("// advanced:start\nx\n", false).is_err());
    }

    #[test]
    fn minifier_keeps_code_and_html() {
        let html = "<p>\n  <b>x</b>\n</p>\n<style>\n a{b:c}\n\n d,\n e{f:g}\n</style>\n<script>\n// c\n let a=1,\n  b=\"//\";\n f(a,\n b)\n x=1 // y,\n z()\n</script>\n";
        let want = "<p>\n  <b>x</b>\n</p>\n<style>\na{b:c}d,e{f:g}\n</style>\n<script>\nlet a=1,b=\"//\";\nf(a,b)\nx=1 // y,\nz()\n</script>\n";
        assert_eq!(super::minify::minify(html).unwrap(), want);
        assert!(super::minify::minify("<script>\nlet s=`a\nb`\n</script>\n").is_err());
        assert!(super::minify::minify("<style>\na{b:\nc}\n</style>\n").is_err());
        assert!(super::minify::minify("<script>\nx\n").is_err());
    }

    #[test]
    fn setup_page_is_the_source_small_and_uses_only_the_setup_endpoints() {
        let html = gunzip(SETUP_HTML_GZ);
        assert_eq!(html, include_bytes!("../web/setup.html"));
        // 1.9 KB of page and 0.9 KB of SHA-256/HMAC (no WebCrypto on http://).
        assert!(SETUP_HTML_GZ.len() <= 3 * 1024, "{} B compressed", SETUP_HTML_GZ.len());
        let html = String::from_utf8(html).unwrap();
        for external in ["src=", "<link", "import("] {
            assert!(!html.contains(external), "external reference {external:?}");
        }
        let fetches: std::vec::Vec<&str> = html.split("fetch(\"").skip(1).map(|s| s.split('"').next().unwrap()).collect();
        assert_eq!(fetches, ["/setup/state", "/setup/scan", "/setup/wifi"]);
        // The page runs on the open AP: it never gets or sends the API token
        // (only a proof), nor the setup key (only its commitment).
        for leak in ["token=", ".token", "token:"] {
            assert!(!html.contains(leak), "{leak:?} on the setup page");
        }
        let body = r#"JSON.stringify({ssid,pass,nonce,proof:st.need_token?X(M($("tok").value.trim(),nonce+"\n"+ssid+"\n"+pass)):"",commit:X(H(E(key)))})"#;
        assert!(html.contains(body), "the credentials request carries only a proof and a commitment");
        assert!(html.contains("#claim="));
    }

    /// The setup page's SHA-256 and HMAC (its first script) give what the
    /// hub checks (when Node is installed).
    #[test]
    fn setup_page_proof_and_commitment_match_the_hub() {
        use hub_core::setup::{Claim, token_proof};
        use std::io::Write;
        let html = String::from_utf8(gunzip(SETUP_HTML_GZ)).unwrap();
        let a = html.find("<script>").unwrap() + "<script>".len();
        let crypto = &html[a..a + html[a..].find("</script>").unwrap()];
        let (token, nonce, ssid, pass, key) = ("0123456789abcdef01234567", "9f8e7d6c5b4a3210", "Home Wi-Fi ü", "pa ss\"word", "00ff00ff00ff00ff");
        let args = serde_args(&[token, nonce, ssid, pass, key]);
        let js = std::format!("{crypto}\nconst [t,n,s,p,k]={args};console.log(X(M(t,n+\"\\n\"+s+\"\\n\"+p))+\" \"+X(H(E(k))))");
        let Ok(mut node) = std::process::Command::new("node")
            .args(["-e", "eval(require('fs').readFileSync(0,'utf8'))"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
        else {
            return;
        };
        node.stdin.take().unwrap().write_all(js.as_bytes()).unwrap();
        let out = node.wait_with_output().unwrap();
        assert!(out.status.success(), "script failed");
        let hex = |b: [u8; 32]| -> String { b.iter().map(|x| std::format!("{x:02x}")).collect() };
        let want = std::format!(
            "{} {}\n",
            hex(token_proof(token, nonce, ssid, pass)),
            hex(Claim::commitment(key.as_bytes().try_into().unwrap()))
        );
        assert_eq!(String::from_utf8(out.stdout).unwrap(), want);
    }

    /// A JS array literal of strings.
    fn serde_args(items: &[&str]) -> String {
        let quoted: std::vec::Vec<String> = items
            .iter()
            .map(|s| {
                let esc: String = s.chars().flat_map(|c| match c {
                    '"' | '\\' => std::vec!['\\', c],
                    c => std::vec![c],
                }).collect();
                std::format!("\"{esc}\"")
            })
            .collect();
        std::format!("[{}]", quoted.join(","))
    }

    #[test]
    fn page_is_self_contained_and_uses_only_the_api() {
        // Device pictures are the one optional outside resource (an <img>,
        // switched off in the page's settings): Zigbee2MQTT's image folder.
        // The fallback icon's SVG namespace is a name, not a fetch.
        let html = String::from_utf8(page())
            .unwrap()
            .replacen("\"https://www.zigbee2mqtt.io/images/devices/\"", "", 1)
            .replace("xmlns=\"http://www.w3.org/2000/svg\"", "");
        for external in ["http://", "https://", "src=\"//", "<link", "<script src"] {
            assert!(!html.contains(external), "external reference {external:?}");
        }
        // Dynamic imports only of the hub's own web tools (`/zdefc/`, served
        // from hubdata).
        assert!(html.match_indices("import(").all(|(i, _)| html[i..].starts_with("import(\"/zdefc/")));
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

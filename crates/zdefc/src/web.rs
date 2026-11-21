//! Web tools bundles (`zdef::webtools`): gzip and content types.

/// Content type by file extension.
pub fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "wasm" => "application/wasm",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Deterministic gzip (no file name, mtime 0, best compression).
pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut gz = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 2, 0xff];
    gz.extend(miniz_oxide::deflate::compress_to_vec(data, 10));
    gz.extend(crc32(data).to_le_bytes());
    gz.extend((data.len() as u32).to_le_bytes());
    gz
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    #[test]
    fn bundle_round_trip() {
        let files = vec![
            (
                "zdefc_wasm.js".to_string(),
                super::content_type("zdefc_wasm.js").to_string(),
                super::gzip(b"export default 1;"),
            ),
            (
                "zdefc_wasm_bg.wasm".to_string(),
                super::content_type("x.wasm").to_string(),
                super::gzip(&[0, 0x61, 0x73, 0x6d]),
            ),
        ];
        let b = zdef::webtools::build(&files).unwrap();
        let p = zdef::webtools::Bundle::parse(&b).unwrap();
        p.verify_with::<zdef::hash::SoftSha256>().unwrap();
        let e = p.find("zdefc_wasm_bg.wasm").unwrap();
        assert_eq!(e.content_type, "application/wasm");
        let raw = miniz_oxide::inflate::decompress_to_vec(&e.data[10..e.data.len() - 8]).unwrap();
        assert_eq!(raw, [0, 0x61, 0x73, 0x6d]);
        let mut bad = b.clone();
        let n = bad.len();
        bad[n - 1] ^= 1;
        assert!(
            zdef::webtools::Bundle::parse(&bad)
                .unwrap()
                .verify_with::<zdef::hash::SoftSha256>()
                .is_err()
        );
        assert!(zdef::webtools::Bundle::parse(&b[..b.len() - 1]).is_err());
    }
}

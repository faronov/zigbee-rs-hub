//! `web/index.html` → `$OUT_DIR/index.html.gz` (deterministic gzip: no
//! file name, mtime 0, OS "unknown") and `$OUT_DIR/etag` (quoted strong
//! ETag from the SHA-256 of the compressed bytes); `web/setup.html` (the
//! setup access point's page) → `$OUT_DIR/setup.html.gz`. No node
//! toolchain: the pages are hand-written and only compressed.

use std::{env, fs, path::Path};

use sha2::{Digest, Sha256};

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn gzip(name: &str) -> Vec<u8> {
    let src = Path::new("web").join(name);
    println!("cargo:rerun-if-changed={}", src.display());
    let html = fs::read(&src).expect("page source");
    let mut gz = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 2, 0xff];
    gz.extend(miniz_oxide::deflate::compress_to_vec(&html, 10));
    gz.extend(crc32(&html).to_le_bytes());
    gz.extend((html.len() as u32).to_le_bytes());
    let out = env::var("OUT_DIR").unwrap();
    fs::write(Path::new(&out).join(format!("{name}.gz")), &gz).unwrap();
    gz
}

fn main() {
    gzip("setup.html");
    let gz = gzip("index.html");
    let out = env::var("OUT_DIR").unwrap();
    let digest = Sha256::digest(&gz);
    let etag: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    fs::write(Path::new(&out).join("etag"), format!("\"{etag}\"")).unwrap();
}

//! `web/index.html` and the tab modules `web/ui/*.html` → `$OUT_DIR`:
//! `index.html.gz` (deterministic gzip: no file name, mtime 0, OS
//! "unknown") with its quoted strong ETag `etag` (from the SHA-256 of the
//! compressed bytes), `ui/<name>.js.gz` for each module of the build, and
//! `ui.rs` (the build hash and the module table). No node toolchain: the
//! page is hand-written, minified line by line (`src/minify.rs`), split
//! and compressed (`src/bundle.rs`).

use std::{env, fs, path::Path};

#[path = "src/bundle.rs"]
mod bundle;
#[path = "src/minify.rs"]
mod minify;
#[path = "src/variant.rs"]
mod variant;

fn main() {
    for f in ["src/bundle.rs", "src/minify.rs", "src/variant.rs", "web/index.html"] {
        println!("cargo:rerun-if-changed={f}");
    }
    for m in bundle::SOURCES {
        println!("cargo:rerun-if-changed=web/ui/{m}.html");
    }
    let f = variant::Features {
        advanced: cfg!(feature = "advanced"),
        ota: cfg!(feature = "ota"),
        ota_store: cfg!(feature = "ota-store"),
        gateway: cfg!(feature = "gateway"),
        dev: cfg!(feature = "dev"),
    };
    let read = |name: &str| fs::read_to_string(Path::new("web").join(name)).unwrap_or_else(|e| panic!("web/{name}: {e}"));
    let b = bundle::bundle(f, &read).unwrap_or_else(|e| panic!("web/{e}"));
    let out = env::var("OUT_DIR").unwrap();
    let out = Path::new(&out);
    let gz = bundle::gzip(b.page.as_bytes());
    fs::write(out.join("index.html.gz"), &gz).unwrap();
    fs::write(out.join("etag"), bundle::etag(&gz)).unwrap();
    fs::create_dir_all(out.join("ui")).unwrap();
    let mut rs = format!("/// The build hash of the page and its modules.\npub const BUILD: &str = \"{}\";\n\n", b.build);
    rs.push_str("/// The tab modules of this build.\npub const MODULES: &[Module] = &[\n");
    for m in &b.modules {
        let gz = bundle::gzip(m.js.as_bytes());
        fs::write(out.join(format!("ui/{}.js.gz", m.name)), &gz).unwrap();
        rs.push_str(&format!(
            "    Module {{ name: \"{n}\", path: \"/ui/{b}/{n}.js\", gz: include_bytes!(concat!(env!(\"OUT_DIR\"), \"/ui/{n}.js.gz\")), etag: {e:?}, gate: Gate::{g} }},\n",
            n = m.name,
            b = b.build,
            e = bundle::etag(&gz),
            g = if m.developer { "DeveloperMode" } else { "Always" },
        ));
    }
    rs.push_str("];\n");
    fs::write(out.join("ui.rs"), rs).unwrap();
}

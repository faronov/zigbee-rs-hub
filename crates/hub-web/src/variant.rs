use std::string::String;
use std::vec::Vec;

/// Feature sections of the page and its modules, `<!-- NAME:start -->` …
/// `<!-- NAME:end -->` (or `// NAME:start` in a script): `advanced` (the
/// manual ZCL/Tuya console), `ota` (the Updates tab: Zigbee device OTA),
/// and inside it `store` (uploads into the OTA image store) and `stream`
/// (serving an OTA file from the browser tab, for images without a store);
/// `gateway` (the gateway firmware's storage claim, firmware update and
/// Ethernet); `dev` (the developer edition: its tools always on) and
/// `devmode` (`advanced` without `dev`: the console behind the hub's
/// developer mode, Settings › Hub).
pub const SECTIONS: [&str; 7] = ["advanced", "ota", "store", "stream", "gateway", "dev", "devmode"];

/// The page features of a build (the crate's cargo features).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Features {
    pub advanced: bool,
    pub ota: bool,
    pub ota_store: bool,
    pub gateway: bool,
    pub dev: bool,
}

impl Features {
    /// Every distinct combination: `ota_store` implies `ota`, `dev`
    /// implies `advanced`.
    #[allow(dead_code)]
    pub fn all() -> Vec<Features> {
        let mut v = Vec::new();
        for gateway in [false, true] {
            for (advanced, dev) in [(false, false), (true, false), (true, true)] {
                for (ota, ota_store) in [(false, false), (true, false), (true, true)] {
                    v.push(Features { advanced, ota, ota_store, gateway, dev });
                }
            }
        }
        v
    }

    /// The sections a build includes.
    pub fn sections(self) -> [(&'static str, bool); 7] {
        let ota = self.ota || self.ota_store;
        let advanced = self.advanced || self.dev;
        [
            ("advanced", advanced),
            ("ota", ota),
            ("store", self.ota_store),
            ("stream", ota && !self.ota_store),
            ("gateway", self.gateway),
            ("dev", self.dev),
            ("devmode", advanced && !self.dev),
        ]
    }
}

/// The page with the sections of `on` kept and the others left out.
pub fn select(src: &str, on: &[(&str, bool)]) -> Result<String, &'static str> {
    let mut out = String::with_capacity(src.len());
    // Open sections, outermost first; a line is kept when all are.
    let mut open: Vec<(&str, bool)> = Vec::new();
    for line in src.lines() {
        let t = line.trim();
        let marker = t
            .strip_prefix("<!-- ")
            .and_then(|m| m.strip_suffix(" -->"))
            .or_else(|| t.strip_prefix("// "))
            .and_then(|m| m.rsplit_once(':'))
            .filter(|(name, _)| SECTIONS.contains(name));
        match marker {
            Some((name, "start")) => {
                if open.iter().any(|(n, _)| *n == name) {
                    return Err("feature section inside itself");
                }
                let keep = on.iter().find(|(n, _)| *n == name).ok_or("feature section not selected")?.1;
                open.push((name, keep));
            }
            Some((name, "end")) => {
                if open.last().is_none_or(|(n, _)| *n != name) {
                    return Err("unexpected feature section end");
                }
                open.pop();
            }
            _ if open.iter().all(|(_, keep)| *keep) => {
                out.push_str(line);
                out.push('\n');
            }
            _ => {}
        }
    }
    if !open.is_empty() {
        return Err("unterminated feature section");
    }
    Ok(out)
}

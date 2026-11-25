use std::string::String;
use std::vec::Vec;

/// Feature sections of the page, `<!-- NAME:start -->` … `<!-- NAME:end -->`
/// (or `// NAME:start` in the script): `advanced` (the manual ZCL/Tuya
/// console), `ota` (the Updates tab: Zigbee device OTA), and inside it
/// `store` (uploads into the OTA image store) and `stream` (serving an OTA
/// file from the browser tab, for images without a store); `gateway` (the
/// gateway firmware's storage claim, firmware update and Ethernet).
pub const SECTIONS: [&str; 5] = ["advanced", "ota", "store", "stream", "gateway"];

/// The sections a build of the S3 hub includes (`ota_store` implies `ota`;
/// the build script uses [`enabled_with`]).
#[allow(dead_code)]
pub fn enabled(advanced: bool, ota: bool, ota_store: bool) -> [(&'static str, bool); 5] {
    enabled_with(advanced, ota, ota_store, false)
}

/// The sections a build includes, `gateway` too.
pub fn enabled_with(advanced: bool, ota: bool, ota_store: bool, gateway: bool) -> [(&'static str, bool); 5] {
    let ota = ota || ota_store;
    [("advanced", advanced), ("ota", ota), ("store", ota_store), ("stream", ota && !ota_store), ("gateway", gateway)]
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

use std::string::String;

/// Feature sections of the page, `<!-- NAME:start -->` … `<!-- NAME:end -->`
/// (or `// NAME:start` in the script): `advanced` (the manual ZCL/Tuya
/// console), `store` (uploads into the OTA image store) and `stream`
/// (serving an OTA file from the browser tab, for images without a store).
pub const SECTIONS: [&str; 3] = ["advanced", "store", "stream"];

/// The sections a build includes.
pub fn enabled(advanced: bool, ota_store: bool) -> [(&'static str, bool); 3] {
    [("advanced", advanced), ("store", ota_store), ("stream", !ota_store)]
}

/// The page with the sections of `on` kept and the others left out.
pub fn select(src: &str, on: &[(&str, bool)]) -> Result<String, &'static str> {
    let mut out = String::with_capacity(src.len());
    let mut inside: Option<(&str, bool)> = None;
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
                if inside.is_some() {
                    return Err("nested feature section");
                }
                let keep = on.iter().find(|(n, _)| *n == name).ok_or("feature section not selected")?.1;
                inside = Some((name, keep));
            }
            Some((name, "end")) => {
                if inside.is_none_or(|(n, _)| n != name) {
                    return Err("unexpected feature section end");
                }
                inside = None;
            }
            _ if inside.is_none_or(|(_, keep)| keep) => {
                out.push_str(line);
                out.push('\n');
            }
            _ => {}
        }
    }
    if inside.is_some() {
        return Err("unterminated feature section");
    }
    Ok(out)
}

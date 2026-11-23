use std::string::String;

pub fn select(src: &str, advanced: bool) -> Result<String, &'static str> {
    let mut out = String::with_capacity(src.len());
    let mut inside = false;
    for line in src.lines() {
        match line.trim() {
            "<!-- advanced:start -->" | "// advanced:start" => {
                if inside {
                    return Err("nested advanced section");
                }
                inside = true;
            }
            "<!-- advanced:end -->" | "// advanced:end" => {
                if !inside {
                    return Err("unexpected advanced section end");
                }
                inside = false;
            }
            _ if advanced || !inside => {
                out.push_str(line);
                out.push('\n');
            }
            _ => {}
        }
    }
    if inside {
        return Err("unterminated advanced section");
    }
    Ok(out)
}

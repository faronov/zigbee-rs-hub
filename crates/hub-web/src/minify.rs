//! Build-time minifier of `web/index.html` (shared by `build.rs` and the
//! tests). Line based and conservative, for the page's hand-written style:
//! inside `<script>` and `<style>` it drops indentation, blank lines and
//! whole-line `//` comments, and joins a script line to the previous one
//! when that one ends in `;`, `{`, `,` or `(` (no automatic semicolon can
//! be inserted there) and has no `//` (which could start a trailing
//! comment). The HTML outside is kept as is. A script line with an odd
//! number of backticks (a template literal spanning lines, whose text
//! would change) is refused.

use std::string::String;

/// The minified page.
pub fn minify(html: &str) -> Result<String, String> {
    #[derive(PartialEq)]
    enum In {
        Html,
        Script,
        Style,
    }
    let mut out = String::with_capacity(html.len());
    let mut mode = In::Html;
    // Whether the last line written may take the next one appended.
    let mut joinable = false;
    for (n, line) in html.lines().enumerate() {
        let t = line.trim();
        match mode {
            In::Html => {
                out.push_str(line);
                out.push('\n');
                if t == "<script>" {
                    mode = In::Script;
                } else if t == "<style>" {
                    mode = In::Style;
                }
                joinable = true;
                continue;
            }
            _ if t == "</script>" || t == "</style>" => {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(line);
                out.push('\n');
                mode = In::Html;
                continue;
            }
            _ if t.is_empty() => continue,
            In::Script if t.starts_with("//") => continue,
            In::Script => {
                if t.bytes().filter(|&b| b == b'`').count() % 2 == 1 {
                    return Err(std::format!("line {}: template literal across lines", n + 1));
                }
                if t.contains("/*") {
                    return Err(std::format!("line {}: block comment", n + 1));
                }
                if !joinable {
                    out.push('\n');
                }
                out.push_str(t);
                joinable = t.ends_with([';', '{', ',', '(']) && !t.contains("//");
            }
            In::Style => {
                // A line break between rules (or after a `{`, `;` or `,`)
                // separates nothing.
                if t.contains("/*") || !t.ends_with(['}', '{', ';', ',']) {
                    return Err(std::format!("line {}: CSS comment or a declaration across lines", n + 1));
                }
                out.push_str(t);
            }
        }
    }
    if mode != In::Html {
        return Err("unterminated <script> or <style>".into());
    }
    Ok(out)
}

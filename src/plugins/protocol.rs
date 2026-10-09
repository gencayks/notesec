//! What goes into and comes out of a plugin call (docs/PLUGINS.md).
//!
//! Input: TOML written by `encode` (basic strings, keys in a fixed order,
//! `block` first). Output: TOML, parsed and checked here; nothing a
//! plugin says is applied unchecked.

use serde::Deserialize;

pub const MAX_ACTIONS: usize = 16;
pub const MAX_TEXT: usize = 64 << 10;
const MAX_STATUS: usize = 200;
pub const MAX_RENDER_TEXT: usize = 8 << 10;
const MAX_RENDER_LINES: usize = 50;

/// A TOML basic string with every special character escaped.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `key = "value"` lines, in this order.
pub fn encode(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(k, v)| format!("{k} = {}\n", quote(v)))
        .collect()
}

/// A command's input: the block text first, then the rest.
pub fn command_input(command: &str, page: &str, block: &str, selection: &str) -> String {
    encode(&[
        ("block", block),
        ("command", command),
        ("page", page),
        ("selection", selection),
        ("api_version", "1"),
    ])
}

/// A render hook's input.
pub fn render_input(args: &str, block: &str) -> String {
    encode(&[("block", block), ("args", args), ("api_version", "1")])
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// A new block after the current one (or at the page's end).
    InsertBlock(String),
    /// The current block's new text (needs a block being edited).
    ReplaceBlock(String),
    SetStatus(String),
    OpenPage(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOutput {
    #[serde(default)]
    actions: Vec<RawAction>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAction {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

fn text_ok(s: &str, max: usize) -> Result<(), String> {
    if s.len() > max {
        return Err(format!("text longer than {max} bytes"));
    }
    if s.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
        return Err("control characters in text".into());
    }
    Ok(())
}

/// A command's output, checked.
pub fn parse_actions(bytes: &[u8]) -> Result<Vec<Action>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "output isn't UTF-8".to_string())?;
    let raw: RawOutput =
        toml::from_str(text).map_err(|err| format!("bad output: {}", err.message()))?;
    if raw.actions.len() > MAX_ACTIONS {
        return Err(format!("more than {MAX_ACTIONS} actions"));
    }
    raw.actions
        .into_iter()
        .map(|a| {
            let need = |v: Option<String>, field: &str| {
                v.ok_or_else(|| format!("action {} needs {field}", a.kind))
            };
            let action = match a.kind.as_str() {
                "insert_block" => Action::InsertBlock(need(a.text, "text")?),
                "replace_block" => Action::ReplaceBlock(need(a.text, "text")?),
                "set_status" => Action::SetStatus(need(a.text, "text")?),
                "open_page" => Action::OpenPage(need(a.title, "title")?),
                other => return Err(format!("unknown action \u{201c}{other}\u{201d}")),
            };
            match &action {
                Action::InsertBlock(t) | Action::ReplaceBlock(t) => text_ok(t, MAX_TEXT)?,
                Action::SetStatus(t) | Action::OpenPage(t) => {
                    text_ok(t, MAX_STATUS)?;
                    if t.contains('\n') || t.trim().is_empty() {
                        return Err("status and titles are one non-empty line".into());
                    }
                }
            }
            Ok(action)
        })
        .collect()
}

/// One line of a render hook's output: plain, `*italic*` or `**bold**`
/// (the whole line). Never markup or HTML.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRender {
    text: String,
}

/// A render hook's output (`text = "..."`), checked and split in lines.
pub fn parse_render(bytes: &[u8]) -> Result<Vec<Line>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "output isn't UTF-8".to_string())?;
    let raw: RawRender =
        toml::from_str(text).map_err(|err| format!("bad output: {}", err.message()))?;
    text_ok(&raw.text, MAX_RENDER_TEXT)?;
    let lines: Vec<&str> = raw.text.lines().collect();
    if lines.len() > MAX_RENDER_LINES {
        return Err(format!("more than {MAX_RENDER_LINES} lines"));
    }
    Ok(lines
        .into_iter()
        .map(|l| {
            let wrapped = |m: &str| l.len() > 2 * m.len() && l.starts_with(m) && l.ends_with(m);
            if wrapped("**") {
                Line {
                    text: l[2..l.len() - 2].to_string(),
                    bold: true,
                    italic: false,
                }
            } else if wrapped("*") || wrapped("_") {
                Line {
                    text: l[1..l.len() - 1].to_string(),
                    bold: false,
                    italic: true,
                }
            } else {
                Line {
                    text: l.to_string(),
                    bold: false,
                    italic: false,
                }
            }
        })
        .collect())
}

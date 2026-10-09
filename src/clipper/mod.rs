//! The web clipper (decision 51): a tiny HTTP endpoint on 127.0.0.1 that
//! takes a page (title, address, HTML) from a bookmarklet or a browser
//! extension and hands it to the app, which saves it as a new page.
//! Off unless turned on in Settings. The request format and the threat
//! model are in `docs/WEB_CLIPPER.md`.
//!
//! No GPUI here: `server` runs the listener on its own threads and passes
//! each accepted clip through a channel (`Delivery`); the app saves it on
//! the UI thread and answers through the delivery's reply channel.

pub mod html;
pub mod http;
mod server;

use std::collections::HashMap;
use std::sync::mpsc;

use crate::import::markdown::{page_from_rows, Row};
use crate::model::Page;
pub use server::Server;

pub const DEFAULT_PORT: u16 = 27183;
/// The one path; everything else is 404.
pub const PATH: &str = "/clip";
pub const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_TITLE_INPUT: usize = 4096;
pub const MAX_URL_INPUT: usize = 4096;
/// The header extensions may send the token in (or the `token` field).
pub const TOKEN_HEADER: &str = "x-notesec-token";

/// A clip, checked and converted, ready to be saved.
#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
    /// Cleaned for a page name (not yet made unique).
    pub title: String,
    /// The page's http(s) address, cleaned.
    pub url: Option<String>,
    /// The content as outline rows (depth, block markdown).
    pub rows: Vec<(usize, String)>,
    /// The content was cut at the output caps.
    pub truncated: bool,
}

/// A clip on its way to the app, and where to say what became of it:
/// `Ok(page title)` or `Err(why)`.
pub struct Delivery {
    pub clip: Clip,
    pub reply: mpsc::Sender<Result<String, String>>,
}

/// A new random token: 64 hex digits from two v4 UUIDs (the `uuid` crate
/// reads the OS random source), 244 random bits.
pub fn new_token() -> String {
    let mut token = String::with_capacity(64);
    for _ in 0..2 {
        for b in uuid::Uuid::new_v4().as_bytes() {
            token.push_str(&format!("{b:02x}"));
        }
    }
    token
}

/// `given == expected` in time that doesn't depend on where they differ
/// (only on `expected`'s length, which isn't secret). An empty expected
/// token matches nothing.
pub fn tokens_match(given: &str, expected: &str) -> bool {
    let (a, b) = (given.as_bytes(), expected.as_bytes());
    let mut diff = u8::from(a.len() != b.len() || b.is_empty());
    for (i, &y) in b.iter().enumerate() {
        diff |= a.get(i).copied().unwrap_or(0) ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// A page name from a clipped page's title: one line, no `[` `]` (it goes
/// inside `[[…]]`) or `/` (namespaces), no leading dot, fitting our file
/// names; from the address's host when there is no title.
pub fn clip_title(raw: &str, url: Option<&str>) -> String {
    let mut title: String = raw
        .chars()
        .map(|c| match c {
            '[' => '(',
            ']' => ')',
            '/' | '\\' => '-',
            c if c.is_control() || c.is_whitespace() => ' ',
            c => c,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    title = title.trim_start_matches(['.', ' ']).to_string();
    if title.is_empty() {
        title = url
            .and_then(|u| u.split("://").nth(1))
            .and_then(|rest| rest.split(['/', '?', '#']).next())
            .map(|host| host.trim_start_matches("www.").to_string())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "Clipped page".to_string());
    }
    crate::import::clean_title(&title)
}

/// `wanted`, or `wanted (2)`, `wanted (3)`…: the first that `taken` says
/// is free.
pub fn unique_title(wanted: &str, taken: impl Fn(&str) -> bool) -> String {
    let mut title = wanted.to_string();
    let mut n = 2;
    while taken(&title) {
        title = format!("{wanted} ({n})");
        n += 1;
    }
    title
}

/// Check the request's fields and convert the HTML (on the connection's
/// thread, not the UI's).
pub fn clip_from_fields(fields: &HashMap<String, String>) -> Result<Clip, &'static str> {
    let get = |k: &str| fields.get(k).map(String::as_str).unwrap_or("");
    let (raw_title, raw_url, html) = (get("title"), get("url"), get("html"));
    if raw_title.len() > MAX_TITLE_INPUT {
        return Err("title too long");
    }
    if raw_url.len() > MAX_URL_INPUT {
        return Err("url too long");
    }
    if raw_title.trim().is_empty() && raw_url.trim().is_empty() && html.trim().is_empty() {
        return Err("nothing to clip: send title, url and html");
    }
    let url = html::clean_url(raw_url, None).filter(|u| !u.starts_with("mailto:"));
    let (rows, truncated) = html::to_rows(html, url.as_deref());
    Ok(Clip {
        title: clip_title(raw_title, url.as_deref()),
        url,
        rows,
        truncated,
    })
}

/// The page for `clip`, titled `title`, clipped on the journal day `day`:
/// the first block holds `source::`, `clipped::` and `tags:: #clipped`
/// (decision 44's form), then the content.
pub fn clip_page(clip: &Clip, title: &str, day: &str) -> Page {
    let mut props = Vec::new();
    if let Some(url) = &clip.url {
        props.push(format!("source:: {url}"));
    }
    props.push(format!("clipped:: [[{day}]]"));
    props.push("tags:: #clipped".to_string());
    let mut rows: Vec<Row> = vec![(0, props.join("\n"), None)];
    rows.extend(clip.rows.iter().map(|(d, t)| (*d, t.clone(), None)));
    if clip.truncated {
        rows.push((
            0,
            "*(Clip shortened: the page was longer than NoteSec keeps.)*".to_string(),
            None,
        ));
    }
    page_from_rows(title, false, rows)
}

/// The bookmarklet: a `javascript:` URL that posts the page (or the
/// selection, when there is one) as a form to the clipper, in a small new
/// window. The readable source is in `docs/WEB_CLIPPER.md`.
pub fn bookmarklet(port: u16, token: &str) -> String {
    // The token is hex, so it can't break out of the string.
    let token: String = token.chars().filter(char::is_ascii_hexdigit).collect();
    format!(
        "javascript:(function(){{var s=getSelection(),h='',d,i,f,k,e,w='notesec'+Date.now();\
if(s&&s.rangeCount&&!s.isCollapsed){{d=document.createElement('div');\
for(i=0;i<s.rangeCount;i++)d.appendChild(s.getRangeAt(i).cloneContents());h=d.innerHTML}}\
else{{h=(document.querySelector('article')||document.querySelector('main')||document.body).innerHTML}}\
var v={{token:'{token}',title:document.title,url:location.href,html:h.slice(0,1500000)}};\
window.open('about:blank',w,'width=420,height=260');\
f=document.createElement('form');f.method='post';f.action='http://127.0.0.1:{port}{PATH}';\
f.target=w;f.acceptCharset='utf-8';\
for(k in v){{e=document.createElement('input');e.type='hidden';e.name=k;e.value=v[k];f.appendChild(e)}}\
document.body.appendChild(f);f.submit();f.remove()}})()"
    )
}

#[cfg(test)]
mod tests;

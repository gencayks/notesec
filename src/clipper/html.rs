//! A clipped page's HTML as outline rows (depth, block markdown), without
//! running or fetching anything: a small tokenizer and a one-pass
//! converter. Only text and a few structures survive: headings,
//! paragraphs, lists, quotes, code, tables (a row per block), links
//! (http, https, mailto) and bold / italic. Everything else is dropped
//! with its content: scripts, styles, frames, forms, embedded objects,
//! SVG, templates, navigation. No attribute is kept but `href` and `src`
//! (so no event handler or style), and images stay links to their
//! address: nothing is downloaded.
//!
//! The text is defanged for our markdown (`defang`): it can't make links,
//! block references, macros, properties (a fake `id::` would hijack
//! block references), bullets or headings that weren't in the page.

/// Output caps: a clip longer than this is cut, with a note.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
pub const MAX_BLOCKS: usize = 5000;

/// Elements dropped with everything inside them.
const SKIP: &[&str] = &[
    "head",
    "script",
    "style",
    "noscript",
    "template",
    "iframe",
    "frame",
    "frameset",
    "object",
    "embed",
    "applet",
    "form",
    "button",
    "select",
    "textarea",
    "input",
    "option",
    "svg",
    "math",
    "canvas",
    "video",
    "audio",
    "picture",
    "map",
    "nav",
    "dialog",
    "title",
    "xmp",
    "noembed",
    "noframes",
    "plaintext",
];
/// Elements whose content isn't HTML: skipped up to their end tag.
const RAW: &[&str] = &[
    "script", "style", "textarea", "title", "xmp", "noembed", "noframes", "iframe", "noscript",
];
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];
/// Elements that start and end a block.
const BLOCK: &[&str] = &[
    "p",
    "div",
    "section",
    "article",
    "main",
    "header",
    "footer",
    "aside",
    "figure",
    "figcaption",
    "address",
    "details",
    "summary",
    "dl",
    "dt",
    "dd",
    "table",
    "thead",
    "tbody",
    "tfoot",
    "caption",
    "hr",
    "center",
    "fieldset",
    "legend",
    "body",
    "html",
];

#[derive(Debug, PartialEq)]
enum Token<'a> {
    Text(&'a str),
    Start {
        name: String,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    End(String),
}

/// The tokens of `html`. Raw-text elements (`<script>`…) come out as a
/// start tag directly followed by their end tag: their content is never
/// looked at.
fn tokens(html: &str) -> Vec<Token<'_>> {
    let bytes = html.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut text_from = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let next = bytes.get(i + 1).copied().unwrap_or(0);
        let tag_end = if next.is_ascii_alphabetic()
            || (next == b'/' && bytes.get(i + 2).is_some_and(u8::is_ascii_alphabetic))
        {
            tag_close(bytes, i + 1)
        } else if html[i..].starts_with("<!--") {
            Some(
                html[i + 4..]
                    .find("-->")
                    .map_or(bytes.len(), |e| i + 4 + e + 3),
            )
        } else if next == b'!' || next == b'?' {
            Some(html[i..].find('>').map_or(bytes.len(), |e| i + e + 1))
        } else {
            None
        };
        let Some(end) = tag_end else {
            i += 1;
            continue;
        };
        if text_from < i {
            out.push(Token::Text(&html[text_from..i]));
        }
        let inner = &html[i + 1..end.saturating_sub(1).max(i + 1)];
        i = end;
        text_from = end;
        if next == b'!' || next == b'?' {
            continue;
        }
        if let Some(name) = inner.strip_prefix('/') {
            out.push(Token::End(tag_name(name)));
            continue;
        }
        let name = tag_name(inner);
        let self_closing = inner.trim_end().ends_with('/');
        let attrs = attributes(&inner[name.len().min(inner.len())..]);
        let raw = RAW.contains(&name.as_str()) && !self_closing;
        out.push(Token::Start {
            name: name.clone(),
            attrs,
            self_closing,
        });
        if raw || name == "plaintext" {
            // Skip to `</name` (any case), then past its `>`.
            let close = format!("</{name}");
            let rest = html[i..].to_ascii_lowercase();
            let stop = if name == "plaintext" {
                None
            } else {
                rest.find(&close)
            };
            match stop {
                Some(at) => {
                    let after = i + at;
                    let gt = html[after..]
                        .find('>')
                        .map_or(bytes.len(), |e| after + e + 1);
                    i = gt;
                }
                None => i = bytes.len(),
            }
            text_from = i;
            out.push(Token::End(name));
        }
    }
    if text_from < bytes.len() {
        out.push(Token::Text(&html[text_from..]));
    }
    out
}

/// The index after the `>` closing the tag that starts at `from` (after
/// its `<`), quotes respected. `None` if it never closes.
fn tag_close(bytes: &[u8], from: usize) -> Option<usize> {
    let mut quote = 0u8;
    for (k, &b) in bytes.iter().enumerate().skip(from) {
        match (quote, b) {
            (0, b'"' | b'\'') => quote = b,
            (0, b'>') => return Some(k + 1),
            (q, b) if q != 0 && b == q => quote = 0,
            _ => {}
        }
    }
    None
}

fn tag_name(inner: &str) -> String {
    inner
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == ':')
        .collect::<String>()
        .to_ascii_lowercase()
}

/// `name="value"`, `name='value'`, `name=value`, `name` (lowercase names,
/// entity-decoded values).
fn attributes(text: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && (chars[i].is_whitespace() || chars[i] == '/') {
            i += 1;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() && !matches!(chars[i], '=' | '/' | '>') {
            i += 1;
        }
        if start == i {
            i += 1;
            continue;
        }
        let name: String = chars[start..i]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase();
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < chars.len() && chars[i] == '=' {
            i += 1;
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            if i < chars.len() && (chars[i] == '"' || chars[i] == '\'') {
                let q = chars[i];
                i += 1;
                let s = i;
                while i < chars.len() && chars[i] != q {
                    i += 1;
                }
                value = chars[s..i].iter().collect();
                i += 1;
            } else {
                let s = i;
                while i < chars.len() && !chars[i].is_whitespace() {
                    i += 1;
                }
                value = chars[s..i].iter().collect();
            }
        }
        attrs.push((name, decode_entities(&value)));
    }
    attrs
}

/// `&amp;`, `&#39;`, `&#x27;` and the common named entities; anything
/// else stays as written.
pub fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let end = rest[1..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '#'))
            .map(|e| e + 1)
            .unwrap_or(rest.len());
        let name = &rest[1..end];
        let decoded = if let Some(num) = name.strip_prefix('#') {
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => num.parse::<u32>().ok(),
            };
            code.map(|c| match char::from_u32(c) {
                Some(ch) if c != 0 => ch,
                _ => '\u{fffd}',
            })
        } else {
            named_entity(name)
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                let skip = if rest[end..].starts_with(';') {
                    end + 1
                } else {
                    end
                };
                rest = &rest[skip..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn named_entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" | "AMP" => '&',
        "lt" | "LT" => '<',
        "gt" | "GT" => '>',
        "quot" | "QUOT" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "ndash" => '–',
        "mdash" => '—',
        "hellip" => '…',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "laquo" => '«',
        "raquo" => '»',
        "middot" => '·',
        "bull" => '•',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "deg" => '°',
        "times" => '×',
        "euro" => '€',
        "pound" => '£',
        "shy" => '\u{ad}',
        "zwj" => '\u{200d}',
        "zwnj" => '\u{200c}',
        _ => return None,
    })
}

const ZWSP: char = '\u{200b}';

/// A line of clipped text made inert for our markdown: a zero-width space
/// breaks `[[`, `((` and `{{`, and goes before a line start that would
/// read as a property (`key:: value`), a bullet (`- `), a heading or a
/// quote. Code lines get only the line-start part (`code_line`).
pub fn defang(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut prev = '\0';
    for c in line.chars() {
        if (c == '[' || c == '(' || c == '{') && prev == c {
            out.push(ZWSP);
        }
        out.push(c);
        prev = c;
    }
    defang_start(out)
}

/// A code line that can't read as a bullet or property when the page is
/// loaded again (a block's later lines are stored indented under it).
pub fn code_line(line: &str) -> String {
    let t = line.trim_start();
    if t.starts_with("- ")
        || t == "-"
        || t.starts_with("* ")
        || crate::publish::property(t).is_some()
    {
        let indent = line.len() - t.len();
        format!("{}{ZWSP}{}", &line[..indent], t)
    } else {
        line.to_string()
    }
}

fn defang_start(line: String) -> String {
    let t = line.trim_start();
    let risky = t.starts_with("- ")
        || t == "-"
        || t.starts_with("* ")
        || t.starts_with('#')
        || t.starts_with('>')
        || t.starts_with("```")
        || t.starts_with("~~~")
        || t.starts_with("TODO ")
        || t.starts_with("DONE ")
        || crate::publish::property(t).is_some();
    if risky {
        format!("{ZWSP}{t}")
    } else {
        line
    }
}

/// `href` / `src` made absolute against the clipped page's address, kept
/// only for http, https and mailto. Control characters (tab, newline) are
/// removed first, as browsers do (so `java\tscript:` must not slip
/// through), and characters that would end or break our link syntax are
/// percent-encoded.
pub fn clean_url(href: &str, base: Option<&str>) -> Option<String> {
    // As browsers do: tabs, newlines (and other control characters) are
    // dropped anywhere, spaces at the ends trimmed; inner spaces encoded.
    let href: String = href.trim().chars().filter(|c| !c.is_control()).collect();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let lower = href.to_ascii_lowercase();
    let absolute = if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
    {
        href
    } else if lower.contains(':')
        && lower.find(':') < lower.find(['/', '?', '#']).or(Some(usize::MAX))
    {
        // Another scheme (javascript:, data:, file:…).
        return None;
    } else {
        resolve(&href, base?)?
    };
    let lower = absolute.to_ascii_lowercase();
    if !(lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:"))
    {
        return None;
    }
    let mut out = String::with_capacity(absolute.len());
    for c in absolute.chars() {
        match c {
            ' ' | '"' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\' | '^' | '`' => {
                out.push_str(&format!("%{:02X}", c as u32))
            }
            _ => out.push(c),
        }
    }
    (out.len() <= 4096).then_some(out)
}

/// A relative reference against an http(s) base: `//host/x`, `/x`, `x`,
/// `?q`.
fn resolve(rel: &str, base: &str) -> Option<String> {
    let lower = base.to_ascii_lowercase();
    let scheme_len = if lower.starts_with("https://") {
        8
    } else if lower.starts_with("http://") {
        7
    } else {
        return None;
    };
    let after = &base[scheme_len..];
    let host_end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let origin = &base[..scheme_len + host_end];
    if let Some(rest) = rel.strip_prefix("//") {
        return Some(format!("{}{rest}", &base[..scheme_len]));
    }
    if rel.starts_with('/') {
        return Some(format!("{origin}{rel}"));
    }
    let path = &after[host_end..];
    let path = &path[..path.find(['?', '#']).unwrap_or(path.len())];
    if rel.starts_with('?') {
        return Some(format!("{origin}{path}{rel}"));
    }
    let dir = &path[..path.rfind('/').map_or(0, |s| s + 1)];
    let dir = if dir.is_empty() { "/" } else { dir };
    Some(format!("{origin}{dir}{rel}"))
}

/// The converter's state while walking the tokens.
struct Converter<'b> {
    base: Option<&'b str>,
    rows: Vec<(usize, String)>,
    bytes: usize,
    truncated: bool,
    /// The block being collected (inline markdown).
    line: String,
    /// Open inline markers (`**`, `*`, `` ` ``) in `line`, to close.
    marks: Vec<&'static str>,
    /// Open links: where their text starts in `line`, and the address.
    links: Vec<(usize, Option<String>)>,
    /// Heading levels above (content nests under headings).
    headings: Vec<usize>,
    /// The heading being collected, if any.
    heading: Option<usize>,
    lists: usize,
    /// The list item's own text is still to come (its first block).
    item_pending: bool,
    quote: usize,
    /// Inside `<pre>`: its raw text.
    pre: Option<String>,
    /// Table cells collected for the current row.
    cells: Option<Vec<String>>,
}

impl Converter<'_> {
    fn depth(&self) -> usize {
        self.headings.len() + self.lists.saturating_sub(usize::from(self.item_pending))
    }

    fn push_row(&mut self, depth: usize, text: String) {
        if self.truncated {
            return;
        }
        if self.rows.len() >= MAX_BLOCKS || self.bytes + text.len() > MAX_OUTPUT_BYTES {
            self.truncated = true;
            return;
        }
        self.bytes += text.len() + depth * 2 + 3;
        self.rows.push((depth, text));
    }

    /// End the current block: close its markers and links, defang it,
    /// and add it if it has any text.
    fn flush(&mut self) {
        while let Some(at) = self.links.pop() {
            self.close_link(at);
        }
        while let Some(mark) = self.marks.pop() {
            self.line.push_str(mark);
        }
        let text = std::mem::take(&mut self.line);
        let lines: Vec<String> = text
            .split('\n')
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(defang)
            .collect();
        if lines.is_empty() {
            return;
        }
        let mut block = lines.join("\n");
        if let Some(level) = self.heading {
            block = format!(
                "{} {}",
                "#".repeat(level.min(3)),
                block.trim_start_matches(ZWSP)
            );
        } else if self.quote > 0 {
            block = format!("> {block}");
        }
        let depth = self.depth();
        self.push_row(depth, block);
        if self.heading.is_none() {
            self.item_pending = false;
        }
    }

    fn close_link(&mut self, (start, href): (usize, Option<String>)) {
        let label: String = self.line[start..]
            .chars()
            .map(|c| match c {
                '[' => '(',
                ']' => ')',
                '\n' => ' ',
                c => c,
            })
            .collect();
        let label = label.trim().to_string();
        self.line.truncate(start);
        match href {
            Some(url) if !label.is_empty() => self.line.push_str(&format!("[{label}]({url})")),
            Some(url) => self.line.push_str(&format!("[{url}]({url})")),
            None => self.line.push_str(&label),
        }
    }

    fn text(&mut self, raw: &str) {
        let text = decode_entities(raw);
        if let Some(pre) = &mut self.pre {
            pre.push_str(&text);
            return;
        }
        if let Some(cells) = &mut self.cells {
            if let Some(cell) = cells.last_mut() {
                cell.push_str(&text);
            }
            return;
        }
        let mut last_space = self.line.is_empty() || self.line.ends_with([' ', '\n']);
        for c in text.chars() {
            if c.is_whitespace() || c == '\u{a0}' {
                if !last_space {
                    self.line.push(' ');
                    last_space = true;
                }
            } else if !c.is_control() {
                self.line.push(c);
                last_space = false;
            }
        }
    }

    fn start(&mut self, name: &str, attrs: &[(String, String)]) {
        let attr = |key: &str| {
            attrs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        if self.pre.is_some() {
            if name == "br" {
                if let Some(pre) = &mut self.pre {
                    pre.push('\n');
                }
            }
            return;
        }
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush();
                let level = usize::from(name.as_bytes()[1] - b'0');
                while self.headings.last().is_some_and(|&l| l >= level) {
                    self.headings.pop();
                }
                self.heading = Some(level);
            }
            "ul" | "ol" | "menu" => {
                self.flush();
                self.lists += 1;
            }
            "li" => {
                self.flush();
                self.item_pending = self.lists > 0;
            }
            "blockquote" => {
                self.flush();
                self.quote += 1;
            }
            "pre" => {
                self.flush();
                self.pre = Some(String::new());
            }
            "tr" => {
                self.flush();
                self.cells = Some(Vec::new());
            }
            "td" | "th" => {
                if let Some(cells) = &mut self.cells {
                    cells.push(String::new());
                }
            }
            "br" => {
                if let Some(cells) = &mut self.cells {
                    if let Some(cell) = cells.last_mut() {
                        cell.push(' ');
                    }
                } else {
                    self.line.push('\n');
                }
            }
            "a" if self.cells.is_none() => {
                let href = attr("href").and_then(|h| clean_url(h, self.base));
                self.links.push((self.line.len(), href));
            }
            "img" => {
                let Some(src) = attr("src").and_then(|s| clean_url(s, self.base)) else {
                    return;
                };
                if src.starts_with("mailto:") {
                    return;
                }
                let alt: String = attr("alt")
                    .unwrap_or("")
                    .chars()
                    .filter(|c| !matches!(c, '[' | ']') && !c.is_control())
                    .collect();
                let alt = alt.trim();
                let label = if alt.is_empty() {
                    "Image".to_string()
                } else {
                    format!("Image: {alt}")
                };
                if let Some(cells) = &mut self.cells {
                    if let Some(cell) = cells.last_mut() {
                        cell.push_str(&label);
                    }
                } else if self.links.is_empty() {
                    if !self.line.is_empty() && !self.line.ends_with([' ', '\n']) {
                        self.line.push(' ');
                    }
                    self.line.push_str(&format!("[{label}]({src})"));
                } else {
                    self.line.push_str(&label);
                }
            }
            "strong" | "b" if self.cells.is_none() => self.mark("**"),
            "em" | "i" if self.cells.is_none() => self.mark("*"),
            "code" | "kbd" | "samp" | "tt" if self.cells.is_none() => self.mark("`"),
            _ if BLOCK.contains(&name) => self.flush(),
            _ => {}
        }
    }

    fn mark(&mut self, mark: &'static str) {
        if self.marks.contains(&"`") {
            return;
        }
        self.line.push_str(mark);
        self.marks.push(mark);
    }

    fn end(&mut self, name: &str) {
        if let Some(pre) = &self.pre {
            if name != "pre" {
                return;
            }
            let code = pre.trim_matches('\n').to_string();
            self.pre = None;
            let mut block = String::from("```");
            for line in code.lines() {
                block.push('\n');
                block.push_str(&code_line(&line.replace('\t', "    ")));
            }
            block.push_str("\n```");
            if !code.trim().is_empty() {
                let depth = self.depth();
                self.push_row(depth, block);
                self.item_pending = false;
            }
            return;
        }
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = self.heading;
                self.flush();
                self.heading = None;
                if let Some(level) = level {
                    self.headings.push(level);
                }
            }
            "ul" | "ol" | "menu" => {
                self.flush();
                self.lists = self.lists.saturating_sub(1);
                self.item_pending = false;
            }
            "li" => {
                self.flush();
                self.item_pending = false;
            }
            "blockquote" => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
            }
            "tr" => {
                let cells: Vec<String> = self
                    .cells
                    .take()
                    .unwrap_or_default()
                    .iter()
                    .map(|c| c.split_whitespace().collect::<Vec<_>>().join(" "))
                    .collect();
                if cells.iter().any(|c| !c.is_empty()) {
                    self.line = cells.join(" · ");
                    self.flush();
                }
            }
            "a" => {
                if let Some(at) = self.links.pop() {
                    self.close_link(at);
                }
            }
            "strong" | "b" => self.unmark("**"),
            "em" | "i" => self.unmark("*"),
            "code" | "kbd" | "samp" | "tt" => self.unmark("`"),
            _ if BLOCK.contains(&name) => self.flush(),
            _ => {}
        }
    }

    fn unmark(&mut self, mark: &'static str) {
        if self.marks.last() == Some(&mark) {
            self.marks.pop();
            if self.line.ends_with(mark) {
                // Nothing inside: drop the opening marker.
                self.line.truncate(self.line.len() - mark.len());
            } else {
                self.line.push_str(mark);
            }
        }
    }
}

/// `html` (from the page at `base`, for relative links) as outline rows,
/// and whether it was cut at the output caps.
pub fn to_rows(html: &str, base: Option<&str>) -> (Vec<(usize, String)>, bool) {
    let mut c = Converter {
        base,
        rows: Vec::new(),
        bytes: 0,
        truncated: false,
        line: String::new(),
        marks: Vec::new(),
        links: Vec::new(),
        headings: Vec::new(),
        heading: None,
        lists: 0,
        item_pending: false,
        quote: 0,
        pre: None,
        cells: None,
    };
    // Inside a skipped element: its name and how many are open.
    let mut skipping: Option<(String, usize)> = None;
    for token in tokens(html) {
        if c.truncated {
            break;
        }
        if let Some((name, open)) = &mut skipping {
            match &token {
                Token::Start {
                    name: n,
                    self_closing: false,
                    ..
                } if n == name && !VOID.contains(&n.as_str()) => *open += 1,
                Token::End(n) if n == name => {
                    *open -= 1;
                    if *open == 0 {
                        skipping = None;
                    }
                }
                _ => {}
            }
            continue;
        }
        match token {
            Token::Text(text) => c.text(text),
            Token::Start {
                name,
                attrs,
                self_closing,
            } => {
                if SKIP.contains(&name.as_str()) {
                    if !self_closing && !VOID.contains(&name.as_str()) {
                        skipping = Some((name, 1));
                    }
                    continue;
                }
                c.start(&name, &attrs);
                if self_closing && !VOID.contains(&name.as_str()) {
                    c.end(&name);
                }
            }
            Token::End(name) => c.end(&name),
        }
    }
    if c.pre.is_some() {
        c.end("pre");
    }
    c.flush();
    (c.rows, c.truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(html: &str) -> Vec<(usize, String)> {
        to_rows(html, Some("https://example.com/blog/post.html?x=1")).0
    }

    fn row(depth: usize, text: &str) -> (usize, String) {
        (depth, text.to_string())
    }

    #[test]
    fn scripts_handlers_and_dangerous_links_are_gone() {
        let html = r#"<html><head><title>T</title><style>p{color:red}</style></head>
            <body onload="evil()"><script>alert("<p>x</p>")</script>
            <p onclick="steal()">Hello <a href="javascript:alert(1)">click</a>
            <a href=" JaVa&#x09;script:alert(1)">tab</a> <a href="data:text/html,x">data</a>
            <a href="vbscript:x">vb</a> <a href="/docs">docs</a> <a href="mailto:a@b.c">mail</a></p>
            <iframe src="https://evil.example"><p>inside frame</p></iframe>
            <form action="/x"><input name=a value=b><p>form text</p></form>
            <svg><script>bad()</script><text>svg text</text></svg>
            <noscript><img src=x onerror=alert(1)></noscript>
            <img src="https://example.com/a.png" alt="A [pic]" onerror="alert(1)">
            <img src="data:image/png;base64,AAAA" alt="inline">
            <SCRIPT type="text/javascript">document.write('<b>')</SCRIPT >
            <p>after</p></body></html>"#;
        let rows = md(html);
        let all: String = rows.iter().map(|r| r.1.clone() + "\n").collect();
        for bad in [
            "alert",
            "steal",
            "evil",
            "onclick",
            "onerror",
            "javascript",
            "data:",
            "vbscript",
            "inside frame",
            "form text",
            "svg text",
            "color:red",
            "bad()",
        ] {
            assert!(
                !all.to_lowercase().contains(&bad.to_lowercase()),
                "{bad} in {all}"
            );
        }
        assert_eq!(
            rows,
            [
                row(
                    0,
                    "Hello click tab data vb [docs](https://example.com/docs) [mail](mailto:a@b.c)"
                ),
                row(0, "[Image: A pic](https://example.com/a.png)"),
                row(0, "after"),
            ]
        );
    }

    #[test]
    fn structure_survives() {
        let html = "<h1>Title</h1><p>Intro <strong>bold</strong> and <em>it</em> and <code>x[[0]]</code></p>\
            <h2>Part</h2><ul><li>one<ul><li>nested <a href='b.html'>rel</a></li></ul></li><li><p>two</p><p>more</p></li></ul>\
            <blockquote><p>quoted</p></blockquote>\
            <pre><code>fn main() {\n    - not a bullet\n    id:: 0f0f0f0f-0f0f-4f0f-8f0f-0f0f0f0f0f0f\n}</code></pre>\
            <table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2 <br>3</td></tr></table>\
            <p>line<br>- two<br>tags:: #x</p>";
        let z = ZWSP;
        assert_eq!(
            md(html),
            [
                row(0, "# Title"),
                row(1, &format!("Intro **bold** and *it* and `x[{z}[0]]`")),
                row(1, "## Part"),
                row(2, "one"),
                row(3, "nested [rel](https://example.com/blog/b.html)"),
                row(2, "two"),
                row(3, "more"),
                row(2, "> quoted"),
                row(2, &format!("```\nfn main() {{\n    {z}- not a bullet\n    {z}id:: 0f0f0f0f-0f0f-4f0f-8f0f-0f0f0f0f0f0f\n}}\n```")),
                row(2, "a · b"),
                row(2, "1 · 2 3"),
                row(2, &format!("line\n{z}- two\n{z}tags:: #x")),
            ]
        );
    }

    #[test]
    fn our_markdown_in_the_text_is_inert() {
        let z = ZWSP;
        let rows = md("<p>see [[Secret]] and ((0f0f0f0f-0f0f-4f0f-8f0f-0f0f0f0f0f0f)) {{embed x}}</p><p>id:: 1234</p><p># not a heading</p><p>TODO fake</p>");
        assert_eq!(
            rows,
            [
                row(0, &format!("see [{z}[Secret]] and ({z}(0f0f0f0f-0f0f-4f0f-8f0f-0f0f0f0f0f0f)) {{{z}{{embed x}}}}")),
                row(0, &format!("{z}id:: 1234")),
                row(0, &format!("{z}# not a heading")),
                row(0, &format!("{z}TODO fake")),
            ]
        );
        // A link's label and address can't break out of the link.
        assert_eq!(
            md(r#"<a href="https://x.com/a b)](javascript:1)">l[a]b</a>"#),
            [row(
                0,
                "[l(a)b](https://x.com/a%20b%29%5D%28javascript:1%29)"
            )]
        );
    }

    #[test]
    fn entities_urls_and_caps() {
        assert_eq!(
            decode_entities("a &amp; b &lt;c&gt; &#39;d&#x27; &bogus; &#0;"),
            "a & b <c> 'd' &bogus; \u{fffd}"
        );
        assert_eq!(
            clean_url("//cdn.x/a.js", Some("https://e.com/p")),
            Some("https://cdn.x/a.js".into())
        );
        assert_eq!(
            clean_url("?q=1", Some("https://e.com/p/q?z")),
            Some("https://e.com/p/q?q=1".into())
        );
        assert_eq!(clean_url("x.html", None), None);
        assert_eq!(clean_url("#top", Some("https://e.com/")), None);
        assert_eq!(clean_url("file:///etc/passwd", None), None);
        assert_eq!(clean_url("  javascript:x", Some("https://e.com/")), None);
        let big = "<p>word</p>".repeat(MAX_BLOCKS + 10);
        let (rows, truncated) = to_rows(&big, None);
        assert!(truncated);
        assert_eq!(rows.len(), MAX_BLOCKS);
        // Unclosed and odd markup doesn't panic or loop.
        for odd in [
            "<",
            "<a",
            "<p><b>x",
            "</p>",
            "<!--",
            "<script>",
            "<pre>x",
            "a < b > c",
            "<a href='x>y</a>",
            "<ul><li>",
            "&#xFFFFFFFF;",
            "é<p>ü",
        ] {
            let _ = to_rows(odd, None);
        }
        assert_eq!(md("a < b > c"), [row(0, "a < b > c")]);
        assert_eq!(md(""), Vec::<(usize, String)>::new());
    }
}

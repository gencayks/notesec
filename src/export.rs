//! Export a page as one self-contained HTML file.
//!
//! Pure code (no GPUI, no files): [`page_html_with_embeds`] turns a page into the whole
//! document as a string, and the caller says how block references resolve
//! and where image bytes come from. `app.rs` writes the result to
//! `<graph>/exports/` (see `Storage::write_export`).
//!
//! The page is rendered the way the reading view shows it, through the same
//! parsers: `split_code` for fenced code, `parse_table` for pipe tables,
//! `parse_images` for images and `DisplayBlock` for everything inline
//! (headings, task keywords, paired `*` emphasis, `[[links]]`, `#tags`,
//! resolved `((block references))`). So the export can't disagree with the
//! app about what a block means. On top of that, `SCHEDULED:` / `DEADLINE:`
//! lines (as `agenda::parse_dates` reads them) get a line of their own.
//!
//! Self-contained means: inline CSS, no scripts, no external resources.
//! Images are embedded as `data:` URIs; a web image or a missing file shows
//! a note instead. Links and tags become styled spans (with the page name in
//! `data-page`), since the pages they point at aren't exported. A
//! Content-Security-Policy meta tag makes a browser refuse anything else.
//!
//! Embeds (`![[Page]]`, `![[((id))]]`, decision 47) are drawn inline, in a
//! bordered box headed by the source page's title, with the same guards as
//! the app (`embed::Resolver`): a circular, too deep or missing embed is a
//! note instead of content.
//!
//! [`document`] is the general form, which `publish` (decision 49) also uses:
//! a stylesheet file instead of the inline one, images as files, and links
//! to the other published pages as real `<a href>`s.

use uuid::Uuid;

use crate::agenda::parse_dates;
use crate::code::{split_code, Part};
use crate::display::DisplayBlock;
use crate::embed::Resolved;
use crate::model::{BlockKind, Page, TaskState};
use crate::table::{parse_table, Align, Table};

/// The stylesheet, inlined into every export (a published bundle has it as
/// `style.css`). Light by default, dark when
/// the system prefers it; printing (the browser's Print to PDF) drops the
/// background colours.
pub const CSS: &str = r#"
:root { --bg: #ffffff; --text: #1f2328; --muted: #6e7781; --accent: #0969da;
  --border: #d0d7de; --code-bg: #f6f8fa; --tag-bg: #ddf4ff; --danger: #cf222e; }
@media (prefers-color-scheme: dark) {
  :root { --bg: #0d1117; --text: #e6edf3; --muted: #8d96a0; --accent: #4493f8;
    --border: #30363d; --code-bg: #161b22; --tag-bg: #12263f; --danger: #f85149; }
}
body { margin: 0; background: var(--bg); color: var(--text);
  font: 16px/1.6 system-ui, -apple-system, "Segoe UI", sans-serif; }
main { max-width: 820px; margin: 0 auto; padding: 32px 24px 64px; }
.page-title { font-size: 2em; margin: 0 0 16px; }
ul { list-style: disc; margin: 0; padding-left: 24px; }
ul ul { border-left: 1px solid var(--border); margin-left: -12px; padding-left: 36px; }
li { margin: 2px 0; }
.block > * + * { margin-top: 4px; }
.heading1 > .text { font-size: 1.6em; font-weight: 700; }
.heading2 > .text { font-size: 1.35em; font-weight: 700; }
.heading3 > .text { font-size: 1.15em; font-weight: 700; }
.quote { border-left: 3px solid var(--border); padding-left: 10px; color: var(--muted); }
.task { display: inline-block; font-size: 0.75em; font-weight: 700; padding: 0 6px;
  margin-right: 6px; border: 1px solid var(--border); border-radius: 4px;
  color: var(--muted); vertical-align: middle; }
.task-doing, .task-now { color: var(--accent); border-color: var(--accent); }
.task-done { color: var(--muted); }
.done > .text { color: var(--muted); text-decoration: line-through; }
.task + .text, .task + .planning { display: inline; }
.link { color: var(--accent); }
a.link, a.tag { text-decoration: none; }
a.link:hover, a.tag:hover { text-decoration: underline; }
.tag { color: var(--accent); background: var(--tag-bg); border-radius: 4px; padding: 0 3px; }
.ref { background: var(--tag-bg); border-bottom: 1px dashed var(--muted); }
.planning { color: var(--muted); font-size: 0.9em; }
.planning-kw { font-weight: 700; }
pre { background: var(--code-bg); border: 1px solid var(--border); border-radius: 6px;
  padding: 8px 12px; overflow-x: auto; margin: 4px 0; }
code { font: 0.9em/1.5 ui-monospace, "SFMono-Regular", Menlo, Consolas, monospace; }
.code-lang { color: var(--muted); font-size: 0.8em; }
table { border-collapse: collapse; margin: 4px 0; }
th, td { border: 1px solid var(--border); padding: 4px 8px; }
th { background: var(--code-bg); }
.left { text-align: left; } .center { text-align: center; } .right { text-align: right; }
img { max-width: 100%; max-height: 480px; display: block; margin: 4px 0; border-radius: 4px; }
.image-missing { display: inline-block; color: var(--muted); border: 1px dashed var(--border);
  border-radius: 4px; padding: 2px 8px; font-size: 0.9em; }
.embed { border: 1px solid var(--border); border-left: 3px solid var(--accent);
  border-radius: 6px; padding: 4px 12px 6px; margin: 6px 0; }
.embed-title { color: var(--muted); font-size: 0.85em; }
.embed-note { color: var(--muted); font-style: italic; font-size: 0.9em; }
@media print { body { background: none; } main { max-width: none; padding: 0; } }
@page { size: A4; margin: 18mm; }
.whiteboard { overflow-x: auto; margin: 1rem 0; }
.whiteboard svg { max-width: 100%; height: auto; }
.whiteboard .card-box { fill: var(--bg); stroke: var(--border); }
.whiteboard .edge { stroke: var(--muted); stroke-width: 1.5; }
.whiteboard .arrow { fill: var(--muted); }
.whiteboard .edge-label { fill: var(--muted); font-size: 12px; text-anchor: middle; }
.whiteboard .card { padding: 8px; overflow: hidden; height: 100%; box-sizing: border-box;
  font-size: 14px; color: #1f2328; }
"#;

/// Resolves a `((block reference))` to the content of the block it names
/// (`None`: no such block, and the reference stays as written).
pub type ResolveRef<'a> = &'a dyn Fn(Uuid) -> Option<String>;

/// The bytes of the image an `![alt](target)` points at (`None`: missing).
pub type LoadImage<'a> = &'a dyn Fn(&str) -> Option<Vec<u8>>;

/// Where an image's bytes are, as [`Options::image`] answers it.
pub enum ImageSrc {
    /// Embedded as a `data:` URI.
    Bytes(Vec<u8>),
    /// A URL relative to the document (`assets/x.png`).
    Url(String),
    /// Not found: a note instead.
    Missing,
    /// There, but not to be shown (publish: outside the graph, or not
    /// an image the app can clean): a note instead.
    Withheld,
}

/// How [`document`] renders: where references, images and links go.
pub struct Options<'a> {
    pub resolve_ref: ResolveRef<'a>,
    /// The image an `![alt](target)` shows (web images and non-images are
    /// a note before this is asked).
    pub image: &'a dyn Fn(&str) -> ImageSrc,
    /// The `href` for a link or tag to the page named so (`None`: a styled
    /// span, as in an export).
    pub href: &'a dyn Fn(&str) -> Option<String>,
    /// A stylesheet file to link instead of the inline `<style>`.
    pub stylesheet: Option<&'a str>,
    /// Notes about missing images show the target as written (else only
    /// its file name, so no local path is published).
    pub show_paths: bool,
}

/// The embeds in one of the exported page's blocks, resolved (see
/// `embed::Resolver::resolve`; nested embeds come resolved inside).
pub type ResolveEmbeds<'a> = &'a dyn Fn(&str) -> Vec<Resolved>;

/// The whole HTML document for `page`: title, then every block as nested
/// lists (all of them: folding is a view setting, not content). Embeds show
/// as written; [`page_html_with_embeds`] draws them.
#[cfg(test)]
pub fn page_html(page: &Page, resolve_ref: ResolveRef, load_image: LoadImage) -> String {
    page_html_with_embeds(page, resolve_ref, &|_| Vec::new(), load_image)
}

/// [`page_html`], with each block's embeds drawn inline under it.
pub fn page_html_with_embeds(
    page: &Page,
    resolve_ref: ResolveRef,
    resolve_embeds: ResolveEmbeds,
    load_image: LoadImage,
) -> String {
    let image = |target: &str| load_image(target).map_or(ImageSrc::Missing, ImageSrc::Bytes);
    let options = Options {
        resolve_ref,
        image: &image,
        href: &|_| None,
        stylesheet: None,
        show_paths: true,
    };
    if crate::whiteboard::is_whiteboard(page) {
        // The drawing replaces the outline (decision 53).
        let board = crate::whiteboard::parse(page);
        let svg = board_svg(&board, &options, &|text| text.to_string());
        return document_with_board(
            &page.title,
            page.is_journal,
            Some(svg),
            Vec::new(),
            &options,
        );
    }

    let rows = page
        .blocks
        .iter()
        .enumerate()
        .map(|(ix, b)| {
            (
                page.depth_of(ix),
                b.content.as_str(),
                resolve_embeds(&b.content),
            )
        })
        .collect();
    document(&page.title, page.is_journal, rows, &options)
}

/// Render a page for PDF printing, using A4 paper and the requested margin.
/// The HTML remains self-contained so a headless browser can print local
/// images and the same wikilinks and tags as the HTML export.
pub fn page_html_for_pdf(
    page: &Page,
    resolve_ref: ResolveRef,
    resolve_embeds: ResolveEmbeds,
    load_image: LoadImage,
    margin_mm: f32,
) -> String {
    let margin_mm = if margin_mm.is_finite() {
        margin_mm.clamp(5.0, 40.0)
    } else {
        18.0
    };
    let html = page_html_with_embeds(page, resolve_ref, resolve_embeds, load_image);
    html.replacen(
        "@page { size: A4; margin: 18mm; }",
        &format!("@page {{ size: A4; margin: {margin_mm:.1}mm; }}"),
        1,
    )
}

/// The whole HTML document for a page titled `title` whose blocks are
/// `rows` (depth, content, resolved embeds), rendered as `options` say.
pub fn document(
    title: &str,
    is_journal: bool,
    rows: Vec<(usize, &str, Vec<Resolved>)>,
    options: &Options,
) -> String {
    document_with_board(title, is_journal, None, rows, options)
}

/// [`document`], with a whiteboard's drawing (`board_svg`) after the
/// title, before the rows.
pub fn document_with_board(
    title: &str,
    is_journal: bool,
    board: Option<String>,
    rows: Vec<(usize, &str, Vec<Resolved>)>,
    options: &Options,
) -> String {
    let ctx = options;
    let title = escape(title);
    let mut out = String::new();
    out.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    // No scripts, nothing from elsewhere: only the stylesheet and images
    // the document itself carries (inline) or sits next to (`'self'`).
    let csp = if ctx.stylesheet.is_some() {
        "default-src 'none'; img-src 'self'; style-src 'self'; base-uri 'none'; form-action 'none'"
    } else {
        "default-src 'none'; img-src data:; style-src 'unsafe-inline'"
    };
    out.push_str(&format!(
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta http-equiv=\"Content-Security-Policy\" content=\"{csp}\">\n\
         <meta name=\"generator\" content=\"notesec\">\n"
    ));
    match ctx.stylesheet {
        Some(href) => out.push_str(&format!(
            "<title>{title}</title>\n<link rel=\"stylesheet\" href=\"{}\">\n",
            escape(href)
        )),
        None => out.push_str(&format!("<title>{title}</title>\n<style>{CSS}</style>\n")),
    }
    out.push_str("</head>\n<body>\n");
    let class = if is_journal { "page journal" } else { "page" };
    out.push_str(&format!(
        "<main class=\"{class}\">\n<h1 class=\"page-title\">{title}</h1>\n"
    ));
    let has_board = board.is_some();
    if let Some(board) = board {
        out.push_str(&board);
    }
    // A whiteboard's export is its board alone.
    if !has_board || !rows.is_empty() {
        outline_html(&mut out, "<ul class=\"outline\">", rows, ctx);
    }
    out.push_str("</main>\n</body>\n</html>\n");
    out
}

type Ctx<'a> = Options<'a>;

/// A whiteboard (decision 53) as an inline SVG: arrows as lines and
/// triangles, each card a box with its text rendered like a block inside
/// a `<foreignObject>`. Everything is placed by SVG attributes, not CSS,
/// so it shows under both documents' CSPs (`style-src 'self'` blocks
/// `style=""`), and inline SVG loads nothing. `clean` prepares a card's
/// text (publish strips properties).
pub fn board_svg(
    board: &crate::whiteboard::Board,
    ctx: &Ctx,
    clean: &dyn Fn(&str) -> String,
) -> String {
    use crate::whiteboard::geom::{arrowhead, edge_line, Rect};
    const MARGIN: f32 = 20.0;
    let mut out = String::from("<div class=\"whiteboard\">");
    let Some(all) = Rect::union(&board.rects()) else {
        out.push_str("<p class=\"image-missing\">Empty whiteboard</p></div>\n");
        return out;
    };
    let (x0, y0) = (all.x - MARGIN, all.y - MARGIN);
    let (w, h) = (all.w + 2.0 * MARGIN, all.h + 2.0 * MARGIN);
    out.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{x0:.1} {y0:.1} {w:.1} {h:.1}\" \
         width=\"{w:.0}\" height=\"{h:.0}\" role=\"img\" aria-label=\"Whiteboard\">\n"
    ));
    for edge in &board.edges {
        let Some((a, b)) = edge_line(board, edge.from, edge.to) else {
            continue;
        };
        out.push_str(&format!(
            "<line class=\"edge\" x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\"/>\n",
            a.x, a.y, b.x, b.y
        ));
        let head = arrowhead(a, b);
        let points: Vec<String> = head
            .iter()
            .map(|p| format!("{:.1},{:.1}", p.x, p.y))
            .collect();
        out.push_str(&format!(
            "<polygon class=\"arrow\" points=\"{}\"/>\n",
            points.join(" ")
        ));
        if let Some(label) = &edge.label {
            out.push_str(&format!(
                "<text class=\"edge-label\" x=\"{:.1}\" y=\"{:.1}\">{}</text>\n",
                (a.x + b.x) / 2.0,
                (a.y + b.y) / 2.0 - 4.0,
                escape(label)
            ));
        }
    }
    for card in &board.cards {
        let r = card.rect;
        let fill = card
            .color
            .and_then(|name| crate::whiteboard::COLORS.iter().find(|(n, _)| *n == name))
            .map(|(_, rgb)| format!(" fill=\"#{rgb:06x}\""))
            .unwrap_or_default();
        out.push_str(&format!(
            "<rect class=\"card-box\" x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"6\"{fill}/>\n\
             <foreignObject x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\">\
             <div xmlns=\"http://www.w3.org/1999/xhtml\" class=\"card\">",
            r.x, r.y, r.w, r.h, r.x, r.y, r.w, r.h
        ));
        block_html(&mut out, &clean(&card.text), ctx);
        out.push_str("</div></foreignObject>\n");
    }
    out.push_str("</svg></div>\n");
    out
}

/// Blocks (depth, content, resolved embeds) as nested `<ul>`s, one `<li>`
/// per block, children in a `<ul>` inside their parent's `<li>`. `open` is
/// the outermost `<ul ...>` tag.
fn outline_html(out: &mut String, open: &str, rows: Vec<(usize, &str, Vec<Resolved>)>, ctx: &Ctx) {
    out.push_str(open);
    out.push('\n');
    let mut prev: Option<usize> = None;
    for (depth, content, embeds) in rows {
        // Blocks are stored in outline order, so a block is at most one
        // level below the one before it.
        let depth = depth.min(prev.map_or(0, |p| p + 1));
        match prev {
            None => {}
            Some(p) if depth > p => out.push_str("\n<ul>\n"),
            Some(p) => {
                out.push_str("</li>\n");
                for _ in depth..p {
                    out.push_str("</ul>\n</li>\n");
                }
            }
        }
        out.push_str("<li>");
        block_html(out, content, ctx);
        for embed in &embeds {
            embed_html(out, embed, ctx);
        }
        prev = Some(depth);
    }
    if let Some(p) = prev {
        out.push_str("</li>\n");
        for _ in 0..p {
            out.push_str("</ul>\n</li>\n");
        }
    }
    out.push_str("</ul>\n");
}

/// One embed: its source's content in a box, or a note.
fn embed_html(out: &mut String, embed: &Resolved, ctx: &Ctx) {
    let (title, rows) = match embed {
        Resolved::Page { title, rows } | Resolved::Block { title, rows } => (title, rows),
        _ => {
            let note = embed.note().unwrap_or_default();
            out.push_str(&format!(
                "<div class=\"embed-note\">{}</div>",
                escape(&note)
            ));
            return;
        }
    };
    out.push_str(&format!(
        "<div class=\"embed\"><div class=\"embed-title\" data-page=\"{t}\">{t}</div>",
        t = escape(title)
    ));
    let rows = rows
        .iter()
        .map(|r| (r.depth, r.content.as_str(), r.embeds.clone()))
        .collect();
    outline_html(out, "<ul class=\"embed-outline\">", rows, ctx);
    out.push_str("</div>");
}

/// One block: `<div class="block ...">` with its task badge, then its
/// prose, code blocks, tables and images in order, like the reading view.
fn block_html(out: &mut String, content: &str, ctx: &Ctx) {
    let (kind, body) = BlockKind::parse(content);
    let (task, _) = TaskState::parse(body);
    let mut class = String::from("block");
    match kind {
        BlockKind::Text => {}
        BlockKind::Heading1 => class.push_str(" heading1"),
        BlockKind::Heading2 => class.push_str(" heading2"),
        BlockKind::Heading3 => class.push_str(" heading3"),
        BlockKind::Quote => class.push_str(" quote"),
    }
    if task == Some(TaskState::Done) {
        class.push_str(" done");
    }
    out.push_str(&format!("<div class=\"{class}\">"));
    if let Some(task) = task {
        let keyword = task.keyword();
        out.push_str(&format!(
            "<span class=\"task task-{}\">{keyword}</span>",
            keyword.to_lowercase()
        ));
    }
    for (p, part) in split_code(content).into_iter().enumerate() {
        let mut rest = match part {
            Part::Code(code) => {
                out.push_str("<pre>");
                if !code.lang.is_empty() {
                    out.push_str(&format!(
                        "<div class=\"code-lang\">{}</div>",
                        escape(&code.lang)
                    ));
                }
                out.push_str(&format!("<code>{}</code></pre>", escape(&code.code)));
                continue;
            }
            Part::Text(text) => text,
        };
        // Only the block's first line carries its type prefix and keyword.
        let mut first = p == 0;
        while let Some(table) = parse_table(&rest) {
            if !table.before.is_empty() {
                prose_html(out, &table.before, first, ctx);
            }
            table_html(out, &table, ctx);
            first = false;
            rest = table.after;
        }
        if !rest.is_empty() {
            prose_html(out, &rest, first, ctx);
        }
    }
    out.push_str("</div>");
}

/// Prose with any images in it: each image on its own between the text
/// around it (the line breaks next to an image are dropped, as in the app).
fn prose_html(out: &mut String, text: &str, first: bool, ctx: &Ctx) {
    let images = crate::assets::parse_images(text);
    if images.is_empty() {
        text_html(out, text, first, ctx);
        return;
    }
    let mut first = first;
    let mut pos = 0;
    let piece = |out: &mut String, t: &str, first: &mut bool| {
        let t = t.trim_matches('\n');
        if !t.trim().is_empty() {
            text_html(out, t, *first, ctx);
        }
        *first = false;
    };
    for image in images {
        piece(out, &text[pos..image.range.start], &mut first);
        image_html(out, &image.alt, &image.target, ctx);
        pos = image.range.end;
    }
    piece(out, &text[pos..], &mut first);
}

/// A run of prose lines. `SCHEDULED:` / `DEADLINE:` lines (any but the
/// block's first line) get a `planning` line of their own; the rest is
/// inline text.
fn text_html(out: &mut String, text: &str, first: bool, ctx: &Ctx) {
    let mut lines: Vec<&str> = Vec::new();
    let mut first = first;
    let flush = |out: &mut String, lines: &mut Vec<&str>, first: &mut bool| {
        if !lines.is_empty() {
            let text = lines.join("\n");
            if *first || !text.trim().is_empty() {
                out.push_str("<div class=\"text\">");
                inline_html(out, &text, *first, ctx);
                out.push_str("</div>");
            }
            lines.clear();
        }
        *first = false;
    };
    for (i, line) in text.split('\n').enumerate() {
        let can_be_planning = !(first && i == 0);
        if can_be_planning && is_planning_line(line) {
            flush(out, &mut lines, &mut first);
            planning_html(out, line);
        } else {
            lines.push(line);
        }
    }
    flush(out, &mut lines, &mut first);
}

/// A line that starts with `SCHEDULED:` or `DEADLINE:` and a date the
/// agenda reads.
fn is_planning_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if !trimmed.starts_with("SCHEDULED:") && !trimmed.starts_with("DEADLINE:") {
        return false;
    }
    let dates = parse_dates(&format!("\n{trimmed}"));
    dates.scheduled.is_some() || dates.deadline.is_some()
}

/// A planning line as written, the keywords in bold.
fn planning_html(out: &mut String, line: &str) {
    let mut html = escape(line.trim());
    for keyword in ["SCHEDULED:", "DEADLINE:"] {
        html = html.replace(
            keyword,
            &format!("<span class=\"planning-kw\">{keyword}</span>"),
        );
    }
    out.push_str(&format!("<div class=\"planning\">{html}</div>"));
}

/// Inline text as the reading view shows it. `block` reads a leading type
/// prefix and task keyword (the block's first line); otherwise they're text.
fn inline_html(out: &mut String, text: &str, block: bool, ctx: &Ctx) {
    let resolve = |id| (ctx.resolve_ref)(id);
    let d = if block {
        DisplayBlock::with_refs(text, resolve)
    } else {
        DisplayBlock::inline(text, resolve)
    };
    let mut pos = 0;
    for (range, format) in d.segments() {
        push_text(out, &d.text[pos..range.start]);
        let mut close: Vec<&str> = Vec::new();
        if format.link || format.tag {
            let target = d
                .links
                .iter()
                .find(|l| l.range.start <= range.start && range.end <= l.range.end)
                .map_or(String::new(), |l| l.target.clone());
            let class = if format.tag { "tag" } else { "link" };
            match (ctx.href)(&target) {
                Some(href) => {
                    out.push_str(&format!(
                        "<a class=\"{class}\" href=\"{}\" data-page=\"{}\">",
                        escape(&href),
                        escape(&target)
                    ));
                    close.push("</a>");
                }
                None => {
                    out.push_str(&format!(
                        "<span class=\"{class}\" data-page=\"{}\">",
                        escape(&target)
                    ));
                    close.push("</span>");
                }
            }
        }
        if format.block_ref {
            out.push_str("<span class=\"ref\">");
            close.push("</span>");
        }
        if format.bold {
            out.push_str("<strong>");
            close.push("</strong>");
        }
        if format.italic {
            out.push_str("<em>");
            close.push("</em>");
        }
        push_text(out, &d.text[range.clone()]);
        for tag in close.iter().rev() {
            out.push_str(tag);
        }
        pos = range.end;
    }
    push_text(out, &d.text[pos..]);
}

/// Escaped text, with line breaks kept as `<br>`.
fn push_text(out: &mut String, text: &str) {
    out.push_str(&escape(text).replace('\n', "<br>\n"));
}

fn table_html(out: &mut String, table: &Table, ctx: &Ctx) {
    out.push_str("<table>");
    for (r, row) in table.rows.iter().enumerate() {
        let cell = if r == 0 { "th" } else { "td" };
        if r == 0 {
            out.push_str("<thead>");
        } else if r == 1 {
            out.push_str("<tbody>");
        }
        out.push_str("<tr>");
        for (c, text) in row.iter().enumerate() {
            let align = match table.align.get(c).copied().unwrap_or_default() {
                Align::Left => "left",
                Align::Center => "center",
                Align::Right => "right",
            };
            out.push_str(&format!("<{cell} class=\"{align}\">"));
            inline_html(out, text, false, ctx);
            out.push_str(&format!("</{cell}>"));
        }
        out.push_str("</tr>");
        if r == 0 {
            out.push_str("</thead>");
        }
    }
    if table.rows.len() > 1 {
        out.push_str("</tbody>");
    }
    out.push_str("</table>");
}

/// An image as a `data:` URI or a file next to the document, or a note
/// saying why it isn't shown.
fn image_html(out: &mut String, alt: &str, target: &str, ctx: &Ctx) {
    let missing = |out: &mut String, why: &str| {
        let shown = if ctx.show_paths || target.contains("://") {
            target
        } else {
            target.rsplit(['/', '\\']).next().unwrap_or(target)
        };
        out.push_str(&format!(
            "<span class=\"image-missing\">{why}: {}</span>",
            escape(shown)
        ));
    };
    if crate::voice::is_audio_target(target) {
        // Audio stays in the vault (decision 52): a page shouldn't carry
        // megabytes of recording, or publish someone's voice by accident.
        return missing(out, "Voice note not included");
    }
    if target.contains("://") {
        // Fetching it would make the file depend on the network.
        return missing(out, "Web image not embedded");
    }
    let Some(mime) = image_mime(target) else {
        return missing(out, "Not an image");
    };
    match (ctx.image)(target) {
        ImageSrc::Bytes(bytes) => out.push_str(&format!(
            "<img src=\"data:{mime};base64,{}\" alt=\"{}\">",
            base64(&bytes),
            escape(alt)
        )),
        ImageSrc::Url(url) => out.push_str(&format!(
            "<img src=\"{}\" alt=\"{}\">",
            escape(&url),
            escape(alt)
        )),
        ImageSrc::Missing => missing(out, "Image not found"),
        ImageSrc::Withheld => missing(out, "Image not published"),
    }
}

/// The MIME type for an image file name, by extension (the formats the
/// app accepts; SVG isn't one, and could carry scripts).
fn image_mime(target: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(target)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        _ => return None,
    })
}

/// `text` safe inside HTML text and double- or single-quoted attributes.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Standard base64 (RFC 4648, with `=` padding), for `data:` URIs. A few
/// lines here rather than a new dependency.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_refs(_: Uuid) -> Option<String> {
        None
    }

    fn no_images(_: &str) -> Option<Vec<u8>> {
        None
    }

    fn html(markdown: &str) -> String {
        page_html(
            &Page::from_markdown("Test", false, markdown),
            &no_refs,
            &no_images,
        )
    }

    /// The `<ul class="outline">` part of an export.
    fn outline(html: &str) -> &str {
        let start = html.find("<ul class=\"outline\">").unwrap();
        let end = html.rfind("</main>").unwrap();
        &html[start..end]
    }

    fn text(t: &str) -> String {
        format!("<div class=\"block\"><div class=\"text\">{t}</div></div>")
    }

    #[test]
    fn blocks_become_nested_lists() {
        let doc = html("- a\n  - b\n    - c\n  - d\n- e\n");
        let expected = format!(
            "<ul class=\"outline\">\n\
             <li>{a}\n<ul>\n\
             <li>{b}\n<ul>\n\
             <li>{c}</li>\n\
             </ul>\n</li>\n\
             <li>{d}</li>\n\
             </ul>\n</li>\n\
             <li>{e}</li>\n\
             </ul>\n",
            a = text("a"),
            b = text("b"),
            c = text("c"),
            d = text("d"),
            e = text("e"),
        );
        assert_eq!(outline(&doc), expected);
        // Climbing two levels at once closes both lists.
        let html2 = html("- a\n  - b\n    - c\n- d\n");
        assert_eq!(html2.matches("<ul>").count(), 2);
        assert_eq!(html2.matches("</ul>").count(), 3);
        assert!(outline(&html2).ends_with(&format!(
            "<li>{}</li>\n</ul>\n</li>\n</ul>\n</li>\n<li>{}</li>\n</ul>\n",
            text("c"),
            text("d")
        )));
        // A page without blocks is an empty list.
        let empty = page_html(&Page::new("Empty", false), &no_refs, &no_images);
        assert_eq!(outline(&empty), "<ul class=\"outline\">\n</ul>\n");
    }

    #[test]
    fn the_document_is_self_contained_and_titled() {
        let html = html("- hi\n");
        assert!(html.starts_with("<!DOCTYPE html>\n"));
        assert!(html.contains("<title>Test</title>"));
        assert!(html.contains("<h1 class=\"page-title\">Test</h1>"));
        assert!(html.contains("<style>"));
        assert!(html.contains("default-src 'none'; img-src data:"));
        for external in ["<script", "<link", "src=\"http", "href=", "@import", "url("] {
            assert!(!html.contains(external), "{external}");
        }
        let journal = page_html(
            &Page::from_markdown("2026-10-08", true, "- day\n"),
            &no_refs,
            &no_images,
        );
        assert!(journal.contains("<main class=\"page journal\">"));
        assert!(journal.contains("<title>2026-10-08</title>"));
    }

    #[test]
    fn everything_is_escaped() {
        let page = Page::from_markdown(
            "<script>alert('x')</script> & \"co\"",
            false,
            "- a < b & \"c\" <b>raw</b> 'q'\n- ```html\n  <script>x</script>\n  ```\n- [[<i>page</i>]]\n",
        );
        let html = page_html(&page, &no_refs, &no_images);
        assert!(!html.contains("<script"));
        assert!(!html.contains("<b>raw") && !html.contains("<i>page"));
        assert!(html.contains(
            "<title>&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; &quot;co&quot;</title>"
        ));
        assert!(html.contains("a &lt; b &amp; &quot;c&quot; &lt;b&gt;raw&lt;/b&gt; &#39;q&#39;"));
        assert!(html.contains("<code>&lt;script&gt;x&lt;/script&gt;</code>"));
        assert!(html.contains("data-page=\"&lt;i&gt;page&lt;/i&gt;\""));
        assert_eq!(escape("&<>\"'é"), "&amp;&lt;&gt;&quot;&#39;é");
    }

    #[test]
    fn headings_emphasis_and_line_breaks() {
        let html =
            html("- ## Big **bold** and *it* ***both***\n- > quoted\n- 2 * 3 = 6\n- one\n  two\n");
        assert!(html.contains(
            "<div class=\"block heading2\"><div class=\"text\">Big <strong>bold</strong> and \
             <em>it</em> <strong><em>both</em></strong></div></div>"
        ));
        assert!(html.contains(&"<div class=\"block quote\"><div class=\"text\">quoted</div>"));
        // Stray stars stay as written.
        assert!(html.contains(">2 * 3 = 6<"));
        assert!(html.contains(">one<br>\ntwo<"));
    }

    #[test]
    fn tasks_are_badges_and_done_is_marked() {
        let html = html("- TODO buy milk\n- DOING write\n- DONE ship\n- ## NOW urgent\n- TODOS\n");
        assert!(html.contains(
            "<div class=\"block\"><span class=\"task task-todo\">TODO</span>\
             <div class=\"text\">buy milk</div></div>"
        ));
        assert!(html.contains("<span class=\"task task-doing\">DOING</span>"));
        assert!(html.contains(
            "<div class=\"block done\"><span class=\"task task-done\">DONE</span>\
             <div class=\"text\">ship</div></div>"
        ));
        assert!(html.contains(
            "<div class=\"block heading2\"><span class=\"task task-now\">NOW</span>\
             <div class=\"text\">urgent</div>"
        ));
        assert!(html.contains(&text("TODOS")), "not a task");
    }

    #[test]
    fn links_and_tags_are_spans_named_after_their_page() {
        let html = html("- see [[Other Page]] and #tag and #[[two words]], **[[Bold]]**\n");
        assert!(html.contains(
            "see <span class=\"link\" data-page=\"Other Page\">[[Other Page]]</span> and \
             <span class=\"tag\" data-page=\"tag\">#tag</span> and \
             <span class=\"tag\" data-page=\"two words\">#[[two words]]</span>, \
             <span class=\"link\" data-page=\"Bold\"><strong>[[Bold]]</strong></span>"
        ));
        assert!(
            !html.contains("<a "),
            "no links to pages that aren't exported"
        );
    }

    #[test]
    fn pdf_html_uses_a4_and_configured_margin() {
        let page = Page::from_markdown("Test", false, "- [[Other]] #tag\n");
        let html = page_html_for_pdf(&page, &no_refs, &|_| Vec::new(), &no_images, 24.0);
        assert!(html.contains("@page { size: A4; margin: 24.0mm; }"));
        assert!(html.contains("data-page=\"Other\""));
        assert!(html.contains("class=\"tag\""));
        let html = page_html_for_pdf(&page, &no_refs, &|_| Vec::new(), &no_images, f32::NAN);
        assert!(html.contains("@page { size: A4; margin: 18.0mm; }"));
    }

    #[test]
    fn block_references_show_the_referenced_text() {
        let id = Uuid::new_v4();
        let missing = Uuid::new_v4();
        let page = Page::from_markdown(
            "Test",
            false,
            &format!("- quote: (({id})) end\n- gone: (({missing}))\n"),
        );
        let resolve = |wanted: Uuid| (wanted == id).then(|| "TODO **the** <answer>".to_string());
        let html = page_html(&page, &resolve, &no_images);
        assert!(
            html.contains("quote: <span class=\"ref\">the &lt;answer&gt;</span> end"),
            "{html}"
        );
        assert!(html.contains(&format!("gone: (({missing}))")));
    }

    #[test]
    fn scheduled_and_deadline_lines_get_their_own_line() {
        let html = html(
            "- TODO pay rent\n  SCHEDULED: <2026-10-09 Fri>\n  DEADLINE: <2026-10-12 Mon 17:00>\n- note\n  SCHEDULED: someday\n- SCHEDULED: <2026-10-09> first line\n",
        );
        assert!(html.contains(
            "<span class=\"task task-todo\">TODO</span><div class=\"text\">pay rent</div>\
             <div class=\"planning\"><span class=\"planning-kw\">SCHEDULED:</span> \
             &lt;2026-10-09 Fri&gt;</div>\
             <div class=\"planning\"><span class=\"planning-kw\">DEADLINE:</span> \
             &lt;2026-10-12 Mon 17:00&gt;</div></div>"
        ));
        // Not a date the agenda reads: ordinary text.
        assert!(html.contains(&text("note<br>\nSCHEDULED: someday")));
        // The block's own first line is never a planning line.
        assert!(html.contains(&text("SCHEDULED: &lt;2026-10-09&gt; first line")));
    }

    #[test]
    fn code_blocks_and_tables() {
        let html = html(
            "- before\n  ```rust\n  fn main() { a && b }\n  ```\n  after\n- | Name | Qty |\n  | :--- | --: |\n  | **tea** | 2 |\n",
        );
        assert!(html.contains(
            "<div class=\"text\">before</div><pre><div class=\"code-lang\">rust</div>\
             <code>fn main() { a &amp;&amp; b }</code></pre><div class=\"text\">after</div>"
        ));
        assert!(html.contains(
            "<table><thead><tr><th class=\"left\">Name</th>\
             <th class=\"right\">Qty</th></tr></thead><tbody><tr>\
             <td class=\"left\"><strong>tea</strong></td>\
             <td class=\"right\">2</td></tr></tbody></table>"
        ));
        // Links inside code are code.
        assert!(!self::html("- ```\n  [[x]] #y\n  ```\n").contains("data-page"));
    }

    #[test]
    fn images_are_embedded_as_data_uris() {
        let page = Page::from_markdown(
            "Test",
            false,
            "- look ![a \"pic\"](../assets/p.png) here\n- ![gone](../assets/missing.png)\n- ![web](https://example.com/x.png)\n- ![doc](../assets/notes.pdf)\n",
        );
        let load = |target: &str| (target == "../assets/p.png").then(|| b"PNGDATA".to_vec());
        let html = page_html(&page, &no_refs, &load);
        assert!(html.contains(
            "<div class=\"text\">look </div>\
             <img src=\"data:image/png;base64,UE5HREFUQQ==\" alt=\"a &quot;pic&quot;\">\
             <div class=\"text\"> here</div>"
        ));
        assert!(html.contains(
            "<span class=\"image-missing\">Image not found: ../assets/missing.png</span>"
        ));
        assert!(html.contains(
            "<span class=\"image-missing\">Web image not embedded: https://example.com/x.png</span>"
        ));
        assert!(
            html.contains("<span class=\"image-missing\">Not an image: ../assets/notes.pdf</span>")
        );
        assert!(!html.contains("src=\"http") && !html.contains("src=\"../"));
        assert_eq!(image_mime("A.JPG"), Some("image/jpeg"));
        // Voice notes (decision 52) stay out, even when the file exists.
        let page = Page::from_markdown("V", false, "- ![voice note](../assets/voice-1.wav)\n");
        let load = |_: &str| Some(b"RIFF....WAVE".to_vec());
        let html = page_html(&page, &no_refs, &load);
        assert!(html.contains(
            "<span class=\"image-missing\">Voice note not included: ../assets/voice-1.wav</span>"
        ));
        assert!(!html.contains("base64") && !html.contains("<audio"));
        assert_eq!(image_mime("x.svg"), None);
    }

    #[test]
    fn base64_matches_rfc_4648() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected, "{input}");
        }
        assert_eq!(base64(&[0xff, 0xfe, 0x00]), "//4A");
    }

    #[test]
    fn embeds_are_drawn_inline_with_the_same_guards() {
        let id = "6f9b2c1e-0000-4000-8000-0000000000e1";
        let pages = vec![
            Page::from_markdown(
                "Host",
                false,
                &format!("- intro\n  - ![[Beta]]\n- ![[(({id}))]]\n- ![[Host]] ![[Gone]]\n"),
            ),
            Page::from_markdown(
                "Beta",
                false,
                &format!("- b <one>\n  id:: {id}\n  - b child\n- **two**\n"),
            ),
        ];
        let resolver = crate::embed::Resolver::new(&pages, None);
        let embeds = |c: &str| resolver.resolve(c, Some(0));
        let html = page_html_with_embeds(&pages[0], &no_refs, &embeds, &no_images);
        let doc = outline(&html);
        let beta = format!(
            "<div class=\"embed\"><div class=\"embed-title\" data-page=\"Beta\">Beta</div>\
             <ul class=\"embed-outline\">\n<li>{}\n<ul>\n<li>{}</li>\n</ul>\n</li>\n<li>{}</li>\n</ul>\n</div>",
            text("b &lt;one&gt;"),
            text("b child"),
            text("<strong>two</strong>"),
        );
        assert!(doc.contains(&beta), "{doc}");
        // The block embed: the block and its child only.
        assert!(doc.contains(&format!(
            "<ul class=\"embed-outline\">\n<li>{}\n<ul>\n<li>{}</li>\n</ul>\n</li>\n</ul>\n</div>",
            text("b &lt;one&gt;"),
            text("b child")
        )));
        assert!(
            doc.contains("<div class=\"embed-note\">Circular embed of \u{201c}Host\u{201d}</div>")
        );
        assert!(doc.contains("<div class=\"embed-note\">Page \u{201c}Gone\u{201d} not found</div>"));
        assert!(html.contains(".embed {"));
        // Without a resolver, embeds stay as written.
        assert!(!page_html(&pages[0], &no_refs, &no_images).contains("class=\"embed"));
    }
}

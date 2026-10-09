//! Publish a page as a static web site: a folder the user hosts anywhere
//! (decision 49). No GPUI; `app/publish_ui.rs` runs it and shows the result.
//!
//! ```text
//! <graph>/published/<slug>/
//!     index.html          the page
//!     <slug>.html         each linked page (with "Publish page with linked pages")
//!     style.css           the export's stylesheet (`export::CSS`)
//!     assets/<name>       the images the pages show, copied
//!     README.txt          how to host it
//!     .notesec-bundle     which page this is and which files the app wrote
//! ```
//!
//! The pages are rendered by the HTML exporter (`export::document`), so a
//! published page looks like an export: embeds inline, block references
//! resolved, no scripts, nothing fetched from elsewhere (a CSP says so).
//! Differences: the stylesheet and images are files next to the HTML
//! (lighter, cacheable), links between the published pages are real
//! relative links, and property lines (`alias::`, `public::`, ...) are left
//! out (`id::` never reaches the content).
//!
//! Images are re-encoded from their pixels ([`clean_image`]), so no EXIF
//! (camera, GPS position, time), XMP, ICC profile or text chunk is
//! published; an EXIF orientation is applied first.
//!
//! Private pages: a page whose first block says `public:: false` or
//! `private:: true` is never published (refused as the page itself; left
//! out as a linked page, as an embed and as the target of a block
//! reference).
//!
//! The bundle owns only the files its manifest lists: publishing again
//! rewrites them and deletes the ones no longer needed, and leaves anything
//! else in the folder alone. Nothing outside the folder is written or
//! deleted.

use std::cell::RefCell;
use std::fs;
use std::io::{self, Cursor};
use std::path::{Component, Path, PathBuf};

use crate::assets::is_image_path;
use crate::embed::{Resolved, Resolver};
use crate::export::{self, ImageSrc, Options};
use crate::model::{find_block, parse_references, resolve_page, Page};
use image::codecs::gif::{GifDecoder, GifEncoder, Repeat};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::metadata::Orientation;
use image::{AnimationDecoder, ColorType, DynamicImage, ImageDecoder, ImageFormat, ImageReader};

/// Where bundles go, inside the graph: not `pages/` or `journals/`, so a
/// bundle is never loaded as pages; not backed up (`backup::IGNORED`).
pub const PUBLISHED_DIR: &str = "published";
/// The bundle's manifest: its first line, the page's title, its files.
pub const MANIFEST: &str = ".notesec-bundle";
const MANIFEST_HEADER: &str = "notesec published bundle";
const ASSETS: &str = "assets";
/// Longest slug, in bytes (ASCII); longer ones are cut at a hyphen.
const MAX_SLUG: usize = 60;

const README: &str = "This folder is \"{title}\", published with notesec as a static web site.
index.html is the page; open it in a browser straight from disk, or host
the folder anywhere that serves static files:

- GitHub Pages: put the files in a repository and turn on Pages.
- Netlify Drop: drag the folder onto https://app.netlify.com/drop
- Any web server; to try it locally run `python3 -m http.server` in this
  folder and open http://localhost:8000

There are no scripts and nothing is loaded from other sites.
Publishing the page again from notesec replaces the files listed in
.notesec-bundle and removes the ones it no longer needs. Files you add
here yourself are left alone.
";

/// One `key:: value` property line: (key, value).
fn property(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    let at = line.find("::")?;
    let key = &line[..at];
    let rest = &line[at + 2..];
    let key_ok = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    (key_ok && (rest.is_empty() || rest.starts_with(char::is_whitespace)))
        .then(|| (key, rest.trim()))
}

/// The page asks not to be published: `public:: false` or `private:: true`
/// in its first block (its page properties), keys and values in any case.
pub fn is_private(page: &Page) -> bool {
    let Some(first) = page.blocks.first() else {
        return false;
    };
    first
        .content
        .split('\n')
        .filter_map(property)
        .any(|(k, v)| {
            (k.eq_ignore_ascii_case("public") && v.eq_ignore_ascii_case("false"))
                || (k.eq_ignore_ascii_case("private") && v.eq_ignore_ascii_case("true"))
        })
}

/// `content` without its property lines (those inside fenced code stay).
pub fn strip_properties(content: &str) -> String {
    let code = crate::code::fenced_ranges(content);
    let mut kept: Vec<&str> = Vec::new();
    let mut at = 0;
    for line in content.split('\n') {
        let in_code = code.iter().any(|c| c.contains(&at));
        if in_code || property(line).is_none() {
            kept.push(line);
        }
        at += line.len() + 1;
    }
    kept.join("\n")
}

/// Latin letters with marks, and ligatures, as ASCII (lowercase input).
fn fold(c: char, out: &mut String) {
    const TABLE: [(&str, &str); 26] = [
        ("àáâãäåāăą", "a"),
        ("çćĉċč", "c"),
        ("ďđ", "d"),
        ("èéêëēĕėęě", "e"),
        ("ĝğġģ", "g"),
        ("ĥħ", "h"),
        ("ìíîïĩīĭįı", "i"),
        ("ĵ", "j"),
        ("ķ", "k"),
        ("ĺļľŀł", "l"),
        ("ñńņňŉ", "n"),
        ("òóôõöøōŏő", "o"),
        ("ŕŗř", "r"),
        ("śŝşšș", "s"),
        ("ţťŧț", "t"),
        ("ùúûüũūŭůűų", "u"),
        ("ŵ", "w"),
        ("ýÿŷ", "y"),
        ("źżž", "z"),
        ("ß", "ss"),
        ("æ", "ae"),
        ("œ", "oe"),
        ("þ", "th"),
        ("ð", "d"),
        ("ĳ", "ij"),
        ("ſ", "s"),
    ];
    if c.is_ascii() {
        out.push(c);
    } else if let Some((_, ascii)) = TABLE.iter().find(|(from, _)| from.contains(c)) {
        out.push_str(ascii);
    }
}

/// A file-name-safe name for `title`: lowercase ASCII letters and digits
/// (accents folded: "Café Ünïcode" is `cafe-unicode`), every other run of
/// characters one hyphen, at most `MAX_SLUG` bytes; "page" if nothing is
/// left (a title in another script).
pub fn slug(title: &str) -> String {
    let mut folded = String::new();
    for c in title.chars().flat_map(char::to_lowercase) {
        fold(c, &mut folded);
    }
    let mut out = String::new();
    for word in folded.split(|c: char| !c.is_ascii_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        let extra = word.len() + usize::from(!out.is_empty());
        if out.len() + extra > MAX_SLUG {
            if out.is_empty() {
                out.push_str(&word[..MAX_SLUG]);
            }
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(word);
    }
    if out.is_empty() {
        "page".to_string()
    } else {
        out
    }
}

/// `base`, or `base-2`, `base-3`, ... : the first that `taken` refuses.
fn unique(base: &str, taken: impl Fn(&str) -> bool) -> String {
    (1..)
        .map(|n| {
            if n == 1 {
                base.to_string()
            } else {
                format!("{base}-{n}")
            }
        })
        .find(|name| !taken(name))
        .expect("an unused name")
}

/// The files of a bundle, before they're written.
pub struct Bundle {
    /// (path relative to the bundle folder, bytes).
    pub files: Vec<(String, Vec<u8>)>,
    /// The pages in it (indices into `pages`), the published page first.
    pub pages: Vec<usize>,
}

/// JPEG quality for re-encoded photos.
const JPEG_QUALITY: u8 = 90;

/// An image's pixels in a fresh file with no metadata, and the extension
/// for it; `None` if the `image` crate can't decode it (then it isn't
/// published: copying it could carry the metadata this exists to drop).
/// The format is read from the bytes, not the file name. The encoders
/// write metadata only when told to (`set_exif_metadata`,
/// `set_icc_profile`), which this never does.
///
/// - JPEG stays JPEG (quality `JPEG_QUALITY`): no APP1 (EXIF, XMP),
///   APP2 (ICC), APP13 (IPTC) or comment segments. EXIF orientation is
///   applied to the pixels, so photos stand the right way up.
/// - PNG stays PNG, lossless: no `tEXt` / `iTXt` / `zTXt` / `eXIf` /
///   `iCCP` chunks.
/// - WebP stays WebP, re-encoded lossless (the crate has no lossy WebP
///   encoder): no EXIF / XMP / ICC chunks; an animation keeps its first
///   frame.
/// - GIF: an animation stays a GIF (frames and delays kept, looping
///   forever; no comment or application extensions besides the loop);
///   a single frame becomes PNG.
/// - BMP and TIFF become PNG (browsers mostly can't show TIFF, and TIFF
///   tags carry EXIF and GPS); orientation applied.
pub fn clean_image(bytes: &[u8]) -> Option<(Vec<u8>, &'static str)> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let format = reader.format()?;
    if format == ImageFormat::Gif {
        let frames = GifDecoder::new(Cursor::new(bytes))
            .ok()?
            .into_frames()
            .collect_frames()
            .ok()?;
        if frames.len() > 1 {
            let mut out = Vec::new();
            {
                let mut encoder = GifEncoder::new(&mut out);
                encoder.set_repeat(Repeat::Infinite).ok()?;
                encoder.encode_frames(frames).ok()?;
            }
            return Some((out, "gif"));
        }
    }
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    image.apply_orientation(orientation);
    let color = image.color();
    let mut out = Vec::new();
    let ext = match format {
        ImageFormat::Jpeg => {
            let image = if color.has_color() {
                DynamicImage::ImageRgb8(image.to_rgb8())
            } else {
                DynamicImage::ImageLuma8(image.to_luma8())
            };
            image
                .write_with_encoder(JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY))
                .ok()?;
            "jpg"
        }
        ImageFormat::WebP => {
            let image = if color.has_alpha() {
                DynamicImage::ImageRgba8(image.to_rgba8())
            } else {
                DynamicImage::ImageRgb8(image.to_rgb8())
            };
            image
                .write_with_encoder(WebPEncoder::new_lossless(&mut out))
                .ok()?;
            "webp"
        }
        _ => {
            // PNG takes 8 and 16 bits per channel, not floats.
            let image = match color {
                ColorType::Rgb32F | ColorType::Rgba32F => {
                    DynamicImage::ImageRgba16(image.to_rgba16())
                }
                _ => image,
            };
            image.write_with_encoder(PngEncoder::new(&mut out)).ok()?;
            "png"
        }
    };
    Some((out, ext))
}

/// An image copied into `assets/`.
struct Asset {
    source: PathBuf,
    name: String,
    bytes: Vec<u8>,
}

/// The image `target` (as a page writes it) as a bundle file: only image
/// files inside the graph, and not inside a hidden folder (`.trash`) or
/// another bundle or export. The same file is copied once.
fn asset(root: &Path, target: &str, assets: &RefCell<Vec<Asset>>) -> ImageSrc {
    let Some(path) = crate::assets::resolve(root, target) else {
        return ImageSrc::Missing;
    };
    let (Ok(real), Ok(real_root)) = (path.canonicalize(), root.canonicalize()) else {
        return ImageSrc::Missing;
    };
    let Ok(inside) = real.strip_prefix(&real_root) else {
        return ImageSrc::Withheld;
    };
    let hidden = inside
        .components()
        .any(|c| c.as_os_str().to_str().is_none_or(|s| s.starts_with('.')));
    let first = inside.components().next().map(|c| c.as_os_str().to_owned());
    let generated = first.is_some_and(|f| f == PUBLISHED_DIR || f == crate::storage::EXPORTS_DIR);
    if !real.is_file() {
        return ImageSrc::Missing;
    }
    if hidden || generated || !is_image_path(&real) {
        return ImageSrc::Withheld;
    }
    if let Some(a) = assets.borrow().iter().find(|a| a.source == real) {
        return ImageSrc::Url(format!("{ASSETS}/{}", a.name));
    }
    let Ok(original) = fs::read(&real) else {
        return ImageSrc::Missing;
    };
    let Some((bytes, ext)) = clean_image(&original) else {
        return ImageSrc::Withheld;
    };
    let stem = real.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
    let mut assets = assets.borrow_mut();
    let base = slug(stem);
    let name = unique(&base, |n| {
        let file = format!("{n}.{ext}");
        assets.iter().any(|a| a.name == file)
    });
    let name = format!("{name}.{ext}");
    assets.push(Asset {
        source: real,
        name: name.clone(),
        bytes,
    });
    ImageSrc::Url(format!("{ASSETS}/{name}"))
}

/// An embed as published: an embed of a private page (or of a block on
/// one) becomes a note, and property lines are left out, all the way down.
fn redact(embed: Resolved, pages: &[Page]) -> Resolved {
    let private = |title: &str| pages.iter().any(|p| p.title == title && is_private(p));
    let clean = |rows: Vec<crate::embed::EmbedRow>| {
        rows.into_iter()
            .map(|mut row| {
                row.content = strip_properties(&row.content);
                row.embeds = row.embeds.into_iter().map(|e| redact(e, pages)).collect();
                row
            })
            .collect()
    };
    match embed {
        Resolved::Page { title, .. } | Resolved::Block { title, .. } if private(&title) => {
            Resolved::Missing("Private page, not published".to_string())
        }
        Resolved::Page { title, rows } => Resolved::Page {
            title,
            rows: clean(rows),
        },
        Resolved::Block { title, rows } => Resolved::Block {
            title,
            rows: clean(rows),
        },
        note => note,
    }
}

/// The pages `main` links to (`[[links]]` and `#tags` in its own blocks,
/// outside property lines) that may be published with it: existing, not
/// journals, not private, each once, in order of first link.
pub fn linked_pages(pages: &[Page], main: usize) -> Vec<usize> {
    let mut out = Vec::new();
    for block in &pages[main].blocks {
        for r in parse_references(&strip_properties(&block.content)) {
            if let Some(p) = resolve_page(pages, &r.target) {
                if p != main && !out.contains(&p) && !pages[p].is_journal && !is_private(&pages[p])
                {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// Render page `main` (and, `with_linked`, the pages it links to) as a
/// bundle. Images are read from the graph at `root`. Refuses a private page.
pub fn build(
    root: &Path,
    pages: &[Page],
    main: usize,
    with_linked: bool,
) -> Result<Bundle, String> {
    let page = &pages[main];
    if is_private(page) {
        return Err(format!(
            "\u{201c}{}\u{201d} is private (public:: false or private:: true), so it isn't published",
            page.title
        ));
    }
    let mut included = vec![main];
    if with_linked {
        included.extend(linked_pages(pages, main));
    }
    // The published page is index.html; the others get their slug.
    let mut names: Vec<(usize, String)> = Vec::new();
    for &p in &included {
        let name = if p == main {
            "index".to_string()
        } else {
            unique(&slug(&pages[p].title), |n| {
                n == "index" || names.iter().any(|(_, m)| m == n)
            })
        };
        names.push((p, name));
    }
    let href = |target: &str| {
        let p = resolve_page(pages, target)?;
        names
            .iter()
            .find(|(q, _)| *q == p)
            .map(|(_, n)| format!("{n}.html"))
    };
    let resolve_ref = |id| {
        find_block(pages, id)
            .filter(|(p, _)| !is_private(&pages[*p]))
            .map(|(p, b)| strip_properties(&pages[p].blocks[b].content))
    };
    let assets = RefCell::new(Vec::new());
    let image = |target: &str| asset(root, target, &assets);
    let options = Options {
        resolve_ref: &resolve_ref,
        image: &image,
        href: &href,
        stylesheet: Some("style.css"),
        show_paths: false,
    };
    let resolver = Resolver::new(pages, None);
    let mut files = Vec::new();
    for (p, name) in &names {
        let page = &pages[*p];
        let contents: Vec<String> = page
            .blocks
            .iter()
            .map(|b| strip_properties(&b.content))
            .collect();
        let mut rows = Vec::new();
        for (ix, block) in page.blocks.iter().enumerate() {
            let depth = page.depth_of(ix);
            let content = contents[ix].as_str();
            let embeds: Vec<Resolved> = resolver
                .resolve(content, Some(*p))
                .into_iter()
                .map(|e| redact(e, pages))
                .collect();
            // A block that only held properties (the page properties)
            // goes, unless it has children.
            let has_children = page.subtree_end(ix) > ix + 1;
            if content.trim().is_empty() && !block.content.trim().is_empty() && !has_children {
                continue;
            }
            rows.push((depth, content, embeds));
        }
        let html = export::document(&page.title, page.is_journal, rows, &options);
        files.push((format!("{name}.html"), html.into_bytes()));
    }
    files.push((
        "style.css".to_string(),
        export::CSS.trim_start().as_bytes().to_vec(),
    ));
    files.push((
        "README.txt".to_string(),
        README.replace("{title}", &page.title).into_bytes(),
    ));
    for a in assets.into_inner() {
        files.push((format!("{ASSETS}/{}", a.name), a.bytes));
    }
    Ok(Bundle {
        files,
        pages: included,
    })
}

/// What a bundle's manifest says: (page title, files).
fn read_manifest(dir: &Path) -> Option<(String, Vec<String>)> {
    let text = fs::read_to_string(dir.join(MANIFEST)).ok()?;
    let mut lines = text.lines();
    if lines.next()? != MANIFEST_HEADER {
        return None;
    }
    let mut title = None;
    let mut files = Vec::new();
    for line in lines {
        if let Some(t) = line.strip_prefix("title: ") {
            title = Some(t.to_string());
        } else if let Some(f) = line.strip_prefix("file: ") {
            files.push(f.to_string());
        }
    }
    Some((title?, files))
}

fn same_title(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// The folder for page `title`'s bundle under `published`: the bundle
/// already published for it (by its manifest), else `<slug>`, `<slug>-2`,
/// ... whichever doesn't exist yet. A folder the app didn't write is
/// never used.
pub fn bundle_dir(published: &Path, title: &str) -> PathBuf {
    if let Ok(entries) = fs::read_dir(published) {
        let mut mine: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|d| read_manifest(d).is_some_and(|(t, _)| same_title(&t, title)))
            .collect();
        mine.sort();
        if let Some(dir) = mine.into_iter().next() {
            return dir;
        }
    }
    let name = unique(&slug(title), |n| {
        published.join(n).symlink_metadata().is_ok()
    });
    published.join(name)
}

/// `rel` under `dir`, if it is a plain relative path whose folders inside
/// `dir` aren't symbolic links (so nothing outside `dir` can be reached).
fn inside(dir: &Path, rel: &str) -> Option<PathBuf> {
    let path = Path::new(rel);
    let parts: Vec<_> = path.components().collect();
    if parts.is_empty() || !parts.iter().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    let mut at = dir.to_path_buf();
    for part in &parts[..parts.len() - 1] {
        at.push(part);
        if let Ok(meta) = at.symlink_metadata() {
            if !meta.is_dir() {
                return None;
            }
        }
    }
    Some(dir.join(path))
}

/// Write `files` as page `title`'s bundle in `dir`: create or replace them,
/// delete the files the previous publish wrote that aren't needed any
/// more, and record the new list in the manifest. Refuses a folder that
/// isn't this page's bundle.
pub fn write_bundle(dir: &Path, title: &str, files: &[(String, Vec<u8>)]) -> io::Result<()> {
    let refuse = |why: &str| io::Error::other(format!("{}: {why}", dir.display()));
    let old = match dir.symlink_metadata() {
        Ok(meta) if !meta.is_dir() => return Err(refuse("not a folder")),
        Ok(_) => match read_manifest(dir) {
            Some((t, files)) if same_title(&t, title) => files,
            _ => return Err(refuse("not this page's published bundle")),
        },
        Err(_) => Vec::new(),
    };
    fs::create_dir_all(dir)?;
    for (rel, bytes) in files {
        let path = inside(dir, rel).ok_or_else(|| refuse("a file name that leaves the bundle"))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Writing through a link would write wherever it points.
        if path.symlink_metadata().is_ok_and(|m| m.is_symlink()) {
            fs::remove_file(&path)?;
        }
        fs::write(&path, bytes)?;
    }
    for rel in old.iter().filter(|r| !files.iter().any(|(f, _)| f == *r)) {
        let Some(path) = inside(dir, rel) else {
            continue;
        };
        if path
            .symlink_metadata()
            .is_ok_and(|m| m.is_file() || m.is_symlink())
        {
            fs::remove_file(&path)?;
        }
    }
    // Only if empty (and a real folder).
    let _ = fs::remove_dir(dir.join(ASSETS));
    let mut manifest = format!("{MANIFEST_HEADER}\ntitle: {title}\n");
    for (rel, _) in files {
        manifest.push_str(&format!("file: {rel}\n"));
    }
    fs::write(dir.join(MANIFEST), manifest)
}

/// A finished publish.
#[derive(Debug, PartialEq)]
pub struct Published {
    /// The bundle folder.
    pub dir: PathBuf,
    /// How many pages are in it.
    pub pages: usize,
}

/// Publish page `main` of the graph at `root` (see the module docs).
pub fn publish(
    root: &Path,
    pages: &[Page],
    main: usize,
    with_linked: bool,
) -> Result<Published, String> {
    let bundle = build(root, pages, main, with_linked)?;
    let title = &pages[main].title;
    let dir = bundle_dir(&root.join(PUBLISHED_DIR), title);
    write_bundle(&dir, title, &bundle.files).map_err(|err| format!("Publishing failed: {err}"))?;
    Ok(Published {
        dir,
        pages: bundle.pages.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// A graph folder with `pages/` (image paths are relative to it) and
    /// `assets/a.png`, `assets/b.png`.
    fn graph(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("notesec-publish-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("pages")).unwrap();
        fs::create_dir_all(dir.join("assets")).unwrap();
        fs::write(
            dir.join("assets/a.png"),
            encoded(&solid(2, 2, [200, 0, 0]), ImageFormat::Png),
        )
        .unwrap();
        fs::write(
            dir.join("assets/B Pic.PNG"),
            encoded(&solid(3, 1, [0, 0, 200]), ImageFormat::Png),
        )
        .unwrap();
        dir
    }

    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> DynamicImage {
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(w, h, image::Rgb(rgb)))
    }

    fn encoded(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    fn decoded(bytes: &[u8]) -> DynamicImage {
        image::load_from_memory(bytes).unwrap()
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    const GPS_MARKER: &[u8] = b"GPS-MARKER-52.5200N-13.4050E";

    /// A JPEG whose left half is red and right half blue (16x8), with an
    /// EXIF APP1 segment right after SOI: orientation 6 (turn 90°
    /// clockwise to show) and a GPS IFD holding `GPS_MARKER`.
    fn jpeg_with_exif() -> Vec<u8> {
        let mut img = image::RgbImage::from_pixel(16, 8, image::Rgb([0, 0, 255]));
        for y in 0..8 {
            for x in 0..8 {
                img.put_pixel(x, y, image::Rgb([255, 0, 0]));
            }
        }
        let mut plain = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_with_encoder(JpegEncoder::new_with_quality(&mut plain, 100))
            .unwrap();
        // TIFF header (little endian), IFD0 at 8 with two entries.
        let mut tiff: Vec<u8> = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 2, 0];
        tiff.extend([0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]); // Orientation = 6
        tiff.extend([0x25, 0x88, 4, 0, 1, 0, 0, 0, 38, 0, 0, 0]); // GPSInfo IFD at 38
        tiff.extend([0, 0, 0, 0]);
        assert_eq!(tiff.len(), 38);
        // GPS IFD: one entry, GPSLatitudeRef = "N".
        tiff.extend([
            1, 0, 0x01, 0x00, 2, 0, 2, 0, 0, 0, b'N', 0, 0, 0, 0, 0, 0, 0,
        ]);
        tiff.extend(GPS_MARKER);
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend(tiff);
        let len = u16::try_from(app1.len() + 2).unwrap().to_be_bytes();
        let mut out = plain[..2].to_vec();
        out.extend([0xFF, 0xE1, len[0], len[1]]);
        out.extend(app1);
        out.extend(&plain[2..]);
        out
    }

    /// The markers of a JPEG's segments up to the image data.
    fn jpeg_markers(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut at = 2;
        while at + 4 <= bytes.len() && bytes[at] == 0xFF {
            let marker = bytes[at + 1];
            out.push(marker);
            if marker == 0xDA {
                break;
            }
            at += 2 + usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
        }
        out
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// A 4x4 PNG with a `tEXt` chunk holding `GPS_MARKER` after IHDR.
    fn png_with_text() -> (Vec<u8>, DynamicImage) {
        let img = solid(4, 4, [10, 120, 30]);
        let plain = encoded(&img, ImageFormat::Png);
        let mut data = b"Comment\0".to_vec();
        data.extend(GPS_MARKER);
        let mut chunk = u32::try_from(data.len()).unwrap().to_be_bytes().to_vec();
        let mut body = b"tEXt".to_vec();
        body.extend(&data);
        chunk.extend(&body);
        chunk.extend(crc32(&body).to_be_bytes());
        // Signature (8) + IHDR (4 length + 4 type + 13 data + 4 crc).
        let mut out = plain[..33].to_vec();
        out.extend(chunk);
        out.extend(&plain[33..]);
        (out, img)
    }

    #[test]
    fn jpeg_exif_and_gps_are_dropped_and_the_orientation_applied() {
        let input = jpeg_with_exif();
        // The crafted EXIF is real to the decoder.
        let mut decoder = ImageReader::new(Cursor::new(&input))
            .with_guessed_format()
            .unwrap()
            .into_decoder()
            .unwrap();
        assert_eq!(decoder.orientation().unwrap(), Orientation::Rotate90);
        assert!(contains(&input, GPS_MARKER) && contains(&input, b"Exif\0\0"));

        let (output, ext) = clean_image(&input).unwrap();
        assert_eq!(ext, "jpg");
        assert!(!contains(&output, b"Exif"), "no EXIF header");
        assert!(!contains(&output, GPS_MARKER));
        let markers = jpeg_markers(&output);
        assert!(markers.contains(&0xDA), "{markers:x?}");
        for app in [0xE1, 0xE2, 0xED, 0xFE] {
            assert!(!markers.contains(&app), "APP/COM {app:x} in {markers:x?}");
        }
        // Turned: 8 wide, 16 high, red (the old left) on top.
        let shown = decoded(&output).to_rgb8();
        assert_eq!(shown.dimensions(), (8, 16));
        let top = shown.get_pixel(4, 3).0;
        let bottom = shown.get_pixel(4, 12).0;
        assert!(top[0] > 200 && top[2] < 60, "{top:?}");
        assert!(bottom[2] > 200 && bottom[0] < 60, "{bottom:?}");
        let mut after = ImageReader::new(Cursor::new(&output))
            .with_guessed_format()
            .unwrap()
            .into_decoder()
            .unwrap();
        assert_eq!(after.orientation().unwrap(), Orientation::NoTransforms);
    }

    #[test]
    fn png_text_chunks_are_dropped_and_the_pixels_kept() {
        let (input, original) = png_with_text();
        assert!(contains(&input, b"tEXt") && contains(&input, GPS_MARKER));
        assert_eq!(
            decoded(&input).to_rgba8(),
            original.to_rgba8(),
            "a valid PNG"
        );
        let (output, ext) = clean_image(&input).unwrap();
        assert_eq!(ext, "png");
        for chunk in [&b"tEXt"[..], b"iTXt", b"zTXt", b"eXIf", b"iCCP"] {
            assert!(!contains(&output, chunk));
        }
        assert!(!contains(&output, GPS_MARKER));
        assert_eq!(decoded(&output).to_rgba8(), original.to_rgba8(), "lossless");
    }

    #[test]
    fn each_format_is_cleaned_into_a_format_browsers_show() {
        let img = solid(5, 3, [1, 2, 3]);
        let ext = |bytes: &[u8]| clean_image(bytes).map(|(_, e)| e);
        assert_eq!(ext(&encoded(&img, ImageFormat::WebP)), Some("webp"));
        assert_eq!(ext(&encoded(&img, ImageFormat::Bmp)), Some("png"));
        assert_eq!(ext(&encoded(&img, ImageFormat::Tiff)), Some("png"));
        assert_eq!(
            ext(&encoded(&img, ImageFormat::Gif)),
            Some("png"),
            "one frame"
        );
        // An animation stays a GIF, every frame kept.
        let frame = |rgb: [u8; 3]| {
            image::Frame::from_parts(
                image::RgbaImage::from_pixel(4, 4, image::Rgba([rgb[0], rgb[1], rgb[2], 255])),
                0,
                0,
                image::Delay::from_numer_denom_ms(100, 1),
            )
        };
        let mut gif = Vec::new();
        GifEncoder::new(&mut gif)
            .encode_frames([frame([255, 0, 0]), frame([0, 0, 255])])
            .unwrap();
        let (out, e) = clean_image(&gif).unwrap();
        assert_eq!(e, "gif");
        let frames = GifDecoder::new(Cursor::new(out))
            .unwrap()
            .into_frames()
            .collect_frames()
            .unwrap();
        assert_eq!(frames.len(), 2);
        // Not decodable: not published.
        assert_eq!(ext(b"\xFF\xD8\xFF\xE1 not really a jpeg"), None);
        assert_eq!(ext(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"), None);
    }

    #[test]
    fn published_images_are_the_cleaned_files_under_their_new_names() {
        let root = graph("cleaned");
        fs::write(root.join("assets/photo.jpg"), jpeg_with_exif()).unwrap();
        let scan = encoded(&solid(2, 2, [9, 9, 9]), ImageFormat::Bmp);
        fs::write(root.join("assets/scan.bmp"), scan).unwrap();
        fs::write(root.join("assets/broken.png"), b"\x89PNG\r\n\x1a\nbroken").unwrap();
        let text =
            "- ![p](../assets/photo.jpg) ![s](../assets/scan.bmp) ![b](../assets/broken.png)\n";
        let dir = publish(&root, &[page("Main", text)], 0, false).unwrap().dir;
        assert_eq!(
            tree(&dir),
            [
                ".notesec-bundle",
                "README.txt",
                "assets/photo.jpg",
                "assets/scan.png",
                "index.html",
                "style.css"
            ]
        );
        let html = read(&dir, "index.html");
        assert!(html.contains("<img src=\"assets/photo.jpg\" alt=\"p\">"));
        assert!(html.contains("<img src=\"assets/scan.png\" alt=\"s\">"));
        assert!(html.contains("Image not published: broken.png"));
        for file in tree(&dir) {
            let bytes = fs::read(dir.join(&file)).unwrap();
            assert!(
                !contains(&bytes, GPS_MARKER) && !contains(&bytes, b"Exif"),
                "{file}"
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    fn page(title: &str, text: &str) -> Page {
        Page::from_markdown(title, false, text)
    }

    /// Every file under `dir`, relative, sorted.
    fn tree(dir: &Path) -> Vec<String> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
            for e in fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(base, &p, out);
                } else {
                    out.push(p.strip_prefix(base).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    fn read(dir: &Path, rel: &str) -> String {
        fs::read_to_string(dir.join(rel)).unwrap()
    }

    #[test]
    fn slugs_are_lowercase_ascii_with_hyphens() {
        assert_eq!(slug("My Page"), "my-page");
        assert_eq!(slug("Café Ünïcode — Straße"), "cafe-unicode-strasse");
        assert_eq!(slug("Area/Sub: notes (2026)!"), "area-sub-notes-2026");
        assert_eq!(slug("  --Hello--  "), "hello");
        assert_eq!(slug("日本語"), "page");
        assert_eq!(slug(""), "page");
        assert_eq!(slug("Œuvre Æsir Øre"), "oeuvre-aesir-ore");
        let long = slug(&"word ".repeat(30));
        assert!(long.len() <= MAX_SLUG && !long.ends_with('-'), "{long}");
        assert_eq!(slug(&"x".repeat(80)).len(), MAX_SLUG);
        // Stable: the same title, the same slug.
        assert_eq!(slug("My Page"), slug("my   page"));
    }

    #[test]
    fn privacy_properties_and_stripping() {
        assert!(is_private(&page("P", "public:: false\n\n- body\n")));
        assert!(is_private(&page("P", "- PRIVATE:: True\n- body\n")));
        assert!(!is_private(&page("P", "public:: true\n\n- body\n")));
        assert!(
            !is_private(&page("P", "- body\n- public:: false\n")),
            "only page properties"
        );
        assert!(!is_private(&page("P", "- the public:: false idea\n")));
        assert_eq!(
            strip_properties(
                "alias:: JS\ntext\ntags:: a, b\nnote: not:: a property\n```\nk:: v\n```"
            ),
            "text\nnote: not:: a property\n```\nk:: v\n```"
        );
        assert_eq!(strip_properties("a::b"), "a::b", "no space after ::");
    }

    #[test]
    fn a_bundle_has_the_page_its_styles_images_and_readme() {
        let root = graph("structure");
        let pages = vec![page(
            "My Page",
            "alias:: Mine\npublic:: true\n\n- hello [[Other]] #tag\n  - ![pic](../assets/a.png) and again ![x](../assets/a.png)\n- ![b](../assets/B Pic.PNG)\n- | a | b |\n  | - | -: |\n  | 1 | 2 |\n",
        ), page("Other", "- other\n")];
        let published = publish(&root, &pages, 0, false).unwrap();
        let dir = root.join("published/my-page");
        assert_eq!(
            published,
            Published {
                dir: dir.clone(),
                pages: 1
            }
        );
        assert_eq!(
            tree(&dir),
            [
                ".notesec-bundle",
                "README.txt",
                "assets/a.png",
                "assets/b-pic.png",
                "index.html",
                "style.css"
            ]
        );
        let html = read(&dir, "index.html");
        assert!(html.contains("<link rel=\"stylesheet\" href=\"style.css\">"));
        assert!(html.contains("default-src 'none'; img-src 'self'; style-src 'self'"));
        assert!(
            !html.contains("<script") && !html.contains("<style>") && !html.contains("style=\"")
        );
        assert!(html.contains("<img src=\"assets/a.png\" alt=\"pic\">"));
        assert_eq!(
            html.matches("assets/a.png").count(),
            2,
            "one file, used twice"
        );
        assert!(html.contains("<img src=\"assets/b-pic.png\""));
        assert_eq!(
            decoded(&fs::read(dir.join("assets/a.png")).unwrap()).to_rgb8(),
            solid(2, 2, [200, 0, 0]).to_rgb8()
        );
        // Not published: Other is a styled span, not a link.
        assert!(html.contains("<span class=\"link\" data-page=\"Other\">"));
        assert!(!html.contains("href=\"other"));
        // Page properties are left out, the block that held only them too.
        assert!(!html.contains("alias") && !html.contains("public::"));
        assert!(html.contains("<td class=\"right\">2</td>"));
        assert_eq!(read(&dir, "style.css"), export::CSS.trim_start());
        let readme = read(&dir, "README.txt");
        assert!(readme.contains("\"My Page\"") && readme.contains("python3 -m http.server"));
        assert_eq!(
            read(&dir, MANIFEST),
            "notesec published bundle\ntitle: My Page\nfile: index.html\nfile: style.css\n\
             file: README.txt\nfile: assets/a.png\nfile: assets/b-pic.png\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn linked_pages_get_relative_links_and_private_ones_stay_out() {
        let root = graph("linked");
        let secret = Uuid::new_v4();
        let open = Uuid::new_v4();
        let pages = vec![
            page(
                "Main",
                &format!(
                    "- see [[Beta]], #gamma, [[Diary]], [[Secret]], [[Nowhere]] and [[Main]]\n\
                     - ![[Beta]]\n- ![[Secret]]\n- ![[(({secret}))]]\n\
                     - ref (({open})) and (({secret}))\n"
                ),
            ),
            page(
                "Beta",
                &format!("- beta, back to [[Main]] and on to [[Gamma]]\n  id:: {open}\n"),
            ),
            page("Gamma", "- tags:: x\n- gamma\n"),
            page(
                "Secret",
                &format!("private:: true\n\n- the secret plan\n  id:: {secret}\n"),
            ),
            Page::from_markdown("Diary", true, "- dear diary\n"),
            page("Index", "- a page called Index\n"),
        ];
        assert_eq!(linked_pages(&pages, 0), [1, 2]);
        let published = publish(&root, &pages, 0, true).unwrap();
        assert_eq!(published.pages, 3);
        let dir = published.dir;
        assert_eq!(
            tree(&dir),
            [
                ".notesec-bundle",
                "README.txt",
                "beta.html",
                "gamma.html",
                "index.html",
                "style.css"
            ]
        );
        let main = read(&dir, "index.html");
        assert!(main.contains("<a class=\"link\" href=\"beta.html\" data-page=\"Beta\">"));
        assert!(main.contains("<a class=\"tag\" href=\"gamma.html\" data-page=\"gamma\">"));
        assert!(main.contains("<a class=\"link\" href=\"index.html\" data-page=\"Main\">"));
        for not_published in ["Diary", "Secret", "Nowhere"] {
            assert!(main.contains(&format!(
                "<span class=\"link\" data-page=\"{not_published}\">"
            )));
        }
        // Embeds inline; a private page's content never.
        assert!(
            main.contains("<div class=\"embed\"><div class=\"embed-title\" data-page=\"Beta\">")
        );
        assert!(!main.contains("the secret plan"));
        assert_eq!(main.matches("Private page, not published").count(), 2);
        // A block reference to a public block resolves; to a private one not.
        assert!(main.contains("<span class=\"ref\">beta, back to"));
        assert!(main.contains(&format!("(({secret}))")));
        let beta = read(&dir, "beta.html");
        assert!(beta.contains("href=\"index.html\" data-page=\"Main\""));
        assert!(beta.contains("href=\"gamma.html\""));
        assert!(!read(&dir, "gamma.html").contains("tags::"));

        // A private page itself is refused, and nothing is written.
        let err = publish(&root, &pages, 3, false).unwrap_err();
        assert!(err.contains("\u{201c}Secret\u{201d} is private"), "{err}");
        assert!(!root.join("published/secret").exists());
        // A linked page called "Index" doesn't take index.html.
        let pages2 = vec![page("Home", "- [[Index]]\n"), pages[5].clone()];
        let dir2 = publish(&root, &pages2, 0, true).unwrap().dir;
        assert!(dir2.join("index-2.html").exists());
        assert!(read(&dir2, "index.html").contains("href=\"index-2.html\""));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn publishing_again_removes_only_the_bundles_stale_files() {
        let root = graph("again");
        let mut pages = vec![
            page("Main", "- [[Beta]] ![a](../assets/a.png)\n"),
            page("Beta", "- b\n"),
        ];
        let dir = publish(&root, &pages, 0, true).unwrap().dir;
        assert!(dir.join("beta.html").exists() && dir.join("assets/a.png").exists());
        // The user's own files in the bundle, and a file next to it.
        fs::write(dir.join("CNAME"), "notes.example.org").unwrap();
        fs::create_dir_all(dir.join("extra")).unwrap();
        fs::write(dir.join("extra/keep.txt"), "mine").unwrap();
        fs::write(root.join("published/victim.txt"), "outside").unwrap();
        // A tampered manifest can't reach outside the bundle.
        let manifest = read(&dir, MANIFEST)
            + "file: ../victim.txt\nfile: /etc/hostname\nfile: extra/../../victim.txt\n";
        fs::write(dir.join(MANIFEST), manifest).unwrap();

        pages[0] = page("Main", "- no links, no images\n");
        let again = publish(&root, &pages, 0, true).unwrap();
        assert_eq!(again.dir, dir, "the same folder");
        assert_eq!(
            tree(&dir),
            [
                ".notesec-bundle",
                "CNAME",
                "README.txt",
                "extra/keep.txt",
                "index.html",
                "style.css"
            ]
        );
        assert!(
            !dir.join("assets").exists(),
            "the emptied assets folder goes"
        );
        assert_eq!(read(&root, "published/victim.txt"), "outside");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_folder_inside_the_bundle_is_never_followed() {
        let root = graph("symlink");
        let pages = vec![page("Main", "- ![a](../assets/a.png)\n")];
        let dir = publish(&root, &pages, 0, false).unwrap().dir;
        // Someone replaces assets/ with a link to a folder elsewhere that
        // has a file of the same name.
        let elsewhere = root.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("a.png"), "precious").unwrap();
        fs::remove_dir_all(dir.join("assets")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("assets")).unwrap();
        // Writing refuses; removing (the image is gone) skips it.
        assert!(publish(&root, &pages, 0, false).is_err());
        let pages = vec![page("Main", "- no image\n")];
        publish(&root, &pages, 0, false).unwrap();
        assert_eq!(read(&elsewhere, "a.png"), "precious");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn folders_are_unique_and_never_taken_from_someone_else() {
        let root = graph("dirs");
        let published = root.join(PUBLISHED_DIR);
        // A folder the app didn't write, with the slug's name.
        fs::create_dir_all(published.join("notes")).unwrap();
        fs::write(published.join("notes/mine.html"), "x").unwrap();
        let a = vec![page("Notes", "- a\n")];
        let dir = publish(&root, &a, 0, false).unwrap().dir;
        assert_eq!(dir, published.join("notes-2"));
        assert_eq!(tree(&published.join("notes")), ["mine.html"]);
        // Another page with the same slug gets its own folder; each page
        // keeps finding its own.
        let b = vec![page("notes!", "- b\n")];
        assert_eq!(
            publish(&root, &b, 0, false).unwrap().dir,
            published.join("notes-3")
        );
        assert_eq!(
            publish(&root, &a, 0, false).unwrap().dir,
            published.join("notes-2")
        );
        assert!(write_bundle(&published.join("notes"), "Notes", &[]).is_err());
        assert!(write_bundle(&published.join("notes-3"), "Notes", &[]).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn only_images_inside_the_graph_are_copied() {
        let root = graph("images");
        let outside = std::env::temp_dir().join(format!(
            "notesec-publish-outside-{}.png",
            std::process::id()
        ));
        fs::write(&outside, b"\x89PNG-outside").unwrap();
        fs::create_dir_all(root.join(".trash/x")).unwrap();
        fs::write(root.join(".trash/x/t.png"), b"t").unwrap();
        fs::write(root.join("state.toml"), "x").unwrap();
        let text = format!(
            "- ![o]({})\n- ![t](../.trash/x/t.png)\n- ![s](../state.toml)\n- ![m](../assets/missing.png)\n- ![w](https://example.org/w.png)\n",
            outside.display()
        );
        let dir = publish(&root, &[page("Main", &text)], 0, false)
            .unwrap()
            .dir;
        assert!(!dir.join("assets").exists());
        let html = read(&dir, "index.html");
        let name = outside.file_name().unwrap().to_string_lossy().into_owned();
        assert!(html.contains(&format!("Image not published: {name}")));
        assert!(
            !html.contains(&*std::env::temp_dir().to_string_lossy()),
            "no local paths"
        );
        assert!(
            html.contains("Image not published: t.png")
                && html.contains("Not an image: state.toml")
        );
        assert!(html.contains("Image not found: missing.png"));
        assert!(html.contains("Web image not embedded: https://example.org/w.png"));
        let _ = fs::remove_file(outside);
        let _ = fs::remove_dir_all(root);
    }
}

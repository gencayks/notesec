//! Images stored next to the pages, Logseq style: files under `assets/` in
//! the graph's root, referenced from blocks as
//! `![image](../assets/image-<timestamp>.png)` (relative to the page file,
//! which lives in `pages/` or `journals/`).

use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

/// File extensions accepted when image files are dropped onto a page.
const IMAGE_EXTENSIONS: [&str; 8] = ["png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff"];

/// Whether `path` looks like an image file (by its extension).
pub fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// An image reference `![alt](target)` in a block's text.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageRef {
    /// Byte range of the whole `![alt](target)`.
    pub range: Range<usize>,
    pub alt: String,
    pub target: String,
}

/// Every `![alt](target)` in `text`, in order. The alt text can't hold `]`
/// and the target can't hold `)` or a line break.
pub fn parse_images(text: &str) -> Vec<ImageRef> {
    let mut found = Vec::new();
    let mut pos = 0;
    while let Some(at) = text[pos..].find("![").map(|i| pos + i) {
        let rest = &text[at + 2..];
        let parsed = rest.find(']').and_then(|close| {
            let alt = &rest[..close];
            let after = rest[close + 1..].strip_prefix('(')?;
            let end = after.find(')')?;
            let target = &after[..end];
            let ok = !alt.contains('\n') && !target.contains('\n') && !target.trim().is_empty();
            // `![` + alt + `](` + target + `)`
            ok.then(|| (alt, target, 2 + close + 2 + end + 1))
        });
        match parsed {
            Some((alt, target, len)) => {
                found.push(ImageRef {
                    range: at..at + len,
                    alt: alt.to_string(),
                    target: target.trim().to_string(),
                });
                pos = at + len;
            }
            None => pos = at + 2,
        }
    }
    found
}

/// The file an image reference points at: relative targets are read from
/// the page's folder (`pages/` or `journals/`, both one level below
/// `root`). `None` for web addresses, which aren't fetched.
pub fn resolve(root: &Path, target: &str) -> Option<PathBuf> {
    if target.contains("://") {
        return None;
    }
    let path = Path::new(target);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join("pages").join(path)
    })
}

/// The markdown a block stores for an image saved as `assets/<file>`.
pub fn image_markdown(file: &str) -> String {
    format!("![image](../assets/{file})")
}

/// The first bytes of every PNG file.
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Save image `bytes` (kept as they are if they're a PNG, otherwise any
/// format the `image` crate reads, converted to PNG) as
/// `assets/image-<millis>.png` under `root`, creating `assets/` if needed. A name already taken gets
/// `-1`, `-2`, ... appended. Returns the file name.
pub fn save_image(root: &Path, bytes: &[u8], millis: i64) -> io::Result<String> {
    let png = if bytes.starts_with(PNG_SIGNATURE) {
        bytes.to_vec()
    } else {
        let image = image::load_from_memory(bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut out = io::Cursor::new(Vec::new());
        image
            .write_to(&mut out, image::ImageFormat::Png)
            .map_err(io::Error::other)?;
        out.into_inner()
    };
    let dir = root.join("assets");
    std::fs::create_dir_all(&dir)?;
    let mut n = 0;
    loop {
        let file = match n {
            0 => format!("image-{millis}.png"),
            n => format!("image-{millis}-{n}.png"),
        };
        // `create_new` so two saves in the same millisecond can't clash.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(&file))
        {
            Ok(mut f) => {
                io::Write::write_all(&mut f, &png)?;
                return Ok(file);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A valid 2x1 PNG, made with the `image` crate.
    pub fn tiny_png() -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(2, 1, image::Rgba([255, 0, 0, 255]));
        let mut out = io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("notesec-assets-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_image_references() {
        let text = "see ![shot](../assets/a.png) and ![](b.jpg)\n![x](  )![bad](no-close";
        let refs = parse_images(text);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].alt, "shot");
        assert_eq!(refs[0].target, "../assets/a.png");
        assert_eq!(&text[refs[0].range.clone()], "![shot](../assets/a.png)");
        assert_eq!(refs[1].target, "b.jpg");
        assert!(parse_images("[[link]] ! [x](y)").is_empty());
    }

    #[test]
    fn targets_resolve_from_the_page_folder() {
        let root = Path::new("/notes");
        assert_eq!(
            resolve(root, "../assets/a.png"),
            Some(PathBuf::from("/notes/pages/../assets/a.png"))
        );
        assert_eq!(
            resolve(root, "/abs/b.png"),
            Some(PathBuf::from("/abs/b.png"))
        );
        assert_eq!(resolve(root, "https://x.org/c.png"), None);
    }

    #[test]
    fn saves_png_creating_assets_and_never_overwrites() {
        let root = temp("save");
        let png = tiny_png();
        let first = save_image(&root, &png, 42).unwrap();
        let second = save_image(&root, &png, 42).unwrap();
        assert_eq!(
            (first.as_str(), second.as_str()),
            ("image-42.png", "image-42-1.png")
        );
        assert_eq!(
            std::fs::read(root.join("assets").join(&first)).unwrap(),
            png
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn other_formats_are_converted_to_png() {
        let root = temp("convert");
        let image = image::RgbImage::from_pixel(3, 2, image::Rgb([0, 128, 255]));
        let mut bmp = io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut bmp, image::ImageFormat::Bmp)
            .unwrap();
        let file = save_image(&root, &bmp.into_inner(), 7).unwrap();
        let saved = std::fs::read(root.join("assets").join(file)).unwrap();
        assert_eq!(
            image::guess_format(&saved).unwrap(),
            image::ImageFormat::Png
        );
        assert_eq!(image::load_from_memory(&saved).unwrap().width(), 3);
        // Not an image at all: an error, and no file is left behind.
        assert!(save_image(&root, b"nope", 8).is_err());
        assert!(!root.join("assets/image-8.png").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn image_files_are_recognised_by_extension() {
        assert!(is_image_path(Path::new("/a/B.PNG")));
        assert!(is_image_path(Path::new("c.jpeg")));
        assert!(!is_image_path(Path::new("notes.md")));
        assert!(!is_image_path(Path::new("noext")));
    }
}

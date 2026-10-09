//! Plain markdown (Obsidian, Notion) into notesec's outline, and the
//! small text helpers the importers share.

use uuid::Uuid;

use crate::model::{Block, Page};

/// A block to be: depth, text, and its id when it must be kept (a Logseq
/// block that something references).
pub type Row = (usize, String, Option<Uuid>);

/// YAML front matter (`---` ... `---` at the very top) and the rest.
pub fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (None, text);
    };
    let mut at = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" || line.trim_end() == "..." {
            return (Some(&rest[..at]), &rest[at + line.len()..]);
        }
        at += line.len();
    }
    (None, text)
}

fn unquote(value: &str) -> String {
    let v = value.trim();
    let inner = v
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')));
    inner.unwrap_or(v).to_string()
}

/// The keys and values of simple YAML front matter: `key: value`,
/// `key: [a, b]`, `key:` followed by `- item` lines. Anything fancier
/// (nested maps, multi-line strings) is read as plain text or dropped.
pub fn parse_yaml(yaml: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for line in yaml.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(item) = trimmed
            .strip_prefix("- ")
            .or((trimmed == "-").then_some(""))
        {
            if line.starts_with([' ', '\t', '-']) {
                if let Some((_, values)) = out.last_mut() {
                    let item = unquote(item);
                    if !item.is_empty() {
                        values.push(item);
                    }
                }
            }
            continue;
        }
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let value = value.trim();
        let values = if let Some(list) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
            list.split(',')
                .map(unquote)
                .filter(|v| !v.is_empty())
                .collect()
        } else if value.is_empty() {
            Vec::new()
        } else {
            vec![unquote(value)]
        };
        out.push((unquote(key), values));
    }
    out
}

/// A tag as notesec writes it in `tags::` (decision with the AI track):
/// `#x`, or `#[[y z]]` when it has spaces or characters a bare tag can't.
pub fn tag_ref(name: &str) -> String {
    let name = name.trim().trim_start_matches('#');
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '/' | '.'));
    if bare {
        format!("#{name}")
    } else {
        format!("#[[{name}]]")
    }
}

/// A property key as notesec writes it: lowercase, `-` for spaces and
/// anything that isn't a letter, digit, `_` or `-`.
pub fn property_key(key: &str) -> String {
    let mut out = String::new();
    for c in key.trim().chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Page properties from a list of (key, values): `tags`/`tag` and
/// `aliases`/`alias` in notesec's form, `title` dropped (it's the title),
/// the rest as `key:: v1, v2`. Values on one line.
pub fn properties(pairs: &[(String, Vec<String>)]) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    let mut aliases: Vec<String> = Vec::new();
    let mut other = Vec::new();
    for (key, values) in pairs {
        let values: Vec<String> = values
            .iter()
            .flat_map(|v| v.split('\n'))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect();
        match key.trim().to_lowercase().as_str() {
            "tags" | "tag" => {
                // `tags: a b` (spaces) and `tags: a, b` both happen; in a
                // list each item is one tag.
                let single = values.len() == 1;
                for v in values {
                    let split: Vec<&str> = if v.contains(',') || !single {
                        v.split(',').collect()
                    } else {
                        v.split_whitespace().collect()
                    };
                    tags.extend(
                        split
                            .into_iter()
                            .map(|t| t.trim().trim_start_matches('#').to_string())
                            .filter(|t| !t.is_empty()),
                    );
                }
            }
            "aliases" | "alias" => aliases.extend(
                values
                    .iter()
                    .flat_map(|v| v.split(','))
                    .map(|v| {
                        v.trim()
                            .trim_start_matches("[[")
                            .trim_end_matches("]]")
                            .to_string()
                    })
                    .filter(|v| !v.is_empty()),
            ),
            "title" => {}
            _ => {
                let key = property_key(key);
                if !key.is_empty() && !values.is_empty() {
                    other.push(format!("{key}:: {}", values.join(", ")));
                }
            }
        }
    }
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    tags.retain(|t| seen.insert(t.to_lowercase()));
    if !tags.is_empty() {
        let refs: Vec<String> = tags.iter().map(|t| tag_ref(t)).collect();
        out.push(format!("tags:: {}", refs.join(", ")));
    }
    if !aliases.is_empty() {
        out.push(format!("alias:: {}", aliases.join(", ")));
    }
    out.extend(other);
    out
}

/// A list item: (indent in columns, text after the marker).
fn list_item(line: &str) -> Option<(usize, &str)> {
    let indent: usize = line
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum();
    let rest = line.trim_start();
    if let Some(text) = rest
        .strip_prefix("- ")
        .or_else(|| rest.strip_prefix("* "))
        .or_else(|| rest.strip_prefix("+ "))
    {
        return Some((indent, text));
    }
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && digits < 10 {
        let after = &rest[digits..];
        if let Some(text) = after
            .strip_prefix(". ")
            .or_else(|| after.strip_prefix(") "))
        {
            return Some((indent, text));
        }
    }
    None
}

fn task(text: &str) -> String {
    if let Some(t) = text.strip_prefix("[ ] ") {
        format!("TODO {t}")
    } else if let Some(t) = text
        .strip_prefix("[x] ")
        .or_else(|| text.strip_prefix("[X] "))
    {
        format!("DONE {t}")
    } else {
        text.to_string()
    }
}

fn is_rule(line: &str) -> bool {
    let t: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    t.len() >= 3
        && (t.chars().all(|c| c == '-')
            || t.chars().all(|c| c == '*')
            || t.chars().all(|c| c == '_'))
}

fn fence(line: &str) -> Option<&str> {
    let t = line.trim_start();
    if t.starts_with("```") {
        Some("```")
    } else if t.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// Markdown as outline rows: each heading a block (`#`, `##`, `###`;
/// deeper levels as `###`) with what follows nested under it (a deeper
/// heading under a shallower one), each paragraph a block (its lines
/// kept together, so a table stays one table), list items blocks nested by
/// their indentation (`- [ ]` / `- [x]` as TODO / DONE), a fenced code block
/// one block, horizontal rules dropped. Never empty: an empty text is one
/// empty block.
pub fn outline(text: &str) -> Vec<(usize, String)> {
    let mut rows: Vec<(usize, String)> = Vec::new();
    let mut headings: Vec<usize> = Vec::new();
    let mut lists: Vec<usize> = Vec::new();
    let mut para: Vec<&str> = Vec::new();
    // The open fence: its marker and indentation.
    let mut code: Option<(&str, usize)> = None;
    let flush = |para: &mut Vec<&str>, rows: &mut Vec<(usize, String)>, depth: usize| {
        if !para.is_empty() {
            rows.push((depth, para.join("\n")));
            para.clear();
        }
    };
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        let base = headings.len();
        if let Some((close, indent)) = code {
            if let Some(last) = rows.last_mut() {
                last.1.push('\n');
                let strip = line.len() - line.trim_start_matches(' ').len();
                last.1.push_str(&line[strip.min(indent)..]);
            }
            if fence(line) == Some(close)
                && line
                    .trim()
                    .chars()
                    .all(|c| c == close.chars().next().unwrap_or('`'))
            {
                code = None;
            }
            continue;
        }
        if line.trim().is_empty() {
            flush(&mut para, &mut rows, base);
            continue;
        }
        if let Some(open) = fence(line) {
            flush(&mut para, &mut rows, base);
            let indent = line.len() - line.trim_start().len();
            let depth = match lists.last() {
                Some(&li) if indent > li => base + lists.len(),
                _ => {
                    lists.clear();
                    base
                }
            };
            rows.push((depth, line.trim_start().to_string()));
            code = Some((open, indent));
            continue;
        }
        let trimmed = line.trim_start();
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            flush(&mut para, &mut rows, base);
            lists.clear();
            while headings.last().is_some_and(|&l| l >= hashes) {
                headings.pop();
            }
            let prefix = "#".repeat(hashes.min(3));
            rows.push((
                headings.len(),
                format!("{prefix} {}", trimmed[hashes..].trim()),
            ));
            headings.push(hashes);
            continue;
        }
        if is_rule(line) && para.is_empty() {
            lists.clear();
            continue;
        }
        if let Some((indent, text)) = list_item(line) {
            flush(&mut para, &mut rows, base);
            while lists.last().is_some_and(|&li| li >= indent) {
                lists.pop();
            }
            rows.push((base + lists.len(), task(text)));
            lists.push(indent);
            continue;
        }
        if !lists.is_empty() && para.is_empty() && line.starts_with([' ', '\t']) {
            // More of the list item above.
            if let Some(last) = rows.last_mut() {
                last.1.push('\n');
                last.1.push_str(trimmed);
            }
            continue;
        }
        lists.clear();
        para.push(line);
    }
    flush(&mut para, &mut rows, headings.len());
    if rows.is_empty() {
        rows.push((0, String::new()));
    }
    rows
}

/// A link found in markdown text.
#[derive(Debug, PartialEq)]
pub enum Link<'a> {
    /// `[[target]]`, or `![[target]]` when `embed`.
    Wiki { embed: bool, inner: &'a str },
    /// `[label](target)`, or `![label](target)` when `image`.
    Markdown {
        image: bool,
        label: &'a str,
        target: &'a str,
    },
}

/// `text` with every link outside code (fenced blocks and `inline code`)
/// replaced by what `f` returns for it (`None` keeps it as written).
pub fn rewrite_links(text: &str, mut f: impl FnMut(Link) -> Option<String>) -> String {
    let fenced = crate::code::fenced_ranges(text);
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    let mut copied = 0;
    while pos < text.len() {
        if let Some(r) = fenced.iter().find(|r| r.contains(&pos)) {
            pos = r.end.max(pos + 1);
            continue;
        }
        let rest = &text[pos..];
        if rest.starts_with('`') {
            let line_end = rest.find('\n').unwrap_or(rest.len());
            pos += match rest[1..line_end].find('`') {
                Some(close) => close + 2,
                None => 1,
            };
            continue;
        }
        let bang = rest.starts_with('!');
        let open = &rest[usize::from(bang)..];
        let found = if let Some(after) = open.strip_prefix("[[") {
            after.find("]]").and_then(|end| {
                let inner = &after[..end];
                (!inner.contains('\n') && !inner.is_empty()).then(|| {
                    (
                        Link::Wiki { embed: bang, inner },
                        usize::from(bang) + 2 + end + 2,
                    )
                })
            })
        } else if let Some(after) = open.strip_prefix('[') {
            after.find(']').and_then(|close| {
                let label = &after[..close];
                let tail = after[close + 1..].strip_prefix('(')?;
                let end = tail.find(')')?;
                let target = &tail[..end];
                let ok = !label.contains('\n')
                    && !target.contains('\n')
                    && !target.trim().is_empty()
                    && !label.contains('[');
                ok.then(|| {
                    (
                        Link::Markdown {
                            image: bang,
                            label,
                            target: target.trim(),
                        },
                        usize::from(bang) + 1 + close + 2 + end + 1,
                    )
                })
            })
        } else {
            None
        };
        match found {
            Some((link, len)) => {
                if let Some(new) = f(link) {
                    out.push_str(&text[copied..pos]);
                    out.push_str(&new);
                    copied = pos + len;
                }
                pos += len;
            }
            None => pos += rest.chars().next().map_or(1, char::len_utf8),
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// `%XX` escapes decoded (invalid ones kept as written).
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A page from rows: nesting from the depths (a jump of more than one
/// level nests one level), kept ids saved.
pub fn page_from_rows(title: &str, is_journal: bool, rows: Vec<Row>) -> Page {
    let mut page = Page::new(title, is_journal);
    let mut stack: Vec<(usize, Uuid)> = Vec::new();
    for (depth, content, id) in rows {
        while stack.last().is_some_and(|&(d, _)| d >= depth) {
            stack.pop();
        }
        let id = match id {
            Some(id) if !page.blocks.iter().any(|b| b.id == id) => {
                page.saved_ids.insert(id);
                id
            }
            _ => Uuid::new_v4(),
        };
        page.blocks.push(Block {
            id,
            content,
            parent_id: stack.last().map(|&(_, id)| id),
            page_id: page.id.clone(),
            order: 0,
        });
        stack.push((depth, id));
    }
    if page.blocks.is_empty() {
        page.push_block(String::new());
    }
    page.renumber();
    page
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_and_simple_yaml() {
        let text = "---\ntitle: \"Hi\"\ntags: [a, \"b c\"]\naliases:\n  - One\n  - 'Two'\nrating: 5\nnested:\n  x: 1\n---\n# Body\n";
        let (yaml, body) = split_frontmatter(text);
        assert_eq!(body, "# Body\n");
        let pairs = parse_yaml(yaml.unwrap());
        let get = |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("title"), Some(vec!["Hi".to_string()]));
        assert_eq!(get("tags"), Some(vec!["a".to_string(), "b c".to_string()]));
        assert_eq!(
            get("aliases"),
            Some(vec!["One".to_string(), "Two".to_string()])
        );
        assert_eq!(get("nested"), Some(vec![]));
        assert_eq!(
            split_frontmatter("no front matter\n---\n"),
            (None, "no front matter\n---\n")
        );
        assert_eq!(
            split_frontmatter("---\nunclosed: yes\n"),
            (None, "---\nunclosed: yes\n")
        );
        assert_eq!(
            properties(&pairs),
            ["tags:: #a, #[[b c]]", "alias:: One, Two", "rating:: 5"]
        );
        let p = properties(&[
            ("tags".into(), vec!["#x y,  z".into()]),
            ("Due Date".into(), vec!["2026-10-09".into()]),
            ("tag".into(), vec!["Z".into()]),
        ]);
        assert_eq!(p, ["tags:: #[[x y]], #z", "due-date:: 2026-10-09"]);
        assert_eq!(tag_ref("area/sub"), "#area/sub");
        assert_eq!(tag_ref("C++"), "#[[C++]]");
    }

    #[test]
    fn markdown_becomes_an_outline() {
        let md = "Intro line one\nline two\n\n# Top\npara under top\n\n## Sub\n- item\n  - nested [ ]\n    more of nested\n- [ ] task\n- [x] done\n1. one\n2) two\n\n---\n\n| a | b |\n| - | - |\n| 1 | 2 |\n\n# Next\n```rust\nfn main() {\n\n    - not a list\n}\n```\n####### seven is text\n";
        let rows = outline(md);
        let expected: Vec<(usize, &str)> = vec![
            (0, "Intro line one\nline two"),
            (0, "# Top"),
            (1, "para under top"),
            (1, "## Sub"),
            (2, "item"),
            (3, "nested [ ]\nmore of nested"),
            (2, "TODO task"),
            (2, "DONE done"),
            (2, "one"),
            (2, "two"),
            (2, "| a | b |\n| - | - |\n| 1 | 2 |"),
            (0, "# Next"),
            (1, "```rust\nfn main() {\n\n    - not a list\n}\n```"),
            (1, "####### seven is text"),
        ];
        let got: Vec<(usize, &str)> = rows.iter().map(|(d, s)| (*d, s.as_str())).collect();
        assert_eq!(got, expected);
        assert_eq!(outline(""), [(0, String::new())]);
        assert_eq!(
            outline("#### deep\ntext\n"),
            [(0, "### deep".to_string()), (1, "text".to_string())]
        );
    }

    #[test]
    fn links_are_found_outside_code_only() {
        let text = "a [[One]] ![[Two|x]] [l](b%20c.md) ![i](img.png) `[[code]]` [x] [y]z\n```\n[[fenced]]\n```\n[[Last]]";
        let mut seen = Vec::new();
        let out = rewrite_links(text, |link| {
            seen.push(format!("{link:?}"));
            match link {
                Link::Wiki { inner, .. } if inner == "One" => Some("ONE".into()),
                _ => None,
            }
        });
        assert_eq!(out, text.replacen("[[One]]", "ONE", 1));
        assert_eq!(
            seen,
            [
                "Wiki { embed: false, inner: \"One\" }",
                "Wiki { embed: true, inner: \"Two|x\" }",
                "Markdown { image: false, label: \"l\", target: \"b%20c.md\" }",
                "Markdown { image: true, label: \"i\", target: \"img.png\" }",
                "Wiki { embed: false, inner: \"Last\" }",
            ]
        );
        assert_eq!(percent_decode("My%20Page%2Fx%zz%"), "My Page/x%zz%");
        assert_eq!(percent_decode("caf%C3%A9"), "café");
    }

    #[test]
    fn pages_from_rows_nest_and_keep_ids() {
        let id = Uuid::new_v4();
        let page = page_from_rows(
            "P",
            false,
            vec![
                (0, "a".into(), None),
                (2, "b".into(), Some(id)),
                (1, "c".into(), Some(id)),
                (0, "d".into(), None),
            ],
        );
        assert_eq!(
            page.to_markdown(),
            format!("- a\n  - b\n    id:: {id}\n  - c\n- d\n")
        );
        assert!(page.saved_ids.contains(&id));
        assert_eq!(page_from_rows("E", false, vec![]).blocks.len(), 1);
    }
}

//! Tag suggestions (decision 44): the prompt that asks the chat model for
//! tags, reading its reply (a JSON list, a `{"tags": [...]}` object, or a
//! plain comma / line list), and writing chosen tags onto the page.
//!
//! Tags are notesec's existing `#tag` / `#[[multi word]]` references
//! (`model::parse_references`), so an applied tag is found by `{{query
//! #tag}}`, "Linked from", the graph and the tag list like one typed by
//! hand. They go on a `tags::` line in the page's first block (the page
//! property block that `alias::` also lives in, decision 36).

use crate::model::{page_aliases, parse_references, tag_counts, Block, Page};
use serde_json::{json, Value};
use std::collections::HashSet;
use uuid::Uuid;

/// Most tags offered at once.
pub const MAX_SUGGESTIONS: usize = 6;
/// Longest tag name accepted (in characters).
const MAX_TAG_CHARS: usize = 40;
/// Most words in one tag name: longer is a sentence, not a tag.
const MAX_TAG_WORDS: usize = 4;
/// How much of the page goes into the prompt (in characters).
const PAGE_CHARS: usize = 4000;
/// How many of the vault's tags (most used first) the prompt lists.
const VAULT_TAGS: usize = 150;

/// The vault's tags, most used first (`model::tag_counts`).
pub fn vault_tags(pages: &[Page]) -> Vec<String> {
    tag_counts(pages)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// Lowercase names the page already has or is: every `#tag` and
/// `[[link]]` on it, its title and its aliases.
pub fn present(page: &Page) -> HashSet<String> {
    let mut out: HashSet<String> = page
        .blocks
        .iter()
        .flat_map(|b| parse_references(&b.content))
        .map(|r| r.target.to_lowercase())
        .collect();
    out.insert(page.title.to_lowercase());
    out.extend(page_aliases(page).iter().map(|a| a.to_lowercase()));
    out
}

/// The chat messages asking for tags for `page`, listing the vault's tags
/// to reuse.
pub fn tag_messages(page: &Page, vault: &[String]) -> Value {
    let mut text = String::new();
    for block in &page.blocks {
        let line = block.content.trim();
        if !line.is_empty() {
            text.push_str("- ");
            text.push_str(line);
            text.push('\n');
        }
    }
    let text: String = text.chars().take(PAGE_CHARS).collect();
    let known = if vault.is_empty() {
        "(none yet)".to_string()
    } else {
        vault
            .iter()
            .take(VAULT_TAGS)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    };
    json!([
        {
            "role": "system",
            "content": "You suggest tags for a page of personal notes. Prefer tags the user already uses \
                        when they fit; add a new tag only for a clear topic they don't cover. Tags are \
                        short topics of one to three words. Reply with only a JSON array of at most 6 \
                        strings, without the # sign, for example [\"rust\", \"reading list\"]."
        },
        {
            "role": "user",
            "content": format!(
                "Tags already used in my notes: {known}\n\nPage \"{}\":\n{text}",
                page.title
            )
        }
    ])
}

/// The raw names in a reply: a JSON array of strings (anywhere in the
/// reply, e.g. inside a code fence), a JSON object's `"tags"` array, or
/// else a list separated by commas, semicolons or lines (with a label
/// such as `Tags:` dropped).
pub fn parse_reply(reply: &str) -> Vec<String> {
    let strings = |value: &Value| -> Option<Vec<String>> {
        let array = value.as_array().or_else(|| value.get("tags")?.as_array())?;
        Some(
            array
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
        )
    };
    for (open, close) in [('[', ']'), ('{', '}')] {
        if let (Some(start), Some(end)) = (reply.find(open), reply.rfind(close)) {
            if start < end {
                if let Some(names) = serde_json::from_str::<Value>(&reply[start..=end])
                    .ok()
                    .as_ref()
                    .and_then(strings)
                {
                    return names;
                }
            }
        }
    }
    let mut out = Vec::new();
    for line in reply.lines() {
        let mut line = line.trim();
        if line.starts_with("```") {
            continue;
        }
        // "Tags: a, b" / "Suggested tags: a" - drop a short label.
        if let Some((label, rest)) = line.split_once(':') {
            if !label.contains(',') && label.chars().count() <= 30 && !rest.starts_with(':') {
                line = rest;
            }
        }
        out.extend(line.split([',', ';']).map(str::to_string));
    }
    out
}

/// A suggested name made clean: list markers, quotes, `#`, `[[ ]]` and
/// trailing dots dropped, spaces collapsed. `None` when what's left isn't
/// a usable tag (empty, too long, a sentence, a number, or characters
/// that would break the markup).
pub fn normalise(raw: &str) -> Option<String> {
    let mut name = raw.trim();
    // List markers: "- x", "* x", "• x", "1. x", "2) x".
    name = name.trim_start_matches(['-', '*', '\u{2022}']).trim_start();
    let digits = name.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && name[digits..].starts_with(['.', ')']) {
        name = name[digits + 1..].trim_start();
    }
    let quote = |c: char| matches!(c, '"' | '\'' | '`') || c.is_whitespace();
    name = name.trim_matches(quote).trim_end_matches(['.', '!', '?']);
    name = name.trim_matches(quote).trim_start_matches('#');
    if let Some(inner) = name.strip_prefix("[[").and_then(|n| n.strip_suffix("]]")) {
        name = inner;
    }
    let name = name.trim_end_matches(['.', '!', '?']).trim();
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let bad = name.is_empty()
        || name.chars().count() > MAX_TAG_CHARS
        || name.split(' ').count() > MAX_TAG_WORDS
        || name.chars().all(|c| c.is_ascii_digit())
        || name.contains(['[', ']', '#', '\n', '(', ')', '{', '}'])
        || name.contains("::");
    (!bad).then_some(name)
}

/// The suggestions to offer from a reply: normalised, spelled like an
/// existing vault tag when one matches (ignoring case), without names the
/// page already has (`present`) or repeats, at most `MAX_SUGGESTIONS`.
pub fn suggestions(reply: &str, page: &Page, vault: &[String]) -> Vec<String> {
    let mut seen = present(page);
    let mut out = Vec::new();
    for raw in parse_reply(reply) {
        let Some(name) = normalise(&raw) else {
            continue;
        };
        let name = vault
            .iter()
            .find(|v| v.to_lowercase() == name.to_lowercase())
            .cloned()
            .unwrap_or(name);
        if seen.insert(name.to_lowercase()) {
            out.push(name);
        }
        if out.len() == MAX_SUGGESTIONS {
            break;
        }
    }
    out
}

/// How `name` is written as a tag: `#name` when that parses back as the
/// whole tag, else `#[[name]]`.
pub fn tag_markup(name: &str) -> String {
    let bare = format!("#{name}");
    let refs = parse_references(&bare);
    match refs.as_slice() {
        [r] if r.is_tag && r.target == name && r.range == (0..bare.len()) => bare,
        _ => format!("#[[{name}]]"),
    }
}

/// `key:: value` with a simple key (letters, digits, `-`, `_`).
fn is_property_line(line: &str) -> bool {
    line.trim().split_once("::").is_some_and(|(key, _)| {
        !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    })
}

/// Add `names` (those the page doesn't already have) to the page's
/// `tags::` line: appended to an existing one in the first block, else a
/// new line after the first block's last property line (`alias::` and
/// friends), else a new first block. Returns whether the page changed.
pub fn apply_tags(page: &mut Page, names: &[String]) -> bool {
    let have = present(page);
    let mut seen = HashSet::new();
    let markup: Vec<String> = names
        .iter()
        .filter(|n| !have.contains(&n.to_lowercase()) && seen.insert(n.to_lowercase()))
        .map(|n| tag_markup(n))
        .collect();
    if markup.is_empty() {
        return false;
    }
    let list = markup.join(", ");
    if let Some(first) = page.blocks.first_mut() {
        let mut lines: Vec<String> = first.content.split('\n').map(str::to_string).collect();
        let tags_line = lines.iter().position(|l| {
            l.trim()
                .get(..6)
                .is_some_and(|k| k.eq_ignore_ascii_case("tags::"))
        });
        if let Some(i) = tags_line {
            let value = lines[i].trim().get(6..).unwrap_or("").trim().to_string();
            let key = lines[i].trim()[..6].to_string();
            lines[i] = if value.is_empty() {
                format!("{key} {list}")
            } else {
                format!("{key} {value}, {list}")
            };
            first.content = lines.join("\n");
            return true;
        }
        if let Some(last) = lines.iter().rposition(|l| is_property_line(l)) {
            lines.insert(last + 1, format!("tags:: {list}"));
            first.content = lines.join("\n");
            return true;
        }
    }
    page.blocks.insert(
        0,
        Block {
            id: Uuid::new_v4(),
            content: format!("tags:: {list}"),
            parent_id: None,
            page_id: page.id.clone(),
            order: 0,
        },
    );
    page.renumber();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn replies_are_read_as_json_objects_or_plain_lists() {
        assert_eq!(
            parse_reply("[\"rust\", \"books\"]"),
            names(&["rust", "books"])
        );
        assert_eq!(
            parse_reply("Sure!\n```json\n[\"a\", \"b c\"]\n```"),
            names(&["a", "b c"])
        );
        assert_eq!(parse_reply("{\"tags\": [\"x\"]}"), names(&["x"]));
        let plain: Vec<String> = parse_reply("Tags: #rust, reading list;\n- learning\n")
            .iter()
            .filter_map(|n| normalise(n))
            .collect();
        assert_eq!(plain, names(&["rust", "reading list", "learning"]));
    }

    #[test]
    fn names_are_cleaned_and_junk_dropped() {
        assert_eq!(
            normalise("  1. \"#[[Machine  Learning]]\". ").as_deref(),
            Some("Machine Learning")
        );
        assert_eq!(normalise("* `rust`").as_deref(), Some("rust"));
        assert_eq!(normalise("2) ai/ml").as_deref(), Some("ai/ml"));
        for junk in [
            "",
            "  ",
            "#",
            "42",
            "a]] b",
            "key:: v",
            "this is a whole sentence about it",
        ] {
            assert_eq!(normalise(junk), None, "{junk:?}");
        }
    }

    #[test]
    fn suggestions_reuse_vault_spelling_and_skip_what_the_page_has() {
        let page = Page::from_markdown(
            "Rust",
            false,
            "- alias:: Rustlang\n- borrowing #ownership and [[Cargo]]\n",
        );
        let vault = names(&["Programming", "ownership"]);
        let got = suggestions(
            "[\"programming\", \"Ownership\", \"rust\", \"rustlang\", \"cargo\", \"memory\", \"memory\", \"\"]",
            &page,
            &vault,
        );
        assert_eq!(got, names(&["Programming", "memory"]));
        let many = (0..10)
            .map(|i| format!("\"t{i}\""))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            suggestions(&format!("[{many}]"), &page, &[]).len(),
            MAX_SUGGESTIONS
        );
    }

    #[test]
    fn the_prompt_lists_the_page_and_the_vaults_tags() {
        let page = Page::from_markdown("Rust", false, "- borrowing rules\n");
        let messages = tag_messages(&page, &names(&["programming", "books"]));
        let user = messages[1]["content"].as_str().unwrap();
        assert!(user.contains("programming, books"));
        assert!(user.contains("Page \"Rust\":\n- borrowing rules"));
        assert!(messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("JSON array"));
    }

    #[test]
    fn tags_go_on_the_first_blocks_tags_line() {
        assert_eq!(tag_markup("rust"), "#rust");
        assert_eq!(tag_markup("reading list"), "#[[reading list]]");
        assert_eq!(tag_markup("C++"), "#[[C++]]");

        // No properties: a new first block.
        let mut page = Page::from_markdown("P", false, "- body\n");
        assert!(apply_tags(&mut page, &names(&["rust", "reading list"])));
        assert_eq!(
            page.to_markdown(),
            "- tags:: #rust, #[[reading list]]\n- body\n"
        );
        // Then appended to that line, skipping what's there.
        assert!(apply_tags(&mut page, &names(&["Rust", "books"])));
        assert_eq!(
            page.blocks[0].content,
            "tags:: #rust, #[[reading list]], #books"
        );
        assert!(!apply_tags(&mut page, &names(&["books"])));
        assert_eq!(page.blocks.len(), 2);

        // A property block (aliases): the line joins it, so aliases still
        // come from the first block.
        let mut page = Page::from_markdown("P", false, "- alias:: Q\n  more text\n");
        assert!(apply_tags(&mut page, &names(&["x"])));
        assert_eq!(page.blocks[0].content, "alias:: Q\ntags:: #x\nmore text");
        assert_eq!(page_aliases(&page), vec!["Q"]);

        // An empty page.
        let mut page = Page::new("E", false);
        assert!(apply_tags(&mut page, &names(&["x"])));
        assert_eq!(page.blocks[0].content, "tags:: #x");
    }
}

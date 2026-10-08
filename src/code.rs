//! Fenced code blocks inside a block:
//!
//! ````text
//! ```rust
//! fn main() {}
//! ```
//! ````
//!
//! A fence is a line starting with three or more backticks (after any
//! indent), optionally followed by a language name; the code runs to a line
//! of at least as many backticks and nothing else, or to the end of the
//! block if there is none. The block's text is never changed: this only
//! says which lines are code, for rendering and so links and tags inside
//! code aren't treated as references.

use std::ops::Range;

/// One fenced code block.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeBlock {
    /// What follows the opening backticks, trimmed (may be empty).
    pub lang: String,
    /// The lines between the fences, exactly as stored.
    pub code: String,
}

/// A block's text cut into prose and code, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    Text(String),
    Code(CodeBlock),
}

/// The backtick count of an opening fence, and its language.
fn opening(line: &str) -> Option<(usize, &str)> {
    let line = line.trim_start();
    let ticks = line.len() - line.trim_start_matches('`').len();
    let rest = &line[ticks..];
    // "```x```" is inline code, not a fence.
    (ticks >= 3 && !rest.contains('`')).then(|| (ticks, rest.trim()))
}

fn closes(line: &str, ticks: usize) -> bool {
    let line = line.trim();
    line.len() >= ticks && line.chars().all(|c| c == '`')
}

/// Each code block found in `content`: its byte range (fences included,
/// up to the end of the closing fence line) and the block itself.
fn code_blocks(content: &str) -> Vec<(Range<usize>, CodeBlock)> {
    // Every line with its byte range, without the `\n`.
    let mut lines = Vec::new();
    let mut start = 0;
    for line in content.split('\n') {
        lines.push((start..start + line.len(), line));
        start += line.len() + 1;
    }
    let mut found = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some((ticks, lang)) = opening(lines[i].1) else {
            i += 1;
            continue;
        };
        let end = (i + 1..lines.len())
            .find(|&j| closes(lines[j].1, ticks))
            .unwrap_or(lines.len());
        let code: Vec<&str> = lines[i + 1..end].iter().map(|(_, l)| *l).collect();
        let last = end.min(lines.len() - 1);
        found.push((
            lines[i].0.start..lines[last].0.end,
            CodeBlock {
                lang: lang.to_string(),
                code: code.join("\n"),
            },
        ));
        i = end + 1;
    }
    found
}

/// The byte ranges of `content` that are code, fences included.
pub fn fenced_ranges(content: &str) -> Vec<Range<usize>> {
    code_blocks(content).into_iter().map(|(r, _)| r).collect()
}

/// `content` cut into prose and code. Prose parts don't include the line
/// breaks around a code block; empty prose between two blocks is dropped.
pub fn split_code(content: &str) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut pos = 0;
    for (range, block) in code_blocks(content) {
        let text = content[pos..range.start].strip_suffix('\n').unwrap_or("");
        if !text.is_empty() {
            parts.push(Part::Text(text.to_string()));
        }
        parts.push(Part::Code(block));
        pos = (range.end + 1).min(content.len());
    }
    let rest = &content[pos..];
    if !rest.is_empty() || parts.is_empty() {
        parts.push(Part::Text(rest.to_string()));
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(lang: &str, code: &str) -> Part {
        Part::Code(CodeBlock {
            lang: lang.into(),
            code: code.into(),
        })
    }

    #[test]
    fn prose_and_code_in_order() {
        let parts = split_code("Run this:\n```sh\n  ls -la  \n\necho hi\n```\nthen [[Done]]");
        assert_eq!(
            parts,
            [
                Part::Text("Run this:".into()),
                code("sh", "  ls -la  \n\necho hi"),
                Part::Text("then [[Done]]".into()),
            ]
        );
    }

    #[test]
    fn a_block_that_is_only_code() {
        assert_eq!(split_code("```\nx\n```"), [code("", "x")]);
        assert_eq!(split_code("```rust\n```"), [code("rust", "")]);
    }

    #[test]
    fn unclosed_fence_runs_to_the_end() {
        assert_eq!(
            split_code("a\n```py\nprint(1)"),
            [Part::Text("a".into()), code("py", "print(1)")]
        );
        assert_eq!(fenced_ranges("a\n```py\nprint(1)"), [2..16]);
    }

    #[test]
    fn longer_fences_hold_shorter_ones() {
        let text = "````md\n```\ninner\n```\n````";
        assert_eq!(split_code(text), [code("md", "```\ninner\n```")]);
        assert_eq!(fenced_ranges(text), [0..text.len()]);
    }

    #[test]
    fn two_blocks_back_to_back() {
        assert_eq!(
            split_code("```a\n1\n```\n```b\n2\n```"),
            [code("a", "1"), code("b", "2")]
        );
    }

    #[test]
    fn not_fences() {
        assert_eq!(split_code("plain"), [Part::Text("plain".into())]);
        assert_eq!(split_code(""), [Part::Text("".into())]);
        assert_eq!(split_code("``two``"), [Part::Text("``two``".into())]);
        assert_eq!(
            split_code("```inline```"),
            [Part::Text("```inline```".into())]
        );
        assert!(fenced_ranges("no code").is_empty());
    }
}

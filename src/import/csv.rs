//! A small CSV reader (RFC 4180, as Notion writes it): commas, fields in
//! double quotes, `""` for a quote inside one, line breaks inside quoted
//! fields, CRLF or LF rows, an optional UTF-8 byte order mark. Enough for
//! database exports; no new crate.

/// The rows of `text`, each a list of fields. Blank lines are skipped. A
/// quote that never closes runs to the end of the text.
pub fn parse(text: &str) -> Vec<Vec<String>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    // The field started with a quote (so it was a field, even if empty).
    let mut was_quoted = false;
    // Some field of this row was quoted (so `""` alone is a row).
    let mut row_quoted = false;
    let mut chars = text.chars().peekable();
    let end_row = |row: &mut Vec<String>, rows: &mut Vec<Vec<String>>, quoted: bool| {
        let blank = row.len() == 1 && row[0].is_empty() && !quoted;
        if !blank {
            rows.push(std::mem::take(row));
        } else {
            row.clear();
        }
    };
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                c => field.push(c),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() && !was_quoted => {
                quoted = true;
                was_quoted = true;
                row_quoted = true;
            }
            ',' => {
                row.push(std::mem::take(&mut field));
                was_quoted = false;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => {
                row.push(std::mem::take(&mut field));
                was_quoted = false;
                end_row(&mut row, &mut rows, row_quoted);
                row_quoted = false;
            }
            c => field.push(c),
        }
    }
    if !field.is_empty() || was_quoted || !row.is_empty() {
        row.push(field);
        end_row(&mut row, &mut rows, row_quoted);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::parse;

    fn rows(text: &str) -> Vec<Vec<&str>> {
        // Leak for easy comparison in tests.
        parse(text)
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|f| &*Box::leak(f.into_boxed_str()))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn plain_rows_and_line_endings() {
        assert_eq!(rows("a,b,c\n1,2,3\n"), [["a", "b", "c"], ["1", "2", "3"]]);
        assert_eq!(rows("a,b\r\n1,2\r\n"), [["a", "b"], ["1", "2"]]);
        assert_eq!(
            rows("a,b\n1,2"),
            [["a", "b"], ["1", "2"]],
            "no final newline"
        );
        assert_eq!(rows("a,,c\n,\n"), [vec!["a", "", "c"], vec!["", ""]]);
        assert_eq!(rows("a\n\n\nb\n"), [["a"], ["b"]], "blank lines skipped");
        assert!(parse("").is_empty());
    }

    #[test]
    fn quotes_escapes_newlines_and_bom() {
        assert_eq!(
            rows("\u{feff}Name,Note\n\"Smith, J\",\"say \"\"hi\"\"\"\n"),
            [["Name", "Note"], ["Smith, J", "say \"hi\""]]
        );
        assert_eq!(
            rows("a,\"line 1\nline 2\r\nline 3\",c\n"),
            [["a", "line 1\nline 2\r\nline 3", "c"]]
        );
        assert_eq!(rows("\"\",x\n"), [["", "x"]], "an empty quoted field");
        assert_eq!(rows("\"\"\n"), [[""]], "a row of one empty quoted field");
        assert_eq!(
            rows("a\"b,c\n"),
            [["a\"b", "c"]],
            "a quote inside a field is text"
        );
        assert_eq!(
            rows("\"open,x\n"),
            [["open,x\n"]],
            "an unclosed quote runs to the end"
        );
        assert_eq!(rows("é,ü\n"), [["é", "ü"]]);
    }
}

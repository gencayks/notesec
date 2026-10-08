//! Pipe-syntax markdown tables inside a block, for the reading view only.
//!
//! ```text
//! | Name | Qty |
//! | :--- | --: |
//! | tea  | 2   |
//! ```
//!
//! A table is a header row, a separator row (one `---` cell per column, a
//! `:` on the left, right or both setting the alignment) and any number of
//! body rows, one per line of the block. Rows may have more or fewer cells
//! than the header: the table is as wide as its widest row and short rows
//! are padded with empty cells. Nothing here changes `content`; the file
//! keeps the table exactly as typed.

/// How a column's cells line up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

/// A block's text split around its (first) table.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    /// Lines of the block before the table, joined with `\n` (may be empty).
    pub before: String,
    /// Header first, then body rows; every row has `align.len()` cells.
    pub rows: Vec<Vec<String>>,
    /// One per column.
    pub align: Vec<Align>,
    /// Lines of the block after the table (may be empty).
    pub after: String,
}

/// The cells of a pipe row: split on `|` (a `\|` is a literal pipe), with
/// the optional outer pipes dropped and each cell trimmed. `None` if the
/// line has no pipe at all.
fn cells(line: &str) -> Option<Vec<String>> {
    let line = line.trim();
    if !line.contains('|') {
        return None;
    }
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cell.push('|');
                chars.next();
            }
            '|' => cells.push(std::mem::take(&mut cell)),
            c => cell.push(c),
        }
    }
    cells.push(cell);
    // `| a | b |` splits into "", " a ", " b ", "": drop the outer empties.
    if line.starts_with('|') {
        cells.remove(0);
    }
    if line.ends_with('|') && !line.ends_with("\\|") {
        cells.pop();
    }
    Some(cells.into_iter().map(|c| c.trim().to_string()).collect())
}

/// The alignments of a separator row like `| :-- | :-: | --: |`, or `None`
/// if `line` isn't one.
fn separator(line: &str) -> Option<Vec<Align>> {
    let cells = cells(line)?;
    if cells.is_empty() {
        return None;
    }
    cells
        .iter()
        .map(|c| {
            let dashes = c.trim_start_matches(':').trim_end_matches(':');
            if dashes.is_empty() || !dashes.chars().all(|ch| ch == '-') {
                return None;
            }
            Some(match (c.starts_with(':'), c.ends_with(':')) {
                (true, true) => Align::Center,
                (false, true) => Align::Right,
                _ => Align::Left,
            })
        })
        .collect()
}

/// The first table in `content`, if it has one.
pub fn parse_table(content: &str) -> Option<Table> {
    let lines: Vec<&str> = content.split('\n').collect();
    let start = (0..lines.len().saturating_sub(1))
        .find(|&i| cells(lines[i]).is_some() && separator(lines[i + 1]).is_some())?;
    let mut align = separator(lines[start + 1])?;
    let mut rows = vec![cells(lines[start])?];
    let mut end = start + 2;
    while let Some(row) = lines.get(end).and_then(|l| cells(l)) {
        rows.push(row);
        end += 1;
    }
    let width = rows
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(0)
        .max(align.len());
    align.resize(width, Align::Left);
    for row in &mut rows {
        row.resize(width, String::new());
    }
    Some(Table {
        before: lines[..start].join("\n"),
        rows,
        align,
        after: lines[end..].join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_separator_and_rows() {
        let t = parse_table("| Name | Qty |\n| :--- | --: |\n| tea | 2 |\n| cake | 10 |").unwrap();
        assert_eq!(t.rows[0], ["Name", "Qty"]);
        assert_eq!(t.rows[2], ["cake", "10"]);
        assert_eq!(t.align, [Align::Left, Align::Right]);
        assert_eq!((t.before.as_str(), t.after.as_str()), ("", ""));
    }

    #[test]
    fn text_around_the_table_is_kept_apart() {
        let t = parse_table("Prices\na | b\n:-: | ---\n1 | 2\nafter **this**").unwrap();
        assert_eq!(t.before, "Prices");
        assert_eq!(t.after, "after **this**");
        assert_eq!(t.rows, [vec!["a", "b"], vec!["1", "2"]]);
        assert_eq!(t.align, [Align::Center, Align::Left]);
    }

    #[test]
    fn ragged_rows_are_padded_to_the_widest() {
        let t = parse_table("| a | b |\n|---|---|\n| 1 |\n| 1 | 2 | 3 |\n||").unwrap();
        assert_eq!(t.align.len(), 3);
        assert!(t.rows.iter().all(|r| r.len() == 3));
        assert_eq!(t.rows[1], ["1", "", ""]);
        assert_eq!(t.rows[2], ["1", "2", "3"]);
        assert_eq!(t.rows[3], ["", "", ""]);
    }

    #[test]
    fn escaped_pipes_stay_in_the_cell() {
        let t = parse_table("| a \\| b | c |\n| --- | --- |").unwrap();
        assert_eq!(t.rows[0], ["a | b", "c"]);
    }

    #[test]
    fn not_a_table() {
        assert_eq!(parse_table("just text"), None);
        assert_eq!(parse_table("a | b"), None);
        assert_eq!(parse_table("a | b\nc | d"), None);
        assert_eq!(parse_table("a | b\n| - x - |"), None);
        assert_eq!(parse_table("|"), None);
        assert_eq!(parse_table(""), None);
    }
}

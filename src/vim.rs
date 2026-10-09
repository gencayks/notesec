//! Vim keybindings for the block being edited (decision 48). Pure state
//! machine, NO ui code here: `Vim::key` takes one key and the block
//! editor's state, edits the text and cursor itself where the edit stays
//! inside the block, and returns an `Effect` for what only the app can do
//! (move to another block, delete or paste whole blocks, undo, save...).
//! `app/vim_ui.rs` feeds it keystrokes and applies the effects.
//!
//! The block is the buffer for motions; for the linewise commands (`dd`,
//! `yy`, `cc`, `p` of yanked blocks, `o`, `O`, `>>`, `<<`) a block is a
//! line, as in an outliner. Lines inside a multi-line block (Shift+Enter)
//! are lines for `j` / `k`, `0`, `^`, `$`, `I`, `A`; `j` / `k` past the
//! block's first or last line go to the block above or below.
//!
//! Supported in Normal mode (with counts where vim takes one):
//! `h j k l`, Space, Backspace, Enter, `w b e`, `0 ^ $`, `gg G` (first /
//! last block of the page), `i a I A o O`, `x X r ~`, `d c y` with
//! `h l w b e 0 ^ $` and doubled (`dd yy cc`), `D C s S Y`, `p P`, `u`,
//! Ctrl+R, `>> <<`, `v V`, and `:` (`:w :q :wq :x`). Visual mode: the
//! motions, `d x y c s > < o`, Esc. Keys with Ctrl, Alt or Super (other
//! than Ctrl+R) aren't vim's: they reach the app's shortcuts unchanged.

use crate::editor::EditorState;
use crate::model::Page;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Keys are commands; nothing is typed.
    #[default]
    Normal,
    /// Keys type text, as without vim; Esc goes back to Normal.
    Insert,
    /// A character-wise selection from where `v` was pressed.
    Visual,
    /// `V`: the whole block selected.
    VisualLine,
}

impl Mode {
    /// The mode indicator's text.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Visual => "VISUAL",
            Mode::VisualLine => "VISUAL LINE",
        }
    }
}

/// One key as vim sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// A typed character (no Ctrl, Alt or Super; Shift is in the case).
    Char(char),
    Escape,
    Enter,
    Backspace,
    Delete,
    /// Ctrl plus a letter or symbol.
    Ctrl(char),
    /// Anything else: arrows, Tab, function keys, Alt / Super chords.
    Other,
}

/// The unnamed register: what `d`, `c`, `x`, `y` took, for `p` / `P`.
#[derive(Clone, Debug, Default)]
pub enum Register {
    #[default]
    Empty,
    /// Text from inside a block.
    Text(String),
    /// Whole blocks (`dd`, `yy`), each with its children, roots at the top
    /// level of this scratch page.
    Blocks(Page),
}

/// What the app must do after a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Edit the visible block `count` blocks up, cursor on its last line at
    /// grapheme column `col`.
    BlockUp {
        count: usize,
        col: usize,
    },
    /// Edit the visible block `count` blocks down, cursor on its first line
    /// at column `col`.
    BlockDown {
        count: usize,
        col: usize,
    },
    /// `gg` / `G`: the first / last visible block of the page.
    FirstBlock,
    LastBlock,
    /// `o` / `O`: a new empty block below / above, in Insert mode.
    OpenBelow,
    OpenAbove,
    /// `dd` / `yy`: this block and the `n - 1` after it (each with its
    /// children) into the register; `dd` removes them.
    DeleteBlocks(usize),
    YankBlocks(usize),
    /// `p` / `P` with blocks in the register: paste them below / above.
    PasteBlocks {
        before: bool,
    },
    Undo(usize),
    Redo(usize),
    /// `>>` / `<<`: indent / outdent the block `n` times.
    Indent(usize),
    Outdent(usize),
    /// `:w`: save now and say so.
    Write,
    /// `:q`: stop editing.
    Quit,
    /// `:wq`, `:x`.
    WriteQuit,
    /// Esc in Normal mode: stop editing (as Esc does without vim).
    StopEditing,
    /// A short message for the status line (an unknown `:` command).
    Error(String),
}

/// The result of one key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Not vim's: the app handles the key as it would without vim (a
    /// shortcut, or typing in Insert mode).
    Pass,
    /// Taken by vim (never typed), with what the app must do, if anything.
    Handled(Option<Effect>),
}

/// The vim layer's state: one for the app's single block editor.
#[derive(Clone, Debug, Default)]
pub struct Vim {
    pub mode: Mode,
    pub register: Register,
    /// A count being typed (`3` of `3j`).
    count: Option<usize>,
    /// An operator waiting for its motion, with the count typed before it.
    op: Option<(char, usize)>,
    /// `g` typed (waiting for the second `g`).
    g: bool,
    /// `r` typed (waiting for the replacement character).
    replace: bool,
    /// The `:` command line being typed (without the `:`).
    cmdline: Option<String>,
    /// In Visual modes: where the selection started, and the vim cursor
    /// (the character under it is selected too, unlike the editor's
    /// cursor, which is shown after it).
    visual: Option<(usize, usize)>,
}

impl Vim {
    /// Back to Normal mode with nothing pending (a block starts being
    /// edited, or editing stopped). The register is kept.
    pub fn reset(&mut self) {
        let register = std::mem::take(&mut self.register);
        *self = Vim {
            register,
            ..Vim::default()
        };
    }

    /// The `:` command line being typed, without the `:`.
    pub fn cmdline(&self) -> Option<&str> {
        self.cmdline.as_deref()
    }

    /// Keys typed so far of an unfinished command (`2d`, `g`), for the
    /// mode indicator.
    pub fn pending(&self) -> String {
        let mut s = String::new();
        if let Some((op, n)) = self.op {
            if n > 1 {
                s.push_str(&n.to_string());
            }
            s.push(op);
        }
        if let Some(n) = self.count {
            s.push_str(&n.to_string());
        }
        if self.g {
            s.push('g');
        }
        if self.replace {
            s.push('r');
        }
        s
    }

    fn clear_pending(&mut self) {
        self.count = None;
        self.op = None;
        self.g = false;
        self.replace = false;
    }

    fn pending_any(&self) -> bool {
        self.count.is_some() || self.op.is_some() || self.g || self.replace
    }

    fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1)
    }

    /// Takes a digit into the count (`0` only after another digit, else
    /// it is a motion). True if it did.
    fn count_digit(&mut self, c: char) -> bool {
        match c.to_digit(10) {
            Some(d) if d != 0 || self.count.is_some() => {
                let n = self.count.unwrap_or(0) * 10 + d as usize;
                self.count = Some(n.min(9999));
                true
            }
            _ => false,
        }
    }

    /// Handle one key in the current mode.
    pub fn key(&mut self, key: Key, ed: &mut EditorState) -> Outcome {
        if self.cmdline.is_some() {
            return self.command_key(key);
        }
        match self.mode {
            Mode::Insert => match key {
                Key::Escape => {
                    // Like vim, the cursor goes back onto the last typed
                    // character.
                    self.mode = Mode::Normal;
                    if ed.cursor > line_start(&ed.text, ed.cursor) {
                        ed.cursor = prev_g(&ed.text, ed.cursor);
                    }
                    ed.anchor = None;
                    clamp_normal(ed);
                    Outcome::Handled(None)
                }
                _ => Outcome::Pass,
            },
            Mode::Normal => self.normal(key, ed),
            Mode::Visual | Mode::VisualLine => self.visual_key(key, ed),
        }
    }

    fn command_key(&mut self, key: Key) -> Outcome {
        let Some(line) = self.cmdline.as_mut() else {
            return Outcome::Pass;
        };
        match key {
            Key::Char(c) => line.push(c),
            Key::Backspace => {
                if line.pop().is_none() {
                    self.cmdline = None;
                }
            }
            Key::Escape => self.cmdline = None,
            Key::Enter => {
                let line = self.cmdline.take().unwrap_or_default();
                return Outcome::Handled(run_command(&line));
            }
            Key::Delete => {}
            Key::Ctrl(_) | Key::Other => return Outcome::Pass,
        }
        Outcome::Handled(None)
    }

    fn normal(&mut self, key: Key, ed: &mut EditorState) -> Outcome {
        // A selection made with the mouse in Normal mode is a Visual one.
        if let Some(sel) = ed.selection() {
            let (anchor, cur) = if ed.cursor == sel.end {
                (sel.start, prev_g(&ed.text, sel.end))
            } else {
                (prev_g(&ed.text, sel.end), sel.start)
            };
            self.clear_pending();
            self.mode = Mode::Visual;
            self.visual = Some((anchor, cur));
            return self.visual_key(key, ed);
        }
        // A click can leave the cursor after a line's last character.
        clamp_normal(ed);
        let c = match key {
            Key::Char(c) => c,
            Key::Enter => 'j',
            Key::Backspace => 'h',
            Key::Delete => 'x',
            Key::Escape => {
                if self.pending_any() {
                    self.clear_pending();
                    return Outcome::Handled(None);
                }
                return Outcome::Handled(Some(Effect::StopEditing));
            }
            Key::Ctrl('r') => {
                let n = self.take_count();
                self.clear_pending();
                return Outcome::Handled(Some(Effect::Redo(n)));
            }
            Key::Ctrl(_) | Key::Other => {
                self.clear_pending();
                return Outcome::Pass;
            }
        };
        Outcome::Handled(self.normal_char(c, ed))
    }

    fn normal_char(&mut self, c: char, ed: &mut EditorState) -> Option<Effect> {
        if self.replace {
            let n = self.take_count();
            self.clear_pending();
            replace_chars(ed, c, n);
            return None;
        }
        if self.count_digit(c) {
            return None;
        }
        if self.g {
            let had_op = self.op.is_some();
            self.clear_pending();
            return (c == 'g' && !had_op).then_some(Effect::FirstBlock);
        }
        if let Some((op, before)) = self.op {
            if c == 'g' {
                // `dgg` isn't supported: wait for the second `g` and drop it.
                self.g = true;
                return None;
            }
            let n = before * self.take_count();
            self.op = None;
            return self.operator(op, c, n, ed);
        }
        let n = self.take_count();
        match c {
            'h' | 'l' | ' ' | 'w' | 'b' | 'e' | '0' | '^' | '$' => {
                if let Some((target, _)) = motion(c, n, &ed.text, ed.cursor) {
                    ed.cursor = target;
                }
                ed.anchor = None;
                clamp_normal(ed);
                None
            }
            'j' => line_move(ed, n as isize),
            'k' => line_move(ed, -(n as isize)),
            'G' => Some(Effect::LastBlock),
            'g' => {
                self.g = true;
                None
            }
            'i' => self.insert_at(ed, ed.cursor),
            'a' => {
                let at = if ed.cursor < line_end(&ed.text, ed.cursor) {
                    next_g(&ed.text, ed.cursor)
                } else {
                    ed.cursor
                };
                self.insert_at(ed, at)
            }
            'I' => self.insert_at(ed, first_nonblank(&ed.text, ed.cursor)),
            'A' => self.insert_at(ed, line_end(&ed.text, ed.cursor)),
            'o' | 'O' => {
                self.mode = Mode::Insert;
                Some(if c == 'o' {
                    Effect::OpenBelow
                } else {
                    Effect::OpenAbove
                })
            }
            'x' => self.operator('d', 'l', n, ed),
            'X' => self.operator('d', 'h', n, ed),
            'D' => self.operator('d', '$', 1, ed),
            'C' => self.operator('c', '$', 1, ed),
            's' => self.operator('c', 'l', n, ed),
            'S' => self.operator('c', 'c', 1, ed),
            'Y' => Some(Effect::YankBlocks(n)),
            'p' | 'P' => self.paste(c == 'P', n, ed),
            'u' => Some(Effect::Undo(n)),
            'r' => {
                self.replace = true;
                self.count = Some(n);
                None
            }
            '~' => {
                toggle_case(ed, n);
                None
            }
            'v' | 'V' => {
                self.mode = if c == 'v' {
                    Mode::Visual
                } else {
                    Mode::VisualLine
                };
                self.visual = Some((ed.cursor, ed.cursor));
                self.show_visual(ed);
                None
            }
            ':' => {
                self.cmdline = Some(String::new());
                None
            }
            'd' | 'c' | 'y' | '>' | '<' => {
                self.op = Some((c, n));
                None
            }
            _ => None,
        }
    }

    fn insert_at(&mut self, ed: &mut EditorState, at: usize) -> Option<Effect> {
        self.mode = Mode::Insert;
        ed.cursor = at;
        ed.anchor = None;
        None
    }

    /// Operator `op` (`d`, `c`, `y`, `>`, `<`) with motion key `c`, `n`
    /// times. `c == op` is the linewise form (`dd`): the whole block.
    fn operator(&mut self, op: char, c: char, n: usize, ed: &mut EditorState) -> Option<Effect> {
        if c == op {
            return match op {
                'd' => Some(Effect::DeleteBlocks(n)),
                'y' => Some(Effect::YankBlocks(n)),
                'c' => {
                    // The block keeps its place (and children); its text goes.
                    if !ed.text.is_empty() {
                        self.register = Register::Text(std::mem::take(&mut ed.text));
                    }
                    self.insert_at(ed, 0)
                }
                '>' => Some(Effect::Indent(n)),
                '<' => Some(Effect::Outdent(n)),
                _ => None,
            };
        }
        if matches!(op, '>' | '<') {
            return None;
        }
        let text = &ed.text;
        let cur = ed.cursor;
        let eol = line_end(text, cur);
        let range = match c {
            // `cw` on a word changes to its end, like `ce`.
            'w' if op == 'c' && !char_at(text, cur).is_some_and(char::is_whitespace) => {
                let (t, _) = motion('e', n, text, cur)?;
                cur..next_g(text, t).max(cur)
            }
            // `dw` stops at the end of the line.
            'w' => {
                let (t, _) = motion('w', n, text, cur)?;
                cur..if cur < eol && t > eol { eol } else { t }
            }
            '$' => cur..eol,
            _ => {
                let (t, inclusive) = motion(c, n, text, cur)?;
                if t < cur {
                    t..cur
                } else if inclusive {
                    cur..next_g(text, t)
                } else {
                    cur..t
                }
            }
        };
        if range.is_empty() {
            return None;
        }
        let taken = text[range.clone()].to_string();
        self.register = Register::Text(taken);
        match op {
            'y' => {
                ed.cursor = range.start;
                clamp_normal(ed);
            }
            'd' => {
                ed.text.replace_range(range.clone(), "");
                ed.cursor = range.start;
                ed.anchor = None;
                clamp_normal(ed);
            }
            'c' => {
                ed.text.replace_range(range.clone(), "");
                return self.insert_at(ed, range.start);
            }
            _ => {}
        }
        None
    }

    fn paste(&mut self, before: bool, n: usize, ed: &mut EditorState) -> Option<Effect> {
        match &self.register {
            Register::Empty => None,
            Register::Blocks(_) => Some(Effect::PasteBlocks { before }),
            Register::Text(t) => {
                let s = t.repeat(n.max(1));
                let at = if before || ed.cursor >= line_end(&ed.text, ed.cursor) {
                    ed.cursor
                } else {
                    next_g(&ed.text, ed.cursor)
                };
                ed.text.insert_str(at, &s);
                ed.cursor = prev_g(&ed.text, at + s.len());
                ed.anchor = None;
                None
            }
        }
    }

    // --- Visual mode ---------------------------------------------------------

    /// Show the visual selection in the editor: its selection covers the
    /// character under the vim cursor too.
    fn show_visual(&self, ed: &mut EditorState) {
        let Some((anchor, cur)) = self.visual else {
            return;
        };
        if self.mode == Mode::VisualLine {
            ed.anchor = Some(0);
            ed.cursor = ed.text.len();
            return;
        }
        let start = anchor.min(cur);
        let end = next_g(&ed.text, anchor.max(cur));
        if cur >= anchor {
            ed.anchor = Some(start);
            ed.cursor = end;
        } else {
            ed.anchor = Some(end);
            ed.cursor = start;
        }
    }

    fn leave_visual(&mut self, ed: &mut EditorState, cursor: usize) {
        self.mode = Mode::Normal;
        self.visual = None;
        ed.anchor = None;
        ed.cursor = cursor.min(ed.text.len());
        clamp_normal(ed);
    }

    fn visual_key(&mut self, key: Key, ed: &mut EditorState) -> Outcome {
        let (anchor, cur) = self.visual.unwrap_or((ed.cursor, ed.cursor));
        let c = match key {
            Key::Char(c) => c,
            Key::Enter => 'j',
            Key::Backspace => 'h',
            Key::Delete => 'd',
            Key::Escape => {
                self.clear_pending();
                self.leave_visual(ed, cur);
                return Outcome::Handled(None);
            }
            Key::Ctrl(_) | Key::Other => {
                self.clear_pending();
                return Outcome::Pass;
            }
        };
        if self.count_digit(c) {
            return Outcome::Handled(None);
        }
        let n = self.take_count();
        let line = self.mode == Mode::VisualLine;
        let (start, end) = if line {
            (0, ed.text.len())
        } else {
            (anchor.min(cur), next_g(&ed.text, anchor.max(cur)))
        };
        let effect = match c {
            'v' | 'V' if (c == 'V') == line => {
                self.leave_visual(ed, cur);
                None
            }
            'v' | 'V' => {
                self.mode = if c == 'v' {
                    Mode::Visual
                } else {
                    Mode::VisualLine
                };
                None
            }
            'o' if !line => {
                self.visual = Some((cur, anchor));
                None
            }
            'd' | 'x' | 'y' | 'c' | 's' if line => {
                self.leave_visual(ed, cur);
                match c {
                    'd' | 'x' => Some(Effect::DeleteBlocks(1)),
                    'y' => Some(Effect::YankBlocks(1)),
                    _ => self.operator('c', 'c', 1, ed),
                }
            }
            'd' | 'x' | 'y' | 'c' | 's' => {
                let taken = ed.text[start..end].to_string();
                if !taken.is_empty() {
                    self.register = Register::Text(taken);
                }
                if c == 'y' {
                    self.leave_visual(ed, start);
                } else {
                    ed.text.replace_range(start..end, "");
                    self.leave_visual(ed, start);
                    if matches!(c, 'c' | 's') {
                        self.insert_at(ed, start);
                    }
                }
                None
            }
            '>' | '<' => {
                self.leave_visual(ed, cur);
                Some(if c == '>' {
                    Effect::Indent(n)
                } else {
                    Effect::Outdent(n)
                })
            }
            'h' | 'l' | ' ' | 'w' | 'b' | 'e' | '0' | '^' | '$' | 'j' | 'k' if !line => {
                ed.anchor = None;
                ed.cursor = cur;
                match c {
                    // Only within the block: the selection can't leave it.
                    'j' | 'k' => {
                        let mut probe = ed.clone();
                        let delta = if c == 'j' { n as isize } else { -(n as isize) };
                        if line_move(&mut probe, delta).is_none() {
                            ed.cursor = probe.cursor;
                        }
                    }
                    _ => {
                        if let Some((t, _)) = motion(c, n, &ed.text, cur) {
                            ed.cursor = t;
                        }
                        clamp_normal(ed);
                    }
                }
                self.visual = Some((anchor, ed.cursor));
                None
            }
            _ => None,
        };
        if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
            self.show_visual(ed);
        }
        Outcome::Handled(effect)
    }
}

/// A `:` command line (without the `:`).
pub fn run_command(line: &str) -> Option<Effect> {
    match line.trim() {
        "" => None,
        "w" | "write" => Some(Effect::Write),
        "q" | "quit" | "q!" | "quit!" => Some(Effect::Quit),
        "wq" | "x" | "wq!" | "x!" | "xit" | "exit" => Some(Effect::WriteQuit),
        other => Some(Effect::Error(format!("Not an editor command: {other}"))),
    }
}

// --- text helpers (byte offsets on grapheme boundaries) ----------------------

fn next_g(text: &str, i: usize) -> usize {
    text[i..].graphemes(true).next().map_or(i, |g| i + g.len())
}

fn prev_g(text: &str, i: usize) -> usize {
    text[..i]
        .graphemes(true)
        .next_back()
        .map_or(i, |g| i - g.len())
}

/// The grapheme boundary at or before `i`.
fn snap(text: &str, i: usize) -> usize {
    if i >= text.len() {
        return text.len();
    }
    text.grapheme_indices(true)
        .map(|(b, _)| b)
        .take_while(|&b| b <= i)
        .last()
        .unwrap_or(0)
}

fn char_at(text: &str, i: usize) -> Option<char> {
    text.get(i..)?.chars().next()
}

fn line_start(text: &str, i: usize) -> usize {
    text[..i].rfind('\n').map_or(0, |p| p + 1)
}

/// Where the line holding `i` ends: its `\n`, or the end of the text.
fn line_end(text: &str, i: usize) -> usize {
    text[i..].find('\n').map_or(text.len(), |p| i + p)
}

/// The last character of the line holding `i` (its start if empty): the
/// rightmost place the cursor can be in Normal mode.
fn last_char(text: &str, i: usize) -> usize {
    let (start, end) = (line_start(text, i), line_end(text, i));
    if end == start {
        start
    } else {
        prev_g(text, end)
    }
}

fn first_nonblank(text: &str, i: usize) -> usize {
    let start = line_start(text, i);
    let end = line_end(text, i);
    text[start..end]
        .find(|c: char| c != ' ' && c != '\t')
        .map_or(end, |p| start + p)
}

/// Keep the cursor on a character in Normal mode: never on a line's `\n`
/// or past the end (unless the line is empty).
pub fn clamp_normal(ed: &mut EditorState) {
    let i = snap(&ed.text, ed.cursor);
    ed.cursor = i.min(last_char(&ed.text, i));
}

/// Grapheme column of `i` in its line.
fn column(text: &str, i: usize) -> usize {
    text[line_start(text, i)..i].graphemes(true).count()
}

/// The offset `col` graphemes into the line starting at `start` (its end
/// if shorter).
fn at_column(text: &str, start: usize, col: usize) -> usize {
    let end = line_end(text, start);
    let mut t = start;
    for _ in 0..col {
        if t >= end {
            break;
        }
        t = next_g(text, t);
    }
    t
}

/// After moving to another block: the cursor on its first (or last) line
/// at column `col`, clamped for Normal mode.
pub fn place_cursor(ed: &mut EditorState, last_line: bool, col: usize) {
    let start = if last_line {
        line_start(&ed.text, ed.text.len())
    } else {
        0
    };
    ed.cursor = at_column(&ed.text, start, col);
    ed.anchor = None;
    clamp_normal(ed);
}

/// `j` / `k`: `delta` lines down (up if negative) inside the block, or
/// the block effect for the rest.
fn line_move(ed: &mut EditorState, delta: isize) -> Option<Effect> {
    let text = &ed.text;
    let col = column(text, ed.cursor);
    let line = text[..ed.cursor].matches('\n').count() as isize;
    let lines = text.matches('\n').count() as isize + 1;
    let target = line + delta;
    if target < 0 {
        return Some(Effect::BlockUp {
            count: (-target) as usize,
            col,
        });
    }
    if target >= lines {
        return Some(Effect::BlockDown {
            count: (target - lines + 1) as usize,
            col,
        });
    }
    let start = match target as usize {
        0 => 0,
        t => text
            .match_indices('\n')
            .nth(t - 1)
            .map_or(0, |(p, _)| p + 1),
    };
    ed.cursor = at_column(text, start, col);
    ed.anchor = None;
    clamp_normal(ed);
    None
}

/// Character classes for word motions: blank, word (letters, digits,
/// `_`, combining marks), other.
fn class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' || ('\u{300}'..='\u{36f}').contains(&c) {
        1
    } else {
        2
    }
}

fn next_char(text: &str, i: usize) -> usize {
    char_at(text, i).map_or(i, |c| i + c.len_utf8())
}

fn prev_char(text: &str, i: usize) -> usize {
    text[..i]
        .chars()
        .next_back()
        .map_or(i, |c| i - c.len_utf8())
}

fn class_at(text: &str, i: usize) -> u8 {
    char_at(text, i).map_or(0, class)
}

/// `w`: the start of the next word (the end of the text if none).
fn word_forward(text: &str, i: usize) -> usize {
    let len = text.len();
    let mut j = i;
    let c0 = class_at(text, j);
    if c0 != 0 {
        while j < len && class_at(text, j) == c0 {
            j = next_char(text, j);
        }
    }
    while j < len && class_at(text, j) == 0 {
        j = next_char(text, j);
    }
    j
}

/// `e`: the last character of this or the next word.
fn word_end(text: &str, i: usize) -> usize {
    let len = text.len();
    let mut j = next_char(text, i);
    while j < len && class_at(text, j) == 0 {
        j = next_char(text, j);
    }
    if j >= len {
        return if len == 0 {
            0
        } else {
            prev_char(text, len).max(i)
        };
    }
    let c = class_at(text, j);
    while next_char(text, j) < len && class_at(text, next_char(text, j)) == c {
        j = next_char(text, j);
    }
    j
}

/// `b`: the start of this or the previous word.
fn word_back(text: &str, i: usize) -> usize {
    if i == 0 {
        return 0;
    }
    let mut j = prev_char(text, i);
    while j > 0 && class_at(text, j) == 0 {
        j = prev_char(text, j);
    }
    let c = class_at(text, j);
    while j > 0 && class_at(text, prev_char(text, j)) == c {
        j = prev_char(text, j);
    }
    j
}

/// Where motion `c` (`n` times) goes from `cur`, and whether it includes
/// the character it lands on when used after an operator (`e`, `$`).
/// `None` for keys that aren't in-block motions.
fn motion(c: char, n: usize, text: &str, cur: usize) -> Option<(usize, bool)> {
    let start = line_start(text, cur);
    let end = line_end(text, cur);
    let repeat = |f: fn(&str, usize) -> usize| (0..n).fold(cur, |t, _| f(text, t));
    let (target, inclusive) = match c {
        'h' => (
            (0..n).fold(cur, |t, _| if t <= start { t } else { prev_g(text, t) }),
            false,
        ),
        'l' | ' ' => (
            (0..n).fold(cur, |t, _| if t >= end { t } else { next_g(text, t) }),
            false,
        ),
        '0' => (start, false),
        '^' => (first_nonblank(text, cur), false),
        '$' => (last_char(text, cur), true),
        'w' => (repeat(word_forward), false),
        'b' => (repeat(word_back), false),
        'e' => (repeat(word_end), true),
        _ => return None,
    };
    Some((snap(text, target), inclusive))
}

/// `r`: replace `n` characters from the cursor with `c` (nothing if the
/// line has fewer).
fn replace_chars(ed: &mut EditorState, c: char, n: usize) {
    if c == '\n' || c == '\r' {
        return;
    }
    let end = line_end(&ed.text, ed.cursor);
    let mut t = ed.cursor;
    for _ in 0..n {
        if t >= end {
            return;
        }
        t = next_g(&ed.text, t);
    }
    let with: String = std::iter::repeat_n(c, n).collect();
    ed.text.replace_range(ed.cursor..t, &with);
    ed.cursor += with.len() - c.len_utf8();
    ed.anchor = None;
}

/// `~`: switch the case of `n` characters and move past them.
fn toggle_case(ed: &mut EditorState, n: usize) {
    let end = line_end(&ed.text, ed.cursor);
    let mut t = ed.cursor;
    for _ in 0..n {
        if t >= end {
            break;
        }
        t = next_g(&ed.text, t);
    }
    let swapped: String = ed.text[ed.cursor..t]
        .chars()
        .flat_map(|c| -> Vec<char> {
            if c.is_lowercase() {
                c.to_uppercase().collect()
            } else {
                c.to_lowercase().collect()
            }
        })
        .collect();
    let start = ed.cursor;
    ed.text.replace_range(start..t, &swapped);
    ed.cursor = start + swapped.len();
    ed.anchor = None;
    clamp_normal(ed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use Effect::*;

    /// An editor from `"ab|c"`: the text without the `|`, cursor there.
    fn ed(marked: &str) -> EditorState {
        let at = marked.find('|').expect("cursor mark");
        let mut ed = EditorState::new(&marked.replacen('|', "", 1));
        ed.cursor = at;
        ed
    }

    /// The editor as `"ab|c"` (selection not shown).
    fn shown(ed: &EditorState) -> String {
        let mut s = ed.text.clone();
        s.insert(ed.cursor, '|');
        s
    }

    /// Keys from `"2dw<esc>"`: `<esc> <cr> <bs> <del> <c-x> <tab>`, else
    /// one character each.
    fn keys(s: &str) -> Vec<Key> {
        let mut out = Vec::new();
        let mut rest = s;
        while let Some(c) = rest.chars().next() {
            if c == '<' {
                if let Some(end) = rest.find('>') {
                    let name = &rest[1..end];
                    out.push(match name {
                        "esc" => Key::Escape,
                        "cr" => Key::Enter,
                        "bs" => Key::Backspace,
                        "del" => Key::Delete,
                        "tab" => Key::Other,
                        n if n.starts_with("c-") => Key::Ctrl(n.chars().nth(2).unwrap()),
                        _ => panic!("bad key {name}"),
                    });
                    rest = &rest[end + 1..];
                    continue;
                }
            }
            out.push(Key::Char(c));
            rest = &rest[c.len_utf8()..];
        }
        out
    }

    /// Press `k` on `vim` / `e`; the effects, in order.
    fn press(vim: &mut Vim, e: &mut EditorState, k: &str) -> Vec<Effect> {
        keys(k)
            .into_iter()
            .filter_map(|key| match vim.key(key, e) {
                Outcome::Handled(effect) => effect,
                Outcome::Pass => None,
            })
            .collect()
    }

    /// From Normal mode on `start`, press `k`: (shown editor, effects).
    fn run(start: &str, k: &str) -> (String, Vec<Effect>) {
        let (mut vim, mut e) = (Vim::default(), ed(start));
        let effects = press(&mut vim, &mut e, k);
        (shown(&e), effects)
    }

    fn after(start: &str, k: &str) -> String {
        let (s, effects) = run(start, k);
        assert_eq!(effects, vec![], "{start} {k}");
        s
    }

    fn text_register(vim: &Vim) -> Option<&str> {
        match &vim.register {
            Register::Text(t) => Some(t),
            _ => None,
        }
    }

    #[test]
    fn h_and_l_stay_on_the_line_with_counts() {
        assert_eq!(after("a|bcd", "l"), "ab|cd");
        assert_eq!(after("a|bcd", "2l"), "abc|d");
        assert_eq!(after("a|bcd", "9l"), "abc|d");
        assert_eq!(after("a|bcd", " "), "ab|cd");
        assert_eq!(after("abc|d", "h"), "ab|cd");
        assert_eq!(after("abc|d", "3h"), "|abcd");
        assert_eq!(after("|abcd", "h<bs>"), "|abcd");
        // Never onto (or past) the line break.
        assert_eq!(after("a|b\ncd", "5l"), "a|b\ncd");
        assert_eq!(after("ab\nc|d", "5h"), "ab\n|cd");
    }

    #[test]
    fn word_motions() {
        let text = "foo bar.baz  qux";
        assert_eq!(after("|foo bar.baz  qux", "w"), "foo |bar.baz  qux");
        assert_eq!(after("|foo bar.baz  qux", "2w"), "foo bar|.baz  qux");
        assert_eq!(after("|foo bar.baz  qux", "3w"), "foo bar.|baz  qux");
        assert_eq!(after("|foo bar.baz  qux", "4w"), "foo bar.baz  |qux");
        // At the last word, `w` stops on the last character.
        assert_eq!(after("foo bar.baz  |qux", "w"), "foo bar.baz  qu|x");
        assert_eq!(after("|foo bar.baz  qux", "e"), "fo|o bar.baz  qux");
        assert_eq!(after("fo|o bar.baz  qux", "e"), "foo ba|r.baz  qux");
        assert_eq!(after("foo bar.baz  q|ux", "b"), "foo bar.baz  |qux");
        assert_eq!(after("foo bar.baz  |qux", "b"), "foo bar.|baz  qux");
        assert_eq!(after("foo bar.baz  |qux", "3b"), "foo |bar.baz  qux");
        assert_eq!(after("f|oo", "9b"), "|foo");
        // Words continue on the next line of the block.
        assert_eq!(after("a|b\ncd", "w"), "ab\n|cd");
        let _ = text;
    }

    #[test]
    fn line_start_first_nonblank_and_end() {
        assert_eq!(after("  ab|cd", "0"), "|  abcd");
        assert_eq!(after("  ab|cd", "^"), "  |abcd");
        assert_eq!(after("|  abcd", "$"), "  abc|d");
        assert_eq!(after("x\n  a|b\ny", "0"), "x\n|  ab\ny");
        assert_eq!(after("x\n|  ab\ny", "$"), "x\n  a|b\ny");
        // `0` after a count digit is part of the count.
        assert_eq!(after("|abcdefghijkl", "10l"), "abcdefghij|kl");
    }

    #[test]
    fn j_and_k_move_between_lines_then_blocks() {
        assert_eq!(after("ab|c\nde\nfghij", "j"), "abc\nd|e\nfghij");
        assert_eq!(after("ab|c\nde\nfghij", "2j"), "abc\nde\nfg|hij");
        assert_eq!(after("abc\nde\nfgh|ij", "k"), "abc\nd|e\nfghij");
        assert_eq!(after("abc\nd|e\nfghij", "<cr>"), "abc\nde\nf|ghij");
        assert_eq!(
            run("abc\nde\n|f", "j").1,
            vec![BlockDown { count: 1, col: 0 }]
        );
        assert_eq!(
            run("abc\nd|e\nf", "4j").1,
            vec![BlockDown { count: 3, col: 1 }]
        );
        assert_eq!(run("ab|c", "k").1, vec![BlockUp { count: 1, col: 2 }]);
        assert_eq!(run("abc\nd|e", "3k").1, vec![BlockUp { count: 2, col: 1 }]);
        // Placing the cursor in the block moved to.
        let mut e = ed("|a\nlong line");
        place_cursor(&mut e, true, 3);
        assert_eq!(shown(&e), "a\nlon|g line");
        place_cursor(&mut e, false, 3);
        assert_eq!(shown(&e), "|a\nlong line");
    }

    #[test]
    fn gg_and_g_go_to_the_first_and_last_block() {
        assert_eq!(run("a|b", "gg").1, vec![FirstBlock]);
        assert_eq!(run("a|b", "G").1, vec![LastBlock]);
        // `g` then another key: nothing.
        assert_eq!(run("a|b", "gx"), ("a|b".into(), vec![]));
        // `dgg` isn't supported and changes nothing.
        assert_eq!(run("a|b", "dgg"), ("a|b".into(), vec![]));
    }

    #[test]
    fn insert_commands_and_escape() {
        let cases = [
            ("ab|cd", "i", "ab|cd"),
            ("ab|cd", "a", "abc|d"),
            ("abc|", "a", "abc|"),
            ("  ab|cd", "I", "  |abcd"),
            ("x\na|b\ny", "A", "x\nab|\ny"),
        ];
        for (start, k, want) in cases {
            let (mut vim, mut e) = (Vim::default(), ed(start));
            assert_eq!(press(&mut vim, &mut e, k), vec![]);
            assert_eq!(vim.mode, Mode::Insert, "{k}");
            assert_eq!(shown(&e), want, "{k}");
        }
        // In Insert mode keys are typed (passed), Esc goes back to Normal
        // with the cursor on the last typed character.
        let (mut vim, mut e) = (Vim::default(), ed("ab|"));
        press(&mut vim, &mut e, "a");
        for key in [Key::Char('x'), Key::Ctrl('k'), Key::Enter, Key::Other] {
            assert_eq!(vim.key(key, &mut e), Outcome::Pass);
        }
        assert_eq!(vim.key(Key::Escape, &mut e), Outcome::Handled(None));
        assert_eq!(vim.mode, Mode::Normal);
        assert_eq!(shown(&e), "a|b");
    }

    #[test]
    fn o_and_capital_o_open_blocks_in_insert_mode() {
        let (mut vim, mut e) = (Vim::default(), ed("a|b"));
        assert_eq!(press(&mut vim, &mut e, "o"), vec![OpenBelow]);
        assert_eq!(vim.mode, Mode::Insert);
        let (mut vim, mut e) = (Vim::default(), ed("a|b"));
        assert_eq!(press(&mut vim, &mut e, "O"), vec![OpenAbove]);
        assert_eq!(vim.mode, Mode::Insert);
    }

    #[test]
    fn x_deletes_characters_into_the_register() {
        let (mut vim, mut e) = (Vim::default(), ed("a|bcd"));
        press(&mut vim, &mut e, "x");
        assert_eq!(shown(&e), "a|cd");
        assert_eq!(text_register(&vim), Some("b"));
        press(&mut vim, &mut e, "5x");
        assert_eq!(shown(&e), "|a");
        assert_eq!(text_register(&vim), Some("cd"));
        assert_eq!(after("ab|cd", "X"), "a|cd");
        assert_eq!(after("ab|cd", "9X"), "|cd");
        assert_eq!(after("a|b\ncd", "x"), "|a\ncd");
        assert_eq!(after("ab\n|\ncd", "x"), "ab\n|\ncd");
        assert_eq!(after("a|bc", "<del>"), "a|c");
    }

    #[test]
    fn d_with_motions() {
        assert_eq!(after("|foo bar baz", "dw"), "|bar baz");
        assert_eq!(after("|foo bar baz", "2dw"), "|baz");
        assert_eq!(after("|foo bar baz", "d2w"), "|baz");
        // Last word: to the end, and the cursor stays on a character.
        assert_eq!(after("foo bar |baz", "dw"), "foo bar| ");
        // `dw` stops at the end of the line.
        assert_eq!(after("ab |cd\nef", "dw"), "ab| \nef");
        assert_eq!(after("|foo bar", "de"), "| bar");
        assert_eq!(after("foo b|ar", "db"), "foo |ar");
        assert_eq!(after("foo b|ar", "d0"), "|ar");
        assert_eq!(after("  foo b|ar", "d^"), "  |ar");
        assert_eq!(after("foo b|ar\nx", "d$"), "foo |b\nx");
        assert_eq!(after("foo b|ar", "D"), "foo |b");
        assert_eq!(after("ab|cd", "dl"), "ab|d");
        assert_eq!(after("ab|cd", "dh"), "a|cd");
        // Unsupported motions and Esc cancel.
        assert_eq!(after("ab|cd", "dj"), "ab|cd");
        assert_eq!(after("ab|cd", "d<esc>x"), "ab|d");
        // `dd` (and a count) is whole blocks: the app does it.
        assert_eq!(run("ab|cd", "dd").1, vec![DeleteBlocks(1)]);
        assert_eq!(run("ab|cd", "2dd").1, vec![DeleteBlocks(2)]);
        assert_eq!(run("ab|cd", "d3d").1, vec![DeleteBlocks(3)]);
        assert_eq!(run("ab|cd", "2d3d").1, vec![DeleteBlocks(6)]);
    }

    #[test]
    fn c_changes_into_insert_mode() {
        let cases = [
            ("|foo bar", "cw", "| bar"),
            ("|foo bar", "c2w", "|"),
            ("foo| bar", "cw", "foo|bar"),
            ("|foo bar", "ce", "| bar"),
            ("foo b|ar", "C", "foo b|"),
            ("foo b|ar", "c$", "foo b|"),
            ("ab|cd", "s", "ab|d"),
            ("ab|cd", "2s", "ab|"),
            ("foo\nbar|", "cc", "|"),
            ("foo|", "S", "|"),
        ];
        for (start, k, want) in cases {
            let (mut vim, mut e) = (Vim::default(), ed(start));
            assert_eq!(press(&mut vim, &mut e, k), vec![], "{start} {k}");
            assert_eq!(vim.mode, Mode::Insert, "{start} {k}");
            assert_eq!(shown(&e), want, "{start} {k}");
        }
        let (mut vim, mut e) = (Vim::default(), ed("foo\nbar|"));
        press(&mut vim, &mut e, "cc");
        assert_eq!(text_register(&vim), Some("foo\nbar"));
    }

    #[test]
    fn y_p_and_the_register() {
        let (mut vim, mut e) = (Vim::default(), ed("|foo bar"));
        assert_eq!(press(&mut vim, &mut e, "yw"), vec![]);
        assert_eq!(text_register(&vim), Some("foo "));
        assert_eq!(shown(&e), "|foo bar");
        press(&mut vim, &mut e, "$p");
        assert_eq!(shown(&e), "foo barfoo| ");
        press(&mut vim, &mut e, "0P");
        assert_eq!(shown(&e), "foo| foo barfoo ");
        press(&mut vim, &mut e, "y$");
        assert_eq!(text_register(&vim), Some(" foo barfoo "));
        let (mut vim, mut e) = (Vim::default(), ed("a|b"));
        vim.register = Register::Text("xy".into());
        press(&mut vim, &mut e, "3p");
        assert_eq!(shown(&e), "abxyxyx|y");
        // Into an empty line: at the cursor.
        let (mut vim, mut e) = (Vim::default(), ed("|"));
        vim.register = Register::Text("z".into());
        press(&mut vim, &mut e, "p");
        assert_eq!(shown(&e), "|z");
        // Nothing to paste: nothing happens.
        assert_eq!(after("a|b", "pP"), "a|b");
        // Blocks are the app's.
        let (mut vim, mut e) = (Vim::default(), ed("a|b"));
        vim.register = Register::Blocks(Page::new("", false));
        assert_eq!(
            press(&mut vim, &mut e, "pP"),
            vec![PasteBlocks { before: false }, PasteBlocks { before: true }]
        );
        assert_eq!(run("a|b", "yy").1, vec![YankBlocks(1)]);
        assert_eq!(run("a|b", "3Y").1, vec![YankBlocks(3)]);
        // The register outlives a reset (next block).
        vim.register = Register::Text("keep".into());
        vim.mode = Mode::Insert;
        vim.reset();
        assert_eq!(
            (vim.mode, text_register(&vim)),
            (Mode::Normal, Some("keep"))
        );
    }

    #[test]
    fn undo_redo_indent_and_outdent() {
        assert_eq!(run("a|b", "u").1, vec![Undo(1)]);
        assert_eq!(run("a|b", "3u").1, vec![Undo(3)]);
        assert_eq!(run("a|b", "<c-r>").1, vec![Redo(1)]);
        assert_eq!(run("a|b", "2<c-r>").1, vec![Redo(2)]);
        assert_eq!(run("a|b", ">>").1, vec![Indent(1)]);
        assert_eq!(run("a|b", "2>>").1, vec![Indent(2)]);
        assert_eq!(run("a|b", "<<").1, vec![Outdent(1)]);
        // `>` with a motion: nothing.
        assert_eq!(run("a|b", ">w").1, vec![]);
    }

    #[test]
    fn replace_and_toggle_case() {
        assert_eq!(after("|abc", "rx"), "|xbc");
        assert_eq!(after("|abc", "2rz"), "z|zc");
        assert_eq!(after("|abc", "4rz"), "|abc");
        assert_eq!(after("|abc", "r<esc>x"), "|bc");
        assert_eq!(after("|aBc", "~"), "A|Bc");
        assert_eq!(after("|aBc", "5~"), "Ab|C");
    }

    #[test]
    fn visual_mode_selects_inclusively_and_operates() {
        let (mut vim, mut e) = (Vim::default(), ed("|abcdef"));
        press(&mut vim, &mut e, "vl");
        assert_eq!(vim.mode, Mode::Visual);
        assert_eq!(e.selection(), Some(0..2));
        press(&mut vim, &mut e, "e");
        assert_eq!(e.selection(), Some(0..6));
        press(&mut vim, &mut e, "hhd");
        assert_eq!(vim.mode, Mode::Normal);
        assert_eq!(shown(&e), "|ef");
        assert_eq!(text_register(&vim), Some("abcd"));

        // Backwards, then yank: the cursor goes to the start.
        let (mut vim, mut e) = (Vim::default(), ed("ab|cdef"));
        press(&mut vim, &mut e, "vhy");
        assert_eq!(text_register(&vim), Some("bc"));
        assert_eq!(shown(&e), "a|bcdef");
        assert_eq!(e.selection(), None);

        // `c` changes, `o` swaps the ends, Esc leaves.
        let (mut vim, mut e) = (Vim::default(), ed("ab|cdef"));
        press(&mut vim, &mut e, "vlohc");
        assert_eq!((vim.mode, shown(&e)), (Mode::Insert, "a|ef".to_string()));
        let (mut vim, mut e) = (Vim::default(), ed("ab|cdef"));
        press(&mut vim, &mut e, "vll<esc>");
        assert_eq!((vim.mode, shown(&e)), (Mode::Normal, "abcd|ef".to_string()));
        assert_eq!(e.selection(), None);
        // `j` / `k` stay inside the block.
        let (mut vim, mut e) = (Vim::default(), ed("a|b\ncd"));
        assert_eq!(press(&mut vim, &mut e, "vjj"), vec![]);
        assert_eq!(e.selection(), Some(1..5));
        press(&mut vim, &mut e, "k");
        assert_eq!(e.selection(), Some(1..2));

        // Visual line: the whole block, linewise operators.
        let (mut vim, mut e) = (Vim::default(), ed("a|b"));
        press(&mut vim, &mut e, "V");
        assert_eq!((vim.mode, e.selection()), (Mode::VisualLine, Some(0..2)));
        assert_eq!(press(&mut vim, &mut e, "d"), vec![DeleteBlocks(1)]);
        assert_eq!(vim.mode, Mode::Normal);
        assert_eq!(run("a|b", "Vy").1, vec![YankBlocks(1)]);
        assert_eq!(run("a|b", "V>").1, vec![Indent(1)]);
        assert_eq!(run("a|b", "v<").1, vec![Outdent(1)]);
        assert_eq!(after("a|b", "VV"), "a|b");
        let (mut vim, mut e) = (Vim::default(), ed("a|b"));
        press(&mut vim, &mut e, "Vvl");
        assert_eq!((vim.mode, e.selection()), (Mode::Visual, Some(1..2)));
        // Shortcuts still pass.
        assert_eq!(vim.key(Key::Ctrl('k'), &mut e), Outcome::Pass);
    }

    #[test]
    fn a_mouse_selection_in_normal_mode_is_visual() {
        let (mut vim, mut e) = (Vim::default(), ed("abc|def"));
        e.anchor = Some(1);
        assert_eq!(press(&mut vim, &mut e, "d"), vec![]);
        assert_eq!(shown(&e), "a|def");
        assert_eq!(text_register(&vim), Some("bc"));
    }

    #[test]
    fn the_command_line() {
        let (mut vim, mut e) = (Vim::default(), ed("a|b"));
        press(&mut vim, &mut e, ":w");
        assert_eq!(vim.cmdline(), Some("w"));
        assert_eq!(press(&mut vim, &mut e, "<cr>"), vec![Write]);
        assert_eq!(vim.cmdline(), None);
        assert_eq!(run("a|b", ":q<cr>").1, vec![Quit]);
        assert_eq!(run("a|b", ":q!<cr>").1, vec![Quit]);
        assert_eq!(run("a|b", ":wq<cr>").1, vec![WriteQuit]);
        assert_eq!(run("a|b", ":x<cr>").1, vec![WriteQuit]);
        assert_eq!(run("a|b", ":<cr>").1, vec![]);
        assert_eq!(
            run("a|b", ":foo<cr>").1,
            vec![Error("Not an editor command: foo".into())]
        );
        // Backspace edits, and on an empty line cancels; Esc cancels.
        assert_eq!(run("a|b", ":wx<bs><cr>").1, vec![Write]);
        let (mut vim, mut e) = (Vim::default(), ed("a|b"));
        press(&mut vim, &mut e, ":w<bs><bs>");
        assert_eq!(vim.cmdline(), None);
        assert_eq!(press(&mut vim, &mut e, "x"), vec![]);
        assert_eq!(shown(&e), "|a");
        let (mut vim, mut e) = (Vim::default(), ed("a|b"));
        assert_eq!(press(&mut vim, &mut e, ":w<esc>"), vec![]);
        assert_eq!(vim.cmdline(), None);
        // Typed into the line, never into the block.
        assert_eq!(run("a|b", ":dd").0, "a|b");
    }

    #[test]
    fn escape_stops_editing_unless_something_is_pending() {
        assert_eq!(run("a|b", "<esc>").1, vec![StopEditing]);
        assert_eq!(run("a|b", "d<esc>").1, vec![]);
        assert_eq!(run("a|b", "3<esc>").1, vec![]);
        assert_eq!(run("a|b", "g<esc><esc>").1, vec![StopEditing]);
        let (mut vim, mut e) = (Vim::default(), ed("a|b"));
        press(&mut vim, &mut e, "2d");
        assert_eq!(vim.pending(), "2d");
        press(&mut vim, &mut e, "3");
        assert_eq!(vim.pending(), "2d3");
        press(&mut vim, &mut e, "<esc>");
        assert_eq!(vim.pending(), "");
    }

    #[test]
    fn shortcuts_and_other_keys_pass_in_normal_mode() {
        let (mut vim, mut e) = (Vim::default(), ed("a|b"));
        for key in [Key::Ctrl('k'), Key::Ctrl('z'), Key::Other] {
            assert_eq!(vim.key(key, &mut e), Outcome::Pass);
        }
        // Unknown letters are swallowed, never typed.
        assert_eq!(vim.key(Key::Char('q'), &mut e), Outcome::Handled(None));
        assert_eq!(vim.key(Key::Char('Z'), &mut e), Outcome::Handled(None));
        assert_eq!(shown(&e), "a|b");
    }

    #[test]
    fn graphemes_are_never_split() {
        let s = "|e\u{301}\u{1f600}b";
        assert_eq!(after(s, "l"), "e\u{301}|\u{1f600}b");
        assert_eq!(after(s, "2l"), "e\u{301}\u{1f600}|b");
        assert_eq!(after(s, "lx"), "e\u{301}|b");
        assert_eq!(after(s, "$"), "e\u{301}\u{1f600}|b");
        assert_eq!(after("caf|e\u{301} x", "b"), "|cafe\u{301} x");
        assert_eq!(after("|cafe\u{301} x", "e"), "caf|e\u{301} x");
        assert_eq!(after("|\u{1f600}", "~"), "|\u{1f600}");
    }

    #[test]
    fn insert_escape_from_the_end_lands_on_the_last_character() {
        let (mut vim, mut e) = (Vim::default(), ed("abc|"));
        vim.mode = Mode::Insert;
        vim.key(Key::Escape, &mut e);
        assert_eq!(shown(&e), "ab|c");
        // At the start of a line it stays.
        let (mut vim, mut e) = (Vim::default(), ed("ab\n|c"));
        vim.mode = Mode::Insert;
        vim.key(Key::Escape, &mut e);
        assert_eq!(shown(&e), "ab\n|c");
    }

    #[test]
    fn mode_labels() {
        assert_eq!(Mode::Normal.label(), "NORMAL");
        assert_eq!(Mode::Insert.label(), "INSERT");
        assert_eq!(Mode::Visual.label(), "VISUAL");
        assert_eq!(Mode::VisualLine.label(), "VISUAL LINE");
    }
}

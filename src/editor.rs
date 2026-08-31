//! Rich editor for the new side of a file, built on tui-textarea, with full
//! mouse support (click to place cursor, drag to select, wheel to scroll).
//!
//! tui-textarea does not expose its internal viewport scroll offset, but its
//! scroll logic is deterministic: the viewport only moves to keep the cursor
//! visible (we never call `TextArea::scroll`). We replicate that exact logic
//! in a shadow viewport so screen coordinates map precisely to buffer
//! positions.

use crate::highlight::EditorHighlight;
use crate::lsp::{Completion, Diagnostic, TextEdit};
use crate::theme::palette;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use tui_textarea::{CursorMove, TextArea};
use unicode_width::UnicodeWidthChar;

pub struct Editor {
    pub textarea: TextArea<'static>,
    /// Repo-relative path, for display and for the GitHub API.
    pub path: String,
    /// Absolute path on disk, for reading/writing.
    pub abs_path: std::path::PathBuf,
    pub dirty: bool,
    /// Set after an Esc press while dirty; a second Esc discards.
    pub discard_armed: bool,
    /// This file is not part of the changeset — opened from a search
    /// result or a jump-to-definition. Closing it returns to the diff
    /// untouched, and saving it has no diff to refresh.
    pub standalone: bool,
    /// The working tree holds a different branch, so what is on screen
    /// came from the commit under review and must not be written back
    /// over the file on disk.
    pub read_only: bool,
    /// Shadow of tui-textarea's internal viewport top (row, col).
    top: (u16, u16),
    /// Inner text area (inside the block borders) from the last render.
    inner: Rect,
    dragging: bool,
    /// Incremental syntax highlighting for the buffer.
    hl: EditorHighlight,
    /// Everything wrong with the buffer, from every tool that has an
    /// opinion, in line order and worst-first on each line.
    ///
    /// Private, and read through [`Editor::problems`]. It is a merge of
    /// the two sources below and its *order* is an invariant four
    /// readers depend on — the gutter mark, the margin note, the status
    /// bar and `F8` all ask for "the worst one here" and have to get the
    /// same answer. A field anyone could assign to is a field where that
    /// order quietly stops holding.
    diagnostics: Vec<Diagnostic>,
    /// What the language server says, refreshed as its notifications
    /// arrive.
    server_diagnostics: Vec<Diagnostic>,
    /// What the linter says, refreshed when it finishes a run. Kept
    /// apart from the server's so that one arriving never wipes the
    /// other: they run on different clocks, and a lint result landing
    /// must not blank the compiler's errors for a frame.
    lint_diagnostics: Vec<Diagnostic>,
    /// The completion popup, when one is open.
    pub completion: Option<CompletionState>,
    /// Hash of the text last handed to the language server, so an idle
    /// tick can tell whether there is anything new to send.
    pub synced: u64,
    /// The signature of the call the cursor is inside, when the server
    /// knows one. Shown in the border beside the file name.
    pub signature: Option<crate::lsp::Signature>,
    /// Find and replace within this buffer. See [`BufferFind`].
    pub find: BufferFind,
    /// The buffer as it was immediately before a format.
    ///
    /// Replacing the whole buffer costs *two* of tui-textarea's undo
    /// steps (the delete, then the insert), so a single Ctrl+Z after a
    /// format would leave the file looking empty — alarming, even for the
    /// moment before the second press. This holds the previous text so
    /// one undo puts it back whole.
    pre_format: Option<String>,
}

/// An open completion popup: what the server offered, and what is left
/// of it after the characters typed since.
pub struct CompletionState {
    /// Everything the server sent, kept so more typing can narrow it
    /// without another round trip.
    all: Vec<Completion>,
    /// Indices into `all` that still match, in the server's order.
    pub shown: Vec<usize>,
    pub sel: usize,
    pub scroll: usize,
    /// Where the word being completed starts: (row, col), 0-based.
    pub start: (usize, usize),
}

/// Find, and optionally replace, inside one buffer.
///
/// Deliberately its own thing rather than a second user of `App::find`.
/// That one searches the diff and counts in display rows, which fold and
/// pair two sides together; this counts in buffer lines. Sharing the state
/// would have meant one of them lying about what a number means.
///
/// The prompt has no pane of its own — it is written into the editor's
/// border, where the file name goes. A find bar that pushed the text down
/// would move the line somebody is reading in order to help them find it.
#[derive(Default)]
pub struct BufferFind {
    pub query: String,
    /// What to put in place of a match, once `Tab` has been pressed.
    pub replacement: String,
    /// Whether keystrokes are going to the prompt, and to which field.
    pub typing: Option<Field>,
    /// Every match: (row, first char col, last char col + 1).
    pub matches: Vec<(usize, usize, usize)>,
    /// Index into `matches` of the one the cursor is on.
    pub at: usize,
    /// Where the cursor was when the prompt opened, so Esc can put it
    /// back after the incremental jumping around.
    origin: (usize, usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Query,
    Replacement,
}

impl BufferFind {
    pub fn active(&self) -> bool {
        !self.query.is_empty()
    }

    /// The match the cursor is on, if there is one.
    pub fn current(&self) -> Option<(usize, usize, usize)> {
        self.matches.get(self.at).copied()
    }
}

/// The line-comment marker for a file, by extension.
///
/// Only the languages loupe already colors and only line comments: a
/// block comment has to be closed, and a toggle that has to decide where
/// to close it is a toggle that gets it wrong on the awkward line.
/// A file whose language has none is left alone rather than guessed at.
fn comment_marker(path: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" | "go" | "ts" | "tsx" | "js" | "jsx" | "mts" | "cts" | "mjs" | "cjs" | "c" | "h"
        | "cpp" | "hpp" | "java" | "kt" | "swift" | "scala" | "cs" | "php" | "dart" | "zig" => "//",
        "py" | "rb" | "sh" | "bash" | "zsh" | "toml" | "yaml" | "yml" | "conf" | "cfg" | "nix"
        | "pl" | "r" | "ex" | "exs" | "jl" => "#",
        "sql" | "lua" | "hs" | "elm" => "--",
        "vim" => "\"",
        "lisp" | "clj" | "cljs" | "el" => ";",
        _ => return None,
    })
}

/// The brackets that pair, for the match highlight.
fn bracket_pair(c: char) -> Option<(char, bool)> {
    match c {
        '(' => Some((')', true)),
        '[' => Some((']', true)),
        '{' => Some(('}', true)),
        ')' => Some(('(', false)),
        ']' => Some(('[', false)),
        '}' => Some(('{', false)),
        _ => None,
    }
}

/// Rows of suggestions on screen at once.
pub const COMPLETION_ROWS: usize = 8;

impl CompletionState {
    pub fn items(&self) -> impl Iterator<Item = &Completion> {
        self.shown.iter().filter_map(|i| self.all.get(*i))
    }

    pub fn selected(&self) -> Option<&Completion> {
        self.all.get(*self.shown.get(self.sel)?)
    }

    pub fn len(&self) -> usize {
        self.shown.len()
    }

    pub fn move_sel(&mut self, delta: i32) {
        if self.shown.is_empty() {
            return;
        }
        let last = self.shown.len() as i32 - 1;
        // Wrapping, because a list this short is quicker to cycle than to
        // clamp against.
        let next = self.sel as i32 + delta;
        self.sel = if next < 0 {
            last as usize
        } else if next > last {
            0
        } else {
            next as usize
        };
        if self.sel < self.scroll {
            self.scroll = self.sel;
        } else if self.sel >= self.scroll + COMPLETION_ROWS {
            self.scroll = self.sel + 1 - COMPLETION_ROWS;
        }
    }

    /// Re-filter against the word typed so far. Returns false when
    /// nothing matches any more, which is the popup's cue to close.
    fn refilter(&mut self, prefix: &str) -> bool {
        let needle = prefix.to_lowercase();
        if needle.is_empty() {
            self.shown = (0..self.all.len()).collect();
            self.sel = 0;
            self.scroll = 0;
            return !self.shown.is_empty();
        }
        // Two passes, and the order between them is the point: a label
        // that *starts* with what has been typed is what was meant, and
        // has to sit above one that merely contains those letters in
        // order. Within each pass the server's own ranking is kept —
        // it knows which member you probably wanted.
        let mut starts = Vec::new();
        let mut loose = Vec::new();
        for (i, c) in self.all.iter().enumerate() {
            let label = c.label.to_lowercase();
            if label.starts_with(&needle) {
                starts.push(i);
            } else if subsequence(&label, &needle) {
                loose.push(i);
            }
        }
        starts.append(&mut loose);
        self.shown = starts;
        self.sel = 0;
        self.scroll = 0;
        !self.shown.is_empty()
    }
}

/// Apply a set of LSP text edits to a document.
///
/// Edits are given against the *original* text and may not overlap, so
/// they are applied last-first — an earlier edit would otherwise shift
/// every position after it. Positions arrive 0-based, with UTF-16
/// columns, which is why this works in char indices per line rather than
/// byte offsets into the whole string.
pub fn apply_text_edits(text: &str, edits: &[TextEdit]) -> String {
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    let mut edits: Vec<&TextEdit> = edits.iter().collect();
    edits.sort_by_key(|e| (e.start.0, e.start.1));
    for edit in edits.into_iter().rev() {
        let (sl, sc) = edit.start;
        let (el, ec) = edit.end;
        if sl >= lines.len() {
            continue;
        }
        let el = el.min(lines.len() - 1);
        let head: String = {
            let chars: Vec<char> = lines[sl].chars().collect();
            let sc = crate::lsp::char_column(&lines[sl], sc).min(chars.len());
            chars[..sc].iter().collect()
        };
        let tail: String = {
            let chars: Vec<char> = lines[el].chars().collect();
            let ec = crate::lsp::char_column(&lines[el], ec).min(chars.len());
            chars[ec..].iter().collect()
        };
        let replacement = format!("{head}{}{tail}", edit.text);
        let new_lines: Vec<String> = replacement.split('\n').map(str::to_string).collect();
        lines.splice(sl..=el, new_lines);
    }
    lines.join("\n")
}

/// Blank columns between the end of the code and the message in the
/// margin, so the two never read as one line of text.
const MARGIN_GAP: usize = 4;
/// The narrowest margin note worth drawing. Below this the reader gets a
/// truncated word and no information.
const MARGIN_MIN: usize = 24;

/// A diagnostic message flattened to one line, for the margin and the
/// status bar. Servers send newlines and runs of spaces inside a single
/// message; both would break the row.
pub fn one_line(message: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for ch in message.chars() {
        if ch.is_whitespace() {
            gap = !out.is_empty();
            continue;
        }
        if gap {
            out.push(' ');
            gap = false;
        }
        out.push(ch);
    }
    out
}

/// The color for one problem's severity: red for an error, yellow for a
/// warning, blue for a note. One place, so the gutter mark, the
/// underline, the margin and the message can never disagree about how
/// bad something is.
pub fn diagnostic_color(d: &Diagnostic) -> Color {
    let p = palette();
    match d.severity {
        1 => p.err,
        2 => p.warn,
        _ => p.hint,
    }
}

/// Whether every character of `needle` appears in `hay`, in order.
///
/// The forgiving half of matching a suggestion list: `frstnm` should
/// still find `firstName` after a typo, rather than closing the popup
/// and taking the server's answer with it.
fn subsequence(hay: &str, needle: &str) -> bool {
    let mut chars = hay.chars();
    needle.chars().all(|n| chars.any(|h| h == n))
}

/// Characters that are part of an identifier being completed.
pub fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// The char range of the identifier at `col` on `line`, if there is one.
///
/// One rule, in one place: the cursor, a double click and the
/// other-uses highlight all have to agree on where a word starts and
/// stops, or the thing that gets selected is not the thing that gets
/// looked up.
fn word_range(line: &str, col: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return None;
    }
    // Sitting just past the end of a word counts as being in it.
    let at = col.min(chars.len().saturating_sub(1));
    let at = if !is_word(chars[at]) && at > 0 && is_word(chars[at - 1]) {
        at - 1
    } else {
        at
    };
    if !is_word(chars[at]) {
        return None;
    }
    let start = chars[..at]
        .iter()
        .rposition(|c| !is_word(*c))
        .map(|i| i + 1)
        .unwrap_or(0);
    let end = chars[at..]
        .iter()
        .position(|c| !is_word(*c))
        .map(|i| at + i)
        .unwrap_or(chars.len());
    Some((start, end))
}

/// Every whole-word occurrence of `word` in `line`, as char ranges.
///
/// Whole-word: `set` must not light up the middle of `offset`. The
/// scan is by character rather than by byte, because the ranges are
/// compared against character columns when the row is drawn.
fn word_occurrences(line: &str, word: &str) -> Vec<(usize, usize)> {
    let hay: Vec<char> = line.chars().collect();
    let needle: Vec<char> = word.chars().collect();
    if needle.is_empty() || hay.len() < needle.len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..=hay.len() - needle.len() {
        if hay[i..i + needle.len()] != needle[..] {
            continue;
        }
        let before_ok = i == 0 || !is_word(hay[i - 1]);
        let after_ok = i + needle.len() == hay.len() || !is_word(hay[i + needle.len()]);
        if before_ok && after_ok {
            out.push((i, i + needle.len()));
        }
    }
    out
}

/// Clip to `width` columns and pad out to it.
fn truncate_pad(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > width {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push_str(&" ".repeat(width.saturating_sub(w)));
    out
}

fn num_digits(n: usize) -> u16 {
    let mut n = n;
    let mut d: u16 = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

/// Identical to tui-textarea's internal `next_scroll_top`.
fn next_scroll_top(prev_top: u16, cursor: u16, len: u16) -> u16 {
    if cursor < prev_top {
        cursor
    } else if prev_top + len <= cursor {
        cursor + 1 - len
    } else {
        prev_top
    }
}

impl Editor {
    pub fn new(path: &str, abs_path: std::path::PathBuf, content: &str) -> Self {
        let lines: Vec<String> = content.split('\n').map(|s| s.to_string()).collect();
        // The buffer is rendered by `render` below (not by the TextArea
        // widget), so the syntax colors can be painted per character; the
        // TextArea still owns editing, cursor, and selection state.
        let textarea = TextArea::new(lines);
        let hl = EditorHighlight::new(path, content);
        Editor {
            textarea,
            path: path.to_string(),
            abs_path,
            dirty: false,
            discard_armed: false,
            standalone: false,
            read_only: false,
            top: (0, 0),
            inner: Rect::default(),
            dragging: false,
            hl,
            diagnostics: Vec::new(),
            server_diagnostics: Vec::new(),
            lint_diagnostics: Vec::new(),
            completion: None,
            synced: 0,
            pre_format: None,
            find: BufferFind::default(),
            signature: None,
        }
    }

    /// Cursor position as the rest of loupe counts: 1-based line, 1-based
    /// char column.
    pub fn cursor_pos(&self) -> (usize, usize) {
        let (row, col) = self.textarea.cursor();
        (row + 1, col + 1)
    }

    /// The identifier the cursor is inside or next to — what a hover or
    /// a definition lookup is about, and what the message names.
    pub fn word_at_cursor(&self) -> String {
        let (row, col) = self.textarea.cursor();
        self.word_at(row, col)
    }

    /// The identifier at a buffer position, or an empty string.
    pub fn word_at(&self, row: usize, col: usize) -> String {
        let Some(line) = self.textarea.lines().get(row) else {
            return String::new();
        };
        match word_range(line, col) {
            Some((a, b)) => line.chars().skip(a).take(b - a).collect(),
            None => String::new(),
        }
    }

    /// Select the whole identifier at a buffer position and leave the
    /// cursor on its end. Returns the word, or `None` when nothing but
    /// punctuation is there.
    ///
    /// The cursor lands *after* the last character rather than inside
    /// the word, because that is where a drag would leave it. Sitting
    /// just past a word still counts as being in it, so `F12` right
    /// after a double click asks about the word that is lit up.
    pub fn select_word_at(&mut self, row: usize, col: usize) -> Option<String> {
        let line = self.textarea.lines().get(row)?.clone();
        let (start, end) = word_range(&line, col)?;
        self.textarea.cancel_selection();
        self.jump(row, start);
        self.textarea.start_selection();
        self.jump(row, end);
        Some(line.chars().skip(start).take(end - start).collect())
    }

    /// Select one whole line, the way a third click does.
    pub fn select_line_at(&mut self, row: usize) {
        let len = self
            .textarea
            .lines()
            .get(row)
            .map(|l| l.chars().count())
            .unwrap_or(0);
        self.textarea.cancel_selection();
        self.jump(row, 0);
        self.textarea.start_selection();
        self.jump(row, len);
    }

    /// Drop any selection and put the cursor at a buffer position.
    /// `CursorMove::Jump` counts in `u16`, so a line longer than 65535
    /// characters is clamped rather than wrapped.
    fn jump(&mut self, row: usize, col: usize) {
        self.textarea.move_cursor(CursorMove::Jump(
            row.min(u16::MAX as usize) as u16,
            col.min(u16::MAX as usize) as u16,
        ));
    }

    /// The word the selection covers, when the selection is exactly one
    /// identifier on one line. Every other place that word appears is
    /// marked while it holds — the reason a double click is how you ask
    /// "where else is this?" before you ask the language server.
    pub fn selected_word(&self) -> Option<String> {
        let ((sr, sc), (er, ec)) = self.textarea.selection_range()?;
        if sr != er || ec <= sc {
            return None;
        }
        let line = self.textarea.lines().get(sr)?;
        let (a, b) = word_range(line, sc)?;
        if (a, b) != (sc, ec) {
            return None;
        }
        Some(line.chars().skip(a).take(b - a).collect())
    }

    /// Problems in the file, worst-first on each line and in line order —
    /// the order the problem list shows them in, and the order `F8` walks.
    pub fn problems(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Put the cursor on the next problem in `delta` direction, wrapping
    /// at the ends. Returns the problem it landed on, or `None` when the
    /// file is clean.
    pub fn step_problem(&mut self, delta: i32) -> Option<Diagnostic> {
        let (line, col) = self.cursor_pos();
        let all: Vec<Diagnostic> = self.problems().to_vec();
        if all.is_empty() {
            return None;
        }
        let next = if delta >= 0 {
            all.iter()
                .find(|d| (d.line, d.col) > (line, col))
                .or_else(|| all.first())
        } else {
            all.iter()
                .rev()
                .find(|d| (d.line, d.col) < (line, col))
                .or_else(|| all.last())
        }?
        .clone();
        self.textarea.cancel_selection();
        self.jump(next.line.saturating_sub(1), next.col.saturating_sub(1));
        Some(next)
    }

    /// The identifier being typed, immediately before the cursor.
    pub fn word_prefix(&self) -> String {
        let (row, col) = self.textarea.cursor();
        let Some(line) = self.textarea.lines().get(row) else {
            return String::new();
        };
        let chars: Vec<char> = line.chars().take(col).collect();
        let start = chars
            .iter()
            .rposition(|c| !is_word(*c))
            .map(|i| i + 1)
            .unwrap_or(0);
        chars[start..].iter().collect()
    }

    /// Take the language server's answer. Returns true when it changed
    /// anything, so a caller can skip a redraw.
    pub fn set_server_diagnostics(&mut self, list: Vec<Diagnostic>) -> bool {
        if self.server_diagnostics == list {
            return false;
        }
        self.server_diagnostics = list;
        self.remerge();
        true
    }

    /// Take the linter's answer.
    pub fn set_lint_diagnostics(&mut self, list: Vec<Diagnostic>) -> bool {
        if self.lint_diagnostics == list {
            return false;
        }
        self.lint_diagnostics = list;
        self.remerge();
        true
    }

    /// Rebuild the merged list: both sources, in line order, worst first
    /// on each line.
    ///
    /// Sorted here rather than at every reader, because four of them —
    /// the gutter, the margin, the status bar and `F8` — all have to
    /// agree on which problem a line's "worst" is.
    fn remerge(&mut self) {
        let mut all: Vec<Diagnostic> = self
            .server_diagnostics
            .iter()
            .chain(self.lint_diagnostics.iter())
            .cloned()
            .collect();
        all.sort_by(|a, b| {
            (a.line, a.col, a.severity)
                .cmp(&(b.line, b.col, b.severity))
                .then_with(|| a.message.cmp(&b.message))
        });
        self.diagnostics = all;
    }

    /// Diagnostics that touch the cursor line, worst first.
    pub fn diagnostics_here(&self) -> Vec<&Diagnostic> {
        let (line, _) = self.cursor_pos();
        let mut here: Vec<&Diagnostic> =
            self.diagnostics.iter().filter(|d| d.line == line).collect();
        here.sort_by_key(|d| d.severity);
        here
    }

    /// The worst diagnostic on a line, for the gutter marker.
    pub fn diagnostic_on(&self, line: usize) -> Option<&Diagnostic> {
        self.diagnostics
            .iter()
            .filter(|d| d.line == line)
            .min_by_key(|d| d.severity)
    }

    /// Open the popup with a fresh set of suggestions. Returns false when
    /// there is nothing worth showing.
    pub fn open_completion(&mut self, items: Vec<Completion>) -> bool {
        if items.is_empty() {
            self.completion = None;
            return false;
        }
        let (row, col) = self.textarea.cursor();
        let prefix = self.word_prefix();
        let mut state = CompletionState {
            all: items,
            shown: Vec::new(),
            sel: 0,
            scroll: 0,
            start: (row, col - prefix.chars().count().min(col)),
        };
        if !state.refilter(&prefix) {
            self.completion = None;
            return false;
        }
        self.completion = Some(state);
        true
    }

    /// After a keystroke: narrow the open popup, or close it when the
    /// cursor has left the word it belongs to.
    pub fn update_completion(&mut self) {
        let Some(state) = &self.completion else {
            return;
        };
        let (row, col) = self.textarea.cursor();
        if row != state.start.0 || col < state.start.1 {
            self.completion = None;
            return;
        }
        let prefix = self.word_prefix();
        let start_col = state.start.1;
        let Some(state) = &mut self.completion else {
            return;
        };
        // The word must still start where the popup thinks it does.
        if col - start_col != prefix.chars().count() {
            self.completion = None;
            return;
        }
        if !state.refilter(&prefix) {
            self.completion = None;
        }
    }

    /// Put the selected suggestion into the buffer, replacing the word
    /// that was being typed.
    pub fn accept_completion(&mut self) -> Option<String> {
        let state = self.completion.take()?;
        let item = state.selected()?.clone();
        let (_, col) = self.textarea.cursor();
        // Prefer the server's own idea of what to replace: it knows when
        // the word started before the cursor.
        let start_col = item
            .replace
            .map(|(_, sc, _, _)| sc)
            .unwrap_or(state.start.1)
            .min(col);
        for _ in 0..col.saturating_sub(start_col) {
            self.textarea.delete_char();
        }
        self.textarea.insert_str(&item.insert);
        self.dirty = true;
        Some(item.label)
    }

    /// Undo a format in one step, if the last thing that happened was
    /// one. Returns false when there is nothing of ours to undo and the
    /// editor's own undo should run instead.
    pub fn undo_format(&mut self) -> bool {
        let Some(before) = self.pre_format.take() else {
            return false;
        };
        let (row, col) = self.textarea.cursor();
        self.textarea.select_all();
        self.textarea.insert_str(&before);
        self.textarea.cancel_selection();
        let rows = self.textarea.lines().len();
        let row = row.min(rows.saturating_sub(1));
        let col = col.min(self.textarea.lines()[row].chars().count());
        self.textarea
            .move_cursor(CursorMove::Jump(row as u16, col as u16));
        true
    }

    /// Any edit of the user's own makes the pre-format text stale.
    pub fn touched(&mut self) {
        self.pre_format = None;
    }

    /// Replace the whole buffer, keeping the cursor where it was (clamped
    /// — formatting moves lines around).
    ///
    /// Done as select-all-then-insert rather than rebuilding the
    /// `TextArea`, so the change is one undo step and Ctrl+Z puts the
    /// unformatted text back.
    pub fn apply_edits(&mut self, edits: &[TextEdit]) -> bool {
        let before = self.content();
        let after = apply_text_edits(&before, edits);
        if after == before {
            return false;
        }
        let (row, col) = self.textarea.cursor();
        self.pre_format = Some(before);
        self.textarea.select_all();
        self.textarea.insert_str(&after);
        self.textarea.cancel_selection();
        let rows = self.textarea.lines().len();
        let row = row.min(rows.saturating_sub(1));
        let col = col.min(self.textarea.lines()[row].chars().count());
        self.textarea
            .move_cursor(CursorMove::Jump(row as u16, col as u16));
        self.dirty = true;
        true
    }

    pub fn content(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// What Ctrl+C would copy: the selection, or the cursor line when
    /// there isn't one — the same rule the diff view uses, so the key
    /// means one thing everywhere.
    pub fn copy_target(&mut self) -> Option<(String, String)> {
        if self.textarea.selection_range().is_some() {
            self.textarea.copy();
            let text = self.textarea.yank_text();
            if !text.is_empty() {
                let n = text.lines().count().max(1);
                let what = if n == 1 {
                    "the selection".to_string()
                } else {
                    format!("{n} lines")
                };
                return Some((text, what));
            }
        }
        let (row, _) = self.textarea.cursor();
        let line = self.textarea.lines().get(row)?.clone();
        Some((line, format!("line {}", row + 1)))
    }

    pub fn jump_to_line(&mut self, line_1based: usize) {
        let row = line_1based.saturating_sub(1) as u16;
        self.textarea.move_cursor(CursorMove::Jump(row, 0));
    }

    /// The first buffer row on screen. The blame pane beside the editor
    /// scrolls with this, and only this — a buffer has no folds, so one
    /// file line is one row there.
    fn match_count(&self) -> String {
        if self.find.matches.is_empty() {
            format!("no match for “{}”", self.find.query)
        } else {
            format!("{}/{}", self.find.at + 1, self.find.matches.len())
        }
    }

    /// The prompt, written into the border where the file name goes.
    fn prompt_title(&self, field: Field) -> String {
        let mark = |f: Field| if f == field { "▏" } else { "" };
        format!(
            " find: {}{} → {}{} — {} · Tab switches · Enter next · Alt+A all · Esc cancels ",
            self.find.query,
            mark(Field::Query),
            self.find.replacement,
            mark(Field::Replacement),
            self.match_count()
        )
    }

    /// A keystroke for the prompt. False when the key was not one of its.
    pub fn find_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};
        let Some(field) = self.find.typing else {
            return false;
        };
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => self.cancel_find(),
            KeyCode::Tab => {
                self.find.typing = Some(match field {
                    Field::Query => Field::Replacement,
                    Field::Replacement => Field::Query,
                });
            }
            KeyCode::Enter => self.step_match(1),
            KeyCode::Char('a') | KeyCode::Char('A') if alt => {
                self.replace_all();
                self.find.typing = None;
            }
            KeyCode::Char('r') | KeyCode::Char('R') if alt => {
                self.replace_current();
            }
            KeyCode::Backspace => {
                match field {
                    Field::Query => {
                        self.find.query.pop();
                    }
                    Field::Replacement => {
                        self.find.replacement.pop();
                    }
                }
                self.refresh_find();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) && !alt => {
                match field {
                    Field::Query => self.find.query.push(c),
                    Field::Replacement => self.find.replacement.push(c),
                }
                self.refresh_find();
            }
            _ => return false,
        }
        true
    }

    /// Open the prompt, remembering where to put the cursor back.
    pub fn open_find(&mut self) {
        self.find.origin = self.textarea.cursor();
        self.find.typing = Some(Field::Query);
    }

    /// Re-scan the buffer and move to the first match at or after where
    /// the search started, so the view follows the query as it is typed.
    pub fn refresh_find(&mut self) {
        self.find.matches.clear();
        if !self.find.query.is_empty() {
            for (row, line) in self.textarea.lines().iter().enumerate() {
                for (a, b) in crate::search::find_ranges(line, &self.find.query) {
                    self.find.matches.push((row, a, b));
                }
            }
        }
        let (from, _) = self.find.origin;
        self.find.at = self
            .find
            .matches
            .iter()
            .position(|(row, ..)| *row >= from)
            .unwrap_or(0);
        self.go_to_match();
    }

    /// Step to the next or previous match, wrapping at each end.
    pub fn step_match(&mut self, delta: i32) {
        let n = self.find.matches.len();
        if n == 0 {
            return;
        }
        self.find.at = (self.find.at as i32 + delta).rem_euclid(n as i32) as usize;
        self.go_to_match();
    }

    fn go_to_match(&mut self) {
        let Some((row, col, _)) = self.find.current() else {
            return;
        };
        self.textarea
            .move_cursor(CursorMove::Jump(row as u16, col as u16));
    }

    /// Put the cursor back where the search started and forget the query.
    pub fn cancel_find(&mut self) {
        let (row, col) = self.find.origin;
        self.find = BufferFind::default();
        self.textarea
            .move_cursor(CursorMove::Jump(row as u16, col as u16));
    }

    /// Replace the match the cursor is on, and move to the next.
    ///
    /// Returns false when there was nothing to replace. The edit goes
    /// through the same insert and delete the keyboard uses, so one undo
    /// step covers it and the language server hears about it like any
    /// other typing.
    pub fn replace_current(&mut self) -> bool {
        let Some((row, col, end)) = self.find.current() else {
            return false;
        };
        self.textarea
            .move_cursor(CursorMove::Jump(row as u16, col as u16));
        self.textarea.start_selection();
        self.textarea
            .move_cursor(CursorMove::Jump(row as u16, end as u16));
        self.textarea.cut();
        let replacement = self.find.replacement.clone();
        if !replacement.is_empty() {
            self.textarea.insert_str(&replacement);
        }
        self.touched();
        self.dirty = true;
        // The text moved, so every match after this one did too.
        let at = self.find.at;
        self.refresh_find();
        self.find.at = at.min(self.find.matches.len().saturating_sub(1));
        self.go_to_match();
        true
    }

    /// Replace every match. Returns how many.
    pub fn replace_all(&mut self) -> usize {
        let mut n = 0;
        // Back to front: replacing shifts the columns of everything after
        // a match on the same line, and nothing before it.
        let mut targets = self.find.matches.clone();
        targets.sort();
        for (row, col, end) in targets.into_iter().rev() {
            self.textarea
                .move_cursor(CursorMove::Jump(row as u16, col as u16));
            self.textarea.start_selection();
            self.textarea
                .move_cursor(CursorMove::Jump(row as u16, end as u16));
            self.textarea.cut();
            let replacement = self.find.replacement.clone();
            if !replacement.is_empty() {
                self.textarea.insert_str(&replacement);
            }
            n += 1;
        }
        if n > 0 {
            self.touched();
            self.dirty = true;
        }
        self.refresh_find();
        n
    }

    /// Comment or uncomment the cursor line, or every line a selection
    /// touches.
    ///
    /// Commenting inserts at the shallowest indent of the lines involved,
    /// so a block keeps its shape instead of having its markers stagger
    /// down the left edge. Uncommenting only happens when *every* line is
    /// already commented — a mixed block gets commented, which is what a
    /// second press then undoes.
    pub fn toggle_comment(&mut self) -> bool {
        let Some(marker) = comment_marker(&self.path) else {
            return false;
        };
        let (first, last) = match self.textarea.selection_range() {
            Some(((a, _), (b, col))) => (a, if col == 0 && b > a { b - 1 } else { b }),
            None => {
                let (row, _) = self.textarea.cursor();
                (row, row)
            }
        };
        let lines = self.textarea.lines();
        let rows: Vec<usize> = (first..=last)
            .filter(|r| lines.get(*r).is_some_and(|l| !l.trim().is_empty()))
            .collect();
        if rows.is_empty() {
            return false;
        }
        let all_commented = rows
            .iter()
            .all(|r| lines[*r].trim_start().starts_with(marker));
        let indent = rows
            .iter()
            .map(|r| lines[*r].len() - lines[*r].trim_start().len())
            .min()
            .unwrap_or(0);

        let (cur_row, cur_col) = self.textarea.cursor();
        let mut edited: Vec<String> = lines.to_vec();
        for r in &rows {
            let line = &edited[*r];
            edited[*r] = if all_commented {
                let at = line.len() - line.trim_start().len();
                let rest = &line[at + marker.len()..];
                // The space this put in when commenting comes back out.
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                format!("{}{rest}", &line[..at])
            } else {
                format!("{}{marker} {}", &line[..indent], &line[indent..])
            };
        }
        self.replace_all_lines(edited);
        let shift = marker.len() + 1;
        let col = if all_commented {
            cur_col.saturating_sub(shift)
        } else {
            cur_col + shift
        };
        self.textarea
            .move_cursor(CursorMove::Jump(cur_row as u16, col as u16));
        self.dirty = true;
        self.touched();
        true
    }

    /// Swap the whole buffer, in one undo step.
    ///
    /// Replacing the text costs two of `tui-textarea`'s history entries —
    /// the delete, then the insert — which is why `pre_format` exists for
    /// the formatter. The same trick serves here.
    fn replace_all_lines(&mut self, lines: Vec<String>) {
        self.pre_format = Some(self.content());
        self.textarea.select_all();
        self.textarea.cut();
        self.textarea.insert_str(lines.join("\n"));
    }

    /// A newline that keeps the indent, and adds one level after a line
    /// that opens a block.
    ///
    /// `tui-textarea` inserts a bare newline, which puts the cursor in
    /// column zero of a file that is indented four levels deep.
    pub fn newline_with_indent(&mut self) {
        let (row, col) = self.textarea.cursor();
        let line = self.textarea.lines().get(row).cloned().unwrap_or_default();
        let indent: String = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        // Only what is left of the cursor decides: pressing Enter in the
        // middle of a line should not indent by a brace further along it.
        let before: String = line.chars().take(col).collect();
        let opens =
            before.trim_end().ends_with(['{', '(', '[', ':']) || before.trim_end().ends_with("=>");
        self.textarea.insert_newline();
        if !indent.is_empty() {
            self.textarea.insert_str(indent);
        }
        if opens {
            // Bound first: `indent()` borrows the textarea and
            // `insert_str` needs it mutably.
            let step = self.textarea.indent();
            self.textarea.insert_str(step);
        }
    }

    /// The bracket matching the one under the cursor, when there is one.
    ///
    /// Counts depth rather than looking for the next of the same kind, so
    /// a nested pair does not steal the match. Bounded, because an unmatched
    /// brace at the top of a large file should cost a screenful of scanning
    /// rather than the whole buffer on every keystroke.
    fn matching_bracket(&self, row: usize, col: usize) -> Option<(usize, usize)> {
        const SCAN_LIMIT: usize = 5_000;
        let lines = self.textarea.lines();
        let here = lines.get(row)?.chars().nth(col)?;
        let (want, forward) = bracket_pair(here)?;
        let mut depth = 0i32;
        let mut seen = 0usize;
        let mut r = row;
        let mut c = col;
        loop {
            let chars: Vec<char> = lines.get(r)?.chars().collect();
            if let Some(ch) = chars.get(c) {
                if *ch == here {
                    depth += 1;
                } else if *ch == want {
                    depth -= 1;
                    if depth == 0 {
                        return Some((r, c));
                    }
                }
            }
            seen += 1;
            if seen > SCAN_LIMIT {
                return None;
            }
            if forward {
                if c + 1 < chars.len() {
                    c += 1;
                } else {
                    r += 1;
                    if r >= lines.len() {
                        return None;
                    }
                    c = 0;
                }
            } else if c > 0 {
                c -= 1;
            } else {
                r = r.checked_sub(1)?;
                c = lines.get(r)?.chars().count().saturating_sub(1);
            }
        }
    }

    pub fn scroll_top(&self) -> usize {
        self.top.0 as usize
    }

    /// Width of the line-number gutter, matching tui-textarea's rendering.
    fn lnum_width(&self) -> u16 {
        num_digits(self.textarea.lines().len()) + 2
    }

    /// Update the shadow viewport exactly the way tui-textarea's widget
    /// would, then render the buffer ourselves — per-character, so syntax
    /// colors show — keeping the exact combined-column layout (gutter +
    /// text, horizontally scrolled by `top.1`) that `hit` assumes.
    /// Must be called every frame the editor is visible.
    pub fn render(&mut self, f: &mut Frame, area: Rect, focused: bool) {
        let title = if let Some(field) = self.find.typing {
            self.prompt_title(field)
        } else if self.find.active() {
            format!(
                " ✎ {} — {} · Alt+N / Alt+B step · Esc clears ",
                self.path,
                self.match_count()
            )
        } else if let Some(sig) = &self.signature {
            format!(" ƒ {} — Esc clears ", sig.label)
        } else if self.read_only {
            format!(" 👁 {} — read-only · Ctrl+C copy · Esc close ", self.path)
        } else {
            format!(
                " ✎ {}{} — Ctrl+S save · Ctrl+C copy · Esc close ",
                self.path,
                if self.dirty { " [+]" } else { "" }
            )
        };
        let p = palette();
        let border_style = if focused {
            Style::default().fg(p.accent)
        } else {
            Style::default().fg(p.faint)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(title);
        self.inner = block.inner(area);
        f.render_widget(block, area);

        let (cur_row, cur_col) = self.textarea.cursor();
        // Row component.
        self.top.0 = next_scroll_top(self.top.0, cur_row as u16, self.inner.height);
        // Column component (replicates the line-number adjustment upstream).
        let mut ccol = cur_col as u16;
        let lnum = self.lnum_width();
        if ccol <= lnum {
            ccol *= 2;
        } else {
            ccol += lnum;
        }
        self.top.1 = next_scroll_top(self.top.1, ccol, self.inner.width);

        // Keep syntax highlighting in sync (cheap no-op when unchanged).
        self.hl.update(self.textarea.lines());

        let n_lines = self.textarea.lines().len();
        // Worked out once per frame rather than once per row: the answer
        // is the same for every row on screen.
        let sel_word = self.selected_word();
        let mut out: Vec<Line> = Vec::with_capacity(self.inner.height as usize);
        for vis in 0..self.inner.height as usize {
            let row = self.top.0 as usize + vis;
            if row >= n_lines {
                out.push(Line::default());
            } else {
                out.push(self.render_row(row, cur_row, cur_col, sel_word.as_deref()));
            }
        }
        f.render_widget(Paragraph::new(out), self.inner);
        self.render_completion(f);
    }

    /// One visible row: gutter + syntax-colored text with selection, cursor
    /// line underline, and a reversed cursor cell, clipped to the viewport.
    /// The completion popup, floated under the cursor (or above it, when
    /// the cursor is near the bottom).
    fn render_completion(&self, f: &mut Frame) {
        let p = palette();
        let Some(state) = &self.completion else {
            return;
        };
        if state.len() == 0 {
            return;
        }
        let rows = state.len().min(COMPLETION_ROWS);
        // Column widths are worked out once for the whole popup, not per
        // row — computing them per row is what makes a list like this
        // come out ragged.
        const KIND_W: usize = 10;
        let label_w = state
            .items()
            .map(|c| c.label.chars().count())
            .max()
            .unwrap_or(12)
            .clamp(8, 32);
        let detail_w = state
            .items()
            .filter_map(|c| c.detail.as_ref().map(|d| d.chars().count()))
            .max()
            .unwrap_or(0)
            .min(34);
        let width = (label_w + KIND_W + detail_w + 2)
            .clamp(24, self.inner.width.saturating_sub(2).max(24) as usize)
            as u16;
        // Whatever the clamp took, take it off the detail column.
        let detail_w = (width as usize).saturating_sub(label_w + KIND_W + 2);

        let (cur_row, _) = self.textarea.cursor();
        let screen_row = self.inner.y + (cur_row as u16).saturating_sub(self.top.0);
        // Below the cursor if it fits, above it otherwise.
        let below = screen_row + 1 + rows as u16 <= self.inner.y + self.inner.height;
        let y = if below {
            screen_row + 1
        } else {
            screen_row.saturating_sub(rows as u16)
        };
        let x = (self.inner.x + self.lnum_width() + state.start.1 as u16)
            .saturating_sub(self.top.1)
            .min(self.inner.x + self.inner.width.saturating_sub(width));

        let area = Rect {
            x,
            y,
            width,
            height: rows as u16,
        };
        f.render_widget(Clear, area);
        let lines: Vec<Line> = state
            .shown
            .iter()
            .skip(state.scroll)
            .take(rows)
            .enumerate()
            .filter_map(|(i, idx)| {
                let item = state.all.get(*idx)?;
                let selected = state.scroll + i == state.sel;
                let base = if selected {
                    Style::default().bg(p.btn_active_bg).fg(p.btn_active_fg)
                } else {
                    Style::default().bg(p.btn_bg).fg(p.text)
                };
                Some(Line::from(vec![
                    Span::styled(
                        format!(" {}", truncate_pad(&item.label, label_w)),
                        if selected {
                            base.add_modifier(Modifier::BOLD)
                        } else {
                            base
                        },
                    ),
                    Span::styled(
                        format!(" {}", truncate_pad(item.kind, KIND_W - 1)),
                        base.fg(p.dim),
                    ),
                    Span::styled(
                        truncate_pad(item.detail.as_deref().unwrap_or(""), detail_w),
                        base.fg(p.faint),
                    ),
                ]))
            })
            .collect();
        f.render_widget(Paragraph::new(lines), area);
    }

    fn render_row(
        &self,
        row: usize,
        cur_row: usize,
        cur_col: usize,
        sel_word: Option<&str>,
    ) -> Line<'static> {
        let p = palette();
        let text = &self.textarea.lines()[row];
        let sel = self.textarea.selection_range();
        let tab = (self.textarea.tab_length() as usize).max(1);
        let width = self.inner.width as usize;
        let skip = self.top.1 as usize;

        // Per-character foreground colors from the highlight cache.
        let char_colors: Vec<Color> = match self.hl.line(row) {
            Some(segs) if !segs.is_empty() => segs
                .iter()
                .flat_map(|(c, t)| t.chars().map(move |_| *c))
                .collect(),
            _ => Vec::new(),
        };
        let is_cursor_line = row == cur_row;
        let selected = |ci: usize| match sel {
            Some((start, end)) => (row, ci) >= start && (row, ci) < end,
            None => false,
        };
        // The bracket under the cursor and its partner, so the shape of a
        // block is visible without counting braces.
        let brackets: Option<((usize, usize), (usize, usize))> = self
            .matching_bracket(cur_row, cur_col)
            .map(|other| ((cur_row, cur_col), other));
        let is_bracket =
            |ci: usize| brackets.is_some_and(|(a, b)| (row, ci) == a || (row, ci) == b);
        // Search matches on this row, and which of them the cursor is on.
        // Painted here rather than through `tui-textarea`'s own search
        // colors: this renderer builds its own spans, so those colors
        // would never reach the screen.
        let here: Vec<(usize, usize)> = self
            .find
            .matches
            .iter()
            .filter(|(r, ..)| *r == row)
            .map(|(_, a, b)| (*a, *b))
            .collect();
        let current = self.find.current().filter(|(r, ..)| *r == row);
        // The other places the selected word appears, marked the way an
        // editor marks them after a double click. Skipped when nothing is
        // selected, so an ordinary cursor lights nothing up.
        let others: Vec<(usize, usize)> = match sel_word {
            Some(w) => word_occurrences(text, w),
            None => Vec::new(),
        };
        let match_style = |ci: usize| -> Option<Style> {
            if !here.iter().any(|(a, b)| ci >= *a && ci < *b) {
                return None;
            }
            let on_this_one = current.is_some_and(|(_, a, b)| ci >= a && ci < b);
            // The same two colors the diff's search uses, so a match
            // means the same thing wherever the reader meets one.
            Some(if on_this_one {
                Style::default().bg(p.selected).add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(p.matched)
            })
        };

        // Build single-column cells in combined (gutter + text) space so
        // horizontal clipping below stays exact.
        let mut cells: Vec<(char, Style)> = Vec::new();
        // The last column of the gutter carries the diagnostic marker, so
        // a problem is visible without moving the cursor to the line.
        let worst = self.diagnostic_on(row + 1);
        let gutter = format!(
            "{:>w$}",
            row + 1,
            w = (self.lnum_width() as usize).saturating_sub(1)
        );
        for ch in gutter.chars() {
            cells.push((ch, Style::default().fg(p.gutter)));
        }
        match worst {
            Some(d) => cells.push((
                d.mark(),
                Style::default()
                    .fg(diagnostic_color(d))
                    .add_modifier(Modifier::BOLD),
            )),
            None => cells.push((' ', Style::default().fg(p.gutter))),
        }
        let mut disp = 0usize;
        // Columns the diagnostic actually covers, so the underline marks
        // the expression rather than the whole line. A zero-width span —
        // TypeScript reports one for "not assignable" — still covers the
        // character it points at, because an underline under nothing is
        // an underline nobody sees.
        let bad = worst.map(|d| {
            (
                crate::lsp::char_column(text, d.col.saturating_sub(1)),
                if d.end_col == usize::MAX {
                    text.chars().count()
                } else {
                    crate::lsp::char_column(text, d.end_col.saturating_sub(1))
                },
            )
        });
        let bad_color = worst.map(diagnostic_color);
        for (ci, ch) in text.chars().enumerate() {
            let fg = char_colors.get(ci).copied().unwrap_or(p.code);
            let mut st = Style::default().fg(fg);
            if bad.is_some_and(|(a, b)| ci >= a && ci < b.max(a + 1)) {
                // Colored *and* underlined. Color alone is the one thing
                // a reader with a color-vision difference cannot use, and
                // an editor that only says "this is wrong" in red says it
                // to some people and not others.
                st = st
                    .fg(bad_color.unwrap_or(p.err))
                    .add_modifier(Modifier::UNDERLINED);
            }
            // The matched pair, under the search colors: a bracket that is
            // also a search hit is a search hit first.
            if is_bracket(ci) {
                st = st.fg(p.accent).add_modifier(Modifier::BOLD);
            }
            // Another use of the selected word. Under the search colors
            // and under the selection itself: the word the reader is
            // standing on has to keep looking selected.
            if others.iter().any(|(a, b)| ci >= *a && ci < *b) {
                st = st.bg(p.matched);
            }
            // A search match, under a live selection: dragging over a
            // match should still look like dragging.
            if let Some(m) = match_style(ci) {
                st = st.patch(m);
            }
            if selected(ci) {
                st = st.bg(p.editor_sel);
            }
            if is_cursor_line {
                st = st.add_modifier(Modifier::UNDERLINED);
                if ci == cur_col {
                    st = st.add_modifier(Modifier::REVERSED);
                }
            }
            if ch == '\t' {
                let n = tab - (disp % tab);
                for _ in 0..n {
                    cells.push((' ', st));
                }
                disp += n;
            } else {
                // Wide characters occupy their natural width when drawn; a
                // clipped one at the edge is dropped rather than split.
                cells.push((ch, st));
                disp += ch.width().unwrap_or(0);
            }
        }
        if is_cursor_line && cur_col >= text.chars().count() {
            cells.push((
                ' ',
                Style::default().add_modifier(Modifier::REVERSED | Modifier::UNDERLINED),
            ));
        }

        // The message itself, in the margin past the end of the line.
        //
        // A mark in the gutter says *that* something is wrong; this says
        // *what*, without moving the cursor to the line to find out. Only
        // when there is real room for it: a note squeezed into six
        // columns is worse than no note, and the code has to stay
        // readable — it is the thing being edited.
        if let Some(d) = worst {
            let used: usize = cells.iter().map(|(c, _)| c.width().unwrap_or(0)).sum();
            let room = (width + skip).saturating_sub(used + MARGIN_GAP);
            if room >= MARGIN_MIN {
                let note = format!("{} {}", d.mark(), one_line(&d.message));
                let style = Style::default().fg(diagnostic_color(d));
                for _ in 0..MARGIN_GAP {
                    cells.push((' ', style));
                }
                let mut w = 0usize;
                for ch in note.chars() {
                    let cw = ch.width().unwrap_or(0);
                    if w + cw > room {
                        cells.push(('…', style));
                        break;
                    }
                    cells.push((ch, style));
                    w += cw;
                }
            }
        }

        // Clip: skip `skip` display columns, keep `width`. Consecutive
        // characters sharing a style are coalesced into one span — a span
        // (and its String) per character would allocate thousands of times
        // per frame on a wide terminal.
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut run = String::new();
        let mut run_style = Style::default();
        let mut pos = 0usize;
        let mut used = 0usize;
        fn flush(run: &mut String, style: Style, spans: &mut Vec<Span<'static>>) {
            if !run.is_empty() {
                spans.push(Span::styled(std::mem::take(run), style));
            }
        }
        for (ch, st) in cells {
            let w = if ch == '\t' {
                1
            } else {
                ch.width().unwrap_or(0)
            };
            if pos + w <= skip {
                pos += w;
                continue;
            }
            if pos < skip {
                // Left-clipped wide character: keep alignment with spaces.
                let vis = (pos + w - skip).min(width - used);
                flush(&mut run, run_style, &mut spans);
                spans.push(Span::styled(" ".repeat(vis), st));
                used += vis;
                pos += w;
                continue;
            }
            if used + w > width {
                break;
            }
            if st != run_style {
                flush(&mut run, run_style, &mut spans);
                run_style = st;
            }
            run.push(ch);
            used += w;
            pos += w;
        }
        flush(&mut run, run_style, &mut spans);
        Line::from(spans)
    }

    /// Map a screen position to a (row, col) in the buffer.
    fn hit(&self, x: u16, y: u16) -> Option<(u16, u16)> {
        if x < self.inner.x
            || y < self.inner.y
            || x >= self.inner.x + self.inner.width
            || y >= self.inner.y + self.inner.height
        {
            return None;
        }
        let row = self.top.0 + (y - self.inner.y);
        let row = row.min(self.textarea.lines().len().saturating_sub(1) as u16);
        // Horizontal: translate display column to character index, accounting
        // for the line-number gutter, horizontal scroll, and wide characters.
        let disp_x = (x - self.inner.x) as i32 + self.top.1 as i32 - self.lnum_width() as i32;
        let line = &self.textarea.lines()[row as usize];
        let col = display_to_char_col(line, disp_x.max(0) as usize, self.textarea.tab_length());
        Some((row, col as u16))
    }

    /// Whether a screen cell is inside the editor surface. The
    /// right-click menu needs to know before it decides to open, and
    /// `hit` clamps rather than refuses.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.inner.x
            && y >= self.inner.y
            && x < self.inner.x + self.inner.width
            && y < self.inner.y + self.inner.height
    }

    pub fn on_click(&mut self, x: u16, y: u16) -> bool {
        if let Some((row, col)) = self.hit(x, y) {
            self.textarea.cancel_selection();
            self.textarea.move_cursor(CursorMove::Jump(row, col));
            self.dragging = true;
            true
        } else {
            false
        }
    }

    /// A second click on the same cell takes the whole word. Returns the
    /// word, or `None` when the pointer is off the surface or on
    /// punctuation.
    ///
    /// The drag is cancelled: the press that starts a double click has
    /// already armed one, and moving the mouse afterwards must not throw
    /// the word away.
    pub fn on_double_click(&mut self, x: u16, y: u16) -> Option<String> {
        let (row, col) = self.hit(x, y)?;
        self.dragging = false;
        self.select_word_at(row as usize, col as usize)
    }

    /// A third click takes the whole line.
    pub fn on_triple_click(&mut self, x: u16, y: u16) -> bool {
        let Some((row, _)) = self.hit(x, y) else {
            return false;
        };
        self.dragging = false;
        self.select_line_at(row as usize);
        true
    }

    /// A right click: put the cursor under the pointer and take the word
    /// there, so the menu that follows is about what was clicked. A click
    /// inside a live selection keeps that selection instead — the reader
    /// selected it on purpose.
    ///
    /// Returns false when the pointer is not on the editor surface, and
    /// the caller leaves the menu closed.
    pub fn on_right_click(&mut self, x: u16, y: u16) -> bool {
        if !self.contains(x, y) {
            return false;
        }
        let Some((row, col)) = self.hit(x, y) else {
            return false;
        };
        self.dragging = false;
        let inside = match self.textarea.selection_range() {
            Some((start, end)) => {
                let at = (row as usize, col as usize);
                at >= start && at < end
            }
            None => false,
        };
        if !inside && self.select_word_at(row as usize, col as usize).is_none() {
            self.textarea.cancel_selection();
            self.textarea.move_cursor(CursorMove::Jump(row, col));
        }
        true
    }

    pub fn on_drag(&mut self, x: u16, y: u16) {
        if !self.dragging {
            return;
        }
        if let Some((row, col)) = self.hit(x, y) {
            if !self.textarea.is_selecting() {
                self.textarea.start_selection();
            }
            self.textarea.move_cursor(CursorMove::Jump(row, col));
        }
    }

    pub fn on_release(&mut self) {
        self.dragging = false;
    }

    pub fn scroll_lines(&mut self, delta: i32) {
        let mv = if delta < 0 {
            CursorMove::Up
        } else {
            CursorMove::Down
        };
        for _ in 0..delta.unsigned_abs() {
            self.textarea.move_cursor(mv);
        }
    }

    /// Page height of the last-rendered viewport (for PageUp/PageDown, which
    /// must NOT reach tui-textarea's own handler: that one calls
    /// `TextArea::scroll` internally and would desync the shadow viewport).
    pub fn page(&self) -> i32 {
        (self.inner.height as i32).max(1)
    }
}

/// Convert a display column (0-based, in terminal cells) to a character index.
/// Tabs advance to the next tab stop, matching tui-textarea's rendering.
fn display_to_char_col(line: &str, disp: usize, tab_len: u8) -> usize {
    let tab = (tab_len as usize).max(1);
    let mut width = 0usize;
    for (i, ch) in line.chars().enumerate() {
        let w = if ch == '\t' {
            tab - (width % tab)
        } else {
            ch.width().unwrap_or(0)
        };
        if width + w > disp {
            return i;
        }
        width += w;
    }
    line.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// The custom editor renderer must paint syntax colors and the cursor.
    #[test]
    fn editor_renders_syntax_colors_and_cursor() {
        // Both the syntax colors and `palette().code` below are process
        // globals now — pin them for the duration.
        let _guard = crate::highlight::test_theme_lock();
        let src = "fn main() {\n    let s = \"text\"; // note\n    let n = 42;\n}\n";
        let mut ed = Editor::new("test.rs", std::path::PathBuf::from("/tmp/x.rs"), src);
        let backend = TestBackend::new(80, 12);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| ed.render(f, f.area(), true)).unwrap();
        let buf = term.backend().buffer();
        let mut colors = std::collections::HashSet::new();
        for y in 1..buf.area.height - 1 {
            for x in 1..buf.area.width - 1 {
                let fg = buf[(x, y)].fg;
                if matches!(fg, Color::Rgb(..)) && fg != palette().code {
                    colors.insert(format!("{fg:?}"));
                }
            }
        }
        assert!(
            colors.len() >= 3,
            "expected keyword/string/number colors in the editor, got {colors:?}"
        );
        // Cursor starts at (0, 0): reversed cell right after the gutter.
        let cx = 1 + ed.lnum_width();
        assert!(buf[(cx, 1)].modifier.contains(Modifier::REVERSED));
    }

    fn ed(text: &str) -> Editor {
        Editor::new("a.ts", std::path::PathBuf::from("/tmp/a.ts"), text)
    }

    fn item(label: &str, insert: &str) -> Completion {
        Completion {
            label: label.into(),
            insert: insert.into(),
            detail: None,
            kind: "fn",
            sort: label.into(),
            replace: None,
        }
    }

    /// Commenting a block inserts at the shallowest indent of the lines
    /// involved, so the markers line up instead of staggering down the
    /// left edge with the code.
    #[test]
    fn commenting_a_block_keeps_its_shape() {
        let mut ed = Editor::new(
            "a.rs",
            std::path::PathBuf::from("a.rs"),
            "    let a = 1;\n        let b = 2;\n",
        );
        ed.textarea.move_cursor(CursorMove::Jump(0, 0));
        ed.textarea.start_selection();
        ed.textarea.move_cursor(CursorMove::Jump(1, 10));

        assert!(ed.toggle_comment());
        assert_eq!(
            ed.content(),
            "    // let a = 1;\n    //     let b = 2;\n",
            "both markers at the shallower indent"
        );

        // And back again, exactly.
        ed.textarea.move_cursor(CursorMove::Jump(0, 0));
        ed.textarea.start_selection();
        ed.textarea.move_cursor(CursorMove::Jump(1, 10));
        assert!(ed.toggle_comment());
        assert_eq!(ed.content(), "    let a = 1;\n        let b = 2;\n");
    }

    /// A block where only some lines are commented gets commented, not
    /// half-uncommented. A second press then undoes the lot.
    #[test]
    fn a_half_commented_block_is_commented() {
        let mut ed = Editor::new("a.py", std::path::PathBuf::from("a.py"), "# one\ntwo\n");
        ed.textarea.move_cursor(CursorMove::Jump(0, 0));
        ed.textarea.start_selection();
        ed.textarea.move_cursor(CursorMove::Jump(1, 3));
        assert!(ed.toggle_comment());
        assert_eq!(ed.content(), "# # one\n# two\n");
    }

    /// A language loupe has no marker for is left alone rather than
    /// guessed at.
    #[test]
    fn an_unknown_language_is_not_commented() {
        let mut ed = Editor::new("data.bin", std::path::PathBuf::from("data.bin"), "x\n");
        assert!(!ed.toggle_comment());
        assert_eq!(ed.content(), "x\n");
    }

    /// Enter keeps the indent, and adds a level after a line that opens a
    /// block — but only for a brace left of the cursor.
    #[test]
    fn a_newline_keeps_the_indent() {
        let mut open = ed("    fn main() {\n");
        open.textarea.move_cursor(CursorMove::Jump(0, 16));
        open.newline_with_indent();
        assert_eq!(
            open.content(),
            "    fn main() {\n        \n",
            "four spaces kept, four more added for the brace"
        );

        // Splitting before the brace must not indent by it.
        let mut mid = ed("    let x = 1; {\n");
        mid.textarea.move_cursor(CursorMove::Jump(0, 15));
        mid.newline_with_indent();
        assert_eq!(mid.content(), "    let x = 1; \n    {\n");
    }

    /// The matching bracket is found by depth, so a nested pair does not
    /// steal it.
    #[test]
    fn the_matching_bracket_counts_depth() {
        let call = ed("f(g(x), y)\n");
        assert_eq!(call.matching_bracket(0, 1), Some((0, 9)), "the outer pair");
        assert_eq!(call.matching_bracket(0, 3), Some((0, 5)), "the inner one");
        assert_eq!(call.matching_bracket(0, 9), Some((0, 1)), "and backwards");
        assert_eq!(call.matching_bracket(0, 0), None, "not a bracket");

        let open = ed("f(x\n");
        assert_eq!(open.matching_bracket(0, 1), None);
    }

    /// Replacing has to work back to front.    /// Replacing has to work back to front. Every match after the one
    /// being replaced sits at a column the replacement is about to move,
    /// so replacing forwards would write the second one into the wrong
    /// place as soon as the replacement is a different length.
    #[test]
    fn replace_all_survives_a_longer_replacement() {
        let mut ed = ed("let a = a + a;\nlet b = a;\n");
        ed.find.query = "a".into();
        ed.find.replacement = "alpha".into();
        ed.refresh_find();
        assert_eq!(
            ed.find.matches.len(),
            4,
            "three on the first line, one on the second"
        );

        assert_eq!(ed.replace_all(), 4);
        assert_eq!(ed.content(), "let alpha = alpha + alpha;\nlet b = alpha;\n");
        assert!(ed.dirty, "the buffer knows it changed");
    }

    /// Stepping wraps, and Esc puts the cursor back where the search
    /// started — the incremental jumping around is undone, not left.
    #[test]
    fn stepping_wraps_and_cancelling_returns_the_cursor() {
        let mut ed = ed("one\ntwo\nthree\ntwo\n");
        ed.textarea.move_cursor(CursorMove::Jump(2, 0));
        let before = ed.cursor_pos();

        ed.open_find();
        ed.find.query = "two".into();
        ed.refresh_find();
        assert_eq!(ed.find.matches.len(), 2);
        assert_eq!(
            ed.cursor_pos().0,
            4,
            "it went to the first match at or after the cursor, 1-based"
        );

        ed.step_match(1);
        assert_eq!(ed.cursor_pos().0, 2, "past the end wraps to the first");

        ed.cancel_find();
        assert_eq!(ed.cursor_pos(), before, "and Esc puts the cursor back");
        assert!(!ed.find.active());
    }

    /// A replacement that contains the query must not be found again, or
    /// replacing `a` with `aa` never finishes.
    #[test]
    fn replacing_does_not_re_match_its_own_replacement() {
        let mut ed = ed("a a\n");
        ed.find.query = "a".into();
        ed.find.replacement = "aa".into();
        ed.refresh_find();
        assert_eq!(ed.replace_all(), 2);
        assert_eq!(ed.content(), "aa aa\n");
    }

    #[test]
    fn text_edits_apply_back_to_front() {
        let text = "const a=1;\nconst b=2;\n";
        // Two edits against the *original* positions: applying the first
        // one first would shift the second.
        let edits = vec![
            TextEdit {
                start: (0, 7),
                end: (0, 8),
                text: " = ".into(),
            },
            TextEdit {
                start: (1, 7),
                end: (1, 8),
                text: " = ".into(),
            },
        ];
        assert_eq!(
            apply_text_edits(text, &edits),
            "const a = 1;\nconst b = 2;\n"
        );

        // An edit spanning lines collapses them.
        let join = vec![TextEdit {
            start: (0, 10),
            end: (1, 0),
            text: " ".into(),
        }];
        assert_eq!(apply_text_edits(text, &join), "const a=1; const b=2;\n");

        // Inserting a line.
        let add = vec![TextEdit {
            start: (0, 0),
            end: (0, 0),
            text: "// header\n".into(),
        }];
        assert_eq!(
            apply_text_edits(text, &add),
            "// header\nconst a=1;\nconst b=2;\n"
        );
        assert_eq!(apply_text_edits(text, &[]), text);
    }

    #[test]
    fn formatting_keeps_the_cursor_and_one_undo_step() {
        let mut e = ed("const a=1;\nconst b=2;\n");
        e.textarea.move_cursor(CursorMove::Jump(1, 5));
        let edits = vec![TextEdit {
            start: (0, 7),
            end: (0, 8),
            text: " = ".into(),
        }];
        assert!(e.apply_edits(&edits));
        assert_eq!(e.textarea.lines()[0], "const a = 1;");
        assert_eq!(e.textarea.cursor(), (1, 5), "the cursor stays put");
        assert!(e.dirty);
        // One keystroke puts the whole thing back — replacing the buffer
        // costs two of tui-textarea's undo steps, and the intermediate
        // one looks like an empty file.
        assert!(e.undo_format());
        assert_eq!(e.textarea.lines()[0], "const a=1;");
        assert_eq!(e.textarea.lines()[1], "const b=2;");
        assert!(!e.undo_format(), "only the once");
        // No edits, no change.
        assert!(!e.apply_edits(&[]));
    }

    #[test]
    fn the_word_under_the_cursor_is_what_gets_looked_up() {
        let mut e = ed("const handleClick = go(x);\n");
        e.textarea.move_cursor(CursorMove::Jump(0, 9));
        assert_eq!(e.word_at_cursor(), "handleClick");
        // Just past the end of a word still counts as being in it.
        e.textarea.move_cursor(CursorMove::Jump(0, 17));
        assert_eq!(e.word_at_cursor(), "handleClick");
        // In the whitespace between words, nothing.
        e.textarea.move_cursor(CursorMove::Jump(0, 18));
        assert_eq!(e.word_at_cursor(), "");
        // The prefix is only what precedes the cursor.
        e.textarea.move_cursor(CursorMove::Jump(0, 10));
        assert_eq!(e.word_prefix(), "hand");
    }

    #[test]
    fn completion_narrows_as_you_type_and_inserts_over_the_prefix() {
        let mut e = ed("const x = to\n");
        e.textarea.move_cursor(CursorMove::Jump(0, 12));
        assert!(e.open_completion(vec![
            item("toFixed", "toFixed"),
            item("toString", "toString"),
            item("valueOf", "valueOf"),
        ]));
        let c = e.completion.as_ref().unwrap();
        assert_eq!(c.len(), 2, "only the ones starting with `to`");
        assert_eq!(c.start, (0, 10));

        // Typing another character narrows further.
        e.textarea.insert_char('F');
        e.update_completion();
        assert_eq!(e.completion.as_ref().unwrap().len(), 1);

        // Accepting replaces the whole typed prefix, not just the tail.
        let label = e.accept_completion();
        assert_eq!(label.as_deref(), Some("toFixed"));
        assert_eq!(e.textarea.lines()[0], "const x = toFixed");
        assert!(e.completion.is_none());
    }

    #[test]
    fn completion_closes_when_the_cursor_leaves_the_word() {
        let mut e = ed("const x = to\n");
        e.textarea.move_cursor(CursorMove::Jump(0, 12));
        e.open_completion(vec![item("toFixed", "toFixed")]);
        // Backspacing past the start of the word closes it.
        e.textarea.move_cursor(CursorMove::Jump(0, 4));
        e.update_completion();
        assert!(e.completion.is_none());

        // So does typing something that matches nothing.
        e.textarea.move_cursor(CursorMove::Jump(0, 12));
        e.open_completion(vec![item("toFixed", "toFixed")]);
        e.textarea.insert_char('z');
        e.update_completion();
        assert!(e.completion.is_none());

        // An empty answer never opens a popup at all.
        assert!(!e.open_completion(Vec::new()));
    }

    #[test]
    fn selection_wraps_around_the_completion_list() {
        let mut e = ed("x\n");
        e.textarea.move_cursor(CursorMove::Jump(0, 1));
        e.open_completion(vec![item("xa", "xa"), item("xb", "xb"), item("xc", "xc")]);
        let c = e.completion.as_mut().unwrap();
        assert_eq!(c.sel, 0);
        c.move_sel(-1);
        assert_eq!(c.sel, 2, "up from the top goes to the bottom");
        c.move_sel(1);
        assert_eq!(c.sel, 0);
    }

    /// A double click takes the whole identifier, wherever in it the
    /// pointer landed, and leaves the cursor where the lookup keys still
    /// see the word.
    #[test]
    fn a_double_click_takes_the_whole_word() {
        let mut e = ed("let total = count_items(list);\n");
        // In the middle of `count_items`.
        let word = e.select_word_at(0, 16).expect("a word is there");
        assert_eq!(word, "count_items");
        assert_eq!(e.textarea.selection_range(), Some(((0, 12), (0, 23))));
        assert_eq!(
            e.word_at_cursor(),
            "count_items",
            "the cursor sits past the word, and that still counts as in it"
        );
        assert_eq!(e.selected_word().as_deref(), Some("count_items"));
    }

    /// Punctuation is not a word, and a click on it selects nothing
    /// rather than the thing beside it.
    #[test]
    fn a_double_click_on_punctuation_selects_nothing() {
        let mut e = ed("a + b\n");
        assert_eq!(e.select_word_at(0, 2), None);
    }

    /// A third click takes the line.
    #[test]
    fn a_triple_click_takes_the_line() {
        let mut e = ed("first line\nsecond line\n");
        e.select_line_at(1);
        assert_eq!(e.textarea.selection_range(), Some(((1, 0), (1, 11))));
        assert_eq!(
            e.selected_word(),
            None,
            "a whole line is not one identifier, so nothing else lights up"
        );
    }

    /// The other-uses highlight is whole-word: `set` must not light up
    /// the middle of `offset`.
    #[test]
    fn other_uses_are_whole_words_only() {
        assert_eq!(
            word_occurrences("set offset set_x set", "set"),
            vec![(0, 3), (17, 20)]
        );
        assert_eq!(word_occurrences("none here", "set"), vec![]);
    }

    /// `F8` walks the problems in order and wraps round the end.
    #[test]
    fn stepping_the_problems_wraps_at_the_ends() {
        let mut e = ed("one\ntwo\nthree\nfour\n");
        e.set_server_diagnostics(vec![diag(4, 2), diag(2, 1)]);
        e.jump_to_line(1);

        assert_eq!(e.step_problem(1).map(|d| d.line), Some(2));
        assert_eq!(e.step_problem(1).map(|d| d.line), Some(4));
        assert_eq!(
            e.step_problem(1).map(|d| d.line),
            Some(2),
            "past the last one comes back to the first"
        );
        assert_eq!(
            e.step_problem(-1).map(|d| d.line),
            Some(4),
            "and back past the first goes to the last"
        );
        assert_eq!(e.cursor_pos(), (4, 2), "the cursor lands on the problem");
    }

    /// A clean file has nothing to step to, and says so by answering
    /// `None` rather than by moving the cursor.
    #[test]
    fn a_clean_file_has_no_next_problem() {
        let mut e = ed("one\ntwo\n");
        assert!(e.step_problem(1).is_none());
    }

    fn diag(line: usize, col: usize) -> Diagnostic {
        Diagnostic {
            line,
            col,
            end_col: col + 1,
            severity: 1,
            message: format!("wrong on line {line}"),
            code: None,
            source: None,
        }
    }

    #[test]
    fn diagnostics_pick_the_worst_per_line() {
        let mut e = ed("a\nb\n");
        let d = |line, severity, message: &str| Diagnostic {
            line,
            col: 1,
            end_col: 2,
            severity,
            message: message.into(),
            code: None,
            source: None,
        };
        e.set_server_diagnostics(vec![
            d(1, 2, "just a warning"),
            d(1, 1, "a real error"),
            d(2, 3, "info"),
        ]);
        assert_eq!(e.diagnostic_on(1).unwrap().message, "a real error");
        assert!(e.diagnostic_on(1).unwrap().is_error());
        assert_eq!(e.diagnostic_on(3), None);
        // The cursor is on line 1: both of its problems, worst first.
        let here = e.diagnostics_here();
        assert_eq!(here.len(), 2);
        assert_eq!(here[0].message, "a real error");
    }
}

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
    /// What the language server says is wrong with the buffer, refreshed
    /// as its notifications arrive.
    pub diagnostics: Vec<Diagnostic>,
    /// The completion popup, when one is open.
    pub completion: Option<CompletionState>,
    /// Hash of the text last handed to the language server, so an idle
    /// tick can tell whether there is anything new to send.
    pub synced: u64,
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
        self.shown = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                needle.is_empty() || c.label.to_lowercase().starts_with(&needle)
            })
            .map(|(i, _)| i)
            .collect();
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

/// Characters that are part of an identifier being completed.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
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
            completion: None,
            synced: 0,
            pre_format: None,
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
        let Some(line) = self.textarea.lines().get(row) else {
            return String::new();
        };
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            return String::new();
        }
        // Sitting just past the end of a word counts as being in it.
        let at = col.min(chars.len().saturating_sub(1));
        let at = if !is_word(chars[at]) && at > 0 && is_word(chars[at - 1]) {
            at - 1
        } else {
            at
        };
        if !is_word(chars[at]) {
            return String::new();
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
        chars[start..end].iter().collect()
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
        let Some(state) = &self.completion else { return };
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
        let title = if self.read_only {
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
        let mut out: Vec<Line> = Vec::with_capacity(self.inner.height as usize);
        for vis in 0..self.inner.height as usize {
            let row = self.top.0 as usize + vis;
            if row >= n_lines {
                out.push(Line::default());
            } else {
                out.push(self.render_row(row, cur_row, cur_col));
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
        let Some(state) = &self.completion else { return };
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

    fn render_row(&self, row: usize, cur_row: usize, cur_col: usize) -> Line<'static> {
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
            Some(d) if d.is_error() => cells.push((
                '●',
                Style::default().fg(p.err).add_modifier(Modifier::BOLD),
            )),
            Some(_) => cells.push(('▲', Style::default().fg(p.stage_partial))),
            None => cells.push((' ', Style::default().fg(p.gutter))),
        }
        let mut disp = 0usize;
        // Columns the diagnostic actually covers, so a squiggle marks the
        // expression rather than the whole line.
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
        for (ci, ch) in text.chars().enumerate() {
            let fg = char_colors.get(ci).copied().unwrap_or(p.code);
            let mut st = Style::default().fg(fg);
            if bad.is_some_and(|(a, b)| ci >= a && ci < b.max(a + 1)) {
                st = st.fg(if worst.is_some_and(|d| d.is_error()) {
                    p.err
                } else {
                    p.stage_partial
                });
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
        };
        e.diagnostics = vec![d(1, 2, "just a warning"), d(1, 1, "a real error"), d(2, 3, "info")];
        assert_eq!(e.diagnostic_on(1).unwrap().message, "a real error");
        assert!(e.diagnostic_on(1).unwrap().is_error());
        assert_eq!(e.diagnostic_on(3), None);
        // The cursor is on line 1: both of its problems, worst first.
        let here = e.diagnostics_here();
        assert_eq!(here.len(), 2);
        assert_eq!(here[0].message, "a real error");
    }
}

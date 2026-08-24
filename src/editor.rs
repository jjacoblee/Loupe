//! Rich editor for the new side of a file, built on tui-textarea, with full
//! mouse support (click to place cursor, drag to select, wheel to scroll).
//!
//! tui-textarea does not expose its internal viewport scroll offset, but its
//! scroll logic is deterministic: the viewport only moves to keep the cursor
//! visible (we never call `TextArea::scroll`). We replicate that exact logic
//! in a shadow viewport so screen coordinates map precisely to buffer
//! positions.

use crate::highlight::EditorHighlight;
use crate::theme::palette;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
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
    /// Shadow of tui-textarea's internal viewport top (row, col).
    top: (u16, u16),
    /// Inner text area (inside the block borders) from the last render.
    inner: Rect,
    dragging: bool,
    /// Incremental syntax highlighting for the buffer.
    hl: EditorHighlight,
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
            top: (0, 0),
            inner: Rect::default(),
            dragging: false,
            hl,
        }
    }

    pub fn content(&self) -> String {
        self.textarea.lines().join("\n")
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
        let title = format!(
            " ✎ {}{} — Ctrl+S save · Esc close ",
            self.path,
            if self.dirty { " [+]" } else { "" }
        );
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
    }

    /// One visible row: gutter + syntax-colored text with selection, cursor
    /// line underline, and a reversed cursor cell, clipped to the viewport.
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
        let gutter = format!(
            "{:>w$} ",
            row + 1,
            w = (self.lnum_width() as usize).saturating_sub(1)
        );
        for ch in gutter.chars() {
            cells.push((ch, Style::default().fg(p.gutter)));
        }
        let mut disp = 0usize;
        for (ci, ch) in text.chars().enumerate() {
            let fg = char_colors.get(ci).copied().unwrap_or(p.code);
            let mut st = Style::default().fg(fg);
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
}

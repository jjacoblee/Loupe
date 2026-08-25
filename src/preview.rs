//! The markdown preview pane.
//!
//! It shares the pane the diff and the editor use, and it is the third
//! way to look at one file: the diff shows what changed, the editor shows
//! the text, and the preview shows the document. `P` moves between the
//! preview and the source, and the two keep their place — toggling to the
//! source lands the cursor on the line the reader was looking at, and
//! toggling back scrolls to the line they just edited.
//!
//! It exists because the files agents write — plan files, review
//! write-ups, design notes — are markdown, and reading them as raw text
//! next to the diff they describe meant leaving loupe for another app.

use crate::markdown::Doc;
use crate::theme::palette;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use std::path::PathBuf;
use std::time::SystemTime;

/// How many rendered rows a wheel notch moves.
pub const WHEEL_ROWS: i32 = 3;

pub struct Preview {
    /// Repo-relative path when the file is in the repository, otherwise
    /// the path as the reader gave it.
    pub path: String,
    pub abs_path: PathBuf,
    /// The markdown source, kept so a toggle to the editor does not have
    /// to read the file again.
    pub src: String,
    doc: Doc,
    /// First rendered row on screen.
    pub scroll: usize,
    /// The inner rectangle from the last draw, for hit-testing and paging.
    inner: Rect,
    /// The file is not part of the changeset — it came from the finder,
    /// the ☰ menu, or the command line. Closing returns to the diff.
    pub standalone: bool,
    /// True when the reader came here from the editor, so closing the
    /// preview should put the editor back rather than drop to the diff.
    pub from_editor: bool,
    /// What the file's modification time was when this text was read.
    /// The idle tick compares against it and reloads when an agent
    /// rewrites the file underneath.
    pub mtime: Option<SystemTime>,
    /// Set while the reader is looking at unsaved editor text, so the
    /// title says so and the reload check stays out of the way.
    pub from_buffer: bool,
    /// A source line to scroll to as soon as the pane knows how wide it
    /// is. Where the rendered rows fall depends on the width, so a jump
    /// asked for before the first draw has to wait for it.
    pending_line: Option<usize>,
}

impl Preview {
    pub fn new(path: &str, abs_path: PathBuf, src: &str) -> Self {
        Preview {
            path: path.to_string(),
            abs_path,
            doc: crate::markdown::parse(src),
            src: src.to_string(),
            scroll: 0,
            inner: Rect::default(),
            standalone: false,
            from_editor: false,
            mtime: None,
            from_buffer: false,
            pending_line: None,
        }
    }

    /// Replace the text and re-render, keeping the reader's place by
    /// source line rather than by row — a rewrite changes the row count,
    /// and staying on row 200 of a different document is not staying put.
    pub fn reload(&mut self, src: &str) {
        let anchor = self.doc.source_line(self.scroll);
        self.doc = crate::markdown::parse(src);
        self.src = src.to_string();
        self.doc.lay_out(self.body_width());
        self.scroll = self.doc.row_of_source(anchor);
        self.clamp();
    }

    /// Re-render with no content change — after a theme switch.
    pub fn restyle(&mut self) {
        self.doc.invalidate();
    }

    fn body_width(&self) -> usize {
        (self.inner.width as usize).max(8)
    }

    /// Rendered rows that fit on screen at once.
    pub fn page(&self) -> i32 {
        (self.inner.height as i32 - 1).max(1)
    }

    fn max_scroll(&self) -> usize {
        self.doc
            .len()
            .saturating_sub((self.inner.height as usize).max(1))
    }

    fn clamp(&mut self) {
        self.scroll = self.scroll.min(self.max_scroll());
    }

    pub fn scroll_rows(&mut self, delta: i32) {
        let next = self.scroll as i64 + delta as i64;
        self.scroll = next.clamp(0, self.max_scroll() as i64) as usize;
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = self.max_scroll();
    }

    /// Jump to the next or previous heading. False when there is none
    /// left in that direction.
    pub fn jump_heading(&mut self, forward: bool) -> bool {
        match self.doc.heading_near(self.scroll, forward) {
            Some(row) => {
                self.scroll = row.min(self.max_scroll());
                true
            }
            None => false,
        }
    }

    /// The source line at the top of the pane, 1-based — where the editor
    /// should put its cursor when the reader toggles to the source.
    pub fn source_line(&self) -> usize {
        self.doc.source_line(self.scroll)
    }

    /// Scroll so that source line `line` is at the top. Used coming back
    /// from the editor. The move happens at the next draw, when the pane
    /// width — and so the row the line lands on — is known.
    pub fn go_to_source(&mut self, line: usize) {
        self.pending_line = Some(line);
    }

    /// A click positions the reader without selecting: the pane is for
    /// reading, and the source view is one key away for anything else.
    pub fn on_click(&mut self, _x: u16, y: u16) {
        if y >= self.inner.y && y < self.inner.y + self.inner.height {
            // Clicking low in the pane scrolls that row toward the middle,
            // which is what "read on from here" means.
            let row = (y - self.inner.y) as usize;
            let half = (self.inner.height / 2) as usize;
            if row > half {
                self.scroll_rows((row - half) as i32);
            }
        }
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        let p = palette();
        let hint = if self.from_buffer {
            " [unsaved] · P source · Esc back "
        } else if self.standalone {
            " · P source · Esc back "
        } else {
            " · P source · e edit · Esc diff "
        };
        let title = format!(" 📖 {}{hint}", self.path);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.accent))
            .title(title);
        self.inner = block.inner(area);
        f.render_widget(block, area);

        self.doc.lay_out(self.body_width());
        if let Some(line) = self.pending_line.take() {
            self.scroll = self.doc.row_of_source(line);
        }
        self.clamp();

        let h = self.inner.height as usize;
        let mut out: Vec<Line> = Vec::with_capacity(h);
        let lines = self.doc.lines();
        for i in 0..h {
            match lines.get(self.scroll + i) {
                Some(l) => out.push(l.clone()),
                None => out.push(Line::default()),
            }
        }
        f.render_widget(Paragraph::new(out), self.inner);
        self.render_scrollbar(f, p);
    }

    /// A one-column bar on the right border. A long plan file gives no
    /// other clue how much of it is left.
    fn render_scrollbar(&self, f: &mut Frame, p: &crate::theme::Palette) {
        let total = self.doc.len();
        let h = self.inner.height as usize;
        if total <= h || h < 3 {
            return;
        }
        let x = self.inner.x + self.inner.width;
        let thumb_h = ((h * h) / total).max(1);
        let span = h.saturating_sub(thumb_h);
        let at = if self.max_scroll() == 0 {
            0
        } else {
            self.scroll * span / self.max_scroll()
        };
        for i in 0..thumb_h {
            let y = self.inner.y + (at + i) as u16;
            if y >= self.inner.y + self.inner.height {
                break;
            }
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "┃",
                    Style::default().fg(p.divider_active),
                ))),
                Rect {
                    x,
                    y,
                    width: 1,
                    height: 1,
                },
            );
        }
    }
}

/// The modification time of a file, or None when it cannot be read.
pub fn mtime_of(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

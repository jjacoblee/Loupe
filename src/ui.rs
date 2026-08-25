//! All rendering. Every clickable region drawn here is recorded into
//! `app.layout` so the mouse handlers can hit-test against it.

use crate::app::{
    App, ButtonId, Dragging, FileEntry, FinderMode, MenuRow, Overlay, Screen, ViewMode, FINDER_ROWS,
};
use crate::blame::{self, Heat};
use crate::diff::{DisplayEntry, Row, RowKind, Selection, Side, TAB_WIDTH};
use crate::gitops::StageState;
use crate::highlight::HlLine;
use crate::theme::palette;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use std::sync::Arc;
use unicode_width::UnicodeWidthChar;

/// Width of the Tree/Flat toggle drawn on the file panel's top border.
const TOGGLE_W: usize = 13;

/// Rows the ☰ menu keeps below the button before it gives up and fills
/// the screen from the top instead.
const MENU_MIN_H: u16 = 8;

pub fn draw(f: &mut Frame, app: &mut App) {
    app.layout = Default::default();
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    match app.screen {
        Screen::PrList => {
            draw_topbar_prlist(f, app, chunks[0]);
            draw_pr_list(f, app, chunks[1]);
        }
        Screen::Review => {
            draw_topbar_review(f, app, chunks[0]);
            draw_review(f, app, chunks[1]);
        }
    }
    draw_status(f, app, chunks[2]);

    match &app.overlay {
        Overlay::None => {}
        Overlay::CheckoutPrompt(n) => {
            let n = *n;
            draw_checkout_prompt(f, app, area, n);
        }
        Overlay::Comment(_) => draw_comment_overlay(f, app, area),
        Overlay::Revert(_) => draw_revert_prompt(f, app, area),
        Overlay::Help => draw_help(f, app, area),
        Overlay::ThemePicker(_) => draw_theme_picker(f, app, area),
        Overlay::Finder(_) => draw_finder(f, app, area),
        Overlay::Hover(_) => draw_hover(f, app, area),
        Overlay::PathMenu(_) => draw_path_menu(f, app, area),
        Overlay::BlameMenu(_) => draw_blame_menu(f, app, area),
        Overlay::Menu(_) => draw_menu(f, app, area),
    }
}

// ------------------------------------------------------------------ helpers

fn truncate_pad(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = if ch == '\t' {
            4
        } else {
            ch.width().unwrap_or(0)
        };
        if w + cw > width {
            break;
        }
        if ch == '\t' {
            out.push_str("    ");
        } else {
            out.push(ch);
        }
        w += cw;
    }
    while w < width {
        out.push(' ');
        w += 1;
    }
    out
}

fn tail_truncate(s: &str, width: usize) -> String {
    let len: usize = s.chars().map(|c| c.width().unwrap_or(0)).sum();
    if len <= width {
        return s.to_string();
    }
    let keep: String = s
        .chars()
        .rev()
        .scan(0usize, |acc, c| {
            *acc += c.width().unwrap_or(0);
            if *acc + 1 > width {
                None
            } else {
                Some(c)
            }
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{keep}")
}

fn disp_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Render a row of clickable "buttons", right-aligned in `area`,
/// recording each button's rect.
/// Draw a right-aligned row of buttons, keeping `reserve` columns free on
/// the left for whatever is already written there (a PR title, a branch
/// name). Without that reservation a narrow terminal paints the buttons
/// straight over it and the row stops being readable at all.
fn buttons_right(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    reserve: u16,
    buttons: &[(&str, ButtonId, bool)],
) {
    let width = |l: &str| {
        l.chars()
            .map(|c| c.width().unwrap_or(0) as u16)
            .sum::<u16>()
    };
    let budget = area.width.saturating_sub(reserve);
    // The rightmost buttons are the ones people reach for (Help, Back),
    // so when they don't all fit, drop from the left.
    let mut first = 0;
    let mut total: u16 = buttons.iter().map(|(l, _, _)| width(l) + 3).sum();
    while total > budget && first + 1 < buttons.len() {
        total -= width(buttons[first].0) + 3;
        first += 1;
    }
    let buttons = &buttons[first..];
    let p = palette();
    let mut x = area.x + area.width.saturating_sub(total);
    // Wipe the strip first: the buttons are separated by a blank column
    // each, and without this the text underneath (a PR title, a branch
    // name) shows through those gaps one letter at a time.
    f.render_widget(
        Paragraph::new(" ".repeat(total as usize)),
        Rect {
            x,
            y: area.y,
            width: area.x + area.width - x,
            height: 1,
        },
    );
    for (label, id, active) in buttons {
        let w: u16 = label
            .chars()
            .map(|c| c.width().unwrap_or(0) as u16)
            .sum::<u16>()
            + 2;
        let rect = Rect {
            x,
            y: area.y,
            width: w,
            height: 1,
        };
        let style = if *active {
            Style::default()
                .bg(p.btn_active_bg)
                .fg(p.btn_active_fg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(p.btn_bg).fg(p.btn_fg)
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {label} "), style))),
            rect,
        );
        app.layout.buttons.push((rect, *id));
        x += w + 1;
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

// ------------------------------------------------------------------ top bars

fn draw_topbar_prlist(f: &mut Frame, app: &mut App, area: Rect) {
    let p = palette();
    let repo = app.repo.clone().unwrap_or_else(|| "…".into());
    // " 🔍 loupe " plus a space, then the repo name — the part of this
    // row the buttons must not paint over.
    let reserve = 12 + disp_width(&repo) as u16;
    let title = Line::from(vec![
        Span::styled(
            " 🔍 loupe ",
            Style::default()
                .bg(p.badge_pr)
                .fg(p.badge_fg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            repo,
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  — open pull requests", Style::default().fg(p.dim)),
    ]);
    f.render_widget(Paragraph::new(title), area);
    buttons_right(
        f,
        app,
        area,
        reserve,
        &[
            ("⎇ Local changes", ButtonId::LocalChanges, false),
            ("⟳", ButtonId::Refresh, false),
            ("☰", ButtonId::Menu, menu_open(app)),
        ],
    );
}

/// True while the ☰ menu is open, so its button draws as pressed.
/// True when the editor holds a markdown file, so the toolbar can offer
/// the way back to the preview.
fn markdown_buffer(app: &App) -> bool {
    app.editor
        .as_ref()
        .is_some_and(|e| crate::markdown::is_markdown(&e.path))
}

fn menu_open(app: &App) -> bool {
    matches!(app.overlay, Overlay::Menu(_))
}

fn draw_topbar_review(f: &mut Frame, app: &mut App, area: Rect) {
    // Badge + title: "PR #N — <title>" for a PR review, "LOCAL — <branch>,
    // uncommitted changes" when reviewing the working tree.
    let p = palette();
    // `loupe md <path>` has no review to name, so the bar names the
    // document instead and carries only the keys that still do anything.
    if app.preview_only {
        let name = app
            .preview
            .as_ref()
            .map(|pv| pv.path.clone())
            .unwrap_or_default();
        let shown = tail_truncate(&name, area.width.saturating_sub(28) as usize);
        let left = Line::from(vec![
            Span::styled(
                " 📖 MARKDOWN ".to_string(),
                Style::default()
                    .bg(p.badge_pr)
                    .fg(p.badge_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                shown,
                Style::default().fg(p.text).add_modifier(Modifier::BOLD),
            ),
        ]);
        f.render_widget(Paragraph::new(left), area);
        buttons_right(
            f,
            app,
            area,
            18,
            &[
                ("✎ Source", ButtonId::PreviewToggle, false),
                ("⟳", ButtonId::Refresh, false),
                ("☰", ButtonId::Menu, menu_open(app)),
            ],
        );
        return;
    }
    let (badge, badge_bg, title, note) = if app.local {
        let branch = app
            .local_branch
            .clone()
            .unwrap_or_else(|| "detached HEAD".into());
        (
            " ⎇ LOCAL ".to_string(),
            p.badge_local,
            branch,
            "  — uncommitted changes vs HEAD",
        )
    } else {
        let (num, title) = app
            .pr
            .as_ref()
            .map(|p| (p.number, p.title.clone()))
            .unwrap_or((0, String::new()));
        (format!(" PR #{num} "), p.badge_pr, title, "")
    };
    // The badge and the PR title (or branch) are what the buttons have to
    // leave room for; the trailing note is expendable.
    let shown_title = tail_truncate(&title, (area.width / 3) as usize);
    let badge_w = disp_width(&badge) as u16;
    let reserve = badge_w + 1 + disp_width(&shown_title) as u16 + 1;
    // The badge is a click target: right-click copies the PR link.
    app.layout.badge = Rect {
        x: area.x,
        y: area.y,
        width: badge_w.min(area.width),
        height: 1,
    };
    let left = Line::from(vec![
        Span::styled(
            badge,
            Style::default()
                .bg(badge_bg)
                .fg(p.badge_fg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            shown_title,
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(note, Style::default().fg(p.dim)),
    ]);
    f.render_widget(Paragraph::new(left), area);

    // The toolbar carries only what fits what you are doing right now;
    // ☰ holds the rest. Eleven buttons on one row left no space for the
    // PR title, and most of them were one keystroke away anyway.
    if app.previewing() {
        buttons_right(
            f,
            app,
            area,
            reserve,
            &[
                ("✎ Source", ButtonId::PreviewToggle, false),
                ("⟳", ButtonId::Refresh, false),
                ("✕ Close", ButtonId::PreviewClose, false),
                ("☰", ButtonId::Menu, menu_open(app)),
            ],
        );
    } else if app.editor.is_some() {
        let md = markdown_buffer(app);
        let mut buttons: Vec<(&str, ButtonId, bool)> = vec![
            ("⇥ Format", ButtonId::EditorFormat, false),
            ("💾 Save", ButtonId::EditorSave, true),
        ];
        if md {
            buttons.push(("📖 Preview", ButtonId::PreviewToggle, false));
        }
        buttons.push(("✕ Close", ButtonId::EditorClose, false));
        buttons.push(("☰", ButtonId::Menu, menu_open(app)));
        buttons_right(f, app, area, reserve, &buttons);
    } else {
        let mut buttons: Vec<(&str, ButtonId, bool)> = Vec::new();
        if app.selection.is_some() {
            // Lines are selected: the only two things anyone does next.
            // Mouse reporting eats the terminal's own selection, so Copy
            // has to be a button and not only the `y` key.
            if !app.local {
                buttons.push(("💬 Comment", ButtonId::Comment, true));
            }
            buttons.push(("⧉ Copy", ButtonId::Copy, true));
        } else {
            buttons.push(("🔍 Find", ButtonId::Find, false));
            // The 📖 button only appears for a file it can render, so the
            // toolbar never offers something that answers with an error.
            if app.can_preview() {
                buttons.push(("📖 Preview", ButtonId::PreviewToggle, false));
            }
            buttons.push(("✎ Edit", ButtonId::Edit, false));
        }
        buttons.push(("⟳", ButtonId::Refresh, app.refreshing()));
        buttons.push(("☰", ButtonId::Menu, menu_open(app)));
        buttons_right(f, app, area, reserve, &buttons);
    }
}

// ------------------------------------------------------------------ PR list

fn draw_pr_list(f: &mut Frame, app: &mut App, area: Rect) {
    let p = palette();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Click a pull request to review it ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.layout.pr_list = inner;

    if app.prs.is_empty() {
        let text = if app.busy() {
            "Loading…"
        } else {
            "No open pull requests — press r to refresh"
        };
        f.render_widget(
            Paragraph::new(text).style(Style::default().fg(p.dim)),
            inner,
        );
        return;
    }

    let h = inner.height as usize;
    let max = app.prs.len().saturating_sub(h);
    if app.pr_scroll > max {
        app.pr_scroll = max;
    }
    let mut lines = Vec::new();
    for (i, pr) in app.prs.iter().enumerate().skip(app.pr_scroll).take(h) {
        let selected = i == app.pr_cursor;
        let draft = if pr.is_draft { " [draft]" } else { "" };
        let text = format!(
            " #{:<5} {}  ",
            pr.number,
            tail_truncate(&pr.title, inner.width.saturating_sub(40) as usize),
        );
        let meta = format!(
            "@{}  {}  +{} −{}{draft}",
            pr.author.login, pr.head_ref_name, pr.additions, pr.deletions
        );
        let base = if selected {
            Style::default().bg(p.row).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(text, base.fg(p.text)),
            Span::styled(meta, base.fg(p.dim)),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

// ------------------------------------------------------------------ review

fn draw_review(f: &mut Frame, app: &mut App, area: Rect) {
    app.layout.review = area;
    // `loupe md <path>`: one document and nothing else, so it gets the
    // whole window rather than a file panel with nothing in it.
    if app.preview_only {
        if let Some(pv) = &mut app.preview {
            pv.render(f, area);
        }
        return;
    }
    // Both panel widths are user-set; re-clamp here so a terminal resize
    // can never leave the diff pane starved. `blame_gutter` is zero when
    // the pane is off, and also when three panes will not fit — a narrow
    // terminal shows two rather than a diff too thin to read.
    // The blame pane cannot line up beside a rendered document — one
    // source line is any number of rows there, or none — so it stands
    // down while the preview is open and comes back with the source.
    let bw = if app.previewing() {
        0
    } else {
        app.blame_gutter()
    };
    let fw = app.clamp_panel_w(app.file_panel_w);
    app.file_panel_w = fw;
    if bw > 0 {
        app.blame_w = bw;
    }
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(fw),
            Constraint::Length(bw),
            Constraint::Min(crate::app::DIFF_MIN_W),
        ])
        .split(area);
    // The two adjacent border columns are the resize handle — a generous
    // target, and the pair already reads as a seam.
    app.layout.divider = Rect {
        x: area.x + fw.saturating_sub(1),
        y: area.y,
        width: 2.min(area.width.saturating_sub(fw.saturating_sub(1))),
        height: area.height,
    };
    draw_file_list(f, app, cols[0]);

    if bw > 0 {
        let seam = fw + bw;
        app.layout.blame_divider = Rect {
            x: area.x + seam.saturating_sub(1),
            y: area.y,
            width: 2.min(area.width.saturating_sub(seam.saturating_sub(1))),
            height: area.height,
        };
        let h = cols[1].height.saturating_sub(2) as usize;
        // The editor keeps its own pane, aligned to its scroll: a buffer
        // has no folds, so one file line is one row there.
        let (rows, cursor, note) = match &app.editor {
            Some(ed) => {
                let top = ed.scroll_top();
                let n_lines = ed.textarea.lines().len();
                let cursor = ed.textarea.cursor().0.checked_sub(top).filter(|n| *n < h);
                // Typing moves the text out from under the blame, and
                // there is no honest way to re-anchor it without a fresh
                // read — so say so instead of drawing a lie in full color.
                let note = if ed.dirty {
                    " · stale"
                } else if app.blame_loading() {
                    " · loading…"
                } else {
                    ""
                };
                (blame_rows_editor(app, h, top, n_lines), cursor, note)
            }
            None => {
                let cursor = app
                    .diff_cursor
                    .checked_sub(app.diff_scroll)
                    .filter(|n| *n < h);
                let note = if app.blame_loading() {
                    " · loading…"
                } else {
                    ""
                };
                (blame_rows_diff(app, h), cursor, note)
            }
        };
        draw_blame(f, app, cols[1], rows, cursor, note);
    }

    if let Some(pv) = &mut app.preview {
        pv.render(f, cols[2]);
    } else if let Some(editor) = &mut app.editor {
        editor.render(f, cols[2], true);
    } else {
        draw_diff(f, app, cols[2]);
    }
    draw_divider_grip(f, app.layout.divider, app.dragging() == Dragging::FilePanel);
    draw_divider_grip(
        f,
        app.layout.blame_divider,
        app.dragging() == Dragging::BlamePane,
    );
}

/// A grip on the divider so it reads as draggable: a few heavy border
/// cells at mid-height, and the whole seam accented while it is being
/// dragged.
fn draw_divider_grip(f: &mut Frame, d: Rect, active: bool) {
    if d.width == 0 || d.height < 3 {
        return;
    }
    let p = palette();
    let style = Style::default().fg(if active { p.divider_active } else { p.divider });
    let (top, height) = if active {
        (d.y, d.height)
    } else {
        (d.y + d.height / 2 - 1, 3)
    };
    for i in 0..height {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled("┃", style))),
            Rect {
                x: d.x,
                y: top + i,
                width: 1,
                height: 1,
            },
        );
    }
}

fn draw_file_list(f: &mut Frame, app: &mut App, area: Rect) {
    let p = palette();
    // Local review counts staged files; PR review counts viewed ones.
    let viewed_n = if app.local {
        app.staged_count()
    } else {
        app.files
            .iter()
            .filter(|fl| app.viewed.contains(&fl.path))
            .count()
    };
    // The Tree/Flat toggle is drawn on the same border row as the title:
    // on a narrowed panel, shorten the title rather than let them collide.
    let n = app.files.len();
    let full = if app.local {
        format!(" Files {viewed_n}/{n} staged ")
    } else {
        format!(" Files {viewed_n}/{n} ✓ ")
    };
    let title = if area.width as usize >= disp_width(&full) + TOGGLE_W + 2 {
        full
    } else {
        format!(" {viewed_n}/{n} ")
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.layout.file_list = inner;

    // Tree/Flat toggle, drawn on the top border.
    if app.editor.is_none() {
        let toggle_area = Rect {
            x: area.x + 1,
            y: area.y,
            width: area.width.saturating_sub(2),
            height: 1,
        };
        buttons_right(
            f,
            app,
            toggle_area,
            0,
            &[
                ("Tree", ButtonId::ViewTree, app.tree_view),
                ("Flat", ButtonId::ViewFlat, !app.tree_view),
            ],
        );
    }

    let h = inner.height as usize;
    let max = app.entries.len().saturating_sub(h);
    if app.file_scroll > max {
        app.file_scroll = max;
    }

    let mut lines = Vec::new();
    for entry in app.entries.iter().skip(app.file_scroll).take(h) {
        match entry {
            FileEntry::Dir { label, path, depth } => {
                let arrow = if app.collapsed_dirs.contains(path) {
                    "▸"
                } else {
                    "▾"
                };
                let text = format!("{}{arrow} {label}", " ".repeat(*depth as usize));
                lines.push(Line::from(Span::styled(
                    truncate_pad(&text, inner.width as usize),
                    Style::default().fg(p.dir),
                )));
            }
            FileEntry::File { idx, depth } => {
                let file = &app.files[*idx];
                let selected = *idx == app.file_cursor;
                let staged = app.stage_state(&file.path);
                // Local review has no PR to mark files viewed on; the same
                // column stages them instead.
                let done = if app.local {
                    staged == StageState::Staged
                } else {
                    app.viewed.contains(&file.path)
                };
                let sc = file.status_char();
                let sc_color = match sc {
                    'A' => p.st_added,
                    'D' => p.st_removed,
                    'R' | 'C' => p.st_renamed,
                    _ => p.st_other,
                };
                let (cb, cb_style) = if app.local {
                    match staged {
                        StageState::Staged => (
                            "[✓]",
                            Style::default().fg(p.st_added).add_modifier(Modifier::BOLD),
                        ),
                        // Staged, then edited again (or `git add -p`).
                        StageState::Partial => (
                            "[±]",
                            Style::default()
                                .fg(p.stage_partial)
                                .add_modifier(Modifier::BOLD),
                        ),
                        StageState::Unstaged => ("[+]", Style::default().fg(p.stage_add)),
                    }
                } else if done {
                    (
                        "[✓]",
                        Style::default().fg(p.st_added).add_modifier(Modifier::BOLD),
                    )
                } else {
                    ("[ ]", Style::default().fg(p.checkbox))
                };
                let name = if app.tree_view {
                    file.path.rsplit('/').next().unwrap_or(&file.path)
                } else {
                    file.path.as_str()
                };
                let counts = format!(" +{} −{}", file.additions, file.deletions);
                let indent = *depth as usize;
                // ↺ at the end of the row throws the whole file's changes
                // away (after asking). Only when there is a working tree to
                // put back — a read-only PR keeps the columns for the name.
                let revert_w = if app.can_revert() {
                    crate::app::REVERT_W as usize
                } else {
                    0
                };
                let name_w = (inner.width as usize)
                    .saturating_sub(indent + 6 + counts.chars().count() + revert_w);
                let name_t = tail_truncate(name, name_w);
                let pad = name_w.saturating_sub(disp_width(&name_t));
                let base = if selected {
                    Style::default().bg(p.row).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let name_fg = if done { p.viewed } else { p.text };
                let mut spans = vec![
                    Span::styled(" ".repeat(indent), base),
                    Span::styled(format!("{cb} "), base.patch(cb_style)),
                    Span::styled(format!("{sc} "), base.fg(sc_color)),
                    Span::styled(format!("{name_t}{}", " ".repeat(pad)), base.fg(name_fg)),
                    Span::styled(counts, base.fg(p.dim)),
                ];
                if revert_w > 0 {
                    spans.push(Span::styled(" ↺", base.fg(p.accent)));
                }
                lines.push(Line::from(spans));
            }
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}

// ------------------------------------------------------------------ diff

fn diff_bg(kind: RowKind, side: Side) -> Option<Color> {
    let p = palette();
    match (kind, side) {
        (RowKind::Added, Side::Right) => Some(p.added),
        (RowKind::Removed, Side::Left) => Some(p.removed),
        (RowKind::Modified, Side::Left) => Some(p.removed),
        (RowKind::Modified, Side::Right) => Some(p.added),
        _ => None,
    }
}

/// Body text as syntax-colored spans: highlight segments when available,
/// plain text otherwise. `skip` drops that many display columns off the
/// left (horizontal scroll); the result is clipped to `width` and padded.
/// `base` carries the row background/modifiers; segment foregrounds come
/// from the highlighter.
/// Body text as syntax-colored spans.
///
/// Two overlays ride on top of the syntax colors, both as char ranges
/// within the *line* (not the segment): `sel` is the selection, `marks`
/// are search hits. A search hit wins where they overlap — it is the
/// thing being hunted for, and it is transient.
#[allow(clippy::too_many_arguments)]
fn hl_body<'a>(
    text: &str,
    hl: Option<&HlLine>,
    skip: usize,
    width: usize,
    base: Style,
    fallback_fg: Color,
    marks: &[(usize, usize)],
    sel: Option<(usize, usize)>,
) -> Vec<Span<'a>> {
    let p = palette();
    let mut spans: Vec<Span> = Vec::new();
    // `col` counts columns of the *whole* line (including scrolled-off
    // ones); `w` counts columns actually emitted; `ci` counts chars, which
    // is what the overlays are measured in.
    let mut col = 0usize;
    let mut w = 0usize;
    let mut ci = 0usize;
    let mut push_seg = |seg: &str, fg: Color, spans: &mut Vec<Span<'a>>| {
        if w >= width {
            // Still count the chars: a later segment's overlays depend on it.
            ci += seg.chars().count();
            return;
        }
        // Runs of one style are coalesced into a single span — no
        // per-character Span allocation in the render hot path.
        let mut out = String::new();
        // 0 = plain, 1 = selected, 2 = search hit.
        let mut out_kind = 0u8;
        macro_rules! flush {
            () => {
                if !out.is_empty() {
                    let mut st = base.fg(fg);
                    match out_kind {
                        1 => st = st.bg(p.selected).add_modifier(Modifier::BOLD),
                        2 => st = st.bg(p.matched),
                        _ => {}
                    }
                    spans.push(Span::styled(std::mem::take(&mut out), st));
                }
            };
        }
        for ch in seg.chars() {
            let idx = ci;
            ci += 1;
            let cw = if ch == '\t' {
                TAB_WIDTH
            } else {
                ch.width().unwrap_or(0)
            };
            if cw == 0 {
                continue;
            }
            let start = col;
            col += cw;
            if start + cw <= skip {
                continue; // entirely left of the window
            }
            let kind = if marks.iter().any(|(s, e)| idx >= *s && idx < *e) {
                2
            } else if sel.is_some_and(|(s, e)| idx >= s && idx < e) {
                1
            } else {
                0
            };
            if kind != out_kind {
                flush!();
                out_kind = kind;
            }
            if start < skip {
                // A tab or wide char straddling the left edge: show the
                // part that survives as blanks, so columns stay aligned.
                let vis = (start + cw - skip).min(width - w);
                out.push_str(&" ".repeat(vis));
                w += vis;
                continue;
            }
            if w + cw > width {
                break;
            }
            if ch == '\t' {
                out.push_str(&" ".repeat(TAB_WIDTH));
            } else {
                out.push(ch);
            }
            w += cw;
        }
        flush!();
    };
    match hl {
        Some(segs) if !segs.is_empty() => {
            for (color, t) in segs {
                push_seg(t, *color, &mut spans);
            }
        }
        _ => push_seg(text, fallback_fg, &mut spans),
    }
    if w < width {
        spans.push(Span::styled(" ".repeat(width - w), base));
    }
    spans
}

#[allow(clippy::too_many_arguments)]
fn cell<'a>(
    row: &Row,
    side: Side,
    skip: usize,
    width: usize,
    sel: Option<Selection>,
    cursor: bool,
    hl: Option<&HlLine>,
    query: &str,
) -> Vec<Span<'a>> {
    let p = palette();
    let (ln, text) = match side {
        Side::Left => (row.old_ln, row.old_text.as_deref()),
        Side::Right => (row.new_ln, row.new_text.as_deref()),
    };
    let ln_str = match ln {
        Some(n) => format!("{n:>4} "),
        None => "     ".into(),
    };
    let body_w = width.saturating_sub(5);
    match text {
        None => {
            // This side has no line here (pure add/remove): hatched filler.
            let filler = truncate_pad("", width);
            let mut style = Style::default().bg(p.empty);
            if cursor {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            vec![Span::styled(filler, style)]
        }
        Some(t) => {
            let len = t.chars().count();
            let cols = ln.and_then(|n| sel.and_then(|s| s.cols_on(side, n, len)));
            // A whole-line selection keeps painting the whole row, padding
            // included — that is what a selected *line* has always looked
            // like. A character selection paints only its characters.
            let linewise = cols.is_some() && sel.is_some_and(|s| s.linewise);
            let bg = if linewise {
                Some(p.selected)
            } else {
                diff_bg(row.kind, side).or(if cursor { Some(p.cursor) } else { None })
            };
            let mut ln_style = Style::default().fg(p.line_no);
            let mut base = Style::default();
            if let Some(bg) = bg {
                ln_style = ln_style.bg(bg);
                base = base.bg(bg);
            }
            if linewise {
                base = base.add_modifier(Modifier::BOLD);
            }
            if cursor {
                ln_style = ln_style.add_modifier(Modifier::UNDERLINED);
                base = base.add_modifier(Modifier::UNDERLINED);
            }
            let marks = crate::search::find_ranges(t, query);
            let mut spans = vec![Span::styled(ln_str, ln_style)];
            spans.extend(hl_body(
                t,
                hl,
                skip,
                body_w,
                base,
                p.text,
                &marks,
                cols.filter(|_| !linewise),
            ));
            spans
        }
    }
}

fn banner_line<'a>(width: usize, label: String, fg: Color, cursor: bool) -> Line<'a> {
    let p = palette();
    let lw = disp_width(&label);
    let left = width.saturating_sub(lw) / 2;
    let right = width.saturating_sub(lw + left);
    let mut style = Style::default().bg(p.empty).fg(fg);
    if cursor {
        style = style.add_modifier(Modifier::UNDERLINED).fg(p.text);
    }
    Line::from(vec![
        Span::styled(" ".repeat(left), style),
        Span::styled(label, style),
        Span::styled(" ".repeat(right), style),
    ])
}

fn fold_line<'a>(width: usize, count: usize, cursor: bool) -> Line<'a> {
    banner_line(
        width,
        format!("···  {count} unchanged lines — click to expand  ···"),
        palette().fold,
        cursor,
    )
}

/// Header above a run the user expanded: click it to fold the run back.
fn unfold_line<'a>(width: usize, count: usize, cursor: bool) -> Line<'a> {
    banner_line(
        width,
        format!("⌃⌃⌃  {count} unchanged lines — click to fold  ⌃⌃⌃"),
        palette().fold,
        cursor,
    )
}

// ------------------------------------------------------------------ blame

/// The bar character for each step of the heat ramp. Color carries the
/// age, but a shape ramp behind it keeps the pane readable on a terminal
/// with a poor palette — and at a glance, without reading the color.
fn heat_bar(h: Heat) -> char {
    match h {
        Heat::Uncommitted | Heat::InChange | Heat::Age(0) => '█',
        Heat::Age(1) | Heat::Age(2) => '▓',
        Heat::Age(3) => '▒',
        _ => '░',
    }
}

/// Columns the pull request number gets: `#` and six digits. A number
/// wider than that is rare enough to be worth an ellipsis, and a
/// *truncated* number would send the reader to a real but wrong pull
/// request — the one failure this column must never have.
const PR_W: usize = 7;

fn pr_label(number: u64) -> String {
    let text = format!("#{number}");
    if text.chars().count() > PR_W {
        return "#…".to_string();
    }
    text
}

fn heat_color(p: &crate::theme::Palette, h: Heat) -> Color {
    match h {
        Heat::Uncommitted => p.blame_uncommitted,
        Heat::InChange => p.blame_change,
        Heat::Age(i) => p.blame_heat[i.min(p.blame_heat.len() - 1)],
    }
}

/// One row of the blame pane, already resolved against whatever is in the
/// pane beside it.
enum BlameRow {
    /// A fold banner: it covers many lines from many commits, so a rule
    /// says "nothing is claimed here" rather than blaming one of them.
    Rule,
    /// Past the end of the file, or a line nothing is blamed on.
    Blank,
    Commit(Arc<blame::Commit>),
}

/// The pane's rows for the diff: the same window `draw_diff` walks, so
/// the two cannot drift.
fn blame_rows_diff(app: &App, h: usize) -> Vec<BlameRow> {
    (0..h)
        .map(|n| {
            let row = app.diff_scroll + n;
            match app.display.get(row) {
                None => BlameRow::Blank,
                Some(DisplayEntry::Line(_)) => match app.blame_for_row(row) {
                    Some(c) => BlameRow::Commit(c),
                    None => BlameRow::Blank,
                },
                Some(_) => BlameRow::Rule,
            }
        })
        .collect()
}

/// The pane's rows for the editor. A buffer has no folds, so one file
/// line is one row and the only thing to follow is the scroll top.
fn blame_rows_editor(app: &App, h: usize, top: usize, n_lines: usize) -> Vec<BlameRow> {
    (0..h)
        .map(|n| {
            let line = top + n;
            if line >= n_lines {
                return BlameRow::Blank;
            }
            match app.blame_new.as_ref().and_then(|b| b.at(line + 1)) {
                Some(c) => BlameRow::Commit(c.clone()),
                None => BlameRow::Blank,
            }
        })
        .collect()
}

/// Draw the blame pane over `area`.
///
/// A run of rows sharing one commit prints its author once, at the top of
/// the run — repeating a name down twenty rows says nothing and hides
/// where one commit ends and the next begins. The heat bar keeps drawing
/// on every row, so the ramp stays continuous.
///
/// `cursor` is the row to mark, as an index into the visible window.
/// `note` is appended to the title: what the pane is waiting for, or why
/// what it shows may no longer line up.
fn draw_blame(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    rows: Vec<BlameRow>,
    cursor: Option<usize>,
    note: &str,
) {
    let p = palette();
    let now = blame::now();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Blame{note} "));
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.layout.blame = inner;
    let w = inner.width as usize;
    if w == 0 || inner.height == 0 {
        return;
    }

    // What fits: the bar and a name always; the age next; the pull
    // request number last, because the popup carries that one too.
    let show_pr = w >= 22;
    let show_age = w >= 14;
    let fixed = 2 + if show_age { 5 } else { 0 } + if show_pr { PR_W + 1 } else { 0 };
    // Cap the name: a pane dragged wide should not float the age and the
    // pull request number away from the name they belong to.
    let name_w = w.saturating_sub(fixed).clamp(3, 22);
    // A run of the same commit is drawn once. Dimmed while the pane is
    // stale, so it never looks more certain than it is.
    let dim = !note.is_empty();

    let mut lines: Vec<Line> = Vec::with_capacity(inner.height as usize);
    let mut prev_sha: Option<String> = None;
    for (n, row) in rows.into_iter().enumerate() {
        let commit = match row {
            BlameRow::Blank => {
                prev_sha = None;
                lines.push(Line::default());
                continue;
            }
            BlameRow::Rule => {
                prev_sha = None;
                lines.push(Line::from(Span::styled(
                    "─".repeat(w),
                    Style::default().fg(p.fold),
                )));
                continue;
            }
            BlameRow::Commit(c) => c,
        };
        let in_change = app.blame_change_set.contains(&commit.sha);
        let h = blame::heat(&commit, now, in_change);
        let mut base = Style::default();
        if cursor == Some(n) {
            base = base.bg(p.cursor).add_modifier(Modifier::UNDERLINED);
        }
        let mut spans = vec![
            Span::styled(
                heat_bar(h).to_string(),
                base.fg(if dim { p.faint } else { heat_color(p, h) })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", base),
        ];
        let repeat = n > 0 && prev_sha.as_deref() == Some(commit.sha.as_str());
        prev_sha = Some(commit.sha.clone());
        if repeat {
            spans.push(Span::styled(" ".repeat(w.saturating_sub(2)), base));
            lines.push(Line::from(spans));
            continue;
        }

        let name = if commit.uncommitted() {
            "uncommitted".to_string()
        } else {
            commit.author.clone()
        };
        let name_style = if dim {
            base.fg(p.faint)
        } else if commit.uncommitted() {
            base.fg(p.blame_uncommitted)
        } else if app.blame_is_mine(&commit) {
            base.fg(p.blame_mine).add_modifier(Modifier::BOLD)
        } else {
            base.fg(p.text)
        };
        spans.push(Span::styled(truncate_pad(&name, name_w), name_style));
        if show_age {
            let age = if commit.uncommitted() {
                "now".to_string()
            } else {
                blame::ago(commit.author_time, now)
            };
            spans.push(Span::styled(format!(" {age:>4}"), base.fg(p.dim)));
        }
        if show_pr {
            let (text, style) = match app.blame_prs.get(&commit.sha) {
                Some(pr) => (pr_label(pr.number), base.fg(p.key)),
                None => (String::new(), base.fg(p.faint)),
            };
            spans.push(Span::styled(
                format!(" {}", truncate_pad(&text, PR_W)),
                style,
            ));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_diff(f: &mut Frame, app: &mut App, area: Rect) {
    let p = palette();
    let file = app.files.get(app.file_cursor);
    // Sideways offset is easy to lose track of — say so in the title.
    let hoff = if app.diff_hscroll > 0 {
        format!(" · ⇥ col {}", app.diff_hscroll + 1)
    } else {
        String::new()
    };
    let title = match (file, &app.diff) {
        (Some(fl), Some(d)) => format!(
            " {} — +{} −{}{}{hoff} ",
            fl.path,
            d.additions,
            d.deletions,
            if app.checked_out { "" } else { " · read-only" }
        ),
        _ => " Diff ".to_string(),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.layout.diff = inner;

    let Some(diff) = &app.diff else {
        f.render_widget(
            Paragraph::new("Select a file on the left.").style(Style::default().fg(p.dim)),
            inner,
        );
        return;
    };

    let h = inner.height as usize;
    let sbs = app.view == ViewMode::SideBySide;
    let max = app.display.len().saturating_sub(h);
    if app.diff_scroll > max {
        app.diff_scroll = max;
    }
    // A terminal resize can widen the pane past the current offset.
    let hmax = app.max_hscroll();
    if app.diff_hscroll > hmax {
        app.diff_hscroll = hmax;
    }
    let hskip = app.diff_hscroll;

    // The active `/` search, highlighted wherever it appears on screen.
    let find_query = app.find.query.clone();
    let find: &str = &find_query;

    let sel = app.selection;

    // The change bar: two columns down the left edge carrying a ↺ on the
    // first row of every changed section. Computed for the visible window
    // up front, because each row has to know whether the section above it
    // is the same one.
    let bar_w = app.revert_gutter() as usize;
    let bars: Vec<Option<bool>> = (app.diff_scroll..app.diff_scroll + h)
        .map(|i| app.change_bar(i))
        .collect();
    let pane_w = (inner.width as usize).saturating_sub(bar_w);

    let mut lines: Vec<Line> = Vec::with_capacity(h);
    for (n, entry) in app.display.iter().skip(app.diff_scroll).take(h).enumerate() {
        // The keyboard cursor row, underlined the way the editor marks its
        // own cursor line.
        let cur = app.diff_scroll + n == app.diff_cursor;
        let mut spans: Vec<Span> = Vec::new();
        if bar_w > 0 {
            spans.push(match bars.get(n).copied().flatten() {
                Some(true) => Span::styled(
                    "↺ ",
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ),
                Some(false) => Span::styled("┃ ", Style::default().fg(p.divider)),
                None => Span::raw("  "),
            });
        }
        match entry {
            DisplayEntry::Fold { count, .. } => {
                spans.extend(fold_line(pane_w, *count, cur).spans);
            }
            DisplayEntry::Unfold { count, .. } => {
                spans.extend(unfold_line(pane_w, *count, cur).spans);
            }
            DisplayEntry::Line(i) if sbs => {
                let row = &diff.rows[*i];
                let lw = pane_w.saturating_sub(1) / 2;
                let rw = pane_w.saturating_sub(1) - lw;
                let lhl = row.old_ln.and_then(|n| app.old_hl.get(n - 1));
                let rhl = row.new_ln.and_then(|n| app.new_hl.get(n - 1));
                spans.extend(cell(row, Side::Left, hskip, lw, sel, cur, lhl, find));
                spans.push(Span::styled("│", Style::default().fg(p.divider)));
                spans.extend(cell(row, Side::Right, hskip, rw, sel, cur, rhl, find));
            }
            DisplayEntry::Line(i) => {
                let w = pane_w;
                let entry = &diff.inline[*i];
                let row = &diff.rows[entry.row];
                let (mark, ln_old, ln_new, text, side) = match entry.side {
                    Side::Left => ('-', row.old_ln, None, row.old_text.as_deref(), Side::Left),
                    Side::Right => {
                        let mark = if row.kind == RowKind::Context {
                            ' '
                        } else {
                            '+'
                        };
                        (
                            mark,
                            row.old_ln,
                            row.new_ln,
                            row.new_text.as_deref(),
                            Side::Right,
                        )
                    }
                };
                let ln_here = match side {
                    Side::Left => row.old_ln,
                    Side::Right => row.new_ln,
                };
                let text_len = text.map(|t| t.chars().count()).unwrap_or(0);
                let cols = ln_here.and_then(|n| sel.and_then(|s| s.cols_on(side, n, text_len)));
                let linewise = cols.is_some() && sel.is_some_and(|s| s.linewise);
                let hl = match side {
                    Side::Left => row.old_ln.and_then(|n| app.old_hl.get(n - 1)),
                    Side::Right => row.new_ln.and_then(|n| app.new_hl.get(n - 1)),
                };
                let bg = if linewise {
                    Some(p.selected)
                } else {
                    match mark {
                        '+' => Some(p.added),
                        '-' => Some(p.removed),
                        _ if cur => Some(p.cursor),
                        _ => None,
                    }
                };
                let gutter = format!(
                    "{} {} {mark} ",
                    match ln_old {
                        Some(n) => format!("{n:>4}"),
                        None => "    ".into(),
                    },
                    match ln_new {
                        Some(n) => format!("{n:>4}"),
                        None => "    ".into(),
                    },
                );
                let body_w = w.saturating_sub(gutter.chars().count());
                let mut gs = Style::default().fg(p.line_no);
                let mut base = Style::default();
                if let Some(bg) = bg {
                    gs = gs.bg(bg);
                    base = base.bg(bg);
                }
                if linewise {
                    base = base.add_modifier(Modifier::BOLD);
                }
                if cur {
                    gs = gs.add_modifier(Modifier::UNDERLINED);
                    base = base.add_modifier(Modifier::UNDERLINED);
                }
                let body = text.unwrap_or("");
                let marks = crate::search::find_ranges(body, find);
                spans.push(Span::styled(gutter, gs));
                spans.extend(hl_body(
                    body,
                    hl,
                    hskip,
                    body_w,
                    base,
                    p.text,
                    &marks,
                    cols.filter(|_| !linewise),
                ));
            }
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

// ------------------------------------------------------------------ overlays

fn draw_checkout_prompt(f: &mut Frame, app: &mut App, area: Rect, number: u64) {
    let p = palette();
    let rect = centered(area, 64, 9);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.accent))
        .style(Style::default().fg(p.text))
        .title(format!(" Open PR #{number} "));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let text = vec![
        Line::from(Span::styled(
            "Check out the PR branch locally?",
            Style::default().fg(p.text),
        )),
        Line::from(Span::styled(
            "Checkout & review — switches your working tree to the PR branch",
            Style::default().fg(p.dim),
        )),
        Line::from(Span::styled(
            "and lets you edit files directly in the diff view.",
            Style::default().fg(p.dim),
        )),
        Line::from(Span::styled(
            "Review only — no checkout; commenting works, editing is off.",
            Style::default().fg(p.dim),
        )),
    ];
    f.render_widget(Paragraph::new(text), inner);

    let btn_area = Rect {
        x: inner.x,
        y: inner.y + inner.height - 1,
        width: inner.width,
        height: 1,
    };
    buttons_right(
        f,
        app,
        btn_area,
        0,
        &[
            ("Checkout & review (c)", ButtonId::CheckoutYes, true),
            ("Review only (o)", ButtonId::CheckoutReviewOnly, false),
            ("Cancel (Esc)", ButtonId::CheckoutCancel, false),
        ],
    );
}

/// "Are you sure?" for a revert. The one dialog in loupe that guards
/// something unrecoverable, so it says plainly what goes and that git
/// cannot bring it back.
fn draw_revert_prompt(f: &mut Frame, app: &mut App, area: Rect) {
    let Overlay::Revert(prompt) = &app.overlay else {
        return;
    };
    let p = palette();
    let whole_file = matches!(prompt.target, crate::app::RevertTarget::File { .. });
    let title = if whole_file {
        " Revert this file? "
    } else {
        " Revert this change? "
    };
    let question = if whole_file {
        "Are you sure you want to revert the changes in this file?".to_string()
    } else {
        "Are you sure you want to revert this section of the diff?".to_string()
    };
    let what = if prompt.deletes {
        format!("{} will be deleted — it is a new file.", prompt.path)
    } else if whole_file {
        format!(
            "{} goes back to the version it was changed from (+{} −{}).",
            prompt.path, prompt.adds, prompt.dels
        )
    } else {
        format!(
            "Putting back {} in {}.",
            crate::app::lines_phrase(prompt.adds, prompt.dels),
            prompt.path
        )
    };
    let scope = if whole_file {
        "The working tree and the index are both put back."
    } else {
        "Only the working tree is touched; nothing staged is unstaged."
    };
    let rect = centered(area, 72, 8);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.err))
        .style(Style::default().fg(p.text))
        .title(title);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let text = vec![
        Line::from(Span::styled(
            question,
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(what, Style::default().fg(p.dim))),
        Line::from(Span::styled(scope, Style::default().fg(p.dim))),
        Line::from(Span::styled(
            "This cannot be undone.",
            Style::default().fg(p.err).add_modifier(Modifier::BOLD),
        )),
    ];
    f.render_widget(Paragraph::new(text), inner);

    let btn_area = Rect {
        x: inner.x,
        y: inner.y + inner.height - 1,
        width: inner.width,
        height: 1,
    };
    buttons_right(
        f,
        app,
        btn_area,
        0,
        &[
            ("↺ Revert (Enter)", ButtonId::RevertYes, true),
            ("Cancel (Esc)", ButtonId::RevertCancel, false),
        ],
    );
}

/// The file-panel right-click menu. It is drawn at the pointer rather
/// than centred, because it is about one row and the answer to "which
/// row?" is where the pointer is.
fn draw_path_menu(f: &mut Frame, app: &mut App, area: Rect) {
    let Overlay::PathMenu(menu) = &app.overlay else {
        return;
    };
    let p = palette();
    let what = if menu.is_dir { "📁" } else { "📄" };
    let title = format!(" {what} {} ", tail_truncate(&menu.path, 40));
    // Two columns of padding, plus " (r)" after the longest label.
    let widest = menu
        .items
        .iter()
        .map(|it| disp_width(it.label) + 4)
        .max()
        .unwrap_or(0);
    let w = (widest.max(disp_width(&title)) as u16 + 4).min(area.width);
    let h = (menu.items.len() as u16 + 2).min(area.height);
    let (ax, ay) = menu.anchor;
    // Keep the whole menu on screen. It opens down and to the right of the
    // pointer, and flips above it when there is no room below.
    let x = ax.min(area.x + area.width.saturating_sub(w));
    let y = if ay + h <= area.y + area.height {
        ay
    } else {
        (ay + 1).saturating_sub(h).max(area.y)
    };
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.accent))
        .style(Style::default().fg(p.text))
        .title(title);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let mut rows: Vec<(Rect, ButtonId)> = Vec::new();
    for (i, item) in menu.items.iter().enumerate() {
        if i as u16 >= inner.height {
            break;
        }
        let r = Rect {
            x: inner.x,
            y: inner.y + i as u16,
            width: inner.width,
            height: 1,
        };
        let selected = i == menu.sel;
        let style = if selected {
            Style::default()
                .bg(p.row)
                .fg(p.text)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.text)
        };
        let line = Line::from(vec![
            Span::styled(item.label.to_string(), style),
            Span::styled(format!(" ({})", item.key), Style::default().fg(p.key)),
        ]);
        f.render_widget(Paragraph::new(line).style(style), r);
        rows.push((r, ButtonId::PathMenuRow(i)));
    }
    app.layout.buttons.extend(rows);
}

/// The popup behind one blame row: the commit, and the ways to follow it.
///
/// The pane has room for a name, an age and a number. This is where the
/// rest of the answer lives — the exact date, the subject, the pull
/// request title, and whether the commit belongs to the change on screen.
fn draw_blame_menu(f: &mut Frame, app: &mut App, area: Rect) {
    let Overlay::BlameMenu(menu) = &app.overlay else {
        return;
    };
    let p = palette();
    let c = &menu.commit;

    // The facts, then the actions. The facts are not selectable, so they
    // are drawn as plain rows above the keyed ones.
    let mut facts: Vec<Line> = Vec::new();
    if c.uncommitted() {
        facts.push(Line::from(Span::styled(
            "Not committed yet — your working tree.",
            Style::default().fg(p.blame_uncommitted),
        )));
    } else {
        facts.push(Line::from(vec![
            Span::styled(
                c.short().to_string(),
                Style::default().fg(p.key).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}", blame::date(c.author_time)),
                Style::default().fg(p.dim),
            ),
        ]));
        facts.push(Line::from(vec![
            Span::styled(
                c.author.clone(),
                Style::default().fg(if menu.mine { p.blame_mine } else { p.text }),
            ),
            Span::styled(
                format!(
                    " <{}>{}",
                    c.author_email,
                    if menu.mine { " · you" } else { "" }
                ),
                Style::default().fg(p.dim),
            ),
        ]));
        facts.push(Line::from(Span::styled(
            c.summary.clone(),
            Style::default().fg(p.text),
        )));
    }
    if menu.in_change {
        facts.push(Line::from(Span::styled(
            "▸ Part of the change you are reviewing.",
            Style::default()
                .fg(p.blame_change)
                .add_modifier(Modifier::BOLD),
        )));
    }
    if let Some(pr) = &menu.pr {
        facts.push(Line::from(vec![
            Span::styled(format!("#{} ", pr.number), Style::default().fg(p.key)),
            Span::styled(pr.title.clone(), Style::default().fg(p.dim)),
        ]));
    }
    if menu.items.is_empty() {
        facts.push(Line::from(Span::styled(
            "any key closes",
            Style::default().fg(p.faint),
        )));
    }

    let widest = facts
        .iter()
        .map(|l| disp_width(&l.to_string()))
        .chain(menu.items.iter().map(|it| disp_width(&it.label) + 4))
        .max()
        .unwrap_or(20);
    let w = (widest as u16 + 4).min(area.width);
    let h = ((facts.len() + menu.items.len()) as u16 + 2).min(area.height);
    let (ax, ay) = menu.anchor;
    // Keep the whole popup on screen: it opens down and to the right of
    // the pointer, and flips above it when there is no room below.
    let x = ax.min(area.x + area.width.saturating_sub(w));
    let y = if ay + h <= area.y + area.height {
        ay
    } else {
        (ay + 1).saturating_sub(h).max(area.y)
    };
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.accent))
        .style(Style::default().fg(p.text))
        .title(" Blame ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let n_facts = facts.len().min(inner.height as usize);
    f.render_widget(
        Paragraph::new(facts),
        Rect {
            height: n_facts as u16,
            ..inner
        },
    );

    let mut rows: Vec<(Rect, ButtonId)> = Vec::new();
    for (i, item) in menu.items.iter().enumerate() {
        let top = n_facts + i;
        if top as u16 >= inner.height {
            break;
        }
        let r = Rect {
            x: inner.x,
            y: inner.y + top as u16,
            width: inner.width,
            height: 1,
        };
        let style = if i == menu.sel {
            Style::default()
                .bg(p.row)
                .fg(p.text)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.text)
        };
        let line = Line::from(vec![
            Span::styled(item.label.clone(), style),
            Span::styled(format!(" ({})", item.key), Style::default().fg(p.key)),
        ]);
        f.render_widget(Paragraph::new(line).style(style), r);
        rows.push((r, ButtonId::BlameMenuRow(i)));
    }
    app.layout.buttons.extend(rows);
}

/// The ☰ menu: everything the top bar no longer has room for, grouped
/// under headings, with the key that does the same thing on the right.
fn draw_menu(f: &mut Frame, app: &mut App, area: Rect) {
    let Overlay::Menu(menu) = &app.overlay else {
        return;
    };
    let p = palette();
    let title = " ☰ Menu ";
    // Two columns of padding, the widest label, a gap, the widest hint,
    // and two columns for the on/off mark.
    let label_w = menu
        .rows
        .iter()
        .map(|r| match r {
            MenuRow::Heading(h) => disp_width(h),
            MenuRow::Item(it) => disp_width(&it.label) + 2,
        })
        .max()
        .unwrap_or(0);
    let hint_w = menu
        .rows
        .iter()
        .map(|r| match r {
            MenuRow::Heading(_) => 0,
            MenuRow::Item(it) => disp_width(it.hint),
        })
        .max()
        .unwrap_or(0);
    let w = ((label_w + hint_w + 5).max(disp_width(title)) as u16 + 2).min(area.width);
    let want = menu.rows.len() as u16 + 2;
    let (ax, ay) = menu.anchor;
    // Hang below the button, pulled left until the whole panel is on
    // screen. A menu taller than the room below it is shortened and
    // scrolls, rather than being flipped up over the top bar — the ☰ it
    // belongs to has to stay visible.
    let x = ax.min(area.x + area.width.saturating_sub(w));
    let below = (area.y + area.height).saturating_sub(ay + 1);
    let (y, h) = if below >= want || below >= MENU_MIN_H {
        (ay + 1, want.min(below))
    } else {
        (area.y, want.min(area.height))
    };
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.accent))
        .style(Style::default().fg(p.text))
        .title(title);
    // Say so when there is more than fits, or the lines below the fold
    // are simply invisible.
    if want > h {
        block = block.title_bottom(Line::from(Span::styled(
            " ▴ scroll ▾ ",
            Style::default().fg(p.dim),
        )));
    }
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    // A menu taller than the terminal scrolls with the selection.
    let height = inner.height as usize;
    let Overlay::Menu(menu) = &mut app.overlay else {
        return;
    };
    menu.scroll_into_view(height);
    let scroll = menu.scroll;
    let sel = menu.sel;
    let rows: Vec<(usize, String, String, Option<bool>, bool, bool)> = menu
        .rows
        .iter()
        .enumerate()
        .skip(scroll)
        .take(height)
        .map(|(i, r)| match r {
            MenuRow::Heading(h) => (i, h.to_string(), String::new(), None, false, false),
            MenuRow::Item(it) => (
                i,
                it.label.clone(),
                it.hint.to_string(),
                it.checked,
                it.enabled,
                true,
            ),
        })
        .collect();

    let mut hits: Vec<(Rect, ButtonId)> = Vec::new();
    for (slot, (i, label, hint, checked, enabled, is_item)) in rows.into_iter().enumerate() {
        let r = Rect {
            x: inner.x,
            y: inner.y + slot as u16,
            width: inner.width,
            height: 1,
        };
        if !is_item {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    label,
                    Style::default().fg(p.dim).add_modifier(Modifier::BOLD),
                ))),
                r,
            );
            continue;
        }
        let selected = i == sel;
        let fg = if enabled { p.text } else { p.faint };
        let row_style = if selected {
            Style::default()
                .bg(p.row)
                .fg(fg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(fg)
        };
        // A switch says what it is set to; a plain line gets the same two
        // columns of indent so the labels stay in one column.
        let mark = match checked {
            Some(true) => "● ",
            Some(false) => "○ ",
            None => "  ",
        };
        let body = format!("{mark}{label}");
        let pad = (inner.width as usize).saturating_sub(disp_width(&body) + disp_width(&hint) + 1);
        let line = Line::from(vec![
            Span::styled(body, row_style),
            Span::styled(" ".repeat(pad), row_style),
            Span::styled(
                hint,
                if enabled {
                    Style::default().fg(p.key)
                } else {
                    Style::default().fg(p.faint)
                },
            ),
            Span::styled(" ", row_style),
        ]);
        f.render_widget(Paragraph::new(line).style(row_style), r);
        if enabled {
            hits.push((r, ButtonId::MenuRow(i)));
        }
    }
    app.layout.buttons.extend(hits);
}

fn draw_comment_overlay(f: &mut Frame, app: &mut App, area: Rect) {
    let Overlay::Comment(draft) = &mut app.overlay else {
        return;
    };
    let rect = centered(area, area.width.saturating_sub(20).clamp(40, 90), 14);
    f.render_widget(Clear, rect);
    let side = if draft.side == Side::Right {
        "new side"
    } else {
        "old side"
    };
    let range = if draft.lo == draft.hi {
        format!("{}", draft.hi)
    } else {
        format!("{}–{}", draft.lo, draft.hi)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette().accent))
        .title(format!(" 💬 Comment · {}:{} ({side}) ", draft.path, range));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let ta_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: inner.height.saturating_sub(1),
    };
    // tui-textarea's own defaults are named ANSI colors chosen for a dark
    // terminal (a bright-black placeholder, a light-blue selection). This
    // is the one widget loupe renders rather than draws, so its styles are
    // set from the palette here — every frame, so the theme picker's
    // light/dark switch reaches it too.
    let p = palette();
    draft.textarea.set_style(Style::default().fg(p.text));
    draft
        .textarea
        .set_placeholder_style(Style::default().fg(p.dim));
    draft
        .textarea
        .set_selection_style(Style::default().bg(p.editor_sel).fg(p.text));
    draft
        .textarea
        .set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    draft
        .textarea
        .set_cursor_line_style(Style::default().fg(p.text));
    f.render_widget(&draft.textarea, ta_area);

    let btn_area = Rect {
        x: inner.x,
        y: inner.y + inner.height.saturating_sub(1),
        width: inner.width,
        height: 1,
    };
    buttons_right(
        f,
        app,
        btn_area,
        0,
        &[
            ("Post (Ctrl+S)", ButtonId::CommentPost, true),
            ("Cancel (Esc)", ButtonId::CommentCancel, false),
        ],
    );
}

fn draw_theme_picker(f: &mut Frame, app: &mut App, area: Rect) {
    let p = palette();
    let rect = centered(area, 84, area.height.min(28));
    f.render_widget(Clear, rect);
    let Overlay::ThemePicker(tp) = &mut app.overlay else {
        return;
    };
    let name = crate::highlight::THEMES[tp.sel].0;
    // Which half of the config the pick will be saved into.
    let slot = if crate::theme::appearance().is_light() {
        "light terminals"
    } else {
        "dark terminals"
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.accent))
        .title(format!(" 🎨 Theme for {slot} — the preview is live "))
        .title_bottom(" j/k or click to try · a light/dark · Enter to keep · Esc to cancel ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    // Left: theme names. Right: a highlighted sample with diff backgrounds.
    let list_w = 26.min(inner.width / 2);
    let list = Rect {
        x: inner.x,
        y: inner.y,
        width: list_w,
        height: inner.height.saturating_sub(1),
    };
    let sample_area = Rect {
        x: inner.x + list_w + 1,
        y: inner.y,
        width: inner.width.saturating_sub(list_w + 1),
        height: inner.height.saturating_sub(1),
    };

    let visible = list.height as usize;
    if tp.sel < tp.scroll {
        tp.scroll = tp.sel;
    } else if visible > 0 && tp.sel >= tp.scroll + visible {
        tp.scroll = tp.sel + 1 - visible;
    }
    let end = crate::highlight::THEMES.len().min(tp.scroll + visible);
    let mut rows: Vec<(Rect, ButtonId)> = Vec::new();
    for (row, idx) in (tp.scroll..end).enumerate() {
        let label = crate::highlight::THEMES[idx].0;
        let r = Rect {
            x: list.x,
            y: list.y + row as u16,
            width: list.width,
            height: 1,
        };
        let selected = idx == tp.sel;
        let style = if selected {
            Style::default()
                .bg(p.selected)
                .fg(p.text)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.dim)
        };
        let marker = if selected { "▸ " } else { "  " };
        f.render_widget(
            Paragraph::new(Span::styled(format!("{marker}{label}"), style)),
            r,
        );
        rows.push((r, ButtonId::ThemeRow(idx)));
    }

    // The sample, with one removed + one added row so the diff backgrounds
    // are previewed against the theme too. The panel is filled with the
    // theme's *own* background, which is what makes a light theme look
    // light here instead of only once you've committed to it.
    let sample_bg = crate::highlight::theme_background(crate::highlight::THEMES[tp.sel].1);
    let sample_lines: Vec<&str> = crate::wizard::SAMPLE.lines().collect();
    let mut lines: Vec<Line> = Vec::new();
    for (i, text) in sample_lines
        .iter()
        .enumerate()
        .take(sample_area.height as usize)
    {
        let bg = match i {
            11 => Some(p.removed),
            12 => Some(p.added),
            _ => sample_bg,
        };
        let spans: Vec<Span> = match tp.preview.get(i) {
            Some(segs) if !segs.is_empty() => segs
                .iter()
                .map(|(color, s)| {
                    let mut st = Style::default().fg(*color);
                    if let Some(b) = bg {
                        st = st.bg(b);
                    }
                    Span::styled(s.clone(), st)
                })
                .collect(),
            _ => vec![Span::styled(*text, Style::default().fg(p.text))],
        };
        let mut line = Line::from(spans);
        if let Some(b) = bg {
            line = line.style(Style::default().bg(b));
        }
        lines.push(line);
    }
    let mut sample = Paragraph::new(lines);
    if let Some(b) = sample_bg {
        sample = sample.style(Style::default().bg(b));
    }
    f.render_widget(sample, sample_area);

    app.layout.buttons.extend(rows);
    let btn_area = Rect {
        x: inner.x,
        y: inner.y + inner.height.saturating_sub(1),
        width: inner.width,
        height: 1,
    };
    let flip = if crate::theme::appearance().is_light() {
        "🌙 Dark (a)"
    } else {
        "☀ Light (a)"
    };
    buttons_right(
        f,
        app,
        btn_area,
        0,
        &[
            (flip, ButtonId::AppearanceToggle, false),
            (&format!("Use {name} (Enter)"), ButtonId::ThemeApply, true),
            ("Cancel (Esc)", ButtonId::ThemeCancel, false),
        ],
    );
}

/// Text with some characters emphasized — the fuzzy match's own letters,
/// or the literal range a grep hit landed on. Runs of one style coalesce.
fn emphasize<'a>(
    text: &str,
    matched: &[usize],
    range: Option<(usize, usize)>,
    base: Style,
    hit: Style,
    width: usize,
) -> Vec<Span<'a>> {
    let mut spans = Vec::new();
    let mut out = String::new();
    let mut on = false;
    let mut w = 0usize;
    for (i, ch) in text.chars().enumerate() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > width {
            break;
        }
        let is_hit = matched.contains(&i) || range.map(|(s, e)| i >= s && i < e).unwrap_or(false);
        if is_hit != on && !out.is_empty() {
            spans.push(Span::styled(
                std::mem::take(&mut out),
                if on { hit } else { base },
            ));
        }
        on = is_hit;
        out.push(ch);
        w += cw;
    }
    if !out.is_empty() {
        spans.push(Span::styled(out, if on { hit } else { base }));
    }
    if w < width {
        spans.push(Span::styled(" ".repeat(width - w), base));
    }
    spans
}

/// What the language server knows about the symbol under the cursor.
/// Deliberately small and dismissible — it answers a question, it isn't a
/// place to live.
fn draw_hover(f: &mut Frame, app: &App, area: Rect) {
    let p = palette();
    let Overlay::Hover(h) = &app.overlay else {
        return;
    };
    let widest = h
        .lines
        .iter()
        .map(|l| disp_width(l))
        .max()
        .unwrap_or(20)
        .clamp(24, area.width.saturating_sub(8) as usize);
    let height = (h.lines.len() as u16 + 2)
        .min(area.height.saturating_sub(4))
        .max(3);
    let rect = centered(area, widest as u16 + 4, height);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.accent))
        .title(format!(" {} ", h.word))
        .title_bottom(" any key closes ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let lines: Vec<Line> = h
        .lines
        .iter()
        .take(inner.height as usize)
        .enumerate()
        .map(|(i, l)| {
            // The first line is the signature; the rest is prose.
            let style = if i == 0 {
                Style::default().fg(p.text).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(p.dim)
            };
            Line::from(Span::styled(l.clone(), style))
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

/// The finder overlay: one input, one list, three modes.
fn draw_finder(f: &mut Frame, app: &mut App, area: Rect) {
    let p = palette();
    let w = area.width.saturating_sub(6).clamp(40, 110);
    // Grow with the results rather than always taking the full height: an
    // overlay mostly made of blank rows reads as "nothing found".
    let listed = match &app.overlay {
        Overlay::Finder(f) => f.rows.len().clamp(1, FINDER_ROWS),
        _ => FINDER_ROWS,
    };
    let h = (listed as u16 + 5).min(area.height.saturating_sub(2));
    let rect = centered(area, w, h);
    f.render_widget(Clear, rect);
    let Overlay::Finder(fd) = &app.overlay else {
        return;
    };
    let mode = fd.mode;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.accent))
        .title(format!(" 🔍 {} ", mode.title()))
        .title_bottom(" Enter opens · ↑↓ moves · Tab changes scope · Esc closes ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.height < 4 {
        return;
    }

    // Input line: the mode prefix, then what was typed, then the caret.
    let typed: String = fd.input.chars().take(fd.cursor).collect();
    let rest: String = fd.input.chars().skip(fd.cursor).collect();
    let mut input_spans = vec![Span::styled(
        format!(" {}", mode.prefix()),
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
    )];
    input_spans.push(Span::styled(typed, Style::default().fg(p.text)));
    input_spans.push(Span::styled(
        "▏",
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
    ));
    input_spans.push(Span::styled(rest, Style::default().fg(p.text)));
    if fd.regex && mode == FinderMode::Grep {
        input_spans.push(Span::styled(
            "   .* regex",
            Style::default().fg(p.stage_partial),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(input_spans)),
        Rect { height: 1, ..inner },
    );
    let note = match app.search_spinner() {
        Some(frame) => format!(" {frame} {}", fd.note),
        None => format!(" {}", fd.note),
    };
    f.render_widget(
        Paragraph::new(Span::styled(note, Style::default().fg(p.faint))),
        Rect {
            y: inner.y + 1,
            height: 1,
            ..inner
        },
    );

    // Results.
    let list_h = inner.height.saturating_sub(3) as usize;
    let end = fd.rows.len().min(fd.scroll + list_h);
    let path_w = (inner.width as usize / 3).clamp(12, 44);
    let mut hits: Vec<(Rect, ButtonId)> = Vec::new();
    let mut lines: Vec<Line> = Vec::new();
    for idx in fd.scroll..end {
        let row = &fd.rows[idx];
        let selected = idx == fd.sel;
        let mut base = Style::default().fg(if row.in_changeset { p.text } else { p.dim });
        let mut hit = Style::default().fg(p.key).add_modifier(Modifier::BOLD);
        if selected {
            base = base.bg(p.row).fg(p.text);
            hit = hit.bg(p.row);
        }
        let mut spans = vec![Span::styled(if selected { " ▸ " } else { "   " }, base)];
        let tag_w = if row.tag.is_empty() {
            0
        } else {
            row.tag.len() + 2
        };
        let avail = (inner.width as usize).saturating_sub(3 + tag_w);
        match row.line {
            // A line inside a file: where, then what.
            Some(line) => {
                let where_ = format!("{}:{} ", tail_truncate(&row.path, path_w), line);
                let ww = disp_width(&where_);
                spans.push(Span::styled(
                    where_,
                    if selected {
                        Style::default().fg(p.dir).bg(p.row)
                    } else {
                        Style::default().fg(p.dir)
                    },
                ));
                spans.extend(emphasize(
                    &row.text,
                    &row.matched,
                    row.range,
                    base,
                    hit,
                    avail.saturating_sub(ww),
                ));
            }
            None => spans.extend(emphasize(
                &row.text,
                &row.matched,
                row.range,
                base,
                hit,
                avail,
            )),
        }
        if !row.tag.is_empty() {
            spans.push(Span::styled(
                format!(" {} ", row.tag),
                Style::default()
                    .fg(if row.tag == "def" {
                        p.badge_fg
                    } else {
                        p.faint
                    })
                    .bg(if row.tag == "def" {
                        p.badge_local
                    } else {
                        p.btn_bg
                    }),
            ));
        }
        lines.push(Line::from(spans));
        hits.push((
            Rect {
                x: inner.x,
                y: inner.y + 2 + (idx - fd.scroll) as u16,
                width: inner.width,
                height: 1,
            },
            ButtonId::FinderRow(idx),
        ));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "   No results.",
            Style::default().fg(p.dim),
        )));
    }
    f.render_widget(
        Paragraph::new(lines),
        Rect {
            y: inner.y + 2,
            height: inner.height.saturating_sub(3),
            ..inner
        },
    );

    // Mode tabs and the scope switch, on the bottom row.
    let repo_scope = fd.repo_scope;
    let btn_area = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: 1,
        ..inner
    };
    app.layout.buttons.extend(hits);
    let scope = if repo_scope {
        "◍ Whole repo"
    } else {
        "◌ Changed files"
    };
    // References and the symbol picker are arrived at, not chosen, so they
    // offer a way back instead of the tab strip.
    let mut buttons = if mode.is_tab() {
        vec![
            (
                "Files",
                ButtonId::FinderMode(FinderMode::Files),
                mode == FinderMode::Files,
            ),
            (
                "# Text",
                ButtonId::FinderMode(FinderMode::Grep),
                mode == FinderMode::Grep,
            ),
            (
                "@ Symbols",
                ButtonId::FinderMode(FinderMode::Symbols),
                mode == FinderMode::Symbols,
            ),
            (scope, ButtonId::FinderScope, repo_scope),
        ]
    } else {
        vec![("◂ Files", ButtonId::FinderMode(FinderMode::Files), false)]
    };
    buttons.push(("✕", ButtonId::FinderClose, false));
    buttons_right(f, app, btn_area, 0, &buttons);
}

fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    let p = palette();
    let rect = centered(area, 108.min(area.width), 42.min(area.height));
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.accent))
        .title(" Help — loupe ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let head = Style::default().fg(p.text).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(p.dim);
    let key = Style::default().fg(p.key);

    // Two columns of "keys — what they do", so the whole reference fits on
    // one screen.
    let col = 40usize;
    let row = |a: (&str, &str), b: (&str, &str)| -> Line<'static> {
        let pad = |(k, d): (&str, &str)| {
            let k = format!("  {k}");
            let gap = col.saturating_sub(disp_width(&k) + disp_width(d) + 1);
            (k, format!("{}{d}", " ".repeat(gap.max(1))))
        };
        let (ka, da) = pad(a);
        let (kb, db) = pad(b);
        Line::from(vec![
            Span::styled(ka, key),
            Span::styled(da, dim),
            Span::styled(kb, key),
            Span::styled(db, dim),
        ])
    };

    let servers = app
        .lsp
        .status()
        .into_iter()
        .map(|(lang, state)| {
            // The install hint is too long for one line here; --lsp has it.
            let short = match state.split_whitespace().next() {
                Some("not") if state.contains("not installed") => "not installed",
                _ => Box::leak(state.into_boxed_str()),
            };
            format!("{lang} {short}")
        })
        .collect::<Vec<_>>()
        .join(" · ");

    let lines = vec![
        Line::from(Span::styled("Mouse — the main way around", head)),
        row(
            ("click file / PR", "open it"),
            ("drag over lines", "select a range"),
        ),
        row(
            ("click a line", "select + move cursor"),
            ("double-click", "edit that line"),
        ),
        row(
            ("click ··· / ⌃⌃⌃", "expand / fold a run"),
            ("right-click a file", "copy its path"),
        ),
        row(
            ("click ☰", "the full menu"),
            ("click ⟳", "re-scan and reload now"),
        ),
        row(
            ("click [ ] / [+]", "viewed / stage file"),
            ("right-click the diff", "clear selection"),
        ),
        row(
            ("click ↺ in the diff", "put that change back"),
            ("click ↺ in the files", "put the whole file back"),
        ),
        row(
            ("wheel", "scroll · Shift for sideways"),
            ("drag the divider", "resize the panel"),
        ),
        row(
            ("right-click PR #n", "copy the link to the PR"),
            ("double-click the divider", "reset the panel width"),
        ),
        Line::from(""),
        Line::from(Span::styled("Move (the cursor row is underlined)", head)),
        row(
            ("j / k, ↑ / ↓", "line down / up"),
            ("Ctrl+D / Ctrl+U", "half page"),
        ),
        row(
            ("gg / G", "first / last line"),
            ("Ctrl+F / Ctrl+B", "full page"),
        ),
        row(
            ("} / {", "next / previous change"),
            ("H / M / L", "top / mid / bottom"),
        ),
        row(
            ("h / l, ← / →", "scroll sideways"),
            ("Ctrl+E / Ctrl+Y", "scroll, keep cursor"),
        ),
        row(
            ("0 / $", "first / last column"),
            ("] / [", "next / previous file"),
        ),
        Line::from(""),
        Line::from(Span::styled("Find", head)),
        row(
            ("/", "search this file, as you type"),
            ("n / N", "next / previous match"),
        ),
        row(
            ("Ctrl+P", "go to file (fuzzy)"),
            ("#", "find in files (grep)"),
        ),
        row(
            ("@", "definitions in this file"),
            ("Tab (in find)", "changed files ⇄ whole repo"),
        ),
        row(
            ("gd / gr", "definition / references"),
            ("K", "what is this? (type + docs)"),
        ),
        Line::from(""),
        Line::from(Span::styled("Act", head)),
        row(
            ("V", "select lines (j/k extends)"),
            ("c", "comment on the selection"),
        ),
        row(
            ("Enter / Space", "expand / fold at cursor"),
            ("z", "fold all unchanged"),
        ),
        row(
            ("e / i", "edit at the cursor line"),
            ("t", "theme picker (live preview)"),
        ),
        row(
            ("P", "read a .md file as a document"),
            ("P (in the preview)", "back to its source"),
        ),
        row(
            ("y or Ctrl+C", "copy the selected lines"),
            ("x", "mark viewed / stage file"),
        ),
        row(
            ("u", "revert the change at the cursor"),
            ("U", "revert every change in the file"),
        ),
        row(
            ("r", "refresh — re-scan and reload"),
            ("Esc", "clear selection, then search"),
        ),
        row(
            ("m", "the ☰ menu (everything else)"),
            ("Esc (in the menu)", "put it away"),
        ),
        row(
            ("` (backtick)", "swap PR ⇄ local view"),
            ("v", "split / inline view"),
        ),
        row(
            ("t then a", "switch light / dark"),
            ("?", "this help"),
        ),
        row(
            ("b / q", "back to the PR list / quit"),
            ("< / >", "narrow / widen panel"),
        ),
        Line::from(""),
        Line::from(Span::styled(
            "  Editor: Ctrl+S save · Ctrl+C copy · Ctrl+Z or Ctrl+U undo · Ctrl+R redo · Alt+P preview markdown · Esc close",
            dim,
        )),
        Line::from(Span::styled(
            "  Markdown preview: } / { walk the headings · r reloads · it reloads itself when an agent rewrites the file · loupe md <path> reads one from the shell",
            dim,
        )),
        Line::from(Span::styled(
            "  Editor + language server: Ctrl+Space complete · Ctrl+G what is this? · Ctrl+] definition · Ctrl+T format",
            dim,
        )),
        Line::from(Span::styled(
            "  Selecting with the mouse across the whole screen: hold Option (macOS) or Shift",
            dim,
        )),
        Line::from(Span::styled(
            "  Local review re-scans the working tree while you sit idle, so an agent's edits appear on their own (☰ → Refresh while idle)",
            dim,
        )),
        // What gd / gr / K can actually answer right now, and why not.
        Line::from(Span::styled(format!("  Language servers: {servers}"), dim)),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

// ------------------------------------------------------------------ status

fn draw_status(f: &mut Frame, app: &mut App, area: Rect) {
    let p = palette();
    if let Some((frame, label, cancellable)) = app.spinner() {
        let cancel = if cancellable {
            "  ·  press c to cancel"
        } else {
            ""
        };
        let line = Line::from(vec![
            Span::styled(
                format!(" {frame} "),
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{label}…"), Style::default().fg(p.key)),
            Span::styled(cancel, Style::default().fg(p.faint)),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    // The `/` prompt lives in the status line, vim-style: it is a mode,
    // not a dialog, so it must not cover the text being searched.
    if app.find.typing {
        let n = app.find.rows.len();
        let count = if app.find.query.is_empty() {
            String::new()
        } else if n == 0 {
            "  no matches".to_string()
        } else {
            format!("  {n} match{}", if n == 1 { "" } else { "es" })
        };
        let line = Line::from(vec![
            Span::styled(
                format!("/{}", app.find.query),
                Style::default().fg(p.text).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "▏",
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                count,
                Style::default().fg(if n == 0 && !app.find.query.is_empty() {
                    p.err
                } else {
                    p.faint
                }),
            ),
            Span::styled(
                "   Enter keeps it · Esc cancels",
                Style::default().fg(p.faint),
            ),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    // In the editor, what the language server says about the cursor line
    // outranks the last status message — it is about the code, not about
    // what loupe just did.
    if let Some(editor) = &app.editor {
        if let Some(d) = editor.diagnostics_here().first() {
            let color = if d.is_error() { p.err } else { p.stage_partial };
            let code = match &d.code {
                Some(c) => format!("  {c}"),
                None => String::new(),
            };
            let hints = " Ctrl+S save · Ctrl+G what is this? · ? help";
            let msg_w = area.width.saturating_sub(hints.chars().count() as u16 + 1) as usize;
            let line = Line::from(vec![
                Span::styled(
                    truncate_pad(
                        &format!(
                            "{} {}{code}",
                            if d.is_error() { "✗" } else { "▲" },
                            d.message
                        ),
                        msg_w,
                    ),
                    Style::default().fg(color),
                ),
                Span::styled(hints, Style::default().fg(p.faint)),
            ]);
            f.render_widget(Paragraph::new(line), area);
            return;
        }
        // Nothing wrong on this line, but something is wrong somewhere:
        // say how much, so a problem off screen isn't invisible.
        if !editor.diagnostics.is_empty() && app.status.is_empty() {
            let mut counts: std::collections::BTreeMap<&'static str, usize> = Default::default();
            for d in &editor.diagnostics {
                *counts.entry(d.label()).or_default() += 1;
            }
            let summary = counts
                .iter()
                .map(|(what, n)| format!("{n} {what}{}", if *n == 1 { "" } else { "s" }))
                .collect::<Vec<_>>()
                .join(" · ");
            let worst_is_error = editor.diagnostics.iter().any(|d| d.is_error());
            let line = Line::from(Span::styled(
                format!(" {summary} in this file"),
                Style::default().fg(if worst_is_error {
                    p.err
                } else {
                    p.stage_partial
                }),
            ));
            f.render_widget(Paragraph::new(line), area);
            return;
        }
    }
    let style = if app.status_err {
        Style::default().fg(p.err)
    } else {
        Style::default().fg(p.ok)
    };
    // `u` only appears when there is a working tree to put back.
    let undo = if app.can_revert() { " · u revert" } else { "" };
    let hints: String = match app.screen {
        Screen::PrList => "l local changes · r refresh · m menu · q quit".into(),
        Screen::Review => {
            if app.previewing() {
                if app.preview_only {
                    "j/k scroll · } { sections · P source · r reload · q quit".into()
                } else {
                    "j/k scroll · } { sections · P source · e edit · Esc diff".into()
                }
            } else if app.editor.is_some() {
                if markdown_buffer(app) {
                    "Ctrl+S save · Alt+P preview · Esc close · ? help".into()
                } else {
                    "Ctrl+S save · Esc close · ? help".into()
                }
            } else if app.find.active() {
                format!("n/N matches · / search · y copy{undo} · ? help")
            } else if app.local {
                format!("/ find · V select · y copy · x stage{undo} · m menu")
            } else {
                format!("/ find · V select · y copy · c comment{undo} · m menu")
            }
        }
    };
    let hint_w = hints.chars().count() as u16 + 1;
    let msg_w = area.width.saturating_sub(hint_w + 1) as usize;
    // A silent refresh shows its spinner in place of the status message —
    // informational only, everything stays clickable.
    let msg: Span = match app.quiet_spinner() {
        Some((frame, label)) => Span::styled(
            truncate_pad(&format!("{frame} {label}…"), msg_w),
            Style::default().fg(p.key),
        ),
        None => Span::styled(truncate_pad(&app.status, msg_w), style),
    };
    let line = Line::from(vec![
        msg,
        Span::raw(" "),
        Span::styled(hints, Style::default().fg(p.faint)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::diff::FileDiff;
    use crate::github::ChangedFile;
    use crate::highlight;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// The preview takes the pane the diff and the editor share, keeps the
    /// file panel beside it, and puts its own keys in the status bar.
    #[test]
    fn the_preview_draws_in_the_diff_pane() {
        let _guard = highlight::test_theme_lock();
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.files = vec![ChangedFile {
            path: "PLAN.md".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 0,
            previous: None,
        }];
        app.rebuild_entries();
        app.preview = Some(crate::preview::Preview::new(
            "PLAN.md",
            "/repo/PLAN.md".into(),
            "# STEP 1\n\n- [x] done\n- [ ] todo\n",
        ));
        let mut term = Terminal::new(TestBackend::new(90, 20)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        let screen: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("STEP 1"), "the document is drawn: {screen}");
        assert!(screen.contains("☑ done"), "task boxes render: {screen}");
        assert!(screen.contains("☐ todo"), "and unticked ones: {screen}");
        assert!(screen.contains("PLAN.md"), "the file panel is still there");
        assert!(screen.contains("P source"), "the way back is offered");
    }

    /// End-to-end guard for "editor colors in the diff": highlight a Rust
    /// snippet, push it through App state, render with a TestBackend, and
    /// assert the diff body actually contains several distinct syntax
    /// foreground colors (beyond the UI's own line-number/divider colors).
    #[test]
    fn diff_renders_syntax_colors() {
        let _guard = highlight::test_theme_lock();
        let mut app = App::new(crate::app::LaunchMode::Auto, None);
        app.screen = Screen::Review;
        app.files = vec![ChangedFile {
            path: "test.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            previous: None,
        }];
        app.rebuild_entries();
        let old = "fn main() {\n    let x = 1;\n}\n";
        let new = "fn main() {\n    let x = 42; // changed\n    let s = \"txt\";\n}\n";
        app.old_hl = highlight::highlight("test.rs", old);
        app.new_hl = highlight::highlight("test.rs", new);
        app.diff = Some(FileDiff::compute(Some(old), Some(new)));
        app.old_content = Some(old.into());
        app.new_content = Some(new.into());
        app.rebuild_display();

        let backend = TestBackend::new(120, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();

        // Scan only the diff body (right of the 34-col file panel and the
        // 5-col line-number gutter), excluding known chrome colors.
        let buf = term.backend().buffer();
        let mut colors = std::collections::HashSet::new();
        for y in 1..buf.area.height - 1 {
            for x in 41..buf.area.width {
                let fg = buf[(x, y)].fg;
                let p = palette();
                if let Color::Rgb(..) = fg {
                    if fg != p.line_no && fg != p.divider {
                        colors.insert(format!("{fg:?}"));
                    }
                }
            }
        }
        assert!(
            colors.len() >= 3,
            "expected several syntax colors in the rendered diff, got {colors:?}"
        );
    }

    fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    /// A one-file local review of `old` → `new`, rendered at 120×20.
    fn render(app: &mut App) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(120, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        term.backend().buffer().clone()
    }

    /// The bug this whole feature exists for: on a light terminal the diff
    /// backgrounds used to stay near-black, so added and removed lines were
    /// dark slabs with dark syntax text on them. Render the same diff under
    /// both appearances and assert the tints actually follow.
    #[test]
    fn diff_backgrounds_follow_the_appearance() {
        let _guard = crate::theme::test_lock();
        let before = crate::theme::appearance();

        // The backgrounds present in the diff pane, ignoring the terminal
        // default (context rows paint no background of their own).
        let backgrounds = |app: &mut App| -> std::collections::HashSet<String> {
            let buf = render(app);
            let mut out = std::collections::HashSet::new();
            for y in 1..buf.area.height - 1 {
                for x in (app.file_panel_w + 1)..buf.area.width {
                    if let Color::Rgb(..) = buf[(x, y)].bg {
                        out.insert(format!("{:?}", buf[(x, y)].bg));
                    }
                }
            }
            out
        };

        let mut app = wide_app();
        app.diff = Some(FileDiff::compute(Some("a\nb\nc\n"), Some("a\nB\nc\n")));
        app.rebuild_display();

        crate::theme::set_appearance(crate::theme::Appearance::Dark);
        let dark = backgrounds(&mut app);
        crate::theme::set_appearance(crate::theme::Appearance::Light);
        let light = backgrounds(&mut app);
        crate::theme::set_appearance(before);

        for (label, want, got) in [
            ("dark", &crate::theme::DARK, &dark),
            ("light", &crate::theme::LIGHT, &light),
        ] {
            for (what, color) in [("added", want.added), ("removed", want.removed)] {
                assert!(
                    got.contains(&format!("{color:?}")),
                    "{label} render is missing the {what} background {color:?}; saw {got:?}"
                );
            }
        }
        assert!(
            dark.is_disjoint(&light),
            "the two appearances must not share any background: {dark:?} vs {light:?}"
        );
    }

    /// WCAG relative luminance, for the contrast audit below.
    fn rel_lum(c: Color) -> f64 {
        let (r, g, b) = match c {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => panic!("expected an explicit RGB color, got {c:?}"),
        };
        let ch = |v: u8| {
            let v = v as f64 / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b)
    }

    fn contrast(fg: Color, bg: Color) -> f64 {
        let (a, b) = (rel_lum(fg), rel_lum(bg));
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Sweep every screen in light mode and check that no drawn glyph
    /// disappears into its background. This is the audit that catches a
    /// color someone forgot to route through the palette — a stray
    /// `Color::White` foreground reads fine on black and vanishes on white.
    #[test]
    fn light_mode_text_stays_readable() {
        let _guard = crate::theme::test_lock();
        let before_appearance = crate::theme::appearance();
        let before_theme = highlight::current_theme();
        crate::theme::set_appearance(crate::theme::Appearance::Light);
        highlight::set_theme(highlight::DEFAULT_LIGHT_THEME);

        // What the terminal itself paints where loupe sets no background.
        let terminal_bg = Color::Rgb(255, 255, 255);

        let mut app = wide_app();
        app.local = false;
        app.diff = Some(FileDiff::compute(
            Some("fn main() {\n    let x = 1;\n}\n"),
            Some("fn main() {\n    let x = 42;\n    let s = \"hi\";\n}\n"),
        ));
        app.old_hl = highlight::highlight("t.rs", "fn main() {\n    let x = 1;\n}\n");
        app.new_hl = highlight::highlight(
            "t.rs",
            "fn main() {\n    let x = 42;\n    let s = \"hi\";\n}\n",
        );
        app.rebuild_display();

        // Colors the syntax theme produces are the theme author's call, not
        // loupe's — this audit is about the chrome, so collect them and
        // step over them.
        let mut syntax: std::collections::HashSet<String> = std::collections::HashSet::new();
        for source in [app.old_hl.clone(), app.new_hl.clone()] {
            for line in source {
                for (c, _) in line {
                    syntax.insert(format!("{c:?}"));
                }
            }
        }
        for theme in crate::highlight::THEMES {
            highlight::set_theme(theme.1);
            for line in highlight::highlight("sample.rs", crate::wizard::SAMPLE) {
                for (c, _) in line {
                    syntax.insert(format!("{c:?}"));
                }
            }
        }
        highlight::set_theme(highlight::DEFAULT_LIGHT_THEME);

        type Case = (&'static str, Box<dyn Fn(&mut App)>);
        let screens: Vec<Case> = vec![
            ("review", Box::new(|_: &mut App| {})),
            ("help", Box::new(|a: &mut App| a.overlay = Overlay::Help)),
            (
                "theme picker",
                Box::new(|a: &mut App| {
                    a.overlay = Overlay::None;
                    a.open_theme_picker();
                }),
            ),
            (
                "checkout prompt",
                Box::new(|a: &mut App| a.overlay = Overlay::CheckoutPrompt(7)),
            ),
            (
                "revert prompt",
                Box::new(|a: &mut App| {
                    a.overlay = Overlay::None;
                    a.checked_out = true;
                    a.ask_revert_file(0);
                    assert!(
                        matches!(a.overlay, Overlay::Revert(_)),
                        "the revert prompt must actually open: {}",
                        a.status
                    );
                }),
            ),
            (
                "comment overlay",
                Box::new(|a: &mut App| {
                    a.overlay = Overlay::None;
                    a.local = false;
                    a.diff_cursor = 1;
                    a.open_comment();
                    assert!(
                        matches!(a.overlay, Overlay::Comment(_)),
                        "the comment overlay must actually open: {}",
                        a.status
                    );
                }),
            ),
            (
                "the menu",
                Box::new(|a: &mut App| {
                    a.overlay = Overlay::None;
                    a.open_menu(110, 0);
                }),
            ),
            (
                "pr list",
                Box::new(|a: &mut App| {
                    a.overlay = Overlay::None;
                    a.screen = Screen::PrList;
                }),
            ),
        ];

        let mut worst: Vec<String> = Vec::new();
        for (name, setup) in &screens {
            setup(&mut app);
            let buf = render(&mut app);
            for y in 0..buf.area.height {
                for x in 0..buf.area.width {
                    let cell = &buf[(x, y)];
                    let sym = cell.symbol();
                    if sym.trim().is_empty() {
                        continue; // blanks carry no glyph to read
                    }
                    // Box drawing: structural rules, deliberately faint on
                    // both appearances.
                    if sym.chars().all(|c| ('\u{2500}'..='\u{257f}').contains(&c)) {
                        continue;
                    }
                    let fg = match cell.fg {
                        Color::Reset => continue, // the terminal's own default
                        other => other,
                    };
                    let bg = match cell.bg {
                        Color::Reset => terminal_bg,
                        other => other,
                    };
                    // Palette colors are all explicit RGB; the named ones
                    // that survive here would be the bug.
                    if !matches!(fg, Color::Rgb(..)) || !matches!(bg, Color::Rgb(..)) {
                        worst.push(format!(
                            "{name} ({x},{y}) {sym:?}: named color {fg:?} on {bg:?}"
                        ));
                        continue;
                    }
                    if syntax.contains(&format!("{fg:?}")) {
                        continue;
                    }
                    let ratio = contrast(fg, bg);
                    if ratio < 2.5 {
                        worst.push(format!(
                            "{name} ({x},{y}) {sym:?}: {fg:?} on {bg:?} is {ratio:.2}:1"
                        ));
                    }
                }
            }
        }

        crate::theme::set_appearance(before_appearance);
        highlight::set_theme(before_theme);
        worst.dedup();
        assert!(
            worst.is_empty(),
            "unreadable cells in light mode:\n  {}",
            worst.join("\n  ")
        );
    }

    fn wide_app() -> App {
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.checked_out = true;
        app.view = ViewMode::Inline;
        app.files = vec![ChangedFile {
            path: "wide.txt".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            previous: None,
        }];
        app.rebuild_entries();
        // A line far wider than the pane: "0123456789" repeated.
        let long = "0123456789".repeat(30);
        app.diff = Some(FileDiff::compute(Some("old\n"), Some(&format!("{long}\n"))));
        app.rebuild_display();
        app
    }

    /// The diff body scrolls sideways while the line-number gutter stays
    /// pinned, so wide lines can be read to the end.
    #[test]
    fn diff_body_scrolls_horizontally() {
        let mut app = wide_app();
        let buf = render(&mut app);
        // Find the added row (the wide one) by its "+" marker.
        let y = (1..19)
            .find(|y| row_text(&buf, *y).contains(" + 0123456789"))
            .expect("the added line is on screen");
        let before = row_text(&buf, y);
        // The change bar sits left of the line-number gutter; neither moves.
        let fixed = crate::app::REVERT_W as usize + 12 + app.file_panel_w as usize;
        let gutter: String = before.chars().take(fixed).collect();

        app.diff_hscroll = 10;
        let buf = render(&mut app);
        let after = row_text(&buf, y);
        assert_eq!(
            after.chars().take(fixed).collect::<String>(),
            gutter,
            "the line-number gutter must not move"
        );
        // Ten columns of a repeating 10-char pattern: the body looks the
        // same, but it is now showing the *second* repeat — so drop 3 more
        // and the text must differ from the unscrolled body by that shift.
        app.diff_hscroll = 3;
        let buf = render(&mut app);
        let shifted = row_text(&buf, y);
        assert_ne!(shifted, before);
        let body_start = (app.file_panel_w as usize) + 1 + crate::app::REVERT_W as usize + 12;
        // Trim the pane's right border off the tail of each slice.
        let trim = |s: &str| s.trim_end_matches(['│', ' ']).to_string();
        let orig_body = trim(&before.chars().skip(body_start + 3).collect::<String>());
        let new_body = trim(&shifted.chars().skip(body_start).collect::<String>());
        assert!(
            new_body.starts_with(&orig_body),
            "scrolling 3 columns should shift the body 3 columns:\n{orig_body:?}\n{new_body:?}"
        );
    }

    /// The keyboard cursor row is underlined (the same mark the editor uses
    /// for its cursor line) so it is findable without a mouse.
    #[test]
    fn cursor_row_is_underlined() {
        let mut app = wide_app();
        app.diff = Some(FileDiff::compute(
            Some("a\nb\nc\nd\n"),
            Some("a\nB\nc\nd\n"),
        ));
        app.rebuild_display();
        app.diff_scroll = 0;
        app.diff_cursor = 2;

        let buf = render(&mut app);
        // First body column: past the panel border and the change bar.
        let body = app.file_panel_w + 2 + crate::app::REVERT_W;
        let underlined: Vec<u16> = (1..8)
            .filter(|y| buf[(body, *y)].modifier.contains(Modifier::UNDERLINED))
            .collect();
        assert_eq!(underlined.len(), 1, "exactly one row carries the cursor");

        // Moving the cursor moves the underline with it.
        app.diff_cursor = 4;
        let buf = render(&mut app);
        let moved: Vec<u16> = (1..8)
            .filter(|y| buf[(body, *y)].modifier.contains(Modifier::UNDERLINED))
            .collect();
        assert_eq!(moved.len(), 1);
        assert_ne!(moved, underlined, "the underline follows the cursor");
    }

    /// The revert affordances have to be *visible*, and clickable where they
    /// look clickable: a ↺ beside the first row of every changed section,
    /// one at the end of every file row, and neither on a review that cannot
    /// change anything.
    #[test]
    fn revert_markers_are_drawn_in_the_diff_and_the_file_list() {
        let mut app = wide_app();
        app.view = ViewMode::SideBySide;
        app.diff = Some(FileDiff::compute(
            Some("a\nb\nc\nd\ne\nf\ng\n"),
            Some("a\nB\nc\nd\ne\nF1\nF2\ng\n"),
        ));
        app.collapse_unchanged = false;
        app.rebuild_display();

        // Where each ↺ landed, split by panel.
        let marks = |buf: &ratatui::buffer::Buffer, app: &App| -> (usize, usize) {
            let (mut files, mut diff) = (0, 0);
            for y in 0..buf.area.height {
                for x in 0..buf.area.width {
                    if buf[(x, y)].symbol() == "↺" {
                        if x < app.file_panel_w {
                            files += 1;
                        } else {
                            diff += 1;
                        }
                    }
                }
            }
            (files, diff)
        };

        let buf = render(&mut app);
        let (files, diff) = marks(&buf, &app);
        assert_eq!(files, 1, "one ↺ per file row");
        assert_eq!(diff, 2, "one ↺ per changed section");
        // The diff's markers sit in the change bar, left of the line numbers.
        let bar_x = app.layout.diff.x;
        let in_bar = (0..buf.area.height)
            .filter(|y| buf[(bar_x, *y)].symbol() == "↺")
            .count();
        assert_eq!(in_bar, 2, "the markers are in the change bar column");

        // A PR opened read-only has no working tree to put back: the columns
        // go back to the code.
        app.local = false;
        app.checked_out = false;
        let buf = render(&mut app);
        assert_eq!(marks(&buf, &app), (0, 0));
    }

    /// Local review shows a staging column: [+] to stage, [±] partly staged,
    /// [✓] fully staged — and the panel title counts staged files.
    #[test]
    fn local_file_panel_shows_staging_icons() {
        let mut app = wide_app();
        app.tree_view = false;
        app.files = ["a.rs", "b.rs", "c.rs"]
            .iter()
            .map(|p| ChangedFile {
                path: (*p).into(),
                status: "modified".into(),
                additions: 1,
                deletions: 1,
                previous: None,
            })
            .collect();
        app.rebuild_entries();
        app.stage.insert("a.rs".into(), StageState::Unstaged);
        app.stage.insert("b.rs".into(), StageState::Partial);
        app.stage.insert("c.rs".into(), StageState::Staged);

        let buf = render(&mut app);
        let panel: String = (1..5).map(|y| row_text(&buf, y)).collect();
        assert!(
            panel.contains("[+] M a.rs"),
            "unstaged file offers [+]: {panel:?}"
        );
        assert!(
            panel.contains("[±] M b.rs"),
            "partly staged file shows [±]: {panel:?}"
        );
        assert!(
            panel.contains("[✓] M c.rs"),
            "fully staged file shows [✓]: {panel:?}"
        );
        assert!(
            row_text(&buf, 1).contains("1/3 staged"),
            "title counts staged files"
        );

        // A pull-request review keeps the viewed checkbox.
        app.local = false;
        let buf = render(&mut app);
        let panel: String = (1..5).map(|y| row_text(&buf, y)).collect();
        assert!(
            panel.contains("[ ] M a.rs"),
            "PR review shows the viewed checkbox: {panel:?}"
        );
        assert!(
            !panel.contains("[+]"),
            "no staging icons on a PR: {panel:?}"
        );
        assert!(row_text(&buf, 1).contains("0/3 ✓"));
    }

    /// The file panel width follows `file_panel_w`, and the divider hit-area
    /// tracks it.
    #[test]
    fn file_panel_resizes() {
        let mut app = wide_app();
        let buf = render(&mut app);
        let row = row_text(&buf, 3);
        assert_eq!(
            row.chars().nth(crate::app::FILE_PANEL_DEFAULT as usize - 1),
            Some('│'),
            "file panel ends at the default width"
        );
        assert_eq!(app.layout.divider.x, crate::app::FILE_PANEL_DEFAULT - 1);

        app.file_panel_w = 50;
        let buf = render(&mut app);
        let row = row_text(&buf, 3);
        assert_eq!(
            row.chars().nth(49),
            Some('│'),
            "panel border moved with the width"
        );
        assert_eq!(app.layout.divider.x, 49);
        assert!(app.layout.diff.x > 49);

        // Absurd width: clamped so the diff pane survives.
        app.file_panel_w = 250;
        render(&mut app);
        assert_eq!(app.file_panel_w, 120 - crate::app::DIFF_MIN_W);
        assert!(app.layout.diff.width >= crate::app::DIFF_MIN_W - 2);
    }

    /// Local-changes review: the top bar shows the LOCAL badge + branch and
    /// offers no Comment button (there is no PR to comment on).
    #[test]
    fn local_mode_topbar_hides_comment() {
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.local_branch = Some("feature/x".into());
        app.checked_out = true;
        app.files = vec![ChangedFile {
            path: "test.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            previous: None,
        }];
        app.rebuild_entries();
        app.diff = Some(FileDiff::compute(Some("a\n"), Some("b\n")));
        app.rebuild_display();

        let backend = TestBackend::new(120, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();

        let buf = term.backend().buffer();
        let mut top = String::new();
        for x in 0..buf.area.width {
            top.push_str(buf[(x, 0)].symbol());
        }
        assert!(
            top.contains("LOCAL"),
            "top bar should carry the LOCAL badge: {top:?}"
        );
        assert!(
            top.contains("feature/x"),
            "top bar should show the branch: {top:?}"
        );
        assert!(
            !top.contains("Comment"),
            "no Comment button in local mode: {top:?}"
        );
        assert!(
            top.contains("Edit"),
            "Edit stays available in local mode: {top:?}"
        );
        assert!(
            top.contains("☰"),
            "everything else is behind the menu: {top:?}"
        );
    }

    /// The PR badge is a click target, so a right-click on it can copy
    /// the PR link. The rect has to cover the badge text and nothing else.
    #[test]
    fn the_pr_badge_is_a_click_target() {
        let mut app = App::new(crate::app::LaunchMode::Pr, None);
        app.screen = Screen::Review;
        app.pr = Some(crate::github::PrDetail {
            id: "node".into(),
            number: 3,
            title: "a change".into(),
            head_ref_oid: "a".repeat(40),
            base_ref_oid: "b".repeat(40),
            base_ref_name: "main".into(),
            head_ref_name: "feat".into(),
            url: "https://github.com/o/r/pull/3".into(),
        });
        app.checked_out = true;
        app.files = vec![ChangedFile {
            path: "test.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            previous: None,
        }];
        app.rebuild_entries();
        app.diff = Some(FileDiff::compute(Some("a\n"), Some("b\n")));
        app.rebuild_display();

        let backend = TestBackend::new(120, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();

        let badge = app.layout.badge;
        assert_eq!(badge.y, 0, "the badge is on the top bar");
        let text: String = (badge.x..badge.x + badge.width)
            .map(|x| buf[(x, 0)].symbol())
            .collect();
        assert_eq!(text, " PR #3 ", "the rect covers the badge: {text:?}");
        assert_eq!(
            app.pr_url().as_deref(),
            Some("https://github.com/o/r/pull/3"),
            "the link comes from `gh pr view --json url`"
        );
    }

    /// The ⇄ swap moved off the toolbar and into the ☰ menu, which names
    /// the side it would take you to.
    #[test]
    fn the_menu_offers_the_swap_to_the_other_side() {
        let mut app = App::new(crate::app::LaunchMode::Pr, None);
        app.screen = Screen::Review;
        app.pr = Some(crate::github::PrDetail {
            id: "node".into(),
            number: 3,
            title: "a change".into(),
            head_ref_oid: "a".repeat(40),
            base_ref_oid: "b".repeat(40),
            base_ref_name: "main".into(),
            head_ref_name: "feat".into(),
            url: "https://github.com/o/r/pull/3".into(),
        });
        app.checked_out = true;
        app.files = vec![ChangedFile {
            path: "test.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            previous: None,
        }];
        app.rebuild_entries();
        app.diff = Some(FileDiff::compute(Some("a\n"), Some("b\n")));
        app.rebuild_display();

        let backend = TestBackend::new(120, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();

        let buf = term.backend().buffer();
        let _ = buf;
        // The toolbar no longer carries it; the menu does.
        app.open_menu(110, 0);
        let backend = TestBackend::new(120, 44);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();
        let all: String = (0..buf.area.height).map(|y| row_text(&buf, y)).collect();
        assert!(
            all.contains("Swap to local changes"),
            "the menu names the other side: {all:?}"
        );
        assert!(
            app.layout
                .buttons
                .iter()
                .any(|(_, id)| matches!(id, ButtonId::MenuRow(_))),
            "every menu line is a click target"
        );
    }

    /// The ☰ menu draws its headings, its on/off switches and the key
    /// that does the same thing outside it, and it stays on screen.
    #[test]
    fn the_menu_draws_grouped_lines_with_their_keys() {
        let _guard = crate::theme::test_lock();
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.local_branch = Some("feature/x".into());
        app.files = vec![ChangedFile {
            path: "test.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            previous: None,
        }];
        app.rebuild_entries();
        app.diff = Some(FileDiff::compute(Some("a\n"), Some("b\n")));
        app.rebuild_display();

        // Open it where the ☰ button is, at the right edge of the top bar.
        let buf = render(&mut app);
        let menu_btn = app
            .layout
            .buttons
            .iter()
            .find(|(_, id)| *id == ButtonId::Menu)
            .map(|(r, _)| *r)
            .expect("the top bar draws ☰");
        let _ = buf;
        app.open_menu(menu_btn.x, menu_btn.y);
        let buf = render(&mut app);
        let all: String = (0..buf.area.height).map(|y| row_text(&buf, y)).collect();

        assert!(all.contains("VIEW"), "headings are drawn: {all:?}");
        assert!(all.contains("ACTIONS"), "headings are drawn: {all:?}");
        assert!(all.contains("Search the repository"));
        // The switches say what they are set to.
        assert!(all.contains("Fold unchanged lines"), "{all:?}");
        assert!(all.contains("●"), "a switch shows its state: {all:?}");
        // Every drawn line is clickable.
        assert!(app
            .layout
            .buttons
            .iter()
            .any(|(_, id)| matches!(id, ButtonId::MenuRow(_))));
        // The panel is pulled left until it fits, never off the edge.
        for (r, id) in &app.layout.buttons {
            if matches!(id, ButtonId::MenuRow(_)) {
                assert!(
                    r.x + r.width <= buf.area.width,
                    "a menu row ran off the screen: {r:?}"
                );
            }
        }
        // Twenty rows cannot hold it, so it says there is more, and it
        // leaves the ☰ it came from visible.
        assert!(all.contains("scroll"), "the cut-off menu says so: {all:?}");
        assert!(
            row_text(&buf, 0).contains("☰"),
            "the top bar is never covered: {:?}",
            row_text(&buf, 0)
        );

        // A terminal with room shows the whole menu and drops the hint.
        let backend = TestBackend::new(120, 44);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();
        let all: String = (0..buf.area.height).map(|y| row_text(&buf, y)).collect();
        assert!(all.contains("SETTINGS"), "{all:?}");
        assert!(all.contains("Quit"), "the last line is reachable: {all:?}");
        assert!(!all.contains("scroll"), "nothing is hidden: {all:?}");
    }

    /// The finder has to render its results *and* record a hit area per
    /// row — this is a mouse-first tool, and a list you can only reach
    /// with the keyboard is half a feature.
    #[test]
    fn finder_overlay_lists_results_and_is_clickable() {
        let _guard = crate::theme::test_lock();
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.files = vec![
            ChangedFile {
                path: "src/app.rs".into(),
                status: "modified".into(),
                additions: 1,
                deletions: 0,
                previous: None,
            },
            ChangedFile {
                path: "src/ui/render.rs".into(),
                status: "modified".into(),
                additions: 1,
                deletions: 0,
                previous: None,
            },
        ];
        app.rebuild_entries();
        app.diff = Some(FileDiff::compute(Some("a\n"), Some("b\n")));
        app.rebuild_display();
        app.open_finder(crate::app::FinderMode::Files);

        let buf = render(&mut app);
        let all: String = (0..buf.area.height).map(|y| row_text(&buf, y)).collect();
        assert!(all.contains("Go to file"), "the overlay names its mode");
        assert!(all.contains("src/app.rs"), "results are listed: {all:?}");
        assert!(all.contains("@ Symbols"), "the mode tabs are drawn");
        assert!(
            app.layout
                .buttons
                .iter()
                .any(|(_, id)| matches!(id, ButtonId::FinderRow(0))),
            "every visible row is a click target"
        );

        // The top bar offers the same thing to the mouse.
        assert!(row_text(&buf, 0).contains("Find"));
    }

    /// A search you can\'t see is not a search: the matched text carries
    /// the highlight background, on top of whatever the row already had.
    #[test]
    fn search_matches_are_highlighted_in_the_diff() {
        let _guard = crate::theme::test_lock();
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.files = vec![ChangedFile {
            path: "test.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 0,
            previous: None,
        }];
        app.rebuild_entries();
        let old = "let alpha = 1;\n";
        let new = "let alpha = 2;\n";
        app.diff = Some(FileDiff::compute(Some(old), Some(new)));
        app.old_content = Some(old.into());
        app.new_content = Some(new.into());
        app.rebuild_display();

        let plain = render(&mut app);
        app.find.query = "alpha".into();
        app.recompute_matches();
        assert_eq!(app.find.rows.len(), 1);
        let marked = render(&mut app);

        let hit = palette().matched;
        let count = |buf: &ratatui::buffer::Buffer| {
            let mut n = 0;
            for y in 0..buf.area.height {
                for x in 0..buf.area.width {
                    if buf[(x, y)].bg == hit {
                        n += 1;
                    }
                }
            }
            n
        };
        assert_eq!(count(&plain), 0, "nothing is highlighted before a search");
        // "alpha" on both sides of a side-by-side view.
        assert_eq!(count(&marked), 10, "both copies of the match are marked");
    }

    /// The right-click menu opens at the pointer, names the row it is
    /// about, and every line of it is clickable.
    #[test]
    fn the_path_menu_draws_at_the_pointer() {
        let _guard = crate::theme::test_lock();
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.files = vec![ChangedFile {
            path: "src/app.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            previous: None,
        }];
        app.rebuild_entries();
        let menu = || {
            Box::new(crate::app::PathMenu {
                path: "src/app.rs".into(),
                is_dir: false,
                items: vec![
                    crate::app::PathMenuItem {
                        key: 'r',
                        label: "Copy relative path",
                        text: "src/app.rs".into(),
                    },
                    crate::app::PathMenuItem {
                        key: 'f',
                        label: "Copy full path",
                        text: "/repo/src/app.rs".into(),
                    },
                ],
                sel: 0,
                anchor: (4, 5),
            })
        };

        app.overlay = Overlay::PathMenu(menu());
        let buf = render(&mut app);
        assert!(
            row_text(&buf, 5).contains("src/app.rs"),
            "the title names the row: {:?}",
            row_text(&buf, 5)
        );
        assert!(
            row_text(&buf, 6).contains("Copy relative path"),
            "{:?}",
            row_text(&buf, 6)
        );
        assert!(row_text(&buf, 7).contains("Copy full path"));
        let rows = app
            .layout
            .buttons
            .iter()
            .filter(|(_, id)| matches!(id, ButtonId::PathMenuRow(_)))
            .count();
        assert_eq!(rows, 2, "each line is clickable");

        // A row near the bottom edge flips the menu above the pointer
        // rather than letting it fall off the screen.
        let mut low = menu();
        low.anchor = (4, 19);
        app.overlay = Overlay::PathMenu(low);
        let buf = render(&mut app);
        assert!(
            row_text(&buf, 17).contains("Copy relative path"),
            "{:?}",
            row_text(&buf, 17)
        );
    }

    /// Copying has to be reachable with the mouse too — the whole reason
    /// it needs building is that mouse reporting took the terminal's own
    /// selection away.
    #[test]
    fn selecting_lines_offers_a_copy_button() {
        let _guard = crate::theme::test_lock();
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.files = vec![ChangedFile {
            path: "test.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            previous: None,
        }];
        app.rebuild_entries();
        app.diff = Some(FileDiff::compute(Some("a\n"), Some("b\n")));
        app.rebuild_display();

        // Nothing selected: the row is crowded enough without an inert
        // button on it, and `y` still works.
        let buf = render(&mut app);
        assert!(!row_text(&buf, 0).contains("Copy"));

        // Select a line and it appears.
        app.selection = Some(crate::diff::Selection::lines(Side::Right, 1, 1));
        let buf = render(&mut app);
        assert!(
            row_text(&buf, 0).contains("Copy"),
            "{:?}",
            row_text(&buf, 0)
        );
        assert!(app
            .layout
            .buttons
            .iter()
            .any(|(_, id)| matches!(id, ButtonId::Copy)));
    }

    /// A crowded top bar must not paint over the branch or PR title.
    #[test]
    fn a_narrow_top_bar_drops_buttons_instead_of_covering_the_title() {
        let _guard = crate::theme::test_lock();
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.local_branch = Some("a-long-branch-name".into());
        app.files = vec![ChangedFile {
            path: "test.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            previous: None,
        }];
        app.rebuild_entries();
        app.diff = Some(FileDiff::compute(Some("a\n"), Some("b\n")));
        app.rebuild_display();

        let backend = TestBackend::new(60, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();
        let top = row_text(&buf, 0);
        assert!(top.contains("LOCAL"), "the badge survives: {top:?}");
        assert!(
            top.contains("a-long-branch"),
            "the branch is still readable: {top:?}"
        );
        assert!(top.contains("☰"), "the last button is kept: {top:?}");
    }

    /// The highlight has to show exactly what a copy would take —
    /// otherwise the selection is a guess and the clipboard is a surprise.
    #[test]
    fn a_character_selection_highlights_only_those_characters() {
        let _guard = crate::theme::test_lock();
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.files = vec![ChangedFile {
            path: "a.ts".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            previous: None,
        }];
        app.rebuild_entries();
        let old = "const alpha = 1;\nconst beta = 2;\n";
        let new = "const alpha = 1;\nconst BETA = 2;\n";
        app.old_content = Some(old.into());
        app.new_content = Some(new.into());
        app.diff = Some(FileDiff::compute(Some(old), Some(new)));
        app.collapse_unchanged = false;
        app.rebuild_display();

        let selected = |buf: &ratatui::buffer::Buffer, y: u16| -> usize {
            let bg = palette().selected;
            (0..buf.area.width)
                .filter(|x| buf[(*x, y)].bg == bg)
                .count()
        };

        // A whole-line selection still paints the whole row, padding
        // included — unchanged from before.
        app.selection = Some(crate::diff::Selection::lines(Side::Left, 2, 2));
        let buf = render(&mut app);
        // Top bar, pane border, file line 1, file line 2.
        let line2 = 3u16;
        assert!(
            selected(&buf, line2) > 40,
            "a line selection fills its half of the row"
        );

        // Five characters of that line: five cells, and they are the right
        // five ("const" at the start of the old side).
        app.selection = Some(crate::diff::Selection {
            side: Side::Left,
            anchor: crate::diff::Pos::new(2, 0),
            end: crate::diff::Pos::new(2, 5),
            linewise: false,
        });
        let buf = render(&mut app);
        assert_eq!(selected(&buf, line2), 5, "exactly the dragged characters");
        let bg = palette().selected;
        let text: String = (0..buf.area.width)
            .filter(|x| buf[(*x, line2)].bg == bg)
            .map(|x| buf[(x, line2)].symbol())
            .collect();
        assert_eq!(text, "const");

        // And nothing at all on the other side of the diff.
        let right_half: usize = (buf.area.width / 2..buf.area.width)
            .filter(|x| buf[(*x, line2)].bg == bg)
            .count();
        assert_eq!(right_half, 0, "the new side is not selected");
    }

    // ------------------------------------------------------------ blame

    /// A blame whose line *n* is claimed by `(author, sha, age_days)`.
    fn fake_blame(lines: &[(&str, &str, i64)]) -> blame::Blame {
        let now = blame::now();
        blame::Blame {
            lines: lines
                .iter()
                .map(|(author, sha, days)| blame::BlameLine {
                    commit: Arc::new(blame::Commit {
                        sha: (*sha).into(),
                        author: (*author).into(),
                        author_email: format!("{}@test", author.to_lowercase()),
                        author_time: now - days * 60 * 60 * 24,
                        summary: format!("work by {author}"),
                        pr: None,
                    }),
                })
                .collect(),
        }
    }

    /// An app with the blame pane on and both sides blamed, over a
    /// one-line modification: inline row 0 is the unchanged first line,
    /// row 1 is the removed old line, row 2 is the added new one.
    fn blame_app() -> App {
        let mut app = wide_app();
        app.view = ViewMode::Inline;
        app.collapse_unchanged = false;
        app.diff = Some(FileDiff::compute(Some("a\nb\n"), Some("a\nB\n")));
        app.rebuild_display();
        app.blame_on = true;
        app.blame_new = Some(fake_blame(&[("Ann", "aaa", 400), ("Bob", "bbb", 0)]));
        app.blame_old = Some(fake_blame(&[("Ann", "aaa", 400), ("Cid", "ccc", 40)]));
        app
    }

    /// The pane sits between the file panel and the diff, and each row
    /// names the commit behind the diff row beside it — the new side
    /// where there is one, the old side for a removed line.
    #[test]
    fn the_blame_pane_lines_up_with_the_diff() {
        let mut app = blame_app();
        let buf = render(&mut app);

        let (fl, bl, df) = (app.layout.file_list, app.layout.blame, app.layout.diff);
        assert!(bl.width > 0, "the pane is drawn");
        assert!(fl.x + fl.width <= bl.x, "it sits right of the file panel");
        assert!(bl.x + bl.width <= df.x, "and left of the diff");

        let pane_row = |y: u16| -> String {
            (bl.x..bl.x + bl.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim()
                .to_string()
        };
        assert!(pane_row(bl.y).contains("Ann"), "row 0: the unchanged line");
        assert!(
            pane_row(bl.y + 1).contains("Cid"),
            "row 1 is a removed line, so it is blamed on the old side"
        );
        assert!(pane_row(bl.y + 2).contains("Bob"), "row 2: the added line");

        // The ages fit alongside the names.
        assert!(pane_row(bl.y).contains("1y"), "Ann's commit is a year old");
        assert!(pane_row(bl.y + 2).contains("now"), "Bob's is today");
    }

    /// Turning the pane off gives its columns back to the diff, and
    /// leaves no hit target behind for a click to land on.
    #[test]
    fn hiding_the_blame_pane_gives_the_diff_its_columns_back() {
        let mut app = blame_app();
        render(&mut app);
        let with = app.layout.diff.width;
        assert!(app.layout.blame.width > 0);
        assert!(app.layout.blame_divider.width > 0);

        app.blame_on = false;
        render(&mut app);
        assert_eq!(app.layout.blame.width, 0, "no pane");
        assert_eq!(app.layout.blame_divider.width, 0, "and no second seam");
        assert_eq!(
            app.layout.diff.width,
            with + app.blame_w,
            "every column the pane had goes back to the diff"
        );
    }

    /// A run of rows sharing one commit names it once. Repeating a name
    /// down a block says nothing and hides where the block ends.
    #[test]
    fn a_run_of_one_commit_is_named_once() {
        let mut app = wide_app();
        app.view = ViewMode::Inline;
        app.collapse_unchanged = false;
        app.diff = Some(FileDiff::compute(Some("a\nb\nc\n"), Some("a\nb\nc\n")));
        app.rebuild_display();
        app.blame_on = true;
        app.blame_new = Some(fake_blame(&[
            ("Ann", "aaa", 1),
            ("Ann", "aaa", 1),
            ("Bob", "bbb", 1),
        ]));
        let buf = render(&mut app);
        let bl = app.layout.blame;
        let names = (bl.y..bl.y + 3)
            .map(|y| {
                (bl.x..bl.x + bl.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(names[0].contains("Ann"));
        assert!(!names[1].contains("Ann"), "the run is named once");
        assert!(names[2].contains("Bob"), "a new commit is named again");
        // The heat bar keeps drawing, so the ramp stays continuous.
        assert!(names
            .iter()
            .all(|r| r.starts_with('█') || r.starts_with('▓')));
    }

    /// `layout.diff` is the rect *inside* the pane's borders, so the
    /// floor the layout enforces shows up two columns narrower here.
    fn diff_pane_w(app: &App) -> u16 {
        app.layout.diff.width + 2
    }

    /// Whatever either divider is dragged to, three panes may never take
    /// the diff below the width it needs to be readable.
    #[test]
    fn three_panes_never_starve_the_diff() {
        for width in [80u16, 100, 140] {
            let mut app = blame_app();
            let backend = TestBackend::new(width, 20);
            let mut term = Terminal::new(backend).unwrap();
            term.draw(|f| draw(f, &mut app)).unwrap();
            // Both dividers dragged as far right as they go.
            app.file_panel_w = app.clamp_panel_w(u16::MAX);
            app.blame_w = app.clamp_blame_w(u16::MAX);
            term.draw(|f| draw(f, &mut app)).unwrap();
            assert!(
                diff_pane_w(&app) >= crate::app::DIFF_MIN_W,
                "at {width} columns the diff pane kept {}",
                diff_pane_w(&app)
            );
            // …and as far left.
            app.file_panel_w = app.clamp_panel_w(0);
            app.blame_w = app.clamp_blame_w(0);
            term.draw(|f| draw(f, &mut app)).unwrap();
            assert!(diff_pane_w(&app) >= crate::app::DIFF_MIN_W);
            assert!(app.file_panel_w >= crate::app::FILE_PANEL_MIN);
            assert!(app.blame_w >= crate::app::BLAME_MIN);
            assert!(app.layout.blame.width > 0, "all three still fit");
        }
    }

    /// A terminal too narrow for three panes shows two, rather than a
    /// diff too thin to read.
    #[test]
    fn a_narrow_terminal_drops_the_blame_pane_instead_of_the_diff() {
        let mut app = blame_app();
        let backend = TestBackend::new(50, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        assert_eq!(app.layout.blame.width, 0, "the pane stands down");
        assert!(app.blame_on, "but the switch stays where the reader put it");
        assert!(diff_pane_w(&app) >= crate::app::DIFF_MIN_W);
    }

    /// A truncated pull request number is a link to the wrong pull
    /// request, which is worse than no link at all.
    #[test]
    fn a_pull_request_number_is_never_truncated() {
        assert_eq!(pr_label(7), "#7");
        assert_eq!(pr_label(14260), "#14260");
        assert_eq!(pr_label(999999), "#999999");
        assert_eq!(pr_label(999999).chars().count(), PR_W);
        assert_eq!(pr_label(1000000), "#…", "too wide to state honestly");
    }

    /// The pane shows real numbers in full at its default width.
    #[test]
    fn the_pane_shows_a_five_digit_number_in_full() {
        let mut app = blame_app();
        app.blame_prs.insert(
            app.blame_new.as_ref().unwrap().at(1).unwrap().sha.clone(),
            crate::github::PrRef {
                number: 14260,
                title: "Support the thing".into(),
                url: "https://github.com/cli/cli/pull/14260".into(),
            },
        );
        let buf = render(&mut app);
        let bl = app.layout.blame;
        let row: String = (bl.x..bl.x + bl.width)
            .map(|x| buf[(x, bl.y)].symbol())
            .collect();
        assert!(row.contains("#14260"), "{row:?}");
    }
}

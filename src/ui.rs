//! All rendering. Every clickable region drawn here is recorded into
//! `app.layout` so the mouse handlers can hit-test against it.

use crate::app::{App, ButtonId, FileEntry, Overlay, Screen, ViewMode};
use crate::diff::{DisplayEntry, Row, RowKind, Side, TAB_WIDTH};
use crate::gitops::StageState;
use crate::highlight::HlLine;
use crate::theme::palette;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthChar;

/// Width of the Tree/Flat toggle drawn on the file panel's top border.
const TOGGLE_W: usize = 13;

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
        Overlay::Help => draw_help(f, area),
        Overlay::ThemePicker(_) => draw_theme_picker(f, app, area),
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
fn buttons_right(f: &mut Frame, app: &mut App, area: Rect, buttons: &[(&str, ButtonId, bool)]) {
    let total: u16 = buttons
        .iter()
        .map(|(l, _, _)| {
            l.chars()
                .map(|c| c.width().unwrap_or(0) as u16)
                .sum::<u16>()
                + 3
        })
        .sum();
    let p = palette();
    let mut x = area.x + area.width.saturating_sub(total);
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
        &[
            ("⎇ Local changes", ButtonId::LocalChanges, false),
            ("⟳ Refresh", ButtonId::Refresh, false),
            ("🎨 Theme", ButtonId::Theme, false),
            ("? Help", ButtonId::Help, false),
        ],
    );
}

fn draw_topbar_review(f: &mut Frame, app: &mut App, area: Rect) {
    // Badge + title: "PR #N — <title>" for a PR review, "LOCAL — <branch>,
    // uncommitted changes" when reviewing the working tree.
    let p = palette();
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
            tail_truncate(&title, (area.width / 3) as usize),
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(note, Style::default().fg(p.dim)),
    ]);
    f.render_widget(Paragraph::new(left), area);

    if app.editor.is_some() {
        buttons_right(
            f,
            app,
            area,
            &[
                ("💾 Save", ButtonId::EditorSave, true),
                ("✕ Close", ButtonId::EditorClose, false),
                ("? Help", ButtonId::Help, false),
            ],
        );
    } else {
        let has_sel = app.selection.is_some();
        let mut buttons = vec![
            (
                "◫ Split",
                ButtonId::ViewSplit,
                app.view == ViewMode::SideBySide,
            ),
            (
                "≡ Inline",
                ButtonId::ViewInline,
                app.view == ViewMode::Inline,
            ),
            ("⇕ Fold", ButtonId::FoldToggle, app.collapse_unchanged),
            ("✎ Edit", ButtonId::Edit, false),
        ];
        // No PR — nothing to comment on.
        if !app.local {
            buttons.push(("💬 Comment", ButtonId::Comment, has_sel));
        }
        // The other side of the PR ⇄ local toggle (also the ` key).
        let swap = if app.local { "⇄ PR" } else { "⇄ Local" };
        buttons.push((swap, ButtonId::SwapView, false));
        buttons.push(("🎨", ButtonId::Theme, false));
        buttons.push(("← PRs", ButtonId::BackToPrs, false));
        buttons.push(("? Help", ButtonId::Help, false));
        buttons_right(f, app, area, &buttons);
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
    // The panel width is user-set; re-clamp here so a terminal resize can
    // never leave the diff pane starved.
    let fw = app.clamp_panel_w(app.file_panel_w);
    app.file_panel_w = fw;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(fw),
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

    if let Some(editor) = &mut app.editor {
        editor.render(f, cols[1], true);
    } else {
        draw_diff(f, app, cols[1]);
    }
    draw_divider_grip(f, app);
}

/// A grip on the divider so it reads as draggable: a few heavy border
/// cells at mid-height, and the whole seam accented while it is being
/// dragged.
fn draw_divider_grip(f: &mut Frame, app: &App) {
    let d = app.layout.divider;
    if d.width == 0 || d.height < 3 {
        return;
    }
    let p = palette();
    let style = Style::default().fg(if app.resizing() {
        p.divider_active
    } else {
        p.divider
    });
    let (top, height) = if app.resizing() {
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
                let name_w =
                    (inner.width as usize).saturating_sub(indent + 6 + counts.chars().count());
                let name_t = tail_truncate(name, name_w);
                let pad = name_w.saturating_sub(disp_width(&name_t));
                let base = if selected {
                    Style::default().bg(p.row).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let name_fg = if done { p.viewed } else { p.text };
                lines.push(Line::from(vec![
                    Span::styled(" ".repeat(indent), base),
                    Span::styled(format!("{cb} "), base.patch(cb_style)),
                    Span::styled(format!("{sc} "), base.fg(sc_color)),
                    Span::styled(format!("{name_t}{}", " ".repeat(pad)), base.fg(name_fg)),
                    Span::styled(counts, base.fg(p.dim)),
                ]));
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
fn hl_body<'a>(
    text: &str,
    hl: Option<&HlLine>,
    skip: usize,
    width: usize,
    base: Style,
    fallback_fg: Color,
) -> Vec<Span<'a>> {
    let mut spans: Vec<Span> = Vec::new();
    // `col` counts columns of the *whole* line (including scrolled-off
    // ones); `w` counts columns actually emitted.
    let mut col = 0usize;
    let mut w = 0usize;
    let mut push_seg = |seg: &str, fg: Color, spans: &mut Vec<Span<'a>>| {
        if w >= width {
            return;
        }
        let mut out = String::new();
        for ch in seg.chars() {
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
        if !out.is_empty() {
            spans.push(Span::styled(out, base.fg(fg)));
        }
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

fn cell<'a>(
    row: &Row,
    side: Side,
    skip: usize,
    width: usize,
    selected: bool,
    cursor: bool,
    hl: Option<&HlLine>,
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
            let bg = if selected {
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
            if selected {
                base = base.add_modifier(Modifier::BOLD);
            }
            if cursor {
                ln_style = ln_style.add_modifier(Modifier::UNDERLINED);
                base = base.add_modifier(Modifier::UNDERLINED);
            }
            let mut spans = vec![Span::styled(ln_str, ln_style)];
            spans.extend(hl_body(t, hl, skip, body_w, base, p.text));
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

    let is_selected = |side: Side, ln: Option<usize>| -> bool {
        match (app.selection, ln) {
            (Some(sel), Some(n)) => sel.contains(side, n),
            _ => false,
        }
    };

    let mut lines: Vec<Line> = Vec::with_capacity(h);
    for (n, entry) in app.display.iter().skip(app.diff_scroll).take(h).enumerate() {
        // The keyboard cursor row, underlined the way the editor marks its
        // own cursor line.
        let cur = app.diff_scroll + n == app.diff_cursor;
        match entry {
            DisplayEntry::Fold { count, .. } => {
                lines.push(fold_line(inner.width as usize, *count, cur));
            }
            DisplayEntry::Unfold { count, .. } => {
                lines.push(unfold_line(inner.width as usize, *count, cur));
            }
            DisplayEntry::Line(i) if sbs => {
                let row = &diff.rows[*i];
                let lw = (inner.width as usize).saturating_sub(1) / 2;
                let rw = (inner.width as usize).saturating_sub(1) - lw;
                let lsel = is_selected(Side::Left, row.old_ln)
                    && matches!(row.kind, RowKind::Removed | RowKind::Modified);
                let rsel = is_selected(Side::Right, row.new_ln);
                let lhl = row.old_ln.and_then(|n| app.old_hl.get(n - 1));
                let rhl = row.new_ln.and_then(|n| app.new_hl.get(n - 1));
                let mut spans = cell(row, Side::Left, hskip, lw, lsel, cur, lhl);
                spans.push(Span::styled("│", Style::default().fg(p.divider)));
                spans.extend(cell(row, Side::Right, hskip, rw, rsel, cur, rhl));
                lines.push(Line::from(spans));
            }
            DisplayEntry::Line(i) => {
                let w = inner.width as usize;
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
                let sel = match side {
                    Side::Left => is_selected(Side::Left, row.old_ln),
                    Side::Right => is_selected(Side::Right, row.new_ln),
                };
                let hl = match side {
                    Side::Left => row.old_ln.and_then(|n| app.old_hl.get(n - 1)),
                    Side::Right => row.new_ln.and_then(|n| app.new_hl.get(n - 1)),
                };
                let bg = if sel {
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
                if sel {
                    base = base.add_modifier(Modifier::BOLD);
                }
                if cur {
                    gs = gs.add_modifier(Modifier::UNDERLINED);
                    base = base.add_modifier(Modifier::UNDERLINED);
                }
                let mut spans = vec![Span::styled(gutter, gs)];
                spans.extend(hl_body(text.unwrap_or(""), hl, hskip, body_w, base, p.text));
                lines.push(Line::from(spans));
            }
        }
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
        &[
            ("Checkout & review (c)", ButtonId::CheckoutYes, true),
            ("Review only (o)", ButtonId::CheckoutReviewOnly, false),
            ("Cancel (Esc)", ButtonId::CheckoutCancel, false),
        ],
    );
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
        &[
            (flip, ButtonId::AppearanceToggle, false),
            (&format!("Use {name} (Enter)"), ButtonId::ThemeApply, true),
            ("Cancel (Esc)", ButtonId::ThemeCancel, false),
        ],
    );
}

fn draw_help(f: &mut Frame, area: Rect) {
    let p = palette();
    let rect = centered(area, 84, 27);
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
            ("right-click", "clear selection"),
        ),
        row(
            ("click [ ] / [+]", "viewed / stage file"),
            ("drag the divider", "resize the panel"),
        ),
        row(("wheel", "scroll"), ("Shift+wheel", "scroll sideways")),
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
            ("n / p", "next / previous file"),
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
            ("v", "split / inline view"),
        ),
        row(("x", "mark viewed / stage file"), ("r", "reload the file")),
        row(
            ("` (backtick)", "swap PR ⇄ local view"),
            ("< / >", "narrow / widen panel"),
        ),
        row(
            ("t", "theme picker (live preview)"),
            ("t then a", "switch light / dark"),
        ),
        row(
            ("b / q", "back to the PR list / quit"),
            ("Esc", "clear selection, then back"),
        ),
        Line::from(""),
        Line::from(Span::styled(
            "  Editor: Ctrl+S save · Esc close (twice to discard) · Ctrl+Z undo · Ctrl+Y redo",
            dim,
        )),
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
    let style = if app.status_err {
        Style::default().fg(p.err)
    } else {
        Style::default().fg(p.ok)
    };
    let hints = match app.screen {
        Screen::PrList => "l local changes · r refresh · ? help · q quit",
        Screen::Review => {
            if app.editor.is_some() {
                "Ctrl+S save · Esc close · ? help"
            } else if app.local {
                "j/k move · V select · x stage · ` PR view · ? help"
            } else {
                "j/k move · V select · c comment · ` local · ? help"
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
        let gutter: String = before
            .chars()
            .take(12 + app.file_panel_w as usize)
            .collect();

        app.diff_hscroll = 10;
        let buf = render(&mut app);
        let after = row_text(&buf, y);
        assert_eq!(
            after
                .chars()
                .take(12 + app.file_panel_w as usize)
                .collect::<String>(),
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
        let body_start = (app.file_panel_w as usize) + 1 + 12;
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
        let body = app.file_panel_w + 2;
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
            top.contains("⇄ PR"),
            "local mode offers the swap-to-PR button: {top:?}"
        );
    }

    /// PR review offers the ⇄ Local toggle in the top bar (the ` key's
    /// clickable twin).
    #[test]
    fn pr_mode_topbar_offers_swap_to_local() {
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
        let mut top = String::new();
        for x in 0..buf.area.width {
            top.push_str(buf[(x, 0)].symbol());
        }
        assert!(
            top.contains("⇄ Local"),
            "PR mode offers the swap-to-local button: {top:?}"
        );
    }
}

//! All rendering. Every clickable region drawn here is recorded into
//! `app.layout` so the mouse handlers can hit-test against it.

use crate::app::{
    App, ButtonId, Dragging, FileEntry, FinderMode, MenuRow, Overlay, Screen, StageSection,
    ViewMode, FINDER_ROWS,
};
use crate::blame::{self, Heat};
use crate::diff::{DisplayEntry, Row, RowKind, Selection, Side, TAB_WIDTH};
use crate::github::Verdict;
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
    // The tab row only takes a line when something is pinned, so a reader
    // who never pins a file never pays for the feature.
    let tabs = u16::from(!app.pins.is_empty() || app.buffers().count() > 0);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(tabs),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    match app.screen {
        Screen::PrList => {
            draw_topbar_prlist(f, app, chunks[0]);
            draw_pr_list(f, app, chunks[2]);
        }
        Screen::Review => {
            draw_topbar_review(f, app, chunks[0]);
            draw_review(f, app, chunks[2]);
        }
    }
    if tabs > 0 {
        draw_pin_tabs(f, app, chunks[1]);
    }
    draw_status(f, app, chunks[3]);

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
        Overlay::CodeActions(_) => draw_code_actions(f, app, area),
        Overlay::Problem(_) => draw_problem(f, app, area),
        Overlay::PathMenu(_) => draw_path_menu(f, app, area),
        Overlay::BlameMenu(_) => draw_blame_menu(f, app, area),
        Overlay::ConflictMenu(_) => draw_conflict_menu(f, app, area),
        Overlay::ReviewConfirm(_) => draw_review_confirm(f, app, area),
        Overlay::VerdictMenu => draw_verdict_menu(f, app, area),
        Overlay::Menu(_) => draw_menu(f, app, area),
        Overlay::OpenPath(_) => draw_open_path(f, app, area),
    }
}

// ------------------------------------------------------- pinned-file tabs

/// The row of pinned files under the top bar.
///
/// One tab per pinned file: its number, its name, and a ✕ that unpins it.
/// A file that lives outside the repository carries a `↗`, because "the
/// plan file" means a different document depending on the answer, and the
/// row is the only place that says so.
fn draw_pin_tabs(f: &mut Frame, app: &mut App, area: Rect) {
    let p = palette();
    app.layout.pin_row = area;
    let open = app.active_pin();
    // Take a copy of what each tab says before drawing: the draw needs
    // `app` mutably to record the click targets.
    let labels = app.pins.labels();
    let outside: Vec<bool> = app.pins.items.iter().map(|i| i.outside).collect();
    // A pinned file's tab is this one, so this is where the mark saying
    // "not on disk yet" goes. It draws no buffer tab of its own.
    let unsaved: Vec<bool> = (0..labels.len()).map(|i| app.pin_dirty(i)).collect();
    let width = |s: &str| disp_width(s) as u16;
    // " 1 ● name ✕ " — the number, the unsaved mark, the name, the close
    // mark, and the spaces that keep two tabs from reading as one.
    let tab_w = |i: usize| {
        let mark = if outside[i] { 2 } else { 0 };
        let dot = if unsaved[i] { 2 } else { 0 };
        width(&labels[i]) + width(&format!("{} ", i + 1)) + mark + dot + 4
    };

    // Keep the open tab on screen. A narrow window shows a window onto
    // the row rather than a squeezed version of all of it.
    let total: u16 = (0..labels.len()).map(tab_w).sum();
    let mut first = app.pins.scroll.min(labels.len().saturating_sub(1));
    if total > area.width {
        if let Some(at) = open {
            if at < first {
                first = at;
            } else {
                // Walk the start forward until the open tab fits.
                while first < at {
                    let shown: u16 = (first..=at).map(tab_w).sum();
                    if shown <= area.width.saturating_sub(2) {
                        break;
                    }
                    first += 1;
                }
            }
        }
    } else {
        first = 0;
    }
    app.pins.scroll = first;

    // Paint the whole row first: a tab that scrolls away must not leave
    // its old text behind.
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ".repeat(area.width as usize),
            Style::default().bg(p.btn_bg),
        ))),
        area,
    );

    let mut x = area.x;
    // A `‹` when tabs have scrolled off the left, so the row admits it.
    if first > 0 && area.width > 2 {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "‹",
                Style::default().bg(p.btn_bg).fg(p.dim),
            ))),
            Rect {
                x,
                y: area.y,
                width: 1,
                height: 1,
            },
        );
        x += 1;
    }
    let right = area.x + area.width;
    let mut hidden = 0usize;
    for i in first..labels.len() {
        let w = tab_w(i);
        if x + w > right.saturating_sub(1) {
            hidden = labels.len() - i;
            break;
        }
        let active = open == Some(i);
        let (bg, fg) = if active {
            (p.btn_active_bg, p.btn_active_fg)
        } else {
            (p.btn_bg, p.btn_fg)
        };
        let base = Style::default().bg(bg).fg(fg);
        let mut spans = vec![
            Span::styled(" ", base),
            Span::styled(
                format!("{} ", i + 1),
                Style::default().bg(bg).fg(if active { fg } else { p.dim }),
            ),
        ];
        if outside[i] {
            // One mark, in the color the review already uses for "this is
            // not part of what you are reviewing".
            spans.push(Span::styled("↗ ", Style::default().bg(bg).fg(p.accent)));
        }
        if unsaved[i] {
            // The same two colors the buffer tabs use, for the same
            // reason: one of them disappears on one of the backgrounds.
            let fg = if active {
                p.tab_dirty_active
            } else {
                p.tab_dirty
            };
            spans.push(Span::styled("● ", Style::default().bg(bg).fg(fg)));
        }
        spans.push(Span::styled(
            labels[i].clone(),
            if active {
                base.add_modifier(Modifier::BOLD)
            } else {
                base
            },
        ));
        spans.push(Span::styled(" ", base));
        spans.push(Span::styled(
            "✕",
            Style::default()
                .bg(bg)
                .fg(if active { fg } else { p.faint }),
        ));
        spans.push(Span::styled(" ", base));
        let rect = Rect {
            x,
            y: area.y,
            width: w,
            height: 1,
        };
        f.render_widget(Paragraph::new(Line::from(spans)), rect);
        // The ✕ is the second-to-last column of the tab; everything else
        // in it opens the file. `button_at` takes the first rect that
        // holds the click, so the ✕ has to be recorded first.
        app.layout.buttons.push((
            Rect {
                x: x + w - 2,
                y: area.y,
                width: 1,
                height: 1,
            },
            ButtonId::PinClose(i),
        ));
        app.layout.buttons.push((rect, ButtonId::PinTab(i)));
        x += w;
    }
    // The open buffers, after the pins and in the same row. They are drawn
    // together and addressed apart: a pin is a bookmark the reader chose
    // and a buffer is a file that happens to be open, and merging their
    // numbering would renumber the pins every time a file was opened.
    // One open file gets a tab of its own. It is the only place the
    // reader is told which file the editor holds, whether one click
    // opened it, and whether it has unsaved work in it.
    let bufs = app.buffer_tabs();
    if !bufs.is_empty() {
        // A divider, so the two kinds of tab do not read as one run.
        if x + 2 <= right && !labels.is_empty() {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "│",
                    Style::default().bg(p.btn_bg).fg(p.faint),
                ))),
                Rect {
                    x,
                    y: area.y,
                    width: 1,
                    height: 1,
                },
            );
            x += 1;
        }
        for (i, tab) in bufs.iter().enumerate() {
            // " ● name ✕ " — a space on each side of the name so the dot
            // does not read as part of it, and a ✕ that closes the file.
            let w = width(&tab.label) + if tab.dirty { 2 } else { 0 } + 4;
            // A seam between one tab and the next. Without it a row of
            // tabs reads as one run of file names with no telling where
            // one ends, which is what the row is for.
            let seam = u16::from(i > 0);
            if x + w + seam > right.saturating_sub(1) {
                hidden += bufs.len() - i;
                break;
            }
            if seam > 0 {
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        "│",
                        Style::default().bg(p.btn_bg).fg(p.faint),
                    ))),
                    Rect {
                        x,
                        y: area.y,
                        width: 1,
                        height: 1,
                    },
                );
                x += 1;
            }
            let (bg, fg) = if tab.active {
                (p.btn_active_bg, p.btn_active_fg)
            } else {
                (p.btn_bg, p.btn_fg)
            };
            let base = Style::default().bg(bg).fg(fg);
            let mut spans = vec![Span::styled(" ", base)];
            if tab.dirty {
                // The selected tab's background is a saturated blue and
                // the row's is grey, so the mark takes its color from
                // which one it is sitting on.
                let dot = if tab.active {
                    p.tab_dirty_active
                } else {
                    p.tab_dirty
                };
                spans.push(Span::styled("● ", Style::default().bg(bg).fg(dot)));
            }
            // Italic says this is the peek tab — one click opened it and
            // the next click replaces it. Bold says it is the buffer on
            // screen. A tab can be both, so the two modifiers are added
            // rather than chosen between.
            let mut name = base;
            if tab.active {
                name = name.add_modifier(Modifier::BOLD);
            }
            if tab.peek {
                name = name.add_modifier(Modifier::ITALIC);
            }
            spans.push(Span::styled(tab.label.clone(), name));
            spans.push(Span::styled(" ", base));
            spans.push(Span::styled(
                "✕",
                Style::default()
                    .bg(bg)
                    .fg(if tab.active { fg } else { p.faint }),
            ));
            spans.push(Span::styled(" ", base));
            let rect = Rect {
                x,
                y: area.y,
                width: w,
                height: 1,
            };
            f.render_widget(Paragraph::new(Line::from(spans)), rect);
            // The ✕ is the second-to-last column; everything else in the
            // tab opens the file. `button_at` takes the first rect that
            // holds the click, so the ✕ has to be recorded first.
            app.layout.buttons.push((
                Rect {
                    x: x + w - 2,
                    y: area.y,
                    width: 1,
                    height: 1,
                },
                ButtonId::BufferClose(i),
            ));
            app.layout.buttons.push((rect, ButtonId::BufferTab(i)));
            x += w;
        }
    }

    // A `›` and a count for the tabs that did not fit — silently dropping
    // them would make the row lie about how many files are pinned.
    if hidden > 0 {
        let note = format!("›{hidden}");
        let w = width(&note).min(area.width);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                note,
                Style::default().bg(p.btn_bg).fg(p.dim),
            ))),
            Rect {
                x: right.saturating_sub(w),
                y: area.y,
                width: w,
                height: 1,
            },
        );
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

/// The "↑2 ↓1 origin/main" counts beside the branch name.
///
/// Ahead and behind answer two different questions — what is not pushed,
/// and what has landed upstream since — so they keep two colors and are
/// both left off when there is nothing to say. An empty list means the
/// branch is level with its upstream, or tracks nothing loupe can resolve.
fn track_spans<'a>(app: &App) -> Vec<Span<'a>> {
    let p = palette();
    let Some(t) = &app.tracking else {
        return Vec::new();
    };
    if t.in_sync() {
        return vec![Span::styled(
            format!("  ≡ {}", t.upstream),
            Style::default().fg(p.faint),
        )];
    }
    let mut out = vec![Span::raw("  ")];
    if t.ahead > 0 {
        out.push(Span::styled(
            format!("↑{}", t.ahead),
            Style::default().fg(p.ahead).add_modifier(Modifier::BOLD),
        ));
    }
    if t.behind > 0 {
        if t.ahead > 0 {
            out.push(Span::raw(" "));
        }
        out.push(Span::styled(
            format!("↓{}", t.behind),
            Style::default().fg(p.behind).add_modifier(Modifier::BOLD),
        ));
    }
    out.push(Span::styled(
        format!(" {}", t.upstream),
        Style::default().fg(p.faint),
    ));
    out
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
    let conflicts = app.conflict_count();
    let (badge, badge_bg, title, note) = if app.local {
        let branch = app
            .local_branch
            .clone()
            .unwrap_or_else(|| "detached HEAD".into());
        // Mid-merge, the badge says so: the working tree is not just
        // "some edits" any more, and the way out is a different one.
        match app.merge_op {
            Some(op) => (
                format!(" ⚠ {} ", op.badge()),
                p.badge_conflict,
                branch,
                if conflicts > 0 {
                    format!("  — resolve the conflicts to finish the {}", op.noun())
                } else {
                    format!("  — conflicts resolved; finish the {}", op.noun())
                },
            ),
            None => (
                " ⎇ LOCAL ".to_string(),
                p.badge_local,
                branch,
                "  — uncommitted changes vs HEAD".to_string(),
            ),
        }
    } else {
        let (num, title) = app
            .pr
            .as_ref()
            .map(|p| (p.number, p.title.clone()))
            .unwrap_or((0, String::new()));
        (format!(" PR #{num} "), p.badge_pr, title, String::new())
    };
    // How far the branch has drifted from the branch it tracks. Two short
    // counts, next to the name they are about.
    let drift = track_spans(app);
    let drift_w: usize = drift.iter().map(|s| disp_width(&s.content)).sum();
    // The badge and the PR title (or branch) are what the buttons have to
    // leave room for; the trailing note is expendable.
    let shown_title = tail_truncate(&title, (area.width / 3) as usize);
    let badge_w = disp_width(&badge) as u16;
    let reserve = badge_w + 1 + disp_width(&shown_title) as u16 + drift_w as u16 + 1;
    // The badge is a click target: right-click copies the PR link.
    app.layout.badge = Rect {
        x: area.x,
        y: area.y,
        width: badge_w.min(area.width),
        height: 1,
    };
    let mut spans = vec![
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
    ];
    spans.extend(drift);
    spans.push(Span::styled(note, Style::default().fg(p.dim)));
    f.render_widget(Paragraph::new(Line::from(spans)), area);

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
                // Pressed while this document already has a tab, so the
                // button says which of the two things it will do.
                ("📌", ButtonId::PinToggle, app.current_is_pinned()),
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
            buttons.push(("📌", ButtonId::PinToggle, app.current_is_pinned()));
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
    // The review composer takes the foot of the file panel. It is only
    // drawn when there is a pull request to review and enough height that
    // the file list is still usable above it.
    let rw = review_box_height(app, cols[0].height);
    let panel = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(rw)])
        .split(cols[0]);
    draw_file_list(f, app, panel[0]);
    if rw > 0 {
        draw_review_box(f, app, panel[1]);
    } else {
        app.layout.review_box = Rect::default();
    }

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
    let conflicts = app.conflict_count();
    let full = if conflicts > 0 {
        format!(
            " Files — ⚠ {conflicts} conflict{} ",
            if conflicts == 1 { "" } else { "s" }
        )
    } else if app.panel == crate::app::PanelMode::Files {
        if app.repo_paths.is_empty() {
            " Files — reading… ".to_string()
        } else {
            format!(" Files — {} in the repo ", app.repo_paths.len())
        }
    } else if app.local {
        format!(" Files {viewed_n}/{n} staged ")
    } else {
        format!(" Files {viewed_n}/{n} ✓ ")
    };
    let short = if conflicts > 0 {
        format!(" ⚠ {conflicts} ")
    } else {
        format!(" {viewed_n}/{n} ")
    };
    let title = if area.width as usize >= disp_width(&full) + TOGGLE_W + 2 {
        full
    } else {
        short
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if conflicts > 0 {
            Style::default().fg(p.conflict)
        } else {
            Style::default()
        })
        .title(title);
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
        // The Changes/Files toggle sits on the bottom border, where there
        // is always a full row to spend — the top one is shared with the
        // title and runs out on a narrowed panel.
        let mode_area = Rect {
            x: area.x + 1,
            y: area.y + area.height.saturating_sub(1),
            width: area.width.saturating_sub(2),
            height: 1,
        };
        buttons_right(
            f,
            app,
            mode_area,
            0,
            &[
                (
                    "Change",
                    ButtonId::PanelChanges,
                    app.panel == crate::app::PanelMode::Changes,
                ),
                (
                    "Files",
                    ButtonId::PanelFiles,
                    app.panel == crate::app::PanelMode::Files,
                ),
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
            FileEntry::ConflictHeading { count } => {
                let text = format!(
                    "⚠ {count} MERGE CONFLICT{}",
                    if *count == 1 { "" } else { "S" }
                );
                lines.push(Line::from(Span::styled(
                    truncate_pad(&text, inner.width as usize),
                    Style::default().fg(p.conflict).add_modifier(Modifier::BOLD),
                )));
            }
            FileEntry::StageHeading {
                section,
                count,
                collapsed,
            } => {
                // The heading reads as a heading, not as a file: no
                // checkbox column, no status letter, and its own color.
                let arrow = if *collapsed { "▸" } else { "▾" };
                let head = format!("{arrow} {} {count}", section.title());
                let action = if *count > 0 {
                    section.action_label()
                } else {
                    ""
                };
                let w = inner.width as usize;
                let pad = w
                    .saturating_sub(disp_width(&head))
                    .saturating_sub(disp_width(action));
                let fg = match section {
                    StageSection::Staged => p.st_added,
                    StageSection::Unstaged => p.dim,
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        truncate_pad(&head, disp_width(&head).min(w)),
                        Style::default().fg(fg).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(action.to_string(), Style::default().fg(p.accent)),
                ]));
            }
            FileEntry::Dir { label, path, depth } => {
                let arrow = if app.collapsed_set().contains(path) {
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
            FileEntry::File { src, depth } => {
                let (file, idx) = match app.diff_row(*src) {
                    Some(file) => (file, app.diff_row_idx(*src).unwrap_or(usize::MAX)),
                    // A row with no diff behind it: a name, and none of
                    // the marks. There is no diff to stage, to mark
                    // viewed, or to put back, so the columns that say so
                    // would all be lying. That is every row in the Files
                    // panel, which lists the repository rather than the
                    // change, and the untouched files in the Changes one.
                    None => {
                        let Some(path) = app.row_path(*src) else {
                            continue;
                        };
                        let name = if app.tree_view {
                            path.rsplit('/').next().unwrap_or(path)
                        } else {
                            path
                        };
                        let indent = *depth as usize;
                        let name_w = (inner.width as usize).saturating_sub(indent + 4);
                        let text =
                            format!("{}    {}", " ".repeat(indent), tail_truncate(name, name_w));
                        // Dim means git is ignoring this file. `.env` is
                        // worth opening and worth knowing is not committed,
                        // so the row says both at once.
                        let fg = if app.row_ignored(*src) { p.dim } else { p.text };
                        // The row whose file the editor holds. There is no
                        // file cursor to follow here — the panel lists the
                        // repository, and what is open is the only thing
                        // "here" can mean.
                        let base = if app.buffer_showing(path) {
                            Style::default().bg(p.row).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        };
                        lines.push(Line::from(Span::styled(
                            truncate_pad(&text, inner.width as usize),
                            base.fg(fg),
                        )));
                        continue;
                    }
                };
                let selected = idx == app.file_cursor;
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
                    '!' => p.conflict,
                    'A' => p.st_added,
                    'D' => p.st_removed,
                    'R' | 'C' => p.st_renamed,
                    _ => p.st_other,
                };
                let conflicted = file.conflicted;
                let (cb, cb_style) = if conflicted {
                    // A conflicted file cannot be staged as it stands, so
                    // the icon column warns instead of counting.
                    (
                        "[!]",
                        Style::default().fg(p.conflict).add_modifier(Modifier::BOLD),
                    )
                } else if app.local {
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
                        // Handled above — a conflicted file never reaches
                        // this arm, but the index can say so first.
                        StageState::Conflicted => (
                            "[!]",
                            Style::default().fg(p.conflict).add_modifier(Modifier::BOLD),
                        ),
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
                // A conflicted file's +/− counts describe the marker text
                // git wrote, not a change anyone made, so they are left
                // off. The columns stay, so the rows still line up.
                let counts = if conflicted {
                    " ".repeat(
                        format!(" +{} −{}", file.additions, file.deletions)
                            .chars()
                            .count(),
                    )
                } else {
                    format!(" +{} −{}", file.additions, file.deletions)
                };
                // Held comments on this file, so a review in progress is
                // visible from the panel rather than only from the diff.
                let held = app.pending_in(&file.path);
                let held_s = if held > 0 {
                    format!(" 💬{held}")
                } else {
                    String::new()
                };
                let indent = *depth as usize;
                // ↺ at the end of the row throws the whole file's changes
                // away (after asking). Only when there is a working tree to
                // put back — a read-only PR keeps the columns for the name.
                let revert_w = if app.can_revert() && !conflicted {
                    crate::app::REVERT_W as usize
                } else {
                    0
                };
                let name_w = (inner.width as usize).saturating_sub(
                    indent + 6 + counts.chars().count() + disp_width(&held_s) + revert_w,
                );
                let name_t = tail_truncate(name, name_w);
                let pad = name_w.saturating_sub(disp_width(&name_t));
                let base = if selected {
                    Style::default().bg(p.row).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let name_fg = if conflicted {
                    p.conflict
                } else if done {
                    p.viewed
                } else {
                    p.text
                };
                let mut spans = vec![
                    Span::styled(" ".repeat(indent), base),
                    Span::styled(format!("{cb} "), base.patch(cb_style)),
                    Span::styled(format!("{sc} "), base.fg(sc_color)),
                    Span::styled(format!("{name_t}{}", " ".repeat(pad)), base.fg(name_fg)),
                    Span::styled(held_s, base.fg(p.accent)),
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

// ----------------------------------------------------------- the review box

/// Rows the review composer takes at the foot of the file panel.
///
/// A border, the heading, the summary box, and the button row. It stands
/// down entirely on a short terminal: a file list squeezed to two rows to
/// make space for it would be the worse trade.
fn review_box_height(app: &App, panel_h: u16) -> u16 {
    if !app.review_box_on() {
        return 0;
    }
    // 2 border + 1 heading + 3 text + 1 buttons.
    const WANT: u16 = 7;
    // …and at least this much left over for the files themselves.
    const KEEP: u16 = 6;
    if panel_h < WANT + KEEP {
        0
    } else {
        WANT
    }
}

/// The composer: a summary, and the verdict to send with it.
///
/// Inline comments say "this line is wrong". This is where the review says
/// what it thinks of the change as a whole — and it is the only control
/// that actually sends anything, held comments included.
fn draw_review_box(f: &mut Frame, app: &mut App, area: Rect) {
    let p = palette();
    let held = app.pending.len();
    let focused = app.review.focused;
    let title = if held == 0 {
        " Review ".to_string()
    } else {
        format!(" Review · {held} held ")
    };
    let border = if focused {
        p.divider_active
    } else if held > 0 {
        p.accent
    } else {
        p.divider
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 4 {
        app.layout.review_box = Rect::default();
        return;
    }

    // Line 1: what is held, and the way to throw it away.
    let head = Rect { height: 1, ..inner };
    if held == 0 {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_pad(" c holds a line comment", inner.width as usize),
                Style::default().fg(p.faint),
            ))),
            head,
        );
    } else {
        const DISCARD: &str = "✕ Discard";
        // The button is drawn over the right of this row, so the label
        // gets what is left rather than running underneath it.
        let room = (inner.width as usize).saturating_sub(disp_width(DISCARD) + 3);
        let label = format!(
            " 💬 {held} comment{} held",
            if held == 1 { "" } else { "s" }
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_pad(&label, room),
                Style::default().fg(p.accent),
            ))),
            head,
        );
        buttons_right(
            f,
            app,
            head,
            room as u16,
            &[(DISCARD, ButtonId::ReviewDiscard, false)],
        );
    }

    // The summary box. It is the same widget the editor and the comment
    // overlay use, so the keys inside it are the keys everywhere else.
    let body_h = inner.height.saturating_sub(2);
    let body = Rect {
        y: inner.y + 1,
        height: body_h,
        ..inner
    };
    app.layout.review_box = body;
    app.layout.buttons.push((body, ButtonId::ReviewBody));
    let ta = &mut app.review.textarea;
    ta.set_style(Style::default().fg(p.text));
    ta.set_placeholder_style(Style::default().fg(p.dim));
    ta.set_cursor_line_style(Style::default());
    // The cursor is only real where the keyboard is; a block cursor in an
    // unfocused box reads as "type here" and would be a lie.
    ta.set_cursor_style(if focused {
        Style::default().bg(p.accent).fg(p.badge_fg)
    } else {
        Style::default()
    });
    f.render_widget(&*ta, body);

    // The split button: the verdict, and a ▾ that offers the other two.
    let row = Rect {
        y: inner.y + inner.height - 1,
        height: 1,
        ..inner
    };
    let v = app.review.verdict;
    let can_send = held > 0 || !app.review.is_empty();
    let label = format!(" {} {} ", v.icon(), v.label());
    let arrow = " ▾ ";
    let lw = disp_width(&label) as u16;
    let aw = disp_width(arrow) as u16;
    let total = (lw + aw).min(row.width);
    let btn = Rect {
        width: total.saturating_sub(aw),
        ..row
    };
    let drop = Rect {
        x: row.x + total.saturating_sub(aw),
        width: aw.min(row.width),
        ..row
    };
    let btn_style = if can_send {
        Style::default()
            .bg(verdict_bg(v))
            .fg(p.btn_active_fg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(p.btn_bg).fg(p.btn_fg)
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_pad(&label, btn.width as usize),
            btn_style,
        ))),
        btn,
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            arrow,
            Style::default()
                .bg(p.btn_bg)
                .fg(if app.review.picking { p.text } else { p.btn_fg }),
        ))),
        drop,
    );
    app.layout.buttons.push((btn, ButtonId::ReviewSubmit));
    app.layout.buttons.push((drop, ButtonId::ReviewVerdict));

    // Whatever room is left after the button says which key sends it.
    let hint_x = row.x + total + 1;
    if hint_x < row.x + row.width {
        let hint = Rect {
            x: hint_x,
            width: row.x + row.width - hint_x,
            ..row
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_pad(if focused { "Ctrl+S" } else { "R" }, hint.width as usize),
                Style::default().fg(p.faint),
            ))),
            hint,
        );
    }
}

/// The button colour for each verdict: approving and asking for changes
/// are opposite answers, so they are not the same shade of "primary".
fn verdict_bg(v: Verdict) -> Color {
    let p = palette();
    match v {
        Verdict::Comment => p.btn_active_bg,
        Verdict::Approve => p.badge_local,
        Verdict::RequestChanges => p.badge_conflict,
    }
}

/// The verdict list, under the ▾.
fn draw_verdict_menu(f: &mut Frame, app: &mut App, area: Rect) {
    let p = palette();
    let all = Verdict::all();
    let rows: Vec<String> = all
        .iter()
        .map(|v| format!(" {} {} ", v.icon(), v.label()))
        .collect();
    let w = (rows.iter().map(|r| disp_width(r)).max().unwrap_or(0) as u16 + 2).min(area.width);
    let h = (all.len() as u16 + 2).min(area.height);
    // Anchored to the ▾ it belongs to, and flipped above it — the button
    // sits at the foot of the panel, so there is never room below.
    let anchor = app
        .layout
        .buttons
        .iter()
        .find(|(_, id)| *id == ButtonId::ReviewVerdict)
        .map(|(r, _)| *r)
        .unwrap_or(area);
    let x = anchor
        .x
        .saturating_sub(w.saturating_sub(anchor.width))
        .min(area.x + area.width.saturating_sub(w));
    let y = anchor.y.saturating_sub(h).max(area.y);
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
        .title(" Send as ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    for (i, text) in rows.iter().enumerate() {
        if i as u16 >= inner.height {
            break;
        }
        let r = Rect {
            y: inner.y + i as u16,
            height: 1,
            ..inner
        };
        let selected = i == app.review.pick;
        let style = if selected {
            Style::default()
                .bg(p.row)
                .fg(p.text)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.text)
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_pad(text, inner.width as usize),
                style,
            ))),
            r,
        );
        app.layout.buttons.push((r, ButtonId::VerdictRow(i)));
    }
}

/// "Send this review?" — everything that is about to reach GitHub, listed.
///
/// A review notifies every watcher of the pull request and cannot be taken
/// back, so this says exactly what goes: the verdict, the summary, and
/// where each held comment will land.
fn draw_review_confirm(f: &mut Frame, app: &mut App, area: Rect) {
    let Overlay::ReviewConfirm(prompt) = &app.overlay else {
        return;
    };
    let p = palette();
    let n = prompt.comments.len();
    // The list is capped: a long review would otherwise push the buttons
    // off a short terminal.
    let shown = n.min(6);
    // Verdict, summary, a blank, the listed comments, the "and N more"
    // line, the stale warning, the "cannot be undone" line, the buttons,
    // and the two border rows.
    let height =
        (3 + shown as u16 + u16::from(n > shown) + u16::from(prompt.stale) + 4).min(area.height);
    let rect = centered(area, 78.min(area.width), height);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(verdict_bg(prompt.verdict)))
        .style(Style::default().fg(p.text))
        .title(format!(" Send this review to PR #{} ? ", prompt.number));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let mut text = vec![Line::from(vec![
        Span::styled(
            format!("{} {}", prompt.verdict.icon(), prompt.verdict.label()),
            Style::default()
                .fg(verdict_bg(prompt.verdict))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            match n {
                0 => "  ·  summary only".to_string(),
                1 => "  ·  1 inline comment".to_string(),
                _ => format!("  ·  {n} inline comments"),
            },
            Style::default().fg(p.dim),
        ),
    ])];
    if prompt.body.trim().is_empty() {
        text.push(Line::from(Span::styled(
            "No summary — the inline comments speak for themselves.",
            Style::default().fg(p.dim),
        )));
    } else {
        let first = prompt.body.lines().next().unwrap_or("");
        let more = prompt.body.lines().count().saturating_sub(1);
        let tail = if more > 0 {
            format!("  (+{more} more line{})", if more == 1 { "" } else { "s" })
        } else {
            String::new()
        };
        text.push(Line::from(vec![
            Span::styled(
                truncate_pad(first, (inner.width as usize).saturating_sub(tail.len())),
                Style::default().fg(p.text),
            ),
            Span::styled(tail, Style::default().fg(p.dim)),
        ]));
    }
    text.push(Line::from(""));
    for c in prompt.comments.iter().take(shown) {
        let head = c.body.lines().next().unwrap_or("");
        let at = c.where_at();
        let room = (inner.width as usize).saturating_sub(disp_width(&at) + 5);
        text.push(Line::from(vec![
            Span::styled("  💬 ", Style::default().fg(p.accent)),
            Span::styled(at, Style::default().fg(p.key)),
            Span::styled(
                format!("  {}", tail_truncate(head, room)),
                Style::default().fg(p.dim),
            ),
        ]));
    }
    if n > shown {
        text.push(Line::from(Span::styled(
            format!("  …and {} more", n - shown),
            Style::default().fg(p.dim),
        )));
    }
    if prompt.stale {
        text.push(Line::from(Span::styled(
            "⚠ The PR head moved since these were written — GitHub may refuse them.",
            Style::default().fg(p.err),
        )));
    }
    text.push(Line::from(Span::styled(
        "This notifies everyone watching the pull request. It cannot be undone.",
        Style::default().fg(p.dim),
    )));
    // Everything above the button row, which is drawn over the last line.
    let body = Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    };
    f.render_widget(Paragraph::new(text), body);

    let btn_area = Rect {
        x: inner.x,
        y: inner.y + inner.height - 1,
        width: inner.width,
        height: 1,
    };
    let send = format!("{} Send (Enter)", prompt.verdict.icon());
    buttons_right(
        f,
        app,
        btn_area,
        0,
        &[
            (send.as_str(), ButtonId::ReviewYes, true),
            ("Cancel (Esc)", ButtonId::ReviewCancel, false),
        ],
    );
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
    // A conflict view is not a before-and-after, so it is not titled like
    // one: it names the two branches whose lines are on each side.
    let title = match (file, &app.diff, &app.conflict) {
        (Some(fl), _, Some(c)) => {
            let n = c.file.len();
            let (ours, theirs) = c.file.labels();
            format!(
                " ⚑ {} — {n} conflict{} · ◀ {ours} │ {theirs} ▶{hoff} ",
                fl.path,
                if n == 1 { "" } else { "s" }
            )
        }
        (Some(fl), Some(d), None) => format!(
            " {} — +{} −{}{}{hoff} ",
            fl.path,
            d.additions,
            d.deletions,
            if app.checked_out { "" } else { " · read-only" }
        ),
        _ => " Diff ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if app.conflict.is_some() {
            Style::default().fg(p.conflict)
        } else {
            Style::default()
        })
        .title(title);
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
    let conflicted = app.conflict.is_some();
    let bars: Vec<Option<bool>> = (app.diff_scroll..app.diff_scroll + h)
        .map(|i| {
            if conflicted {
                app.conflict_bar(i)
            } else {
                app.change_bar(i)
            }
        })
        .collect();
    let pane_w = (inner.width as usize).saturating_sub(bar_w);

    let mut lines: Vec<Line> = Vec::with_capacity(h);
    for (n, entry) in app.display.iter().skip(app.diff_scroll).take(h).enumerate() {
        // The keyboard cursor row, underlined the way the editor marks its
        // own cursor line.
        let cur = app.diff_scroll + n == app.diff_cursor;
        let mut spans: Vec<Span> = Vec::new();
        if bar_w > 0 {
            // ↺ puts a change back; ⚑ opens the resolve menu. Two marks,
            // because they do two very different things to the file.
            let (mark, color) = if conflicted {
                ("⚑ ", p.conflict)
            } else {
                ("↺ ", p.accent)
            };
            // A held comment on this row outranks both markers. It is
            // the one thing in the column that says something about
            // *this* line rather than offering to do something to it.
            let held = app.pending_on_row(app.diff_scroll + n);
            spans.push(if held {
                Span::styled(
                    "💬",
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                )
            } else {
                match bars.get(n).copied().flatten() {
                    Some(true) => Span::styled(
                        mark,
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Some(false) => Span::styled(
                        "┃ ",
                        Style::default().fg(if conflicted { color } else { p.divider }),
                    ),
                    None => Span::raw("  "),
                }
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

/// The "open a file by path" box (`Ctrl+O`).
///
/// It exists for the two cases a drop cannot cover: a terminal that does
/// not report drops at all, and a path an agent has just printed, which
/// is faster to paste than to find in a file browser and drag.
fn draw_open_path(f: &mut Frame, app: &mut App, area: Rect) {
    let Overlay::OpenPath(box_) = &app.overlay else {
        return;
    };
    let p = palette();
    let w = (area.width.saturating_sub(8)).clamp(24, 80);
    let rect = centered(area, w, 7);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.accent))
        .title(" 📌 Open a file ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let field_w = inner.width.saturating_sub(2) as usize;
    // Scroll the text so the caret stays in view on a long path.
    let chars: Vec<char> = box_.input.chars().collect();
    let from = box_.caret.saturating_sub(field_w.saturating_sub(1));
    let shown: String = chars[from.min(chars.len())..]
        .iter()
        .take(field_w)
        .collect();
    let caret_col = (box_.caret - from).min(field_w) as u16;

    let hint = if box_.input.trim().is_empty() {
        "Type a path, or paste one. A relative path is read from the repository root."
    } else {
        "Enter opens and pins it · Esc cancels"
    };
    let text = vec![
        Line::from(Span::styled(
            "Any file on this machine — it does not have to be in the repository.",
            Style::default().fg(p.dim),
        )),
        Line::default(),
        Line::from(vec![
            Span::styled("› ", Style::default().fg(p.accent)),
            Span::styled(shown, Style::default().fg(p.text)),
        ]),
        Line::default(),
        Line::from(Span::styled(hint, Style::default().fg(p.faint))),
    ];
    f.render_widget(Paragraph::new(text), inner);
    // A real caret, so the box reads as a text field rather than a label.
    f.set_cursor_position((inner.x + 2 + caret_col, inner.y + 2));

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
            ("Open (Enter)", ButtonId::OpenPathGo, true),
            ("Cancel (Esc)", ButtonId::OpenPathCancel, false),
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

/// The resolve menu for a merge conflict.
///
/// Two lines per entry: what it keeps, and a note saying how much. A
/// conflict is settled once and hard to undo, so the menu says what each
/// line will do rather than trusting the label to carry it.
fn draw_conflict_menu(f: &mut Frame, app: &mut App, area: Rect) {
    let Overlay::ConflictMenu(menu) = &app.overlay else {
        return;
    };
    let p = palette();
    let title = format!(" ⚑ {} ", menu.title);
    let widest = menu
        .items
        .iter()
        .map(|it| disp_width(&it.label).max(disp_width(&it.note) + 2) + 4)
        .max()
        .unwrap_or(0);
    let w = (widest.max(disp_width(&title)) as u16 + 4).min(area.width);
    // Two rows per line, plus the border and the closing hint.
    let h = (menu.items.len() as u16 * 2 + 3).min(area.height);
    let (ax, ay) = menu.anchor;
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
        .border_style(Style::default().fg(p.conflict))
        .style(Style::default().fg(p.text))
        .title(title);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let mut rows: Vec<(Rect, ButtonId)> = Vec::new();
    for (i, item) in menu.items.iter().enumerate() {
        let top = i as u16 * 2;
        if top + 1 >= inner.height {
            break;
        }
        let r = Rect {
            x: inner.x,
            y: inner.y + top,
            width: inner.width,
            height: 2,
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
        let lines = vec![
            Line::from(vec![
                Span::styled(item.label.clone(), style),
                Span::styled(format!(" ({})", item.key), Style::default().fg(p.key)),
            ]),
            Line::from(Span::styled(
                format!("  {}", item.note),
                Style::default().fg(p.dim),
            )),
        ];
        f.render_widget(Paragraph::new(lines).style(style), r);
        // Only the label row is a click target: the note under it belongs
        // to the same line, and a two-row target is easy to hit by mistake.
        rows.push((Rect { height: 1, ..r }, ButtonId::ConflictMenuRow(i)));
    }
    if inner.height > 0 {
        let hint = Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Esc leaves it alone",
                Style::default().fg(p.faint),
            ))),
            hint,
        );
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
/// One problem, laid out: the claim, then each reason under the one it
/// explains, with names and types picked out of the prose.
///
/// The status bar already carries the message for the cursor line, in
/// one line. This is the other half of the same information — for the
/// message that does not fit in one line, and whose real answer is three
/// levels inside the sentence. See [`crate::explain`].
fn draw_problem(f: &mut Frame, app: &App, area: Rect) {
    let Overlay::Problem(panel) = &app.overlay else {
        return;
    };
    let p = palette();
    let color = match panel.severity {
        1 => p.err,
        2 => p.warn,
        _ => p.hint,
    };
    let mark = match panel.severity {
        1 => '✗',
        2 => '▲',
        3 => 'ℹ',
        _ => '·',
    };

    // Lay the rows out first, so the box can be the size of what is in
    // it rather than a guess that clips the last line.
    let w = (area.width.saturating_sub(8)).clamp(30, 88);
    let inner_w = w.saturating_sub(4) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, row) in panel.rows.iter().enumerate() {
        // The claim carries the severity mark; every reason under it is
        // introduced by an elbow, so the chain is visible as a shape and
        // not only as indentation.
        let lead = if i == 0 {
            format!("{mark} ")
        } else {
            format!("{}└ ", "  ".repeat(row.depth))
        };
        let mut spans = vec![Span::styled(
            lead.clone(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )];
        let hang = " ".repeat(disp_width(&lead));
        let mut used = disp_width(&lead);
        for part in &row.parts {
            let (text, style) = match part {
                crate::explain::Part::Text(t) => (t.clone(), Style::default().fg(p.text)),
                crate::explain::Part::Quoted(t) => (
                    t.clone(),
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ),
            };
            // A wrapped type arrives with newlines already in it; every
            // other part is wrapped here, to the width of the box.
            for (n, chunk) in text.split('\n').enumerate() {
                if n > 0 {
                    lines.push(Line::from(std::mem::take(&mut spans)));
                    spans.push(Span::raw(hang.clone()));
                    used = hang.len();
                }
                for word in wrap_words(chunk, inner_w.saturating_sub(hang.len()).max(8)) {
                    if used + disp_width(&word) > inner_w && used > hang.len() {
                        lines.push(Line::from(std::mem::take(&mut spans)));
                        spans.push(Span::raw(hang.clone()));
                        used = hang.len();
                    }
                    used += disp_width(&word);
                    spans.push(Span::styled(word, style));
                }
            }
        }
        lines.push(Line::from(spans));
    }
    if let Some(code) = &panel.code {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("{code} · line {}", panel.line),
            Style::default().fg(p.faint),
        )));
    }
    if panel.of > 1 {
        lines.push(Line::from(Span::styled(
            format!(
                "{} more on this line — Alt+E lists them",
                panel.of.saturating_sub(1)
            ),
            Style::default().fg(p.faint),
        )));
    }
    lines.push(Line::from(Span::styled(
        "any key closes",
        Style::default().fg(p.faint),
    )));

    let h = (lines.len() as u16 + 2).min(area.height);
    let rect = centered(area, w, h);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(format!(
            " {} ",
            match panel.severity {
                1 => "Error",
                2 => "Warning",
                3 => "Note",
                _ => "Hint",
            }
        ));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_widget(Paragraph::new(lines), inner);
}

/// Split text into words, keeping the space that followed each one, so
/// re-joining them reproduces the line.
fn wrap_words(text: &str, _width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    for ch in text.chars() {
        word.push(ch);
        if ch == ' ' {
            out.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

fn draw_menu(f: &mut Frame, app: &mut App, area: Rect) {
    let Overlay::Menu(menu) = &app.overlay else {
        return;
    };
    let p = palette();
    let title = menu.title.clone();
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
    let w = ((label_w + hint_w + 5).max(disp_width(&title)) as u16 + 2).min(area.width);
    let want = menu.rows.len() as u16 + 2;
    let (ax, ay) = menu.anchor;
    // Hang below the anchor, pulled left until the whole panel is on
    // screen. The ☰ menu is shortened and scrolls rather than flipped up
    // over the top bar — the ☰ it belongs to has to stay visible. A menu
    // opened at the pointer has no such button, so it flips instead: a
    // right click near the last line would otherwise throw its menu to
    // the top of the screen, nowhere near what was clicked.
    let x = ax.min(area.x + area.width.saturating_sub(w));
    let below = (area.y + area.height).saturating_sub(ay + 1);
    let (y, h) = if below >= want || below >= MENU_MIN_H {
        (ay + 1, want.min(below))
    } else if menu.flip {
        let h = want.min(area.height).min(ay.saturating_sub(area.y));
        (ay.saturating_sub(h).max(area.y), h.max(1))
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
    // The title says which review this joins, so "add to review" is not a
    // button whose effect has to be guessed at.
    let held = app.pending.len();
    let into = match held {
        0 => " · Ctrl+S starts a review".to_string(),
        n => format!(" · joins {n} held"),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette().accent))
        .title(format!(
            " 💬 Comment · {}:{} ({side}){into} ",
            draft.path, range
        ));
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
            ("✎ Add to review (Ctrl+S)", ButtonId::CommentHold, true),
            ("Post now (Ctrl+Enter)", ButtonId::CommentPost, false),
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
/// The fixes and refactors a language server offers, centred over the
/// editor. A list rather than a set of hotkeys: their titles are sentences
/// a server wrote, and there is no letter to give them.
fn draw_code_actions(f: &mut Frame, app: &App, area: Rect) {
    let Overlay::CodeActions(menu) = &app.overlay else {
        return;
    };
    let p = palette();
    let w = menu
        .actions
        .iter()
        .map(|a| disp_width(&a.title) + 6)
        .max()
        .unwrap_or(30)
        .clamp(30, area.width.saturating_sub(8) as usize) as u16;
    let h = (menu.actions.len() as u16 + 2).min(area.height.saturating_sub(4));
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.accent))
        .title(" Fixes and refactors ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let lines: Vec<Line> = menu
        .actions
        .iter()
        .enumerate()
        .take(inner.height as usize)
        .map(|(i, a)| {
            let base = if i == menu.sel {
                Style::default().bg(p.row).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            // How many files it touches, because "extract function" over
            // one file and over nine are different decisions.
            let n = a.edits.len();
            let where_ = if n > 1 {
                format!(" ({n} files)")
            } else {
                String::new()
            };
            Line::from(vec![
                Span::styled(if i == menu.sel { " ▸ " } else { "   " }, base.fg(p.accent)),
                Span::styled(a.title.clone(), base.fg(p.text)),
                Span::styled(where_, base.fg(p.dim)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

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
        row(
            ("double-click a word", "select it (in the editor)"),
            ("right-click the editor", "the code menu for that word"),
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
        Line::from(Span::styled(
            "Pinned files — the tab row at the top",
            head,
        )),
        row(
            ("drag a file onto loupe", "pin it and read it, from anywhere"),
            ("Ctrl+O", "open a file by path instead"),
        ),
        row(
            ("=", "pin / unpin the file you are on"),
            ("-", "unpin the one you are reading"),
        ),
        row(
            ("1 … 9", "open that tab"),
            (", / .", "previous / next tab"),
        ),
        row(
            ("click a tab / its ✕", "open it / unpin it"),
            ("Alt+ the same keys", "from inside the editor"),
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
        row(
            ("F12 / F10", "the same two, on function keys"),
            ("F3 / Shift+F3", "next / previous match"),
        ),
        Line::from(""),
        Line::from(Span::styled("Act", head)),
        row(
            ("V", "select lines (j/k extends)"),
            ("c", "comment on the selection"),
        ),
        row(
            ("Ctrl+S (in a comment)", "hold it for one review"),
            ("Ctrl+Enter", "post that comment on its own"),
        ),
        row(
            ("R", "the review box — summary + verdict"),
            ("Ctrl+S / Tab (in it)", "submit / change the verdict"),
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
            ("Y", "copy the context for your agent"),
            ("", ""),
        ),
        row(
            ("u", "revert the change at the cursor"),
            ("U", "revert every change in the file"),
        ),
        row(
            ("o", "resolve the conflict at the cursor"),
            ("click ⚑ / [!]", "the same, with the mouse"),
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
            "  Editor + language server: suggestions appear as you type (Tab takes one) · Ctrl+Space asks now · Ctrl+G what is this? · Ctrl+T format",
            dim,
        )),
        Line::from(Span::styled(
            "  Problems: ✗ error (red) · ▲ warning (yellow) · the span is underlined and the message sits in the margin · Alt+X explains one · F8 walks them · Alt+E lists them",
            dim,
        )),
        Line::from(Span::styled(
            "  Lint runs beside the compiler — eslint and ruff, the project's own copy first, on the buffer rather than the file on disk. loupe --lsp says what it found.",
            dim,
        )),
        Line::from(Span::styled(
            "  Editor, on the function keys: F12 definition · F10 (or Shift+F12) every use · F2 rename · F8 / Shift+F8 next / previous problem · Alt+E lists them · F1 this help",
            dim,
        )),
        Line::from(Span::styled(
            "  Editor, with the mouse: double-click a word to select it (every other use lights up) · triple-click takes the line · right-click asks the language server about it",
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
        Line::from(Span::styled(
            "  Reviewing a PR: c writes a line comment, and Ctrl+S holds it rather than posting it. Held comments show as 💬 in the change bar and in the file panel.",
            dim,
        )),
        Line::from(Span::styled(
            "  R opens the review box under the file list: write the summary, pick Comment / Approve / Request changes on the button, and Ctrl+S sends the whole thing as one review.",
            dim,
        )),
        Line::from(Span::styled(
            "  Merge conflicts sort to the top of the file panel in red. The diff then shows our version on the left and theirs on the right, one section per conflict:",
            dim,
        )),
        Line::from(Span::styled(
            "  } and { walk them · o keeps one side (o ours · t theirs · b both · e edit by hand) · the last one resolved stages the file, and ↑ ↓ by the branch name count commits against the upstream",
            dim,
        )),
        // What gd / gr / K can actually answer right now, and why not.
        Line::from(Span::styled(format!("  Language servers: {servers}"), dim)),
    ];

    // The box is as tall as the list, rather than a number kept in step
    // with it by hand: a reference that silently loses its last line as
    // it grows is worse than one that is a row taller.
    let rect = centered(
        area,
        108.min(area.width),
        (lines.len() as u16 + 2).min(area.height),
    );
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.accent))
        .title(" Help — loupe ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);
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
    // A language-server request the reader asked for outranks everything
    // else in here: they pressed a key, and the answer can be half a
    // minute away on a server that is still indexing. Silence for that
    // long reads as a key that did nothing.
    if let Some((frame, label)) = app.editor_waiting() {
        let line = Line::from(vec![
            Span::styled(
                format!(" {frame} "),
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(label.to_string(), Style::default().fg(p.key)),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    // In the editor, what the language server says about the cursor line
    // outranks the last status message — it is about the code, not about
    // what loupe just did.
    if let Some(editor) = &app.editor {
        if let Some(d) = editor.diagnostics_here().first() {
            let color = crate::editor::diagnostic_color(d);
            let code = match d.code_label() {
                Some(c) => format!("  {c}"),
                None => String::new(),
            };
            let hints = " Alt+X explains · F8 next · ? help";
            let msg_w = area.width.saturating_sub(hints.chars().count() as u16 + 1) as usize;
            // The quoted names are the part a reader looks for, so they
            // are picked out here the way the panel picks them out.
            // Everything else about this line stays one line: the status
            // bar has exactly one.
            let mut spans = vec![Span::styled(
                format!("{} ", d.mark()),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )];
            let mut used = 2usize;
            let flat = crate::editor::one_line(&d.message);
            for row in crate::explain::rows(&flat) {
                for part in row.parts {
                    let (text, style) = match part {
                        crate::explain::Part::Text(t) => (t, Style::default().fg(color)),
                        crate::explain::Part::Quoted(t) => (
                            t,
                            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                        ),
                    };
                    let text = crate::editor::one_line(&text);
                    if used >= msg_w {
                        break;
                    }
                    let clipped = truncate_pad(&text, disp_width(&text).min(msg_w - used));
                    used += disp_width(&clipped);
                    spans.push(Span::styled(clipped, style));
                }
            }
            if used < msg_w {
                let tail = truncate_pad(&code, msg_w - used);
                spans.push(Span::styled(tail, Style::default().fg(p.faint)));
            }
            spans.push(Span::styled(hints, Style::default().fg(p.faint)));
            f.render_widget(Paragraph::new(Line::from(spans)), area);
            return;
        }
        // Nothing wrong on this line, but something is wrong somewhere:
        // say how much, so a problem off screen isn't invisible.
        if !editor.problems().is_empty() && app.status.is_empty() {
            let mut counts: std::collections::BTreeMap<&'static str, usize> = Default::default();
            for d in editor.problems() {
                *counts.entry(d.label()).or_default() += 1;
            }
            let summary = counts
                .iter()
                .map(|(what, n)| format!("{n} {what}{}", if *n == 1 { "" } else { "s" }))
                .collect::<Vec<_>>()
                .join(" · ");
            let worst_is_error = editor.problems().iter().any(|d| d.is_error());
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
            } else if app.review.focused {
                "Ctrl+S submit · Tab changes the verdict · Esc leaves the box".into()
            } else if !app.pending.is_empty() {
                let n = app.pending.len();
                format!("{n} held · R the review box · c comment · y copy · m menu")
            } else if app.conflict.is_some() {
                // Mid-conflict there is one thing worth doing, so the hint
                // row says how rather than listing everything else.
                "} { next conflict · o resolve · e edit by hand · m menu".into()
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

    /// Render the whole frame to a string, the way most tests here read it.
    fn screen_of(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn changed(path: &str, conflicted: bool) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            previous: None,
            conflicted,
        }
    }

    /// The two panels share their rows and mean different things by them.
    /// The Changes panel row is a diff: a staging box, a status letter and
    /// the +/- counts. The Files panel row is a file, and carries none of
    /// them even when the change happens to touch that file.
    #[test]
    fn the_files_panel_lists_files_not_diffs() {
        let _guard = highlight::test_theme_lock();
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.files = vec![ChangedFile {
            path: "src/app.rs".into(),
            status: "modified".into(),
            additions: 10,
            deletions: 2,
            previous: None,
            conflicted: false,
        }];
        app.rebuild_files();
        app.apply_repo_listing(crate::search::RepoListing {
            paths: vec!["src/app.rs".into(), "docs/plan.md".into()],
            ignored_from: 2,
            stubs: Vec::new(),
        });

        let screen = screen_of(&mut app, 110, 24);
        assert!(
            screen.contains("+10 −2"),
            "the Changes panel counts the diff: {screen}"
        );
        assert!(screen.contains("[+]"), "and offers to stage it: {screen}");

        app.set_panel(crate::app::PanelMode::Files);
        app.collapsed().remove("src");
        app.rebuild_entries();
        let screen = screen_of(&mut app, 110, 24);
        assert!(
            screen.contains("app.rs"),
            "the same file is listed: {screen}"
        );
        assert!(
            !screen.contains("+10 −2"),
            "with no counts — this panel is not the change: {screen}"
        );
        assert!(
            !screen.contains("[+]") && !screen.contains("[ ]"),
            "and no staging box: {screen}"
        );
    }

    /// The Files panel keeps its own collapse set. The arrow has to read
    /// that one, or every folder in the repository draws open however the
    /// reader shuts it.
    #[test]
    fn the_files_panel_arrow_follows_the_folder() {
        let _guard = highlight::test_theme_lock();
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.apply_repo_listing(crate::search::RepoListing {
            paths: vec!["src/app.rs".into(), "docs/plan.md".into()],
            ignored_from: 2,
            stubs: Vec::new(),
        });
        app.set_panel(crate::app::PanelMode::Files);

        let screen = screen_of(&mut app, 110, 24);
        assert!(
            screen.contains("\u{25b8} src") && screen.contains("\u{25b8} docs"),
            "a shut folder points right: {screen}"
        );

        app.collapsed().remove("src");
        app.rebuild_entries();
        let screen = screen_of(&mut app, 110, 24);
        assert!(
            screen.contains("\u{25be} src"),
            "and an open one points down: {screen}"
        );
        assert!(
            screen.contains("\u{25b8} docs"),
            "while the folder beside it is unchanged: {screen}"
        );
    }

    /// A conflict is impossible to miss: a heading and a red row at the top
    /// of the file panel, a warning badge in the top bar, and ⚑ markers
    /// down the change bar of the diff.
    #[test]
    fn a_merge_conflict_is_marked_everywhere_it_shows() {
        let _guard = highlight::test_theme_lock();
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.checked_out = true;
        app.local_branch = Some("main".into());
        app.merge_op = Some(crate::gitops::MergeOp::Merge);
        app.files = vec![
            changed("src/merge.rs", true),
            changed("src/other.rs", false),
        ];
        app.rebuild_files();

        let text = "keep\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> feature\nkeep2\n";
        let parsed = crate::conflict::Conflicted::parse(text).unwrap();
        let sides = parsed.sides();
        app.old_content = Some(sides.ours.clone());
        app.new_content = Some(sides.theirs.clone());
        app.diff = Some(FileDiff::compute(Some(&sides.ours), Some(&sides.theirs)));
        app.conflict = Some(crate::app::ConflictView {
            file: std::sync::Arc::new(parsed),
            old_owner: sides.ours_owner,
            new_owner: sides.theirs_owner,
        });
        app.collapse_unchanged = false;
        app.rebuild_display();

        let screen = screen_of(&mut app, 110, 24);
        assert!(screen.contains("⚠ MERGE"), "the top bar warns: {screen}");
        assert!(
            screen.contains("1 MERGE CONFLICT"),
            "the panel heading: {screen}"
        );
        assert!(screen.contains("[!]"), "the row icon warns: {screen}");
        assert!(screen.contains("⚑"), "the change bar marks it: {screen}");
        assert!(
            screen.contains("HEAD") && screen.contains("feature"),
            "the title names both branches: {screen}"
        );
        assert!(
            screen.contains("o resolve"),
            "the status bar says how: {screen}"
        );
        // The marker lines themselves are never drawn.
        assert!(!screen.contains("<<<<<<<"), "{screen}");
        assert!(!screen.contains("======="), "{screen}");
    }

    /// The resolve menu opens where it was asked for and lists its keys.
    #[test]
    fn the_conflict_menu_lists_each_side_with_its_key() {
        let _guard = highlight::test_theme_lock();
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.checked_out = true;
        app.files = vec![changed("merge.rs", true)];
        app.rebuild_files();
        let text = "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> feature\n";
        let parsed = crate::conflict::Conflicted::parse(text).unwrap();
        let sides = parsed.sides();
        app.old_content = Some(sides.ours.clone());
        app.new_content = Some(sides.theirs.clone());
        app.diff = Some(FileDiff::compute(Some(&sides.ours), Some(&sides.theirs)));
        app.conflict = Some(crate::app::ConflictView {
            file: std::sync::Arc::new(parsed),
            old_owner: sides.ours_owner,
            new_owner: sides.theirs_owner,
        });
        app.rebuild_display();
        app.open_conflict_menu(0, 20, 6);

        let screen = screen_of(&mut app, 110, 24);
        assert!(screen.contains("Conflict 1 of 1"), "{screen}");
        assert!(screen.contains("Take ours — HEAD"), "{screen}");
        assert!(screen.contains("Take theirs — feature"), "{screen}");
        assert!(screen.contains("Take both"), "{screen}");
        assert!(screen.contains("Edit it by hand"), "{screen}");
        assert!(screen.contains("(o)") && screen.contains("(t)"), "{screen}");
        // It is clickable: every line records a hit area.
        assert!(app
            .layout
            .buttons
            .iter()
            .any(|(_, id)| matches!(id, ButtonId::ConflictMenuRow(0))));
    }

    /// The ahead / behind counts sit beside the branch name.
    #[test]
    fn the_top_bar_counts_commits_against_the_upstream() {
        let _guard = highlight::test_theme_lock();
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.local_branch = Some("feature".into());
        app.tracking = Some(crate::gitops::Tracking {
            upstream: "origin/feature".into(),
            ahead: 3,
            behind: 2,
        });
        let screen = screen_of(&mut app, 110, 12);
        assert!(screen.contains("↑3"), "commits not pushed: {screen}");
        assert!(screen.contains("↓2"), "commits not pulled: {screen}");
        assert!(screen.contains("origin/feature"), "{screen}");

        // Level with the upstream: one quiet mark instead of two counts.
        app.tracking = Some(crate::gitops::Tracking {
            upstream: "origin/feature".into(),
            ahead: 0,
            behind: 0,
        });
        let screen = screen_of(&mut app, 110, 12);
        assert!(screen.contains("≡ origin/feature"), "{screen}");
        assert!(!screen.contains("↑"), "{screen}");
    }

    fn pr_review_app() -> App {
        let mut app = App::new(crate::app::LaunchMode::Pr, None);
        app.screen = Screen::Review;
        app.local = false;
        app.checked_out = true;
        app.repo = Some("acme/tool".into());
        app.pr = Some(crate::github::PrDetail {
            id: "node".into(),
            number: 42,
            title: "Add the widget".into(),
            head_ref_oid: "a".repeat(40),
            base_ref_oid: "b".repeat(40),
            base_ref_name: "main".into(),
            head_ref_name: "feat".into(),
            url: String::new(),
        });
        app.files = vec![changed("src/a.rs", false), changed("src/b.rs", false)];
        app.rebuild_files();
        let (old, new) = ("one\ntwo\nthree\n", "one\nTWO\nthree\n");
        app.old_content = Some(old.into());
        app.new_content = Some(new.into());
        app.diff = Some(FileDiff::compute(Some(old), Some(new)));
        app.collapse_unchanged = false;
        app.rebuild_display();
        app
    }

    /// An inline comment offers both exits, and says which review it
    /// would join.
    #[test]
    fn a_comment_can_be_held_or_posted_on_its_own() {
        let _guard = highlight::test_theme_lock();
        let mut app = pr_review_app();
        app.selection = Some(crate::diff::Selection::lines(Side::Right, 2, 2));
        app.open_comment();
        let screen = screen_of(&mut app, 110, 24);
        assert!(screen.contains("Add to review (Ctrl+S)"), "{screen}");
        assert!(screen.contains("Post now (Ctrl+Enter)"), "{screen}");
        assert!(screen.contains("Cancel (Esc)"), "{screen}");
        assert!(
            screen.contains("Ctrl+S starts a review"),
            "with none held, this one starts one: {screen}"
        );
        for id in [
            ButtonId::CommentHold,
            ButtonId::CommentPost,
            ButtonId::CommentCancel,
        ] {
            assert!(
                app.layout.buttons.iter().any(|(_, b)| *b == id),
                "{id:?} has no hit area"
            );
        }

        // With comments already held, it says which review it joins.
        app.pending.push(crate::github::ReviewComment {
            path: "src/a.rs".into(),
            side: crate::github::CommentSide::Right,
            line: 1,
            start_line: None,
            body: "first".into(),
        });
        let screen = screen_of(&mut app, 110, 24);
        assert!(screen.contains("joins 1 held"), "{screen}");
    }

    /// The composer sits at the foot of the file panel with the verdict on
    /// its button, and held comments are marked wherever they were left.
    #[test]
    fn the_review_box_shows_what_is_held_and_what_it_will_send() {
        let _guard = highlight::test_theme_lock();
        let mut app = pr_review_app();
        // Nothing held yet: the box still invites a summary.
        let screen = screen_of(&mut app, 110, 24);
        assert!(screen.contains("Review"), "{screen}");
        assert!(screen.contains("Summary of this review"), "{screen}");
        // The emoji is a wide char, so the test grid reads it as two
        // cells — assert on the word beside it, not the spacing.
        assert!(
            screen.contains("Comment  ▾"),
            "the default verdict: {screen}"
        );
        assert!(screen.contains("c holds a line comment"), "{screen}");

        app.pending.push(crate::github::ReviewComment {
            path: "src/a.rs".into(),
            side: crate::github::CommentSide::Right,
            line: 2,
            start_line: None,
            body: "rename this".into(),
        });
        app.review.verdict = Verdict::Approve;
        let screen = screen_of(&mut app, 110, 24);
        assert!(screen.contains("Review · 1 held"), "{screen}");
        assert!(screen.contains("1 comment held"), "{screen}");
        assert!(screen.contains("✕ Discard"), "{screen}");
        assert!(
            screen.contains("Approve  ▾"),
            "the button follows the verdict: {screen}"
        );
        // The held comment is marked in the file panel and in the diff.
        assert!(
            screen
                .lines()
                .any(|l| l.contains("a.rs") && l.contains('💬')),
            "the file row counts it: {screen}"
        );
        assert!(
            screen
                .lines()
                .any(|l| l.contains("💬") && l.contains("two")),
            "the change bar marks the line it is on: {screen}"
        );
        // Every control is clickable.
        for id in [
            ButtonId::ReviewBody,
            ButtonId::ReviewSubmit,
            ButtonId::ReviewVerdict,
            ButtonId::ReviewDiscard,
        ] {
            assert!(
                app.layout.buttons.iter().any(|(_, b)| *b == id),
                "{id:?} has no hit area"
            );
        }
    }

    /// The ▾ offers the other two verdicts.
    #[test]
    fn the_verdict_dropdown_lists_all_three() {
        let _guard = highlight::test_theme_lock();
        let mut app = pr_review_app();
        app.activate(ButtonId::ReviewVerdict);
        let screen = screen_of(&mut app, 110, 24);
        assert!(screen.contains("Send as"), "{screen}");
        assert!(screen.contains("Comment"), "{screen}");
        assert!(screen.contains("Approve"), "{screen}");
        assert!(screen.contains("Request changes"), "{screen}");
        assert!(app
            .layout
            .buttons
            .iter()
            .any(|(_, id)| matches!(id, ButtonId::VerdictRow(2))));
    }

    /// The prompt says what is about to be sent, comment by comment.
    #[test]
    fn the_submit_prompt_lists_every_held_comment() {
        let _guard = highlight::test_theme_lock();
        let mut app = pr_review_app();
        for (line, body) in [(2usize, "rename this"), (3, "and this one too")] {
            app.pending.push(crate::github::ReviewComment {
                path: "src/a.rs".into(),
                side: crate::github::CommentSide::Right,
                line,
                start_line: None,
                body: body.into(),
            });
        }
        app.review.verdict = Verdict::Approve;
        app.ask_submit_review();
        let screen = screen_of(&mut app, 110, 24);
        assert!(screen.contains("Send this review to PR #42"), "{screen}");
        assert!(screen.contains("Approve  ·"), "{screen}");
        assert!(screen.contains("2 inline comments"), "{screen}");
        assert!(screen.contains("src/a.rs:2"), "{screen}");
        assert!(screen.contains("rename this"), "{screen}");
        assert!(screen.contains("src/a.rs:3"), "{screen}");
        assert!(screen.contains("cannot be undone"), "{screen}");
        assert!(
            screen.contains("Send (Enter)") && screen.contains("Cancel (Esc)"),
            "{screen}"
        );
    }

    /// A short terminal keeps the file list rather than the composer: a
    /// two-row file panel would be the worse trade.
    #[test]
    fn a_short_terminal_drops_the_review_box() {
        let _guard = highlight::test_theme_lock();
        let mut app = pr_review_app();
        let screen = screen_of(&mut app, 110, 10);
        assert!(!screen.contains("Summary of this review"), "{screen}");
        assert!(
            screen.contains("src/a.rs"),
            "the review itself still draws: {screen}"
        );
        // …and nothing is left claiming a click.
        assert!(!app
            .layout
            .buttons
            .iter()
            .any(|(_, id)| *id == ButtonId::ReviewSubmit));
    }

    /// Local review has no pull request to say anything about.
    #[test]
    fn local_review_has_no_review_box() {
        let _guard = highlight::test_theme_lock();
        let mut app = pr_review_app();
        app.local = true;
        app.pr = None;
        let screen = screen_of(&mut app, 110, 24);
        assert!(!screen.contains("Summary of this review"), "{screen}");
    }

    /// A review with two pinned files: one in the repository, one from
    /// somewhere else on the machine.
    fn pinned_app() -> App {
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.repo_root = "/repo".into();
        app.files = vec![changed("src/app.rs", false)];
        app.rebuild_files();
        app.pins
            .add(crate::pins::Pin::new(
                std::path::Path::new("/repo"),
                "/repo/docs/PLAN.md".into(),
            ))
            .unwrap();
        app.pins
            .add(crate::pins::Pin::new(
                std::path::Path::new("/repo"),
                "/home/me/Downloads/review.md".into(),
            ))
            .unwrap();
        app
    }

    /// The row names every tab, numbers them for the keyboard, marks the
    /// one that is not in the repository, and offers a ✕ on each.
    #[test]
    fn the_tab_row_lists_the_pinned_files() {
        let _guard = highlight::test_theme_lock();
        let mut app = pinned_app();
        let screen = screen_of(&mut app, 90, 20);
        let row = screen.lines().nth(1).expect("the row under the top bar");
        assert!(row.contains("1 PLAN.md"), "{row}");
        assert!(row.contains("2 ↗ review.md"), "{row}");
        assert_eq!(row.matches('✕').count(), 2, "one close mark each: {row}");
    }

    /// The row is the only place a reader is told which files are open,
    /// which one a click is about to replace, and which hold work that is
    /// not on disk yet. Italic says the first, a dot says the second, and
    /// the dot has to read on the selected tab as well as the rest —
    /// the selected tab is the one being typed into.
    #[test]
    fn the_tab_row_marks_the_peek_tab_and_unsaved_work() {
        let _guard = highlight::test_theme_lock();
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.open_buffer(crate::editor::Editor::new(
            "src/kept.rs",
            "src/kept.rs".into(),
            "one\n",
        ));
        app.open_buffer(crate::editor::Editor::new(
            "src/glance.rs",
            "src/glance.rs".into(),
            "two\n",
        ));
        app.set_peek_for_test("src/glance.rs");
        // Back to the first file, which is the case the dot has to
        // survive: the tab being typed into is the selected one.
        app.switch_buffer("src/kept.rs");
        app.editor.as_mut().unwrap().dirty = true;

        let mut term = Terminal::new(TestBackend::new(90, 20)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        let row: String = (0..buf.area.width).map(|x| buf[(x, 1)].symbol()).collect();
        assert!(
            row.contains("● kept.rs"),
            "the unsaved one has a dot, spaced off the name: {row}"
        );
        assert!(
            !row.contains("● glance.rs"),
            "the saved one does not: {row}"
        );
        assert!(row.contains('│'), "a seam divides the tabs: {row}");
        assert_eq!(row.matches('✕').count(), 2, "one close mark each: {row}");

        let cell = |s: &str| {
            let at = row.find(s).unwrap_or_else(|| panic!("no {s}: {row}"));
            buf[(at as u16, 1)].style()
        };
        // Italic on the peek tab, and on that one only.
        let italic = |s: &str| cell(s).add_modifier.contains(Modifier::ITALIC);
        assert!(italic("glance.rs"), "the peek tab is italic: {row}");
        assert!(!italic("kept.rs"), "the kept tab is not: {row}");

        // The dot is not the color it is sitting on. `kept.rs` is the
        // buffer on screen, so its dot is on the selected tab's blue —
        // which is where a single dot color used to disappear.
        let dot = cell("●");
        assert_ne!(
            dot.fg, dot.bg,
            "the unsaved mark is visible on the selected tab"
        );
        assert_eq!(
            dot.fg,
            Some(crate::theme::palette().tab_dirty_active),
            "and it is the color meant for that background"
        );
    }

    /// Click, hold and drag a tab to put it where you want it. Driven
    /// through the mouse handler rather than the move itself, because the
    /// row's click targets are rebuilt on every draw and the drag is the
    /// one thing that reads them while they are changing underneath it.
    #[test]
    fn a_tab_can_be_dragged_to_a_new_place_in_the_row() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let _guard = highlight::test_theme_lock();
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        for path in ["a.rs", "b.rs", "c.rs"] {
            app.open_buffer(crate::editor::Editor::new(path, path.into(), "one\n"));
        }
        let labels = |app: &App| -> Vec<String> {
            app.buffer_tabs().iter().map(|t| t.label.clone()).collect()
        };
        assert_eq!(labels(&app), vec!["a.rs", "b.rs", "c.rs"]);

        // The x of each tab, read off the row the draw just laid out.
        let mut term = Terminal::new(TestBackend::new(90, 20)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let tab_x = |app: &App, i: usize| -> u16 {
            app.layout
                .buttons
                .iter()
                .find(|(_, id)| matches!(id, ButtonId::BufferTab(n) if *n == i))
                .map(|(r, _)| r.x + 1)
                .unwrap_or_else(|| panic!("no rect for tab {i}"))
        };
        let y = app.layout.pin_row.y;
        let (first, last) = (tab_x(&app, 0), tab_x(&app, 2));

        let ev = |kind, x| MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        // Pick the last tab up and drop it on the first.
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), last));
        app.handle_mouse(ev(MouseEventKind::Drag(MouseButton::Left), first));
        app.handle_mouse(ev(MouseEventKind::Up(MouseButton::Left), first));

        assert_eq!(
            labels(&app),
            vec!["c.rs", "a.rs", "b.rs"],
            "the dragged tab took the place it was dropped on"
        );
        assert_eq!(
            app.editor.as_ref().unwrap().path,
            "c.rs",
            "and it is the file on screen, because the press opened it"
        );

        // The drag ended, so moving the pointer over the row again does
        // not carry the tab along with it.
        term.draw(|f| draw(f, &mut app)).unwrap();
        let third = tab_x(&app, 2);
        app.handle_mouse(ev(MouseEventKind::Drag(MouseButton::Left), third));
        assert_eq!(labels(&app), vec!["c.rs", "a.rs", "b.rs"], "nothing moved");
    }

    /// The pane must not blink back to the diff between two files.
    ///
    /// A click on the tree starts a read, and a read takes a frame or
    /// two. Loupe used to close the old buffer at the click and open the
    /// new one when the read landed, so every frame in between drew a
    /// pane with nothing in it — which is the diff. Clicking down a list
    /// of ten files flashed the whole window ten times.
    ///
    /// This is that complaint measured: draw the frames the reader would
    /// actually see, in order, and read what is on them.
    #[test]
    fn switching_files_in_the_tree_never_blinks_back_to_the_diff() {
        use crate::app::tests::{click_file, tree_app};
        let _guard = highlight::test_theme_lock();
        let dir = std::env::temp_dir().join(format!("loupe-noflash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut app = tree_app(&dir);
        click_file(&mut app, "src/one.rs");
        assert!(
            screen_of(&mut app, 110, 24).contains("fn one()"),
            "the first file is on screen"
        );

        // The click that starts the second read, and then every frame
        // between it and the file landing.
        // Aimed at the row as the draw just laid it out: the panel has a
        // border, so its first row is not the first row of the window.
        let at = app
            .entries
            .iter()
            .position(|e| match e {
                crate::app::FileEntry::File { src, .. } => app.row_path(*src) == Some("src/two.rs"),
                _ => false,
            })
            .expect("a row for src/two.rs");
        let fl = app.layout.file_list;
        let y = fl.y + (at - app.file_scroll) as u16;
        app.handle_mouse(crate::app::tests::mouse(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            fl.x + 6,
            y,
        ));

        let mut frames = 0;
        loop {
            let screen = screen_of(&mut app, 110, 24);
            assert!(
                screen.contains("fn one()") || screen.contains("fn two()"),
                "frame {frames} shows neither file — the pane fell back:\n{screen}"
            );
            if screen.contains("fn two()") {
                break;
            }
            frames += 1;
            assert!(frames < 500, "the file never landed");
            app.poll_jobs();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        // And the row swapped the file into the tab rather than growing.
        let tabs = app.buffer_tabs();
        assert_eq!(tabs.len(), 1, "one tab, not two");
        assert_eq!(tabs[0].label, "two.rs");
        assert!(tabs[0].peek, "still the tab a click replaces");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Nothing pinned means no row at all — the feature costs no height
    /// until it is used.
    #[test]
    fn no_pins_means_no_tab_row() {
        let _guard = highlight::test_theme_lock();
        let mut app = pinned_app();
        let with = screen_of(&mut app, 90, 20);
        app.pins.items.clear();
        let without = screen_of(&mut app, 90, 20);
        assert!(with.lines().nth(1).unwrap().contains("PLAN.md"));
        assert!(
            !without.lines().nth(1).unwrap().contains("PLAN.md"),
            "the row is gone and the review moved up"
        );
    }

    /// A row too narrow for every tab says how many it could not draw,
    /// rather than quietly showing fewer than are pinned.
    #[test]
    fn a_narrow_tab_row_admits_what_it_hides() {
        let _guard = highlight::test_theme_lock();
        let mut app = pinned_app();
        let screen = screen_of(&mut app, 24, 20);
        let row = screen.lines().nth(1).expect("the row");
        assert!(row.contains('›'), "it marks the ones off the end: {row}");
    }

    /// The help overlay lists every key, and it all has to fit on one
    /// screen: a reference that scrolls is a reference nobody reads.
    #[test]
    fn the_help_fits_on_one_screen() {
        let _guard = highlight::test_theme_lock();
        let mut app = pinned_app();
        app.overlay = crate::app::Overlay::Help;
        let screen = screen_of(&mut app, 120, 80);
        // The last group is the one most at risk of being cut off.
        // The very last line of the overlay, which is what one row too
        // many would push off the bottom.
        assert!(
            screen.contains("Language servers:"),
            "the end of the list is still drawn"
        );
        assert!(screen.contains("pin / unpin the file you are on"));
        assert!(screen.contains("drag a file onto loupe"));
    }

    /// The 📌 button is two columns wide, and the toolbar has to leave it
    /// exactly that: an emoji counted as one column paints over its
    /// neighbour.
    #[test]
    fn the_pin_button_sits_in_the_toolbar_without_overlap() {
        let _guard = highlight::test_theme_lock();
        let mut app = pinned_app();
        let bar = screen_of(&mut app, 110, 20)
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert!(bar.contains("📌"), "the button is drawn: {bar}");
        // Each toolbar button keeps a blank column on either side, so the
        // pin never runs into Edit before it or ⟳ after it.
        assert!(bar.contains(" 📌 "), "it keeps its padding: {bar}");
        assert!(bar.contains("✎ Edit"), "and Edit is still whole: {bar}");
        assert!(bar.contains("⟳"), "and so is refresh: {bar}");
    }

    /// The path box says what it accepts, and offers both buttons.
    #[test]
    fn the_open_path_box_explains_itself() {
        let _guard = highlight::test_theme_lock();
        let mut app = pinned_app();
        app.open_path_box(crate::app::PathBoxKind::Open);
        let screen = screen_of(&mut app, 90, 20);
        assert!(screen.contains("Open a file"), "{screen}");
        assert!(screen.contains("does not have to be in the repository"));
        assert!(screen.contains("Open (Enter)"));
        assert!(screen.contains("Cancel (Esc)"));
    }

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
            conflicted: false,
        }];
        app.rebuild_files();
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
            conflicted: false,
        }];
        app.rebuild_files();
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
            conflicted: false,
        }];
        app.rebuild_files();
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
                conflicted: false,
            })
            .collect();
        app.stage.insert("a.rs".into(), StageState::Unstaged);
        app.stage.insert("b.rs".into(), StageState::Partial);
        app.stage.insert("c.rs".into(), StageState::Staged);
        app.rebuild_files();

        let buf = render(&mut app);
        let panel: String = (1..8).map(|y| row_text(&buf, y)).collect();
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
            row_text(&buf, 1).contains("2/3 staged"),
            "title counts what is in the index, partly staged files included"
        );
        // The two sections, and the file in each of them.
        assert!(
            panel.contains("STAGED 2") && panel.contains("UNSTAGED 1"),
            "the headings count each section: {panel:?}"
        );

        // A pull-request review has no index to divide: no headings, and
        // the viewed checkbox is back.
        app.local = false;
        app.rebuild_files();
        let buf = render(&mut app);
        let panel: String = (1..5).map(|y| row_text(&buf, y)).collect();
        assert!(
            !panel.contains("STAGED"),
            "no staging sections on a PR: {panel:?}"
        );
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
    /// The file panel used to index `app.files` straight from the row, so
    /// a row that named a file outside the change would have panicked. It
    /// now asks, gets `None`, and draws a plain name with none of the
    /// marks — no stage box, no status letter, no counts, because there is
    /// no diff behind it for any of them to describe.
    #[test]
    fn a_row_outside_the_change_draws_without_marks() {
        let mut app = wide_app();
        app.repo_paths = vec!["src/untouched.rs".into()];
        app.entries.push(crate::app::FileEntry::File {
            src: crate::app::RowSrc::Path(0),
            depth: 0,
        });

        let buf = render(&mut app);
        let row = (0..buf.area.height)
            .map(|y| row_text(&buf, y))
            .find(|r| r.contains("untouched.rs"))
            .expect("the row outside the change is drawn");

        let panel: String = row.chars().take(app.file_panel_w as usize).collect();
        assert!(
            !panel.contains('['),
            "no stage or viewed box on a file the change never touched: {panel:?}"
        );
        assert!(
            !panel.contains('+') && !panel.contains('−'),
            "no line counts either: {panel:?}"
        );
    }

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
            conflicted: false,
        }];
        app.rebuild_files();
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
            conflicted: false,
        }];
        app.rebuild_files();
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
            conflicted: false,
        }];
        app.rebuild_files();
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
            conflicted: false,
        }];
        app.rebuild_files();
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
                conflicted: false,
            },
            ChangedFile {
                path: "src/ui/render.rs".into(),
                status: "modified".into(),
                additions: 1,
                deletions: 0,
                previous: None,
                conflicted: false,
            },
        ];
        app.rebuild_files();
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
            conflicted: false,
        }];
        app.rebuild_files();
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

    /// Four things say a problem at once, and each of them has to
    /// actually reach the screen: the gutter mark, the underline under
    /// the span, the message in the margin, and the color that says how
    /// bad it is.
    #[test]
    fn a_problem_is_marked_underlined_and_named_in_the_margin() {
        let _guard = highlight::test_theme_lock();
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        let mut ed = crate::editor::Editor::new(
            "a.ts",
            "a.ts".into(),
            "const x = totl;
const y = 2;
",
        );
        ed.set_server_diagnostics(vec![crate::lsp::Diagnostic {
            line: 1,
            col: 11,
            end_col: 15,
            severity: 1,
            message: "Cannot find name 'totl'.".into(),
            code: Some("2552".into()),
            source: Some("typescript".into()),
        }]);
        ed.set_lint_diagnostics(vec![crate::lsp::Diagnostic {
            line: 2,
            col: 7,
            end_col: 8,
            severity: 2,
            message: "'y' is never used.".into(),
            code: Some("no-unused-vars".into()),
            source: Some("eslint".into()),
        }]);
        // The cursor is parked on the blank third line: the cursor's own
        // row is underlined too, and this test is about the underline
        // the *diagnostic* draws.
        ed.jump_to_line(3);
        app.open_buffer(ed);

        let buf = render(&mut app);
        let p = palette();
        let all: String = (0..buf.area.height)
            .map(|y| row_text(&buf, y))
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        assert!(all.contains('✗'), "the error is marked: {all}");
        assert!(all.contains('▲'), "and the warning differently: {all}");
        assert!(
            all.contains("Cannot find name"),
            "the message is in the margin: {all}"
        );
        assert!(all.contains("'y' is never used."), "{all}");

        // The span itself: red and underlined for the error, yellow for
        // the lint warning.
        let underlined_in = |color| {
            (0..buf.area.height).any(|y| {
                (0..buf.area.width).any(|x| {
                    let cell = &buf[(x, y)];
                    cell.fg == color
                        && cell.modifier.contains(Modifier::UNDERLINED)
                        && cell.symbol() != " "
                })
            })
        };
        assert!(
            underlined_in(p.err),
            "the bad name is red and underlined — color alone is not readable to everyone"
        );
        assert!(
            underlined_in(p.warn),
            "and the lint warning is yellow, not red"
        );
    }

    /// A message too long for the status bar is laid out: the claim, the
    /// reason under it, and the names picked out of the prose. This is
    /// the whole reason the panel exists.
    #[test]
    fn the_problem_panel_lays_the_reasoning_out() {
        let _guard = highlight::test_theme_lock();
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        let mut ed = crate::editor::Editor::new("a.ts", "a.ts".into(), "const s: string = 1;\n");
        ed.set_server_diagnostics(vec![crate::lsp::Diagnostic {
            line: 1,
            col: 7,
            end_col: 8,
            severity: 1,
            message: "Type 'A' is not assignable to type 'B'.\n  Types of property 'a' are \
                      incompatible.\n    Type 'number' is not assignable to type 'string'."
                .into(),
            code: Some("2322".into()),
            source: Some("typescript".into()),
        }]);
        app.open_buffer(ed);
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));

        let buf = render(&mut app);
        let all: String = (0..buf.area.height)
            .map(|y| row_text(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("Error"), "the box says how bad it is: {all}");
        assert!(all.contains("not assignable"), "{all}");
        assert!(
            all.contains("Types of property"),
            "the reason comes with it: {all}"
        );
        assert!(all.contains("└"), "and it hangs under the claim: {all}");
        assert!(all.contains("typescript(2322)"), "with who said so: {all}");
        // Any key puts it away.
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert!(matches!(app.overlay, crate::app::Overlay::None));
    }

    /// A cold language server can take half a minute to answer. The
    /// status bar has to say so for the whole wait, or the key that
    /// asked reads as a key that did nothing.
    #[test]
    fn the_status_bar_names_what_the_editor_is_waiting_for() {
        let _guard = highlight::test_theme_lock();
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.open_buffer(crate::editor::Editor::new(
            "src/a.rs",
            "src/a.rs".into(),
            "fn count_items() {}\n",
        ));
        app.set_editor_waiting_for_test("Finding the definition of count_items…");

        let buf = render(&mut app);
        let status = row_text(&buf, buf.area.height - 1);
        assert!(
            status.contains("Finding the definition of count_items"),
            "the wait is invisible: {status:?}"
        );
        assert!(
            app.searching(),
            "and the main loop keeps ticking, so the spinner turns"
        );
    }

    /// The editor's right-click menu names the word it is about and
    /// flips above the pointer when a click near the last line would
    /// otherwise throw it to the top of the screen.
    #[test]
    fn the_code_menu_names_the_word_and_flips_up() {
        let _guard = highlight::test_theme_lock();
        let mut app = App::new(crate::app::LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.open_buffer(crate::editor::Editor::new(
            "src/a.rs",
            "src/a.rs".into(),
            "fn count_items() {}\n",
        ));
        // One frame first: the editor learns its own rectangle from it,
        // and the click needs that to land on a buffer position.
        render(&mut app);

        // A right click on `count_items`, through the same path a real
        // one takes: the click takes the word, and the menu is about it.
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Right),
            column: 44,
            row: 3,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        let buf = render(&mut app);
        let all: String = (0..buf.area.height)
            .map(|y| row_text(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all.contains("⌁ count_items"),
            "the title names the word: {all}"
        );
        assert!(
            all.contains("Go to the definition"),
            "and the menu offers the lookup: {all}"
        );
        assert!(all.contains("F12"), "the key is shown beside it: {all}");
        // Flipped: the box ends at the pointer rather than starting at
        // the top of the screen.
        assert!(
            !row_text(&buf, 18).contains("Go to the definition"),
            "nothing is drawn below the pointer"
        );
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
            conflicted: false,
        }];
        app.rebuild_files();
        let menu = || {
            Box::new(crate::app::PathMenu {
                path: "src/app.rs".into(),
                is_dir: false,
                items: vec![
                    crate::app::PathMenuItem {
                        key: 'r',
                        label: "Copy relative path",
                        action: crate::app::PathAction::Copy("src/app.rs".into()),
                    },
                    crate::app::PathMenuItem {
                        key: 'f',
                        label: "Copy full path",
                        action: crate::app::PathAction::Copy("/repo/src/app.rs".into()),
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
            conflicted: false,
        }];
        app.rebuild_files();
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
            conflicted: false,
        }];
        app.rebuild_files();
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
            conflicted: false,
        }];
        app.rebuild_files();
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

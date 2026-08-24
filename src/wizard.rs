//! First-launch setup wizard: a big logo, a live theme picker, and a default
//! mode choice, ending with the config file written for the user. Runs when
//! no global config exists yet, and on demand via `loupe setup`.
//!
//! Deliberately self-contained: it owns its own tiny event loop, draw
//! functions, and hit-testing, so it can run before [`crate::app::App`]
//! exists and never entangles with review state.

use crate::config;
use crate::highlight::{self, HlLine};
use crate::theme::{self, palette, Appearance};
use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use std::time::Duration;

pub enum WizardEnd {
    /// Setup finished; the config file holds the choices.
    Done,
    /// Setup skipped; a marker config was written so it won't re-run.
    Skipped,
    /// The user quit outright — don't launch the app.
    Quit,
}

/// The block-letter logo, ANSI-Shadow style. 42 columns wide.
pub const LOGO: [&str; 6] = [
    "██╗      ██████╗ ██╗   ██╗██████╗ ███████╗",
    "██║     ██╔═══██╗██║   ██║██╔══██╗██╔════╝",
    "██║     ██║   ██║██║   ██║██████╔╝█████╗  ",
    "██║     ██║   ██║██║   ██║██╔═══╝ ██╔══╝  ",
    "███████╗╚██████╔╝╚██████╔╝██║     ███████╗",
    "╚══════╝ ╚═════╝  ╚═════╝ ╚═╝     ╚══════╝",
];

/// One color per logo row — a Catppuccin-flavored gradient. The pastels
/// only hold up against a dark background; on a light one the same hues are
/// used at Latte's darker weights so the logo doesn't wash out.
pub const LOGO_COLORS: [Color; 6] = [
    Color::Rgb(245, 194, 231), // pink
    Color::Rgb(203, 166, 247), // mauve
    Color::Rgb(180, 190, 254), // lavender
    Color::Rgb(137, 180, 250), // blue
    Color::Rgb(116, 199, 236), // sapphire
    Color::Rgb(148, 226, 213), // teal
];

pub const LOGO_COLORS_LIGHT: [Color; 6] = [
    Color::Rgb(234, 118, 203), // pink
    Color::Rgb(136, 57, 239),  // mauve
    Color::Rgb(114, 135, 253), // lavender
    Color::Rgb(30, 102, 245),  // blue
    Color::Rgb(32, 159, 181),  // sapphire
    Color::Rgb(23, 146, 153),  // teal
];

/// The gradient for the active appearance.
pub fn logo_colors() -> [Color; 6] {
    if theme::appearance().is_light() {
        LOGO_COLORS_LIGHT
    } else {
        LOGO_COLORS
    }
}

/// Rust sample the theme step previews — small enough to re-highlight on
/// every selection change, varied enough to show keyword/type/string/number/
/// comment/macro colors.
pub const SAMPLE: &str = r#"use std::collections::HashMap;

/// Orbits, indexed by catalog number.
#[derive(Debug, Default)]
pub struct Catalog {
    orbits: HashMap<u32, Orbit>,
}

impl Catalog {
    pub fn eccentricity(&self, id: u32) -> Option<f64> {
        let orbit = self.orbits.get(&id)?;
        let (ra, rp) = (orbit.apogee_km, orbit.perigee_km);
        Some((ra - rp) / (ra + rp))
    }

    pub fn insert(&mut self, id: u32, orbit: Orbit) {
        println!("tracking #{id}");
        self.orbits.insert(id, orbit);
    }
}
"#;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Welcome,
    Theme,
    Mode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Btn {
    Continue,
    Back,
    Skip,
    /// The ☀/🌙 light-dark switch on the theme step.
    Appearance,
    ThemeRow(usize),
    ModeRow(usize),
}

const MODES: [(&str, &str, &str); 3] = [
    (
        "auto",
        "Auto  (recommended)",
        "review uncommitted changes if there are any, else pull requests",
    ),
    ("pr", "Pull requests", "always go straight to the PR picker"),
    ("local", "Local changes", "always review the working tree"),
];

struct Wizard {
    step: Step,
    theme_sel: usize,
    theme_scroll: usize,
    mode_sel: usize,
    /// Theme active before the wizard started, restored on skip/quit.
    prev_theme: two_face::theme::EmbeddedThemeName,
    /// Appearance before the wizard started, restored on skip/quit.
    prev_appearance: Appearance,
    /// The row selected the last time each appearance was active, as
    /// `[dark, light]` — theme pairing is many-to-one, so without this a
    /// round trip through `a` would not come back to the same theme.
    remembered: [Option<usize>; 2],
    /// Sample highlighted with the currently selected theme.
    preview: Vec<HlLine>,
    /// Clickable regions recorded during the last draw.
    hits: Vec<(Rect, Btn)>,
}

impl Wizard {
    fn new() -> Self {
        let current = highlight::current_theme();
        let theme_sel = highlight::THEMES
            .iter()
            .position(|(_, t)| *t == current)
            .unwrap_or(0);
        let mut w = Wizard {
            step: Step::Welcome,
            theme_sel,
            theme_scroll: 0,
            mode_sel: 0,
            prev_theme: current,
            prev_appearance: theme::appearance(),
            remembered: [None, None],
            preview: Vec::new(),
            hits: Vec::new(),
        };
        w.rehighlight();
        w
    }

    fn selected_theme(&self) -> two_face::theme::EmbeddedThemeName {
        highlight::THEMES[self.theme_sel].1
    }

    /// Apply the selected theme process-wide and refresh the preview — this
    /// is what makes the picker "live".
    fn rehighlight(&mut self) {
        highlight::set_theme(self.selected_theme());
        self.preview = highlight::highlight("sample.rs", SAMPLE);
    }

    fn select_theme(&mut self, idx: usize) {
        self.theme_sel = idx.min(highlight::THEMES.len() - 1);
        self.rehighlight();
    }

    /// Flip light ⇄ dark. The selection follows to the counterpart of the
    /// current theme, so the preview stays a coherent whole rather than a
    /// dark theme sitting in a light frame — or back to whatever was
    /// selected the last time this appearance was active.
    fn toggle_appearance(&mut self) {
        let current = theme::appearance();
        let next = current.other();
        self.remembered[usize::from(current.is_light())] = Some(self.theme_sel);
        theme::set_appearance(next);
        self.theme_sel = self.remembered[usize::from(next.is_light())].unwrap_or_else(|| {
            let paired = highlight::for_appearance(self.selected_theme(), next);
            highlight::THEMES
                .iter()
                .position(|(_, t)| *t == paired)
                .unwrap_or(self.theme_sel)
        });
        self.rehighlight();
    }

    fn finish(&self) -> Result<()> {
        let name = highlight::theme_key(self.selected_theme());
        let appearance = theme::appearance();
        let mut pairs = vec![
            (config::theme_key_for(appearance), name),
            ("mode", MODES[self.mode_sel].0),
        ];
        // Toggling twice is not an override — pinning `appearance` then
        // would turn detection off on every other terminal for nothing.
        if appearance != self.prev_appearance {
            pairs.push(("appearance", appearance.key()));
        }
        config::save_global(&pairs)?;
        Ok(())
    }

    fn skip(&self) -> Result<()> {
        highlight::set_theme(self.prev_theme);
        theme::set_appearance(self.prev_appearance);
        // Mark setup as done so the wizard doesn't nag on every launch.
        config::ensure_global_exists()?;
        Ok(())
    }
}

/// Run the wizard on an already-initialized terminal. Only returns an error
/// for terminal or config-write failures.
pub fn run(terminal: &mut DefaultTerminal) -> Result<WizardEnd> {
    let mut w = Wizard::new();
    loop {
        terminal.draw(|f| draw(f, &mut w))?;
        // Blocking-ish poll: the wizard has no background work.
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                match (w.step, key.code) {
                    // ---- welcome
                    (Step::Welcome, KeyCode::Enter | KeyCode::Char(' ')) => w.step = Step::Theme,
                    (Step::Welcome, KeyCode::Esc | KeyCode::Char('q')) => {
                        w.skip()?;
                        return Ok(WizardEnd::Skipped);
                    }
                    // ---- theme list
                    (Step::Theme, KeyCode::Up | KeyCode::Char('k')) => {
                        w.select_theme(w.theme_sel.saturating_sub(1));
                    }
                    (Step::Theme, KeyCode::Down | KeyCode::Char('j')) => {
                        w.select_theme(w.theme_sel + 1);
                    }
                    (Step::Theme, KeyCode::PageUp) => {
                        w.select_theme(w.theme_sel.saturating_sub(8));
                    }
                    (Step::Theme, KeyCode::PageDown) => w.select_theme(w.theme_sel + 8),
                    (Step::Theme, KeyCode::Home) => w.select_theme(0),
                    (Step::Theme, KeyCode::End) => w.select_theme(usize::MAX),
                    (Step::Theme, KeyCode::Char('a')) => w.toggle_appearance(),
                    (Step::Theme, KeyCode::Enter) => w.step = Step::Mode,
                    (Step::Theme, KeyCode::Esc) => w.step = Step::Welcome,
                    // ---- mode list
                    (Step::Mode, KeyCode::Up | KeyCode::Char('k')) => {
                        w.mode_sel = w.mode_sel.saturating_sub(1);
                    }
                    (Step::Mode, KeyCode::Down | KeyCode::Char('j')) => {
                        w.mode_sel = (w.mode_sel + 1).min(MODES.len() - 1);
                    }
                    (Step::Mode, KeyCode::Enter) => {
                        w.finish()?;
                        return Ok(WizardEnd::Done);
                    }
                    (Step::Mode, KeyCode::Esc) => w.step = Step::Theme,
                    // ---- global
                    (_, KeyCode::Char('q')) => {
                        w.skip()?;
                        return Ok(WizardEnd::Quit);
                    }
                    _ => {}
                }
            }
            Event::Mouse(m) => {
                if let Some(end) = handle_mouse(&mut w, m)? {
                    return Ok(end);
                }
            }
            _ => {}
        }
    }
}

fn handle_mouse(w: &mut Wizard, m: MouseEvent) -> Result<Option<WizardEnd>> {
    let at = |hits: &[(Rect, Btn)], x: u16, y: u16| {
        hits.iter()
            .find(|(r, _)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
            .map(|(_, b)| *b)
    };
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => match at(&w.hits, m.column, m.row) {
            Some(Btn::Continue) => match w.step {
                Step::Welcome => w.step = Step::Theme,
                Step::Theme => w.step = Step::Mode,
                Step::Mode => {
                    w.finish()?;
                    return Ok(Some(WizardEnd::Done));
                }
            },
            Some(Btn::Back) => match w.step {
                Step::Welcome => {}
                Step::Theme => w.step = Step::Welcome,
                Step::Mode => w.step = Step::Theme,
            },
            Some(Btn::Skip) => {
                w.skip()?;
                return Ok(Some(WizardEnd::Skipped));
            }
            Some(Btn::Appearance) => w.toggle_appearance(),
            Some(Btn::ThemeRow(i)) => w.select_theme(i),
            Some(Btn::ModeRow(i)) => w.mode_sel = i,
            None => {}
        },
        MouseEventKind::ScrollDown if w.step == Step::Theme => w.select_theme(w.theme_sel + 1),
        MouseEventKind::ScrollUp if w.step == Step::Theme => {
            w.select_theme(w.theme_sel.saturating_sub(1));
        }
        _ => {}
    }
    Ok(None)
}

// ---------------------------------------------------------------- rendering

fn draw(f: &mut Frame, w: &mut Wizard) {
    w.hits.clear();
    f.render_widget(Clear, f.area());
    match w.step {
        Step::Welcome => draw_welcome(f, w),
        Step::Theme => draw_theme(f, w),
        Step::Mode => draw_mode(f, w),
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

/// The logo (or a plain fallback on very narrow terminals) as colored lines.
pub fn logo_lines(max_width: u16) -> Vec<Line<'static>> {
    let colors = logo_colors();
    if max_width >= 44 {
        LOGO.iter()
            .zip(colors)
            .map(|(row, color)| {
                Line::from(Span::styled(
                    *row,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ))
                .centered()
            })
            .collect()
    } else {
        vec![Line::from(Span::styled(
            "🔍 L O U P E",
            Style::default().fg(colors[1]).add_modifier(Modifier::BOLD),
        ))
        .centered()]
    }
}

fn buttons(f: &mut Frame, w: &mut Wizard, area: Rect, defs: &[(&str, Btn, bool)]) {
    // Display width, not character count: the ☀/🌙 labels are wide, and a
    // count would centre the row a column off and clip the last pad cell.
    let width_of = |s: &str| -> u16 {
        s.chars()
            .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as u16)
            .sum()
    };
    let total: u16 = defs.iter().map(|(l, _, _)| width_of(l) + 3).sum();
    let p = palette();
    let mut x = area.x + (area.width.saturating_sub(total)) / 2;
    for (label, btn, active) in defs {
        let wdt = width_of(label) + 2;
        let rect = Rect {
            x,
            y: area.y,
            width: wdt,
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
            Paragraph::new(Span::styled(format!(" {label} "), style)),
            rect,
        );
        w.hits.push((rect, *btn));
        x += wdt + 1;
    }
}

fn draw_welcome(f: &mut Frame, w: &mut Wizard) {
    let p = palette();
    let area = f.area();
    let rect = centered(area, 58, 15);
    let logo = logo_lines(rect.width);
    let mut lines: Vec<Line> = Vec::new();
    lines.extend(logo);
    lines.push(Line::from(""));
    lines.push(
        Line::from(Span::styled(
            "mouse-first PR review in the terminal",
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        ))
        .centered(),
    );
    lines.push(Line::from(""));
    lines.push(
        Line::from(Span::styled(
            "Let's set loupe up — a theme and a default mode,",
            Style::default().fg(p.dim),
        ))
        .centered(),
    );
    lines.push(
        Line::from(Span::styled(
            "saved to your config so this only happens once.",
            Style::default().fg(p.dim),
        ))
        .centered(),
    );
    f.render_widget(Paragraph::new(lines), rect);

    let btn_area = Rect {
        x: rect.x,
        y: rect.y + rect.height.saturating_sub(1),
        width: rect.width,
        height: 1,
    };
    buttons(
        f,
        w,
        btn_area,
        &[
            ("Get started (Enter)", Btn::Continue, true),
            ("Skip (Esc)", Btn::Skip, false),
        ],
    );
}

fn draw_theme(f: &mut Frame, w: &mut Wizard) {
    let p = palette();
    let area = f.area();
    let rect = centered(area, 88.min(area.width), area.height.min(30));
    // Say which background loupe thinks it is on: the detection is usually
    // right, and when it isn't, `a` is the fix and it is right there.
    let kind = if theme::appearance().is_light() {
        "light terminal"
    } else {
        "dark terminal"
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.accent))
        .title(format!(
            " Pick a theme for your {kind} — the preview is live "
        ))
        .title_bottom(" j/k or click to try · a light/dark · Enter to keep · Esc back ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    // Left: the theme list. Right: the highlighted sample.
    let list_w = 26.min(inner.width / 2);
    let list = Rect {
        x: inner.x,
        y: inner.y,
        width: list_w,
        height: inner.height.saturating_sub(1),
    };
    let preview = Rect {
        x: inner.x + list_w + 1,
        y: inner.y,
        width: inner.width.saturating_sub(list_w + 1),
        height: inner.height.saturating_sub(1),
    };
    // Keep the selection visible.
    let visible = list.height as usize;
    if w.theme_sel < w.theme_scroll {
        w.theme_scroll = w.theme_sel;
    } else if visible > 0 && w.theme_sel >= w.theme_scroll + visible {
        w.theme_scroll = w.theme_sel + 1 - visible;
    }
    let end = highlight::THEMES.len().min(w.theme_scroll + visible);
    for (row, idx) in (w.theme_scroll..end).enumerate() {
        let label = highlight::THEMES[idx].0;
        let rect = Rect {
            x: list.x,
            y: list.y + row as u16,
            width: list.width,
            height: 1,
        };
        let selected = idx == w.theme_sel;
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
            rect,
        );
        w.hits.push((rect, Btn::ThemeRow(idx)));
    }

    draw_sample(f, preview, &w.preview, w.selected_theme());

    let btn_area = Rect {
        x: inner.x,
        y: inner.y + inner.height.saturating_sub(1),
        width: inner.width,
        height: 1,
    };
    let name = highlight::theme_key(w.selected_theme());
    let flip = if theme::appearance().is_light() {
        "🌙 Dark (a)"
    } else {
        "☀ Light (a)"
    };
    buttons(
        f,
        w,
        btn_area,
        &[
            (&format!("Use {name} (Enter)"), Btn::Continue, true),
            (flip, Btn::Appearance, false),
            ("Back (Esc)", Btn::Back, false),
            ("Skip setup", Btn::Skip, false),
        ],
    );
}

/// Render the highlighted sample, with one added and one removed row so the
/// diff backgrounds are previewed against the theme too.
fn draw_sample(
    f: &mut Frame,
    area: Rect,
    preview: &[HlLine],
    theme: two_face::theme::EmbeddedThemeName,
) {
    let p = palette();
    // Fill the panel with the theme's own background, so a light theme
    // looks light here rather than only after you commit to it.
    let sample_bg = highlight::theme_background(theme);
    let sample_lines: Vec<&str> = SAMPLE.lines().collect();
    let mut lines: Vec<Line> = Vec::new();
    for (i, text) in sample_lines.iter().enumerate().take(area.height as usize) {
        let bg = match i {
            11 => Some(p.removed), // the `let (ra, rp)` line reads as removed
            12 => Some(p.added),   // and its successor as added
            _ => sample_bg,
        };
        let spans: Vec<Span> = match preview.get(i) {
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
    f.render_widget(sample, area);
}

fn draw_mode(f: &mut Frame, w: &mut Wizard) {
    let p = palette();
    let area = f.area();
    let rect = centered(area, 66, 13);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.accent))
        .title(" What should `loupe` open by default? ")
        .title_bottom(" j/k or click · Enter to finish · Esc back ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    for (i, (_, title, desc)) in MODES.iter().enumerate() {
        let y = inner.y + (i as u16) * 3;
        let row = Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: 2,
        };
        let selected = i == w.mode_sel;
        let marker = if selected { "▸ " } else { "  " };
        let head = if selected {
            Style::default()
                .bg(p.selected)
                .fg(p.text)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.text).add_modifier(Modifier::BOLD)
        };
        let lines = vec![
            Line::from(Span::styled(format!("{marker}{title}"), head)),
            Line::from(Span::styled(
                format!("    {desc}"),
                Style::default().fg(p.dim),
            )),
        ];
        f.render_widget(Paragraph::new(lines), row);
        w.hits.push((row, Btn::ModeRow(i)));
    }

    let btn_area = Rect {
        x: inner.x,
        y: inner.y + inner.height.saturating_sub(1),
        width: inner.width,
        height: 1,
    };
    buttons(
        f,
        w,
        btn_area,
        &[
            ("Finish setup (Enter)", Btn::Continue, true),
            ("Back (Esc)", Btn::Back, false),
        ],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_rows_align() {
        let widths: Vec<usize> = LOGO
            .iter()
            .map(|r| {
                r.chars()
                    .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
                    .sum()
            })
            .collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}");
        assert_eq!(LOGO.len(), LOGO_COLORS.len());
    }

    #[test]
    fn sample_highlights_and_fits() {
        // The preview must produce real colors for the sample, and the
        // sample must fit the preview pane the theme step allocates.
        let _guard = highlight::test_theme_lock();
        let hl = highlight::highlight("sample.rs", SAMPLE);
        assert_eq!(hl.len(), SAMPLE.lines().count());
        assert!(SAMPLE.lines().count() <= 28);
        let distinct: std::collections::HashSet<String> =
            hl.iter().flatten().map(|(c, _)| format!("{c:?}")).collect();
        assert!(distinct.len() >= 4, "sample should show several colors");
    }

    #[test]
    fn wizard_starts_on_the_current_theme() {
        // `Wizard::new` previews immediately, so it *writes* the global
        // theme as well as reading it — hold the lock and put it back.
        let _guard = highlight::test_theme_lock();
        let before = highlight::current_theme();
        let w = Wizard::new();
        assert_eq!(w.selected_theme(), highlight::current_theme());
        assert_eq!(w.mode_sel, 0, "auto is the default mode");
        highlight::set_theme(before);
    }
}

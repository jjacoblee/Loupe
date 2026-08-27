//! Appearance (light or dark) and the UI color palette.
//!
//! Every color loupe paints itself — diff backgrounds, gutters, buttons,
//! borders, status text — comes from [`palette`], which returns one of two
//! hand-tuned [`Palette`] constants. The syntax colors inside the diff come
//! from the syntect theme instead (see [`crate::highlight`]); the two are
//! kept in step by resolving the appearance first and then picking the
//! matching theme.
//!
//! The appearance is resolved once at startup, in this order:
//!
//! 1. `--light` / `--dark` on the command line
//! 2. `appearance = "light" | "dark"` in the config file
//! 3. the terminal's actual background color, queried with OSC 11
//! 4. `COLORFGBG`, which a few terminals export instead
//! 5. the background of the configured syntax theme
//! 6. dark
//!
//! Steps 3 and 4 are why the defaults are usually right without any config:
//! a light terminal gets light diff backgrounds on its own.

use ratatui::style::Color;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

impl Appearance {
    pub fn is_light(self) -> bool {
        self == Appearance::Light
    }

    /// The config-file name (also what the CLI flags map to).
    pub fn key(self) -> &'static str {
        match self {
            Appearance::Dark => "dark",
            Appearance::Light => "light",
        }
    }

    pub fn other(self) -> Appearance {
        match self {
            Appearance::Dark => Appearance::Light,
            Appearance::Light => Appearance::Dark,
        }
    }

    /// Classify a background color. The weights are the usual perceptual
    /// ones; the threshold sits well clear of both common extremes.
    pub fn of_background(r: u8, g: u8, b: u8) -> Appearance {
        let lum = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
        if lum > 128.0 {
            Appearance::Light
        } else {
            Appearance::Dark
        }
    }
}

// ------------------------------------------------------------------ palette

/// Every color the UI paints itself with. Two instances exist: [`DARK`] and
/// [`LIGHT`]. Syntax colors are *not* here — those come from the syntect
/// theme, which is chosen to match the appearance.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Background of an added line (and the added side of a modification).
    pub added: Color,
    /// Background of a removed line.
    pub removed: Color,
    /// Filler where one side of a side-by-side diff has no line at all.
    pub empty: Color,
    /// Background of a selected diff line.
    pub selected: Color,
    /// Background of text matching the active search.
    pub matched: Color,
    /// Background of the keyboard cursor row (the underline does the real
    /// work; this only makes it easier to find at a glance).
    pub cursor: Color,
    /// Background of the selected row in the file and PR lists.
    pub row: Color,

    /// Primary text — file names, PR titles, code with no highlighting.
    pub text: Color,
    /// Secondary text — counts, notes, explanatory lines.
    pub dim: Color,
    /// Tertiary text — key hints along the status bar.
    pub faint: Color,
    /// Line numbers in the diff gutter.
    pub line_no: Color,
    /// Directory rows in the file tree.
    pub dir: Color,
    /// A file already marked viewed (or staged).
    pub viewed: Color,
    /// The empty `[ ]` checkbox.
    pub checkbox: Color,
    /// `[+]` — changed but not staged.
    pub stage_add: Color,
    /// `[±]` — staged, then edited again.
    pub stage_partial: Color,
    /// The "N unchanged lines" fold banners.
    pub fold: Color,
    /// The seam between the file panel and the diff.
    pub divider: Color,
    /// …while it is being dragged.
    pub divider_active: Color,
    /// Overlay borders and the spinner.
    pub accent: Color,
    /// Key names in the help overlay, and in-progress labels.
    pub key: Color,
    /// A status message that went well.
    pub ok: Color,
    /// …and one that did not.
    pub err: Color,

    /// A language server's or linter's warning: the gutter mark, the
    /// underline, and the message.
    ///
    /// Its own color rather than a borrowed one. `stage_partial` is the
    /// same amber and was doing this job, but it means "staged, then
    /// edited again" — and a reader who has learned that mark should not
    /// have to unlearn it to read a warning.
    pub warn: Color,
    /// A hint or an informational note — the third and fourth severities,
    /// which are advice rather than a problem.
    pub hint: Color,

    /// Merge conflicts: the warning icon and the file name in the panel,
    /// the ⚑ in the change bar, and the panel border while any file is
    /// conflicted. One strong color, used nowhere else, so a conflict is
    /// never mistaken for an error message or a removed line.
    pub conflict: Color,
    /// The "⚠ MERGE" badge in the review top bar.
    pub badge_conflict: Color,
    /// The ↑ ahead / ↓ behind counts beside the branch name.
    pub ahead: Color,
    pub behind: Color,

    /// Status letters in the file list: added / removed / renamed / other.
    pub st_added: Color,
    pub st_removed: Color,
    pub st_renamed: Color,
    pub st_other: Color,

    pub btn_bg: Color,
    pub btn_fg: Color,
    pub btn_active_bg: Color,
    pub btn_active_fg: Color,
    /// The ● on a tab holding unsaved work.
    ///
    /// Two colors because the tab has two backgrounds, and one color
    /// cannot read on both: the row's own grey and the selected tab's
    /// saturated blue are at opposite ends of the scale. One color made
    /// the mark vanish on whichever tab the reader had selected — which
    /// is the tab they are typing into, and so the one the mark is most
    /// needed on.
    pub tab_dirty: Color,
    pub tab_dirty_active: Color,
    /// The "PR #N" badge in the review top bar.
    pub badge_pr: Color,
    /// …and the "⎇ LOCAL" one.
    pub badge_local: Color,
    pub badge_fg: Color,

    /// Editor: text with no highlighting.
    pub code: Color,
    /// Editor: the line-number gutter.
    pub gutter: Color,
    /// Editor: selected text.
    pub editor_sel: Color,

    /// Blame pane: the age ramp, newest first. Six steps — under a day,
    /// a week, a month, three months, a year, older. The scale is
    /// absolute, so a color means the same age in every file.
    ///
    /// One hue, lightness only: the same neutral the borders and line
    /// numbers already use, running from the primary text color down to
    /// the divider. Keeping the ramp colorless is what lets the two
    /// classes above it — uncommitted, and part of this change — be the
    /// only *colored* things in the column, so the question the pane
    /// exists to answer is the one that catches the eye.
    pub blame_heat: [Color; 6],
    /// Blame pane: a line that is not committed yet — your working tree.
    /// Brighter than the top of the ramp, because it is newer than any
    /// commit can be.
    pub blame_uncommitted: Color,
    /// Blame pane: a line from a commit that belongs to the change under
    /// review. This is the color that answers "is this mine, now?".
    pub blame_change: Color,
    /// Blame pane: an author name matching your own `git config
    /// user.email`. Kept apart from the ramp so "mine" and "recent" stay
    /// two signals rather than one.
    pub blame_mine: Color,
}

/// For a dark terminal background. These are the colors loupe shipped with.
pub const DARK: Palette = Palette {
    added: Color::Rgb(16, 50, 26),
    removed: Color::Rgb(58, 22, 22),
    empty: Color::Rgb(24, 24, 28),
    selected: Color::Rgb(28, 66, 120),
    matched: Color::Rgb(122, 92, 20),
    cursor: Color::Rgb(38, 38, 48),
    row: Color::Rgb(40, 40, 60),

    text: Color::White,
    dim: Color::Gray,
    faint: Color::Rgb(100, 100, 110),
    line_no: Color::Rgb(110, 110, 120),
    dir: Color::Rgb(140, 170, 230),
    viewed: Color::Rgb(125, 125, 135),
    checkbox: Color::Rgb(200, 200, 210),
    stage_add: Color::Rgb(150, 200, 255),
    stage_partial: Color::Rgb(230, 190, 100),
    fold: Color::Rgb(130, 140, 160),
    divider: Color::Rgb(60, 60, 70),
    divider_active: Color::Rgb(120, 170, 240),
    accent: Color::Cyan,
    key: Color::Rgb(150, 200, 255),
    ok: Color::Rgb(140, 200, 140),
    err: Color::Rgb(255, 140, 140),
    warn: Color::Rgb(240, 200, 110),
    hint: Color::Rgb(140, 180, 210),

    conflict: Color::Rgb(255, 150, 90),
    badge_conflict: Color::Rgb(150, 60, 20),
    ahead: Color::Rgb(140, 200, 140),
    behind: Color::Rgb(230, 190, 100),

    st_added: Color::Green,
    st_removed: Color::Red,
    st_renamed: Color::Yellow,
    st_other: Color::Cyan,

    btn_bg: Color::Rgb(45, 45, 55),
    btn_fg: Color::Gray,
    btn_active_bg: Color::Rgb(30, 90, 160),
    btn_active_fg: Color::White,
    tab_dirty: Color::Rgb(230, 190, 100),
    tab_dirty_active: Color::Rgb(255, 214, 120),
    badge_pr: Color::Rgb(90, 50, 140),
    badge_local: Color::Rgb(20, 95, 55),
    badge_fg: Color::White,

    code: Color::Rgb(220, 223, 228),
    gutter: Color::DarkGray,
    editor_sel: Color::Rgb(38, 79, 120),

    // Text grey down to divider grey — the chrome's own neutral, which
    // leans very slightly blue the way the rest of the dark palette does.
    blame_heat: [
        Color::Rgb(222, 222, 232),
        Color::Rgb(186, 186, 196),
        Color::Rgb(152, 152, 162),
        Color::Rgb(120, 120, 130),
        Color::Rgb(90, 90, 100),
        Color::Rgb(64, 64, 74),
    ],
    blame_uncommitted: Color::Rgb(120, 230, 150),
    blame_change: Color::Rgb(120, 170, 240),
    blame_mine: Color::Rgb(190, 210, 255),
};

/// For a light terminal background. The diff tints are picked against two
/// constraints at once: light enough that dark syntax colors stay
/// readable on top of them, saturated enough to tell added from removed at
/// a glance.
pub const LIGHT: Palette = Palette {
    added: Color::Rgb(214, 245, 222),
    removed: Color::Rgb(255, 223, 219),
    empty: Color::Rgb(240, 240, 244),
    selected: Color::Rgb(200, 222, 250),
    matched: Color::Rgb(252, 222, 138),
    cursor: Color::Rgb(234, 237, 243),
    row: Color::Rgb(214, 227, 246),

    text: Color::Rgb(24, 26, 30),
    dim: Color::Rgb(92, 98, 110),
    faint: Color::Rgb(110, 119, 129),
    line_no: Color::Rgb(114, 122, 133),
    dir: Color::Rgb(36, 88, 168),
    viewed: Color::Rgb(116, 124, 134),
    checkbox: Color::Rgb(118, 124, 138),
    stage_add: Color::Rgb(28, 100, 188),
    stage_partial: Color::Rgb(150, 100, 8),
    fold: Color::Rgb(108, 116, 130),
    divider: Color::Rgb(178, 184, 194),
    divider_active: Color::Rgb(36, 108, 200),
    accent: Color::Rgb(18, 104, 152),
    key: Color::Rgb(30, 88, 170),
    ok: Color::Rgb(22, 106, 58),
    err: Color::Rgb(176, 32, 42),
    warn: Color::Rgb(146, 94, 6),
    hint: Color::Rgb(38, 92, 140),

    conflict: Color::Rgb(190, 74, 10),
    badge_conflict: Color::Rgb(168, 68, 12),
    ahead: Color::Rgb(22, 106, 58),
    behind: Color::Rgb(150, 100, 8),

    st_added: Color::Rgb(26, 122, 56),
    st_removed: Color::Rgb(186, 40, 44),
    st_renamed: Color::Rgb(150, 100, 8),
    st_other: Color::Rgb(18, 104, 152),

    btn_bg: Color::Rgb(224, 227, 233),
    btn_fg: Color::Rgb(58, 63, 72),
    btn_active_bg: Color::Rgb(28, 100, 188),
    btn_active_fg: Color::Rgb(255, 255, 255),
    tab_dirty: Color::Rgb(150, 100, 8),
    tab_dirty_active: Color::Rgb(255, 205, 90),
    badge_pr: Color::Rgb(106, 66, 162),
    badge_local: Color::Rgb(24, 116, 68),
    badge_fg: Color::Rgb(255, 255, 255),

    code: Color::Rgb(30, 33, 38),
    gutter: Color::Rgb(118, 125, 135),
    editor_sel: Color::Rgb(198, 219, 246),

    // The same ramp inverted: on a light background "recent" is the dark
    // end, and history fades out past the divider grey into the page.
    blame_heat: [
        Color::Rgb(28, 30, 36),
        Color::Rgb(66, 70, 80),
        Color::Rgb(104, 109, 120),
        Color::Rgb(138, 144, 155),
        Color::Rgb(170, 176, 186),
        Color::Rgb(196, 201, 210),
    ],
    blame_uncommitted: Color::Rgb(20, 130, 70),
    blame_change: Color::Rgb(28, 100, 188),
    blame_mine: Color::Rgb(38, 68, 140),
};

// ------------------------------------------------------------- global state

/// 0 = dark, 1 = light. An atomic rather than a lock: the renderer reads it
/// once per drawn row, and the theme picker writes it between frames.
static ACTIVE: AtomicU8 = AtomicU8::new(0);

pub fn set_appearance(a: Appearance) {
    ACTIVE.store(u8::from(a.is_light()), Ordering::Relaxed);
}

pub fn appearance() -> Appearance {
    if ACTIVE.load(Ordering::Relaxed) == 0 {
        Appearance::Dark
    } else {
        Appearance::Light
    }
}

/// The palette for the active appearance. Cheap enough to call per row.
pub fn palette() -> &'static Palette {
    match appearance() {
        Appearance::Dark => &DARK,
        Appearance::Light => &LIGHT,
    }
}

/// Serializes every test that reads or writes the process-global visual
/// state — the appearance here and the syntax theme in
/// [`crate::highlight`]. They are one lock because they are one look: a
/// test that pins the appearance and one that pins the theme would
/// otherwise render each other's frames.
#[cfg(test)]
pub fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------- detection

/// What detection found, and where it came from — the `loupe appearance`
/// command reports this so "why is it dark?" has an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detected {
    /// The terminal answered the OSC 11 query with this background color.
    Terminal(u8, u8, u8),
    /// No answer, but `COLORFGBG` said this.
    ColorFgBg(Appearance),
}

impl Detected {
    pub fn appearance(self) -> Appearance {
        match self {
            Detected::Terminal(r, g, b) => Appearance::of_background(r, g, b),
            Detected::ColorFgBg(a) => a,
        }
    }
}

/// Ask the terminal what its background is, and fall back to `COLORFGBG`.
/// `None` means "no idea" — the caller decides what to do about that.
///
/// Must run with the terminal already in raw mode (otherwise the reply is
/// line-buffered and echoed) and before the event loop starts reading, so
/// the reply can't be mistaken for user input.
pub fn detect() -> Option<Appearance> {
    detect_detailed().map(|d| d.appearance())
}

/// [`detect`], keeping the evidence.
pub fn detect_detailed() -> Option<Detected> {
    query_background()
        .map(|(r, g, b)| Detected::Terminal(r, g, b))
        .or_else(|| from_colorfgbg().map(Detected::ColorFgBg))
}

/// How long to wait for the terminal to answer. Terminals reply within a
/// millisecond or two; the budget only matters for ones that never will,
/// and those hit the DA1 sentinel below long before the deadline.
const REPLY_TIMEOUT_MS: u64 = 120;

/// Grace period for the DA1 reply once the background is already known —
/// for the rare terminal that answers OSC 11 but not device attributes,
/// so it costs a blink rather than the full budget.
const DRAIN_TIMEOUT_MS: u64 = 30;

/// Write an OSC 11 background query, read the reply.
///
/// A terminal that doesn't implement OSC 11 simply says nothing, so the
/// query is followed by a primary device-attributes request (`CSI c`) —
/// which every terminal answers. Replies come back in the order asked, so
/// seeing the DA1 reply means the OSC 11 answer is never coming and we can
/// stop waiting immediately instead of burning the whole timeout.
///
/// Both replies are read to completion even once the color is in hand.
/// Anything left in the tty queue would be handed to crossterm a moment
/// later and typed into the review as if the user had pressed the keys.
///
/// The question goes out on the terminal loupe already owns — stdout and
/// stdin — rather than down a second handle on `/dev/tty`. Opening and
/// closing another descriptor on the same terminal costs the *first*
/// thing typed after launch on macOS: it disturbs the registration the
/// event reader has on stdin, and the next input to arrive is swallowed
/// re-arming it, whenever it arrives. A file dropped on the window is
/// exactly that kind of input, so the second handle was the difference
/// between a drop that opens and a drop that silently vanishes.
///
/// stdin is read through `libc::read` rather than [`std::io::Stdin`] for
/// the neighbouring reason: `Stdin` buffers, and bytes it held back would
/// never reach the event reader at all.
#[cfg(unix)]
fn query_background() -> Option<(u8, u8, u8)> {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;
    use std::time::{Duration, Instant};

    let fd = std::io::stdin().as_raw_fd();
    let out_fd = std::io::stdout().as_raw_fd();
    // Redirected either way: there is no terminal to ask, and writing the
    // query into a pipe would only corrupt whatever is reading it.
    // SAFETY: isatty on two descriptors this process owns.
    if unsafe { libc::isatty(fd) } != 1 || unsafe { libc::isatty(out_fd) } != 1 {
        return None;
    }
    let mut out = std::io::stdout();
    out.write_all(b"\x1b]11;?\x07\x1b[c").ok()?;
    out.flush().ok()?;

    let mut deadline = Instant::now() + Duration::from_millis(REPLY_TIMEOUT_MS);
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    let mut chunk = [0u8; 256];
    // Read until the sentinel lands — that is the point at which the
    // terminal has said everything it is going to say.
    while !has_da1_reply(&buf) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialized pollfd on a descriptor this process owns.
        if unsafe { libc::poll(&mut pfd, 1, left.as_millis() as libc::c_int) } <= 0 {
            break;
        }
        // SAFETY: `chunk` is live and `chunk.len()` bounds the write.
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
        if buf.len() > 4096 {
            break; // something else is writing to the tty; give up
        }
        if parse_osc11(&buf).is_some() {
            deadline = deadline.min(Instant::now() + Duration::from_millis(DRAIN_TIMEOUT_MS));
        }
    }
    parse_osc11(&buf)
}

#[cfg(not(unix))]
fn query_background() -> Option<(u8, u8, u8)> {
    None
}

/// A complete primary device-attributes reply: `CSI ? <params> c`, where
/// the parameters are digits and semicolons and the final byte is `c`.
/// Anything else that starts `CSI ?` — a mode report, say — must not be
/// mistaken for the sentinel, or the wait ends before the color arrives.
fn has_da1_reply(buf: &[u8]) -> bool {
    let mut i = 0;
    while i + 2 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' && buf[i + 2] == b'?' {
            let mut j = i + 3;
            while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
                j += 1;
            }
            if buf.get(j) == Some(&b'c') {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Pull the color out of an `OSC 11 ; <spec> ST` reply.
///
/// A reply with no terminator yet is a partial read, not a short color:
/// parsing `rgb:e8e8/e8e8/e` as if it were finished would both get the
/// blue channel wrong and leave the rest of the bytes in the input queue.
fn parse_osc11(buf: &[u8]) -> Option<(u8, u8, u8)> {
    let text = String::from_utf8_lossy(buf);
    let start = text.find("]11;")? + 4;
    let rest = &text[start..];
    // The reply ends at BEL, or at the ESC of a String Terminator.
    let end = rest.find(['\u{7}', '\u{1b}'])?;
    parse_color_spec(&rest[..end])
}

/// X11 color specs as terminals report them: `rgb:RR/GG/BB` with one to
/// four hex digits per component, or `#RRGGBB`-style hex.
///
/// The whole spec must be ASCII. Replies arrive as raw bytes and reach
/// here through `from_utf8_lossy`, so a garbled one can carry multi-byte
/// replacement characters — and the `#` form slices its body by thirds,
/// which would panic on a boundary inside one of those.
fn parse_color_spec(spec: &str) -> Option<(u8, u8, u8)> {
    let spec = spec.trim();
    if !spec.is_ascii() {
        return None;
    }
    if let Some(body) = spec
        .strip_prefix("rgb:")
        .or_else(|| spec.strip_prefix("rgba:"))
    {
        let mut parts = body.split('/');
        let r = scale_hex(parts.next()?)?;
        let g = scale_hex(parts.next()?)?;
        let b = scale_hex(parts.next()?)?;
        return Some((r, g, b));
    }
    let body = spec.strip_prefix('#')?;
    if body.len() % 3 != 0 || body.is_empty() || body.len() > 12 {
        return None;
    }
    let n = body.len() / 3;
    Some((
        scale_hex(&body[..n])?,
        scale_hex(&body[n..2 * n])?,
        scale_hex(&body[2 * n..])?,
    ))
}

/// Widen a 1–4 digit hex component to 8 bits: "f" and "ffff" are both 255.
fn scale_hex(part: &str) -> Option<u8> {
    let part = part.trim();
    if part.is_empty() || part.len() > 4 || !part.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(part, 16).ok()?;
    let max = (1u32 << (4 * part.len())) - 1;
    Some(((value * 255 + max / 2) / max) as u8)
}

/// `COLORFGBG` is `<fg>;<bg>` (sometimes with a middle field). Terminals
/// that set it use ANSI color numbers, where 0–6 and 8 are the dark ones —
/// the same rule vim has used for decades.
fn from_colorfgbg() -> Option<Appearance> {
    parse_colorfgbg(&std::env::var("COLORFGBG").ok()?)
}

fn parse_colorfgbg(value: &str) -> Option<Appearance> {
    let bg: u8 = value.rsplit(';').next()?.trim().parse().ok()?;
    if bg > 15 {
        return None;
    }
    Some(if matches!(bg, 0..=6 | 8) {
        Appearance::Dark
    } else {
        Appearance::Light
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_backgrounds() {
        // Typical dark terminals.
        assert_eq!(Appearance::of_background(0, 0, 0), Appearance::Dark);
        assert_eq!(Appearance::of_background(30, 30, 46), Appearance::Dark); // mocha
        assert_eq!(Appearance::of_background(40, 42, 54), Appearance::Dark); // dracula
        assert_eq!(Appearance::of_background(0, 43, 54), Appearance::Dark); // solarized
                                                                            // Typical light ones.
        assert_eq!(Appearance::of_background(255, 255, 255), Appearance::Light);
        assert_eq!(Appearance::of_background(239, 241, 245), Appearance::Light); // latte
        assert_eq!(Appearance::of_background(253, 246, 227), Appearance::Light); // solarized
        assert_eq!(Appearance::of_background(251, 241, 199), Appearance::Light);
        // gruvbox
    }

    #[test]
    fn parses_osc11_replies() {
        // xterm / most terminals: 16 bits per component, BEL-terminated.
        assert_eq!(
            parse_osc11(b"\x1b]11;rgb:1e1e/1e1e/2e2e\x07"),
            Some((30, 30, 46))
        );
        // ST-terminated, 8 bits per component.
        assert_eq!(
            parse_osc11(b"\x1b]11;rgb:ff/ff/ff\x1b\\"),
            Some((255, 255, 255))
        );
        // Some terminals answer in hex.
        assert_eq!(parse_osc11(b"\x1b]11;#fdf6e3\x07"), Some((253, 246, 227)));
        // Short components widen: "f" is full brightness, not 15.
        assert_eq!(parse_osc11(b"\x1b]11;rgb:f/f/f\x07"), Some((255, 255, 255)));
        // A reply that arrived alongside other bytes still parses.
        assert_eq!(
            parse_osc11(b"junk\x1b]11;rgb:0000/0000/0000\x07\x1b[?62;c"),
            Some((0, 0, 0))
        );
        // Nothing usable.
        assert_eq!(parse_osc11(b""), None);
        assert_eq!(parse_osc11(b"\x1b[?62;1;c"), None);
        assert_eq!(parse_osc11(b"\x1b]11;rgb:zz/zz/zz\x07"), None);
        assert_eq!(parse_osc11(b"\x1b]11;rgb:ff/ff\x07"), None);
        // Not terminated yet: a partial read, not a short color. Parsing
        // it would report a wrong blue and leave "8e8" in the input queue
        // to be typed into the review a moment later.
        assert_eq!(parse_osc11(b"\x1b]11;rgb:e8e8/e8e8/e"), None);
        assert_eq!(
            parse_osc11(b"\x1b]11;rgb:e8e8/e8e8/e8e8\x07"),
            Some((232, 232, 232))
        );
    }

    /// Replies are raw bytes: invalid UTF-8 becomes multi-byte U+FFFD, and
    /// the `#` form slices its body into thirds. A non-ASCII body used to
    /// slice through the middle of a replacement character and panic —
    /// at startup, with the terminal already in raw mode.
    #[test]
    fn garbled_replies_do_not_panic() {
        for reply in [
            &b"\x1b]11;#\xff\x07"[..],
            &b"\x1b]11;#\xff\xfe\xfd\x07"[..],
            &b"\x1b]11;\xc3\x28\x07"[..],
            &b"\x1b]11;rgb:\xff/\xff/\xff\x07"[..],
            &b"\x1b]11;#\xe2\x82\xac\x07"[..],
            &b"\x1b]11;\x07"[..],
            &b"\x1b]11;#\x07"[..],
            &b"\xff\xfe\x1b]11;#ffffff\x07"[..],
        ] {
            // The only requirement is that it returns rather than panics.
            let _ = parse_osc11(reply);
            let _ = has_da1_reply(reply);
        }
        // The last one is still a perfectly good reply.
        assert_eq!(
            parse_osc11(b"\xff\xfe\x1b]11;#ffffff\x07"),
            Some((255, 255, 255))
        );
    }

    #[test]
    fn spots_the_da1_sentinel() {
        assert!(has_da1_reply(b"\x1b[?62;1;6;9;15c"));
        assert!(has_da1_reply(b"\x1b]11;rgb:00/00/00\x07\x1b[?1;2c"));
        // Incomplete — still worth waiting for the rest.
        assert!(!has_da1_reply(b"\x1b[?62;1"));
        assert!(!has_da1_reply(b""));
        // Some other `CSI ?` report is not the sentinel: treating it as one
        // would abandon the wait before the color arrives.
        assert!(!has_da1_reply(b"\x1b[?2026;2$y"));
        assert!(!has_da1_reply(b"\x1b[?1000;1$y and a c later on"));
        // …but a real reply after one still counts.
        assert!(has_da1_reply(b"\x1b[?2026;2$y\x1b[?62;1c"));
    }

    #[test]
    fn reads_colorfgbg() {
        assert_eq!(parse_colorfgbg("15;0"), Some(Appearance::Dark));
        assert_eq!(parse_colorfgbg("0;15"), Some(Appearance::Light));
        assert_eq!(parse_colorfgbg("7;0"), Some(Appearance::Dark));
        assert_eq!(parse_colorfgbg("0;7"), Some(Appearance::Light));
        // rxvt's three-field form.
        assert_eq!(parse_colorfgbg("12;default;7"), Some(Appearance::Light));
        // 8 is "bright black" — still a dark background.
        assert_eq!(parse_colorfgbg("15;8"), Some(Appearance::Dark));
        // Unusable values mean "no idea", not a wrong guess.
        assert_eq!(parse_colorfgbg("default;default"), None);
        assert_eq!(parse_colorfgbg(""), None);
        assert_eq!(parse_colorfgbg("15;234"), None);
    }

    #[test]
    fn appearance_switches_the_palette() {
        let _guard = test_lock();
        let before = appearance();
        set_appearance(Appearance::Light);
        assert_eq!(appearance(), Appearance::Light);
        assert_eq!(palette().added, LIGHT.added);
        set_appearance(Appearance::Dark);
        assert_eq!(palette().added, DARK.added);
        set_appearance(before);
    }

    /// The whole point of the light palette: the diff tints must be light
    /// enough that dark syntax colors stay readable on them, and the dark
    /// ones dark enough for light syntax colors. A regression here is
    /// exactly the "way too dark on a light theme" bug.
    #[test]
    fn diff_tints_match_their_appearance() {
        let lum = |c: Color| match c {
            Color::Rgb(r, g, b) => 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32,
            other => panic!("diff backgrounds must be explicit RGB, got {other:?}"),
        };
        for c in [LIGHT.added, LIGHT.removed, LIGHT.empty, LIGHT.selected] {
            assert!(lum(c) > 170.0, "{c:?} is too dark for a light background");
        }
        for c in [DARK.added, DARK.removed, DARK.empty, DARK.selected] {
            assert!(lum(c) < 90.0, "{c:?} is too light for a dark background");
        }
        // Added and removed have to be told apart, not just "tinted".
        assert!(lum(LIGHT.added) != lum(LIGHT.removed));
        assert!(lum(DARK.added) != lum(DARK.removed));
    }

    /// Nothing in the light palette may fall back to a color that only
    /// works on black — `Color::White` foregrounds are the classic way an
    /// otherwise-light UI turns invisible.
    #[test]
    fn light_palette_has_no_dark_only_colors() {
        let p = LIGHT;
        for (name, c) in [
            ("text", p.text),
            ("dim", p.dim),
            ("faint", p.faint),
            ("line_no", p.line_no),
            ("code", p.code),
            ("gutter", p.gutter),
            ("fold", p.fold),
            ("viewed", p.viewed),
            ("checkbox", p.checkbox),
            ("accent", p.accent),
            ("btn_fg", p.btn_fg),
            ("tab_dirty", p.tab_dirty),
        ] {
            match c {
                Color::Rgb(r, g, b) => assert!(
                    Appearance::of_background(r, g, b) == Appearance::Dark,
                    "light palette {name} ({c:?}) is too pale to read on white"
                ),
                other => panic!("light palette {name} must be explicit RGB, got {other:?}"),
            }
        }
    }

    /// The age ramp is one hue with lightness doing all the work. That is
    /// what keeps the two classes above it — uncommitted, and part of
    /// this change — the only *colored* things in the blame column, so
    /// the question the pane exists to answer is the one that catches the
    /// eye. A ramp that drifted into color would drown them out.
    #[test]
    fn the_blame_ramp_is_one_hue() {
        for (name, p) in [("dark", &DARK), ("light", &LIGHT)] {
            for (i, c) in p.blame_heat.iter().enumerate() {
                let Color::Rgb(r, g, b) = *c else {
                    panic!("{name} step {i} must be an explicit RGB step");
                };
                // A grey with the palette's own slight blue lean. A real
                // hue would spread three or four times this far.
                let spread = r.max(g).max(b) - r.min(g).min(b);
                assert!(
                    spread <= 20,
                    "{name} step {i} is {r},{g},{b} — a spread of {spread} is a hue, not a grey"
                );
                assert!(
                    r <= g && g <= b,
                    "{name} step {i} is {r},{g},{b} — every step must lean the same way"
                );
            }
        }
    }

    /// …and the lightness runs one way, so "brighter is newer" holds at
    /// every step rather than only at the ends.
    #[test]
    fn the_blame_ramp_fades_in_one_direction() {
        let lum = |c: &Color| match c {
            Color::Rgb(r, g, b) => *r as i32 + *g as i32 + *b as i32,
            other => panic!("expected an RGB step, got {other:?}"),
        };
        // Dark terminal: newest is brightest, and every step recedes.
        for w in DARK.blame_heat.windows(2) {
            assert!(lum(&w[0]) > lum(&w[1]), "dark ramp must fall: {w:?}");
        }
        // Light terminal: the same ramp inverted — newest is darkest.
        for w in LIGHT.blame_heat.windows(2) {
            assert!(lum(&w[0]) < lum(&w[1]), "light ramp must rise: {w:?}");
        }
        // The oldest step still has to be visible against the pane, not
        // painted out of existence.
        assert_ne!(DARK.blame_heat[5], DARK.empty);
        assert_ne!(LIGHT.blame_heat[5], LIGHT.empty);
    }

    /// The two classes that outrank the ramp have to read as colored
    /// against it, or the pane's whole signal is lost.
    #[test]
    fn the_blame_badges_are_colored_where_the_ramp_is_not() {
        for (name, p) in [("dark", &DARK), ("light", &LIGHT)] {
            for (what, c) in [
                ("uncommitted", p.blame_uncommitted),
                ("in this change", p.blame_change),
            ] {
                let Color::Rgb(r, g, b) = c else {
                    panic!("{name} {what} must be an explicit RGB color");
                };
                let spread = r.max(g).max(b) - r.min(g).min(b);
                assert!(
                    spread > 40,
                    "{name} {what} is {r},{g},{b} — too grey to stand out from the ramp"
                );
            }
        }
    }
}

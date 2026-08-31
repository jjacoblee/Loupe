//! Syntax highlighting for diff, file content, and the editor, via syntect.
//!
//! Whole-file highlighting runs in the background file-load job (it can take
//! a moment on large files) and produces plain `(color, text)` segments per
//! line; the renderer overlays diff backgrounds (green/red/selection) on top,
//! so the code looks like it does in an editor with the diff painted
//! underneath.
//!
//! The editor can't afford whole-file re-highlighting on every keystroke
//! (~170ms for a 1.5k-line Rust file in release builds), so
//! [`EditorHighlight`] keeps a per-line cache of syntect parse/highlight
//! states and, after an edit, recomputes only from the first changed line
//! until the running state converges with the cached suffix — one or two
//! lines for typical typing.
//!
//! Syntaxes and theme come from `two-face`: unlike syntect's stock set it
//! covers TypeScript/TSX, TOML, Dockerfile, and friends, and its One Half
//! Dark theme is vivid enough to read on a terminal — stock syntect only
//! has muted base16 themes, which come out near-white.

use crate::theme::Appearance;
use ratatui::style::Color;
use std::sync::{OnceLock, RwLock};
use syntect::easy::HighlightLines;
use syntect::highlighting::{HighlightIterator, HighlightState, Highlighter, Theme};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use two_face::theme::{EmbeddedLazyThemeSet, EmbeddedThemeName};

/// One source line as a list of (foreground color, text) segments.
pub type HlLine = Vec<(Color, String)>;

/// Above this many lines the editor skips highlighting entirely — the
/// one-time full parse on open would hitch for seconds.
const MAX_EDITOR_HL_LINES: usize = 8_000;

/// The theme every subsequent highlight call uses. Switchable at any time
/// (the theme picker and setup wizard preview themes live); already-computed
/// highlights keep their colors until their owner re-highlights.
static CURRENT_THEME: RwLock<EmbeddedThemeName> = RwLock::new(DEFAULT_THEME);

/// What new installs get on a dark terminal, and the fallback when nothing
/// is configured.
pub const DEFAULT_THEME: EmbeddedThemeName = EmbeddedThemeName::CatppuccinMocha;

/// The same, for a light terminal — the light sibling of [`DEFAULT_THEME`].
pub const DEFAULT_LIGHT_THEME: EmbeddedThemeName = EmbeddedThemeName::CatppuccinLatte;

/// The default theme for an appearance.
pub fn default_theme(appearance: Appearance) -> EmbeddedThemeName {
    match appearance {
        Appearance::Dark => DEFAULT_THEME,
        Appearance::Light => DEFAULT_LIGHT_THEME,
    }
}

/// Select the syntax theme for all highlighting from this call on. Cheap:
/// themes are lazily deserialized once each and cached by the theme set.
pub fn set_theme(theme: EmbeddedThemeName) {
    *CURRENT_THEME.write().expect("theme lock poisoned") = theme;
}

/// The currently active theme.
pub fn current_theme() -> EmbeddedThemeName {
    *CURRENT_THEME.read().expect("theme lock poisoned")
}

/// Serializes tests (in any module) that mutate or depend on the
/// process-global current theme, so they can't race each other. Shared with
/// the appearance, which is the other half of the same look.
#[cfg(test)]
pub fn test_theme_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::theme::test_lock()
}

/// The config-file name of a theme (reverse of [`theme_by_name`]).
pub fn theme_key(theme: EmbeddedThemeName) -> &'static str {
    THEMES
        .iter()
        .find(|(_, t)| *t == theme)
        .map(|(k, _)| *k)
        .unwrap_or("catppuccin-mocha")
}

/// Every selectable theme, by config-file name. Names are kebab-case; the
/// lookup in [`theme_by_name`] also tolerates spaces, underscores, and case.
pub const THEMES: &[(&str, EmbeddedThemeName)] = &[
    ("ansi", EmbeddedThemeName::Ansi),
    ("base16", EmbeddedThemeName::Base16),
    ("base16-256", EmbeddedThemeName::Base16_256),
    (
        "base16-eighties-dark",
        EmbeddedThemeName::Base16EightiesDark,
    ),
    ("base16-mocha-dark", EmbeddedThemeName::Base16MochaDark),
    ("base16-ocean-dark", EmbeddedThemeName::Base16OceanDark),
    ("base16-ocean-light", EmbeddedThemeName::Base16OceanLight),
    ("catppuccin-frappe", EmbeddedThemeName::CatppuccinFrappe),
    ("catppuccin-latte", EmbeddedThemeName::CatppuccinLatte),
    (
        "catppuccin-macchiato",
        EmbeddedThemeName::CatppuccinMacchiato,
    ),
    ("catppuccin-mocha", EmbeddedThemeName::CatppuccinMocha),
    ("coldark-cold", EmbeddedThemeName::ColdarkCold),
    ("coldark-dark", EmbeddedThemeName::ColdarkDark),
    ("dark-neon", EmbeddedThemeName::DarkNeon),
    ("dracula", EmbeddedThemeName::Dracula),
    ("github", EmbeddedThemeName::Github),
    ("gruvbox-dark", EmbeddedThemeName::GruvboxDark),
    ("gruvbox-light", EmbeddedThemeName::GruvboxLight),
    ("inspired-github", EmbeddedThemeName::InspiredGithub),
    ("leet", EmbeddedThemeName::Leet),
    ("monokai-extended", EmbeddedThemeName::MonokaiExtended),
    (
        "monokai-extended-bright",
        EmbeddedThemeName::MonokaiExtendedBright,
    ),
    (
        "monokai-extended-light",
        EmbeddedThemeName::MonokaiExtendedLight,
    ),
    (
        "monokai-extended-origin",
        EmbeddedThemeName::MonokaiExtendedOrigin,
    ),
    ("nord", EmbeddedThemeName::Nord),
    ("one-half-dark", EmbeddedThemeName::OneHalfDark),
    ("one-half-light", EmbeddedThemeName::OneHalfLight),
    ("solarized-dark", EmbeddedThemeName::SolarizedDark),
    ("solarized-light", EmbeddedThemeName::SolarizedLight),
    ("sublime-snazzy", EmbeddedThemeName::SublimeSnazzy),
    ("two-dark", EmbeddedThemeName::TwoDark),
    ("zenburn", EmbeddedThemeName::Zenburn),
];

/// Resolve a config-file theme name, forgiving about case/spacing.
pub fn theme_by_name(name: &str) -> Option<EmbeddedThemeName> {
    let key = name.trim().to_lowercase().replace(['_', ' '], "-");
    THEMES.iter().find(|(k, _)| *k == key).map(|(_, t)| *t)
}

/// Themes that come as a matched pair — same palette, opposite background.
/// Switching appearance moves between the two halves, so someone who likes
/// Gruvbox on a dark terminal gets Gruvbox on a light one, not a stranger.
const PAIRS: &[(EmbeddedThemeName, EmbeddedThemeName)] = &[
    (
        EmbeddedThemeName::CatppuccinMocha,
        EmbeddedThemeName::CatppuccinLatte,
    ),
    (
        EmbeddedThemeName::CatppuccinMacchiato,
        EmbeddedThemeName::CatppuccinLatte,
    ),
    (
        EmbeddedThemeName::CatppuccinFrappe,
        EmbeddedThemeName::CatppuccinLatte,
    ),
    (
        EmbeddedThemeName::OneHalfDark,
        EmbeddedThemeName::OneHalfLight,
    ),
    (EmbeddedThemeName::TwoDark, EmbeddedThemeName::OneHalfLight),
    (
        EmbeddedThemeName::GruvboxDark,
        EmbeddedThemeName::GruvboxLight,
    ),
    (
        EmbeddedThemeName::SolarizedDark,
        EmbeddedThemeName::SolarizedLight,
    ),
    (
        EmbeddedThemeName::Base16OceanDark,
        EmbeddedThemeName::Base16OceanLight,
    ),
    (
        EmbeddedThemeName::Base16MochaDark,
        EmbeddedThemeName::Base16OceanLight,
    ),
    (
        EmbeddedThemeName::Base16EightiesDark,
        EmbeddedThemeName::Base16OceanLight,
    ),
    (
        EmbeddedThemeName::ColdarkDark,
        EmbeddedThemeName::ColdarkCold,
    ),
    (
        EmbeddedThemeName::MonokaiExtended,
        EmbeddedThemeName::MonokaiExtendedLight,
    ),
    (
        EmbeddedThemeName::MonokaiExtendedBright,
        EmbeddedThemeName::MonokaiExtendedLight,
    ),
    (
        EmbeddedThemeName::MonokaiExtendedOrigin,
        EmbeddedThemeName::MonokaiExtendedLight,
    ),
    (EmbeddedThemeName::Zenburn, EmbeddedThemeName::Github),
    (EmbeddedThemeName::Nord, EmbeddedThemeName::InspiredGithub),
];

/// Whether a theme is meant for a light background, read from the theme's
/// own background color rather than a hand-kept list — a theme set that
/// grows a new entry classifies itself. Themes that leave the background
/// unset (`ansi`, which defers to the terminal's own palette) count as dark.
///
/// Deserializing one theme is cheap and the theme set caches it, so this is
/// fine to call on the startup path.
pub fn theme_is_light(theme: EmbeddedThemeName) -> bool {
    theme_set()
        .get(theme)
        .settings
        .background
        .map(|c| Appearance::of_background(c.r, c.g, c.b).is_light())
        .unwrap_or(false)
}

/// The theme to use for `appearance`, starting from `theme`.
///
/// A theme that already suits the appearance is returned untouched. One that
/// doesn't is swapped for its counterpart from [`PAIRS`], or — when it has
/// none — for the appearance's default.
pub fn for_appearance(theme: EmbeddedThemeName, appearance: Appearance) -> EmbeddedThemeName {
    if theme_is_light(theme) == appearance.is_light() {
        return theme;
    }
    let paired = PAIRS.iter().find_map(|(dark, light)| {
        if *dark == theme {
            Some(*light)
        } else if *light == theme {
            Some(*dark)
        } else {
            None
        }
    });
    paired
        .filter(|t| theme_is_light(*t) == appearance.is_light())
        .unwrap_or_else(|| default_theme(appearance))
}

fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

fn theme_set() -> &'static EmbeddedLazyThemeSet {
    static THEMES: OnceLock<EmbeddedLazyThemeSet> = OnceLock::new();
    THEMES.get_or_init(two_face::theme::extra)
}

/// The active theme, deserialized on first use (per theme) and cached.
fn theme() -> &'static Theme {
    theme_set().get(current_theme())
}

/// A theme's own background color, for previewing it honestly. `None` for
/// themes that defer to the terminal's palette.
pub fn theme_background(theme: EmbeddedThemeName) -> Option<Color> {
    theme_set()
        .get(theme)
        .settings
        .background
        .map(|c| Color::Rgb(c.r, c.g, c.b))
}

/// Kick off asset loading on a background thread. The `OnceLock`s make this
/// a cheap no-op if they are already (being) loaded.
pub fn warm() {
    std::thread::spawn(|| {
        let _ = syntaxes();
        let _ = theme();
    });
}

fn find_syntax(path: &str, first_line: &str) -> &'static SyntaxReference {
    let ss = syntaxes();
    let base = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path);
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    ss.find_syntax_by_extension(ext)
        .or_else(|| ss.find_syntax_by_extension(base))
        .or_else(|| ss.find_syntax_by_token(base))
        .or_else(|| ss.find_syntax_by_first_line(first_line))
        .unwrap_or_else(|| ss.find_syntax_plain_text())
}

/// Resolve a fenced-code-block info string ("rust", "sh", "```yaml") to a
/// syntax. The tag is a language name, not a file name, so the lookup goes
/// by name and by extension and ignores case. `None` means "draw it plain".
fn syntax_for_tag(tag: &str) -> Option<&'static SyntaxReference> {
    // The info string can carry attributes after the language
    // ("```rust,ignore", "```js title=x"); only the first word names it.
    let tag = tag
        .trim()
        .split([' ', ',', '\t', '{'])
        .next()
        .unwrap_or("")
        .trim_matches('"')
        .to_ascii_lowercase();
    if tag.is_empty() {
        return None;
    }
    // Tags whose spelling neither the name table nor the extension table
    // knows. Everything else falls through to the two lookups below.
    let tag = match tag.as_str() {
        "shell" | "bash" | "zsh" | "console" | "shell-session" | "ksh" => "sh",
        "node" | "javascript" | "mjs" | "cjs" => "js",
        "typescript" => "ts",
        "python" | "python3" => "py",
        "rust" => "rs",
        "golang" => "go",
        "yml" => "yaml",
        "markdown" => "md",
        "dockerfile" => "Dockerfile",
        "csharp" => "cs",
        "kotlin" => "kt",
        "docker" | "makefile" | "make" => "make",
        // Nothing to color, and asking syntect for a syntax by these names
        // finds odd matches.
        "text" | "txt" | "plain" | "plaintext" | "none" | "output" | "log" => return None,
        other => other,
    };
    let ss = syntaxes();
    ss.find_syntax_by_extension(tag)
        .or_else(|| ss.find_syntax_by_token(tag))
        .or_else(|| {
            ss.syntaxes()
                .iter()
                .find(|s| s.name.eq_ignore_ascii_case(tag))
        })
}

/// Highlight one fenced code block. Returns one entry per line of `code`,
/// or an empty vector when the tag names no syntax loupe has — the caller
/// then draws the block in the plain code color.
pub fn highlight_block(tag: &str, code: &str) -> Vec<HlLine> {
    let Some(syntax) = syntax_for_tag(tag) else {
        return Vec::new();
    };
    let ss = syntaxes();
    let mut hl = HighlightLines::new(syntax, theme());
    let mut out = Vec::new();
    for line in LinesWithEndings::from(code) {
        let segs = hl.highlight_line(line, ss).map(to_segs).unwrap_or_default();
        out.push(segs);
    }
    out
}

fn to_segs(regions: Vec<(syntect::highlighting::Style, &str)>) -> HlLine {
    let mut segs = HlLine::new();
    for (style, text) in regions {
        let text = text.trim_end_matches('\n');
        if text.is_empty() {
            continue;
        }
        let c = style.foreground;
        segs.push((Color::Rgb(c.r, c.g, c.b), text.to_string()));
    }
    segs
}

/// Highlight a whole file. Returns one entry per line (same order/count as
/// `content.lines()`); an empty segment list means "no highlighting" and the
/// renderer falls back to plain text.
pub fn highlight(path: &str, content: &str) -> Vec<HlLine> {
    let ss = syntaxes();
    let syntax = find_syntax(path, content.lines().next().unwrap_or(""));
    let mut hl = HighlightLines::new(syntax, theme());
    let mut out = Vec::new();
    for line in LinesWithEndings::from(content) {
        let segs = hl.highlight_line(line, ss).map(to_segs).unwrap_or_default();
        out.push(segs);
    }
    out
}

// --------------------------------------------------------- editor (incremental)

struct CachedLine {
    /// The text this entry was computed from.
    text: String,
    spans: HlLine,
    /// Parser/highlighter states AFTER this line (= before the next one).
    after_parse: ParseState,
    after_hl: HighlightState,
}

/// Incrementally maintained per-line highlighting for the editor buffer.
pub struct EditorHighlight {
    /// None: plain text or file too large — highlighting disabled.
    engine: Option<Engine>,
    cache: Vec<CachedLine>,
}

struct Engine {
    syntax: &'static SyntaxReference,
    highlighter: Highlighter<'static>,
}

impl Engine {
    fn initial_states(&self) -> (ParseState, HighlightState) {
        (
            ParseState::new(self.syntax),
            HighlightState::new(&self.highlighter, ScopeStack::new()),
        )
    }

    /// Highlight one line, advancing the running states past it.
    fn line_spans(
        &self,
        parse: &mut ParseState,
        hstate: &mut HighlightState,
        line: &str,
    ) -> HlLine {
        let ss = syntaxes();
        let with_nl = format!("{line}\n");
        match parse.parse_line(&with_nl, ss) {
            Ok(ops) => {
                let iter = HighlightIterator::new(hstate, &ops, &with_nl, &self.highlighter);
                to_segs(iter.collect())
            }
            Err(_) => HlLine::new(),
        }
    }
}

impl EditorHighlight {
    pub fn new(path: &str, content: &str) -> Self {
        let engine = if content.lines().count() > MAX_EDITOR_HL_LINES {
            None
        } else {
            let syntax = find_syntax(path, content.lines().next().unwrap_or(""));
            let plain = std::ptr::eq(syntax, syntaxes().find_syntax_plain_text());
            (!plain).then(|| Engine {
                syntax,
                // Snapshot of the active theme; the app recreates the editor
                // highlight when the theme changes while an editor is open.
                highlighter: Highlighter::new(theme()),
            })
        };
        EditorHighlight {
            engine,
            cache: Vec::new(),
        }
    }

    /// Spans for one buffer line, if highlighting is active and up to date.
    pub fn line(&self, idx: usize) -> Option<&HlLine> {
        self.cache.get(idx).map(|e| &e.spans)
    }

    /// Bring the cache in sync with the current buffer. Cheap no-op when
    /// nothing changed; otherwise recomputes from the first changed line and
    /// stops as soon as the running state matches the cached suffix again.
    pub fn update(&mut self, lines: &[String]) {
        let Some(engine) = &self.engine else { return };
        let n_new = lines.len();
        let n_old = self.cache.len();

        // Longest unchanged prefix.
        let mut p = 0;
        while p < n_new.min(n_old) && self.cache[p].text == lines[p] {
            p += 1;
        }
        if p == n_new && n_old == n_new {
            return; // identical buffer
        }
        // Longest unchanged suffix, disjoint from the prefix.
        let mut s = 0;
        while s < n_new - p
            && s < n_old - p
            && self.cache[n_old - 1 - s].text == lines[n_new - 1 - s]
        {
            s += 1;
        }

        // Running states entering new line `p`.
        let (mut parse, mut hstate) = if p == 0 {
            engine.initial_states()
        } else {
            (
                self.cache[p - 1].after_parse.clone(),
                self.cache[p - 1].after_hl.clone(),
            )
        };

        let suffix_start = n_new - s;
        let shift = n_old as isize - n_new as isize;
        let mut recomputed: Vec<CachedLine> = Vec::new();
        let mut old_end = n_old; // old range end replaced by `recomputed`
        let mut i = p;
        while i < n_new {
            if i >= suffix_start {
                // The old entry for this line; reuse the cached suffix when
                // the running states match what it was computed from.
                let j = (i as isize + shift) as usize;
                let matches = if j == 0 {
                    let (ip, ih) = engine.initial_states();
                    parse == ip && hstate == ih
                } else {
                    self.cache[j - 1].after_parse == parse && self.cache[j - 1].after_hl == hstate
                };
                if matches {
                    old_end = j;
                    break;
                }
            }
            let spans = engine.line_spans(&mut parse, &mut hstate, &lines[i]);
            recomputed.push(CachedLine {
                text: lines[i].clone(),
                spans,
                after_parse: parse.clone(),
                after_hl: hstate.clone(),
            });
            i += 1;
        }
        self.cache.splice(p..old_end, recomputed);
        debug_assert_eq!(self.cache.len(), n_new);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn distinct_colors(hl: &[HlLine]) -> usize {
        let mut colors = HashSet::new();
        for line in hl {
            for (c, _) in line {
                colors.insert(format!("{c:?}"));
            }
        }
        colors.len()
    }

    #[test]
    fn theme_names_resolve() {
        // Every listed name resolves to its own variant, forgivingly.
        assert_eq!(THEMES.len(), 32);
        for (name, theme) in THEMES {
            assert_eq!(theme_by_name(name), Some(*theme), "{name}");
        }
        assert_eq!(
            theme_by_name("One Half Dark"),
            Some(EmbeddedThemeName::OneHalfDark)
        );
        assert_eq!(
            theme_by_name("GRUVBOX_DARK"),
            Some(EmbeddedThemeName::GruvboxDark)
        );
        assert_eq!(theme_by_name("  nord "), Some(EmbeddedThemeName::Nord));
        assert_eq!(theme_by_name("not-a-theme"), None);
    }

    /// Switching the theme changes the colors that come out of `highlight`,
    /// and switching back restores them — the picker's live preview depends
    /// on this being a plain runtime switch.
    #[test]
    fn theme_switch_changes_highlight_output() {
        let _guard = test_theme_lock();
        let src = "fn main() { let s = \"hi\"; }\n";
        // Set the theme rather than trusting the one already in place: a
        // test that panicked mid-switch leaves its theme behind, and
        // `before` would then be colored by that one while `after` is
        // colored by DEFAULT_THEME.
        set_theme(DEFAULT_THEME);
        let before = highlight("t.rs", src);
        set_theme(EmbeddedThemeName::CatppuccinLatte);
        let latte = highlight("t.rs", src);
        set_theme(DEFAULT_THEME);
        let after = highlight("t.rs", src);
        assert_ne!(before, latte, "latte should color differently than mocha");
        assert_eq!(before, after, "switching back must restore the colors");
        assert_eq!(theme_key(DEFAULT_THEME), "catppuccin-mocha");
    }

    #[test]
    fn rust_gets_multiple_colors() {
        let _guard = test_theme_lock();
        let src = "fn main() {\n    let x: u32 = 42; // answer\n    println!(\"hi {x}\");\n}\n";
        let hl = highlight("test.rs", src);
        assert_eq!(hl.len(), 4);
        assert!(
            distinct_colors(&hl) >= 4,
            "expected keyword/string/number/comment colors to differ"
        );
    }

    /// Stock syntect has no TypeScript syntax — the two-face set must.
    #[test]
    fn typescript_gets_multiple_colors() {
        let _guard = test_theme_lock();
        let src = "export const f = (n: number): string => `v${n}`;\n";
        let hl = highlight("test.ts", src);
        assert!(
            distinct_colors(&hl) >= 3,
            "TypeScript should highlight (two-face syntax set)"
        );
    }

    #[test]
    fn unknown_type_falls_back_to_plain() {
        let _guard = test_theme_lock();
        let hl = highlight("data.xyzunknown", "just words\n");
        assert_eq!(hl.len(), 1);
        // Plain text still produces the theme's default foreground.
        assert_eq!(distinct_colors(&hl), 1);
    }

    fn spans_of(h: &EditorHighlight, n: usize) -> Vec<HlLine> {
        (0..n)
            .map(|i| h.line(i).cloned().unwrap_or_default())
            .collect()
    }

    /// After arbitrary edits, the incremental cache must equal a from-scratch
    /// computation of the same buffer.
    #[test]
    fn editor_incremental_matches_fresh() {
        // The incremental-vs-fresh comparison assumes one stable theme.
        let _guard = test_theme_lock();
        let src = "/* block\ncomment */\nfn main() {\n    let s = \"str\";\n    let n = 42;\n}\n";
        let mut lines: Vec<String> = src.split('\n').map(str::to_string).collect();
        let mut inc = EditorHighlight::new("t.rs", src);
        inc.update(&lines);

        type Edit = Box<dyn Fn(&mut Vec<String>)>;
        let edits: Vec<Edit> = vec![
            // Typing on one line.
            Box::new(|l| l[4] = "    let n = 421;".into()),
            // Inserting a line.
            Box::new(|l| l.insert(3, "    // note".into())),
            // Deleting a line.
            Box::new(|l| {
                l.remove(1);
            }),
            // Edit that changes downstream state: open a block comment.
            Box::new(|l| l[2] = "fn main() { /*".into()),
            // ...and close it again.
            Box::new(|l| l[2] = "fn main() {".into()),
        ];
        for edit in edits {
            edit(&mut lines);
            inc.update(&lines);
            let mut fresh = EditorHighlight::new("t.rs", &lines.join("\n"));
            fresh.update(&lines);
            assert_eq!(
                spans_of(&inc, lines.len()),
                spans_of(&fresh, lines.len()),
                "incremental cache diverged from fresh computation"
            );
        }
    }

    #[test]
    fn editor_plain_text_disables_engine() {
        let mut h = EditorHighlight::new("notes.xyzunknown", "hello\nworld");
        h.update(&["hello".into(), "world".into()]);
        assert!(h.line(0).is_none());
    }

    /// Themes classify themselves from their own background color, so the
    /// obviously-light ones must come out light and vice versa.
    #[test]
    fn themes_know_which_background_they_want() {
        for name in [
            "catppuccin-latte",
            "github",
            "gruvbox-light",
            "inspired-github",
            "one-half-light",
            "solarized-light",
            "monokai-extended-light",
            "base16-ocean-light",
            "coldark-cold",
        ] {
            let t = theme_by_name(name).unwrap_or_else(|| panic!("{name} exists"));
            assert!(theme_is_light(t), "{name} should be a light theme");
        }
        for name in [
            "catppuccin-mocha",
            "dracula",
            "nord",
            "gruvbox-dark",
            "one-half-dark",
            "solarized-dark",
            "zenburn",
            "two-dark",
        ] {
            let t = theme_by_name(name).unwrap_or_else(|| panic!("{name} exists"));
            assert!(!theme_is_light(t), "{name} should be a dark theme");
        }
        // The two defaults have to be a matched pair, or a fresh install on
        // a light terminal gets dark syntax colors.
        assert!(!theme_is_light(DEFAULT_THEME));
        assert!(theme_is_light(DEFAULT_LIGHT_THEME));
    }

    /// Switching appearance keeps the theme family where one exists, and
    /// falls back to the default otherwise — never to a mismatched theme.
    #[test]
    fn appearance_pairs_stay_in_the_family() {
        let pair = |name: &str, appearance| {
            theme_key(for_appearance(theme_by_name(name).unwrap(), appearance))
        };
        assert_eq!(
            pair("gruvbox-dark", Appearance::Light),
            "gruvbox-light",
            "same family"
        );
        assert_eq!(pair("gruvbox-light", Appearance::Dark), "gruvbox-dark");
        assert_eq!(pair("one-half-dark", Appearance::Light), "one-half-light");
        assert_eq!(pair("solarized-light", Appearance::Dark), "solarized-dark");
        assert_eq!(
            pair("catppuccin-mocha", Appearance::Light),
            "catppuccin-latte"
        );
        // Already suitable: returned untouched, no surprise substitutions.
        assert_eq!(pair("nord", Appearance::Dark), "nord");
        assert_eq!(pair("github", Appearance::Light), "github");
        // No counterpart in the set — fall back to the default, which is
        // guaranteed to suit the appearance.
        assert_eq!(pair("dracula", Appearance::Light), "catppuccin-latte");
        // Whatever comes out must always match the requested appearance.
        for (_, t) in THEMES {
            for appearance in [Appearance::Dark, Appearance::Light] {
                let out = for_appearance(*t, appearance);
                assert_eq!(
                    theme_is_light(out),
                    appearance.is_light(),
                    "{} -> {} is the wrong appearance",
                    theme_key(*t),
                    theme_key(out)
                );
            }
        }
    }
}

//! Application state and event handling.
//!
//! Blocking work (gh/git calls, diffing, highlighting) runs on background
//! threads as "jobs"; the main loop keeps drawing (spinner in the status bar)
//! and applies each job's outcome when it lands. Foreground jobs are modal —
//! input is ignored while one runs, except `c`/Esc to cancel (when the job is
//! cancellable) and `q` to quit. Viewed-state syncs to GitHub run as
//! fire-and-forget background jobs with optimistic local state.

use crate::blame::{self, Blame};
use crate::clipboard;
use crate::diff::{DisplayEntry, FileDiff, Pos, RowKind, Selection, Side};
use crate::editor::Editor;
use crate::github::{self, ChangedFile, CommentSide, PrDetail, PrRef, PrSummary};
use crate::gitops::{self, StageState};
use crate::highlight::{self, HlLine};
use crate::lsp::{self, Lsp};
use crate::markdown;
use crate::preview::{self, Preview};
use crate::search;
use crate::theme::Appearance;
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tui_textarea::TextArea;

/// How loupe was launched (see `--pr` / `--local` in main.rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    /// Default: review uncommitted local changes if there are any,
    /// otherwise fall through to the pull-request flow.
    Auto,
    /// Straight to pull requests, skipping the local scan.
    Pr,
    /// Local changes only, even when the working tree is clean.
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    PrList,
    Review,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    SideBySide,
    Inline,
}

pub struct CommentDraft {
    pub textarea: TextArea<'static>,
    pub path: String,
    pub side: Side,
    pub lo: usize,
    pub hi: usize,
}

pub enum Overlay {
    None,
    CheckoutPrompt(u64),
    Comment(Box<CommentDraft>),
    Help,
    ThemePicker(ThemePicker),
    Finder(Box<Finder>),
    /// What the language server says a symbol is (`K`).
    Hover(Box<HoverPanel>),
    /// "Are you sure?" for a revert — the one thing loupe does that
    /// destroys work, so it is the one thing that asks first.
    Revert(Box<RevertPrompt>),
    /// The right-click menu on a file-panel row.
    PathMenu(Box<PathMenu>),
    /// The commit behind one blame row: who, when, and the pull request
    /// it landed in.
    BlameMenu(Box<BlameMenu>),
    /// The ☰ menu in the top bar — everything the toolbar no longer has
    /// room to show.
    Menu(Box<Menu>),
}

/// One line of the ☰ menu. A heading is a label with no action; an item
/// runs a [`ButtonId`] through the same dispatch the toolbar uses, so a
/// menu line and a button can never drift apart.
pub enum MenuRow {
    Heading(&'static str),
    Item(MenuItem),
}

pub struct MenuItem {
    pub label: String,
    /// The key that does the same thing outside the menu, shown greyed on
    /// the right. Empty when there is no key for it.
    pub hint: &'static str,
    pub id: ButtonId,
    /// False for a line that does not apply right now (no selection to
    /// copy, nothing to revert). It is drawn dim and does nothing.
    pub enabled: bool,
    /// `Some` turns the line into a switch and draws its state.
    pub checked: Option<bool>,
}

/// The ☰ menu: a scrollable list of [`MenuRow`], anchored under the button
/// that opened it.
pub struct Menu {
    pub rows: Vec<MenuRow>,
    /// Index into `rows`; always an enabled [`MenuRow::Item`].
    pub sel: usize,
    /// First visible row, for a menu taller than the terminal.
    pub scroll: usize,
    /// The cell the ☰ button occupies. The menu hangs below it.
    pub anchor: (u16, u16),
}

impl Menu {
    /// The next selectable row in `step` direction, or the current one when
    /// there is nothing further. Headings and disabled lines are skipped.
    fn next_selectable(&self, from: usize, step: isize) -> usize {
        let mut i = from as isize;
        loop {
            i += step;
            if i < 0 || i as usize >= self.rows.len() {
                return from;
            }
            if matches!(&self.rows[i as usize], MenuRow::Item(it) if it.enabled) {
                return i as usize;
            }
        }
    }

    /// The first selectable row, for the initial selection.
    fn first_selectable(&self) -> usize {
        self.rows
            .iter()
            .position(|r| matches!(r, MenuRow::Item(it) if it.enabled))
            .unwrap_or(0)
    }

    /// Keep the selection inside the `height` visible rows.
    pub fn scroll_into_view(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.sel < self.scroll {
            self.scroll = self.sel;
        } else if self.sel >= self.scroll + height {
            self.scroll = self.sel + 1 - height;
        }
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(height));
    }
}

/// One line of the file-panel right-click menu.
pub struct PathMenuItem {
    /// The key that runs this line without selecting it first.
    pub key: char,
    pub label: &'static str,
    /// What the line puts on the clipboard.
    pub text: String,
}

/// The right-click menu on a file-panel row: the ways to copy the path of
/// the file or directory under the pointer. A path is the one thing in the
/// file panel that the diff selection cannot copy, because the panel shows
/// it shortened and split across tree rows.
pub struct PathMenu {
    /// The repo-relative path the menu is about, for its title.
    pub path: String,
    /// True when the row is a directory rather than a file.
    pub is_dir: bool,
    pub items: Vec<PathMenuItem>,
    pub sel: usize,
    /// The cell that was clicked. The menu is drawn next to it.
    pub anchor: (u16, u16),
}

/// One line of the blame popup: what it does, and the key that does it
/// without selecting the line first.
pub struct BlameMenuItem {
    pub key: char,
    pub label: String,
    pub action: BlameAction,
}

/// What a line of the blame popup does when it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlameAction {
    /// Open the pull request in the reader's browser.
    OpenPr(u64),
    /// Put text on the clipboard — a pull request link, a commit hash.
    Copy(String),
}

/// The popup behind one blame row: the commit that last touched the line,
/// and the ways to follow it somewhere.
///
/// This is the half of the pane that answers "is this related to what I
/// am doing?" — the pane itself only has room for a name, an age and a
/// number, and the rest of the story is here.
pub struct BlameMenu {
    pub commit: Arc<blame::Commit>,
    /// The pull request the commit landed in, once it is known.
    pub pr: Option<PrRef>,
    /// Whether this commit belongs to the change under review.
    pub in_change: bool,
    /// Whether the author is the reader.
    pub mine: bool,
    pub items: Vec<BlameMenuItem>,
    pub sel: usize,
    /// The cell that was clicked. The popup is drawn next to it.
    pub anchor: (u16, u16),
}

/// Which divider is under the pointer during a drag. The two are never
/// dragged at once, and naming the one in flight keeps the resize
/// arithmetic from being applied to the wrong panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dragging {
    None,
    /// The seam between the file panel and whatever is to its right.
    FilePanel,
    /// The seam between the blame pane and the diff.
    BlamePane,
}

/// What a revert is about to undo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevertTarget {
    /// Every change in one file, index and working tree both.
    File { idx: usize },
    /// One run of changed rows in the open diff, as a `[start, end)` range
    /// of [`FileDiff::rows`](crate::diff::FileDiff) — what the diff view
    /// calls a section.
    Section { start: usize, end: usize },
}

/// The confirm prompt, with everything it needs to say on it. The counts
/// are captured when the prompt opens so the message and the work can never
/// describe different things.
pub struct RevertPrompt {
    pub target: RevertTarget,
    pub path: String,
    pub adds: usize,
    pub dels: usize,
    /// The file goes away entirely: the change created it, so there is no
    /// earlier version to put back.
    pub deletes: bool,
}

/// The hover panel: a symbol's type and documentation, as plain text.
pub struct HoverPanel {
    pub word: String,
    pub lines: Vec<String>,
}

// ------------------------------------------------------------------ finder

/// Which question the finder is answering. The mode is picked by a prefix
/// character typed into the same input — the way a command palette does
/// it — so one overlay covers all three without three key bindings and
/// three sets of muscle memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinderMode {
    /// Fuzzy match on file paths (no prefix).
    Files,
    /// `#` — text across files, via `git grep`.
    Grep,
    /// `@` — definitions in the open file.
    Symbols,
    /// Everywhere a symbol is used, from the language server (`gr`).
    /// Not typed into — the list arrives, typing filters it.
    Refs,
    /// "Which of these did you mean?" — the symbols on one line, when a
    /// keyboard request has to choose between them.
    Pick,
}

impl FinderMode {
    pub fn prefix(self) -> &'static str {
        match self {
            FinderMode::Files => "",
            FinderMode::Grep => "#",
            FinderMode::Symbols => "@",
            FinderMode::Refs | FinderMode::Pick => "",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            FinderMode::Files => "Go to file",
            FinderMode::Grep => "Find in files",
            FinderMode::Symbols => "Symbols in this file",
            FinderMode::Refs => "References",
            FinderMode::Pick => "Which symbol?",
        }
    }

    /// Whether the mode is one the user can type their way into (the tabs
    /// along the bottom); `Refs` and `Pick` are arrived at, not chosen.
    pub fn is_tab(self) -> bool {
        matches!(
            self,
            FinderMode::Files | FinderMode::Grep | FinderMode::Symbols
        )
    }
}

/// A symbol loupe can ask a language server about: what it is, and where
/// it sits in the file being shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub path: String,
    /// 1-based line in the file on `side`.
    pub line: usize,
    /// 1-based character column.
    pub col: usize,
    pub word: String,
    /// Which side of the diff the line came from — the old side is a
    /// legitimate target, and answering from the *old* text is the honest
    /// thing to do when the cursor is on a removed line.
    pub side: Side,
}

/// The three questions loupe asks a language server about a symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspAction {
    Definition,
    References,
    Hover,
}

impl LspAction {
    fn verb(self) -> &'static str {
        match self {
            LspAction::Definition => "definition of",
            LspAction::References => "references to",
            LspAction::Hover => "type of",
        }
    }
}

/// One row of results.
pub struct FinderRow {
    pub path: String,
    /// 1-based line, when the row points inside a file.
    pub line: Option<usize>,
    /// The text shown: the path for a file row, the source line otherwise.
    pub text: String,
    /// Char indices within `text` that the fuzzy match landed on.
    pub matched: Vec<usize>,
    /// Char range of a literal match within `text`.
    pub range: Option<(usize, usize)>,
    /// Short tag shown on the right of the row.
    pub tag: &'static str,
    /// Whether this path is part of the changeset under review.
    pub in_changeset: bool,
    /// Index into the finder's `targets`, for a row that picks a symbol
    /// rather than a place — filtering reorders rows, so the link has to
    /// travel with the row itself.
    pub pick: Option<usize>,
}

/// The command palette: type to filter, Enter to go there.
pub struct Finder {
    pub mode: FinderMode,
    pub input: String,
    /// Insertion point, as a char index into `input`.
    pub cursor: usize,
    pub rows: Vec<FinderRow>,
    pub sel: usize,
    pub scroll: usize,
    /// Search the whole repository rather than just the changeset.
    pub repo_scope: bool,
    /// Treat the query as a regular expression (grep mode).
    pub regex: bool,
    /// A line of explanation under the input — result counts, why the
    /// list is empty, what to install.
    pub note: String,
    /// A query waiting out the debounce before it costs a subprocess.
    pending: Option<(String, Instant)>,
    /// Paths in the changeset — the default haystack, and what marks a
    /// result as "part of this change".
    changeset: Vec<String>,
    /// Every path in the repository, once someone asks for that scope.
    repo_files: Option<Vec<String>>,
    /// Definitions in the open file.
    symbols: Vec<search::Symbol>,
    symbol_path: String,
    /// Rows produced by something other than typing (references, or the
    /// symbols on one line); the input filters them rather than replacing
    /// them.
    preset: Vec<FinderRow>,
    /// Targets parallel to `preset`, for [`FinderMode::Pick`].
    targets: Vec<Target>,
    /// What to do with the pick once it's made.
    pending_action: Option<LspAction>,
}

/// How long a keystroke has to be the last one before grep runs. Long
/// enough that typing a word costs one subprocess, short enough that it
/// still feels live.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(140);

/// How long typing has to pause before the buffer is pushed to the
/// language server. Short enough that diagnostics feel live, long enough
/// that a fast typist doesn't send a copy of the file per keystroke.
const SYNC_DEBOUNCE: Duration = Duration::from_millis(220);

/// Rows the finder shows at once (the overlay is sized to match).
pub const FINDER_ROWS: usize = 14;

/// How long input has to stop before local review re-scans the working
/// tree by itself. Short enough that an agent's edits appear while you
/// read, long enough that it never runs mid-gesture.
const IDLE_BEFORE_RESCAN: Duration = Duration::from_millis(2000);

/// Floor between two automatic re-scans. A long read is a long idle, and
/// without this floor it would be one `git status` every 2 seconds.
const RESCAN_MIN_GAP: Duration = Duration::from_secs(5);

impl Finder {
    fn new(
        mode: FinderMode,
        changeset: Vec<String>,
        symbols: Vec<search::Symbol>,
        symbol_path: String,
    ) -> Self {
        Finder {
            mode,
            input: String::new(),
            cursor: 0,
            rows: Vec::new(),
            sel: 0,
            scroll: 0,
            repo_scope: false,
            regex: false,
            note: String::new(),
            pending: None,
            changeset,
            repo_files: None,
            symbols,
            symbol_path,
            preset: Vec::new(),
            targets: Vec::new(),
            pending_action: None,
        }
    }

    fn insert(&mut self, ch: char) {
        let at = self
            .input
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len());
        self.input.insert(at, ch);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let at = self
            .input
            .char_indices()
            .nth(self.cursor - 1)
            .map(|(i, _)| i);
        if let Some(at) = at {
            self.input.remove(at);
            self.cursor -= 1;
        }
    }

    /// Ctrl+W: rub out the word before the cursor.
    fn delete_word(&mut self) {
        while self.cursor > 0 && self.char_before().is_some_and(|c| c.is_whitespace()) {
            self.backspace();
        }
        while self.cursor > 0 && self.char_before().is_some_and(|c| !c.is_whitespace()) {
            self.backspace();
        }
    }

    fn char_before(&self) -> Option<char> {
        self.input.chars().nth(self.cursor.checked_sub(1)?)
    }

    fn move_sel(&mut self, delta: i32) {
        if self.rows.is_empty() {
            self.sel = 0;
            return;
        }
        let last = self.rows.len() as i32 - 1;
        self.sel = (self.sel as i32 + delta).clamp(0, last) as usize;
        if self.sel < self.scroll {
            self.scroll = self.sel;
        } else if self.sel >= self.scroll + FINDER_ROWS {
            self.scroll = self.sel + 1 - FINDER_ROWS;
        }
    }

    /// The haystack for file matching: the changeset, or the whole repo
    /// once its file list has arrived.
    fn file_haystack(&self) -> &[String] {
        match (&self.repo_files, self.repo_scope) {
            (Some(all), true) => all,
            _ => &self.changeset,
        }
    }

    /// Recompute the rows that can be produced without a subprocess.
    /// Grep is not one of them — it sets `pending` instead.
    fn rebuild(&mut self) {
        self.rows.clear();
        self.sel = 0;
        self.scroll = 0;
        match self.mode {
            FinderMode::Files => {
                let query = self.input.trim().to_string();
                let changeset = self.changeset.clone();
                let mut scored: Vec<(i32, FinderRow)> = Vec::new();
                for path in self.file_haystack() {
                    let row = |score: i32, matched: Vec<usize>| {
                        let in_changeset = changeset.iter().any(|p| p == path);
                        (
                            score,
                            FinderRow {
                                path: path.clone(),
                                line: None,
                                text: path.clone(),
                                matched,
                                range: None,
                                tag: if in_changeset { "changed" } else { "" },
                                in_changeset,
                                pick: None,
                            },
                        )
                    };
                    if query.is_empty() {
                        scored.push(row(0, Vec::new()));
                    } else if let Some((score, matched)) = search::fuzzy(&query, path) {
                        scored.push(row(score, matched));
                    }
                }
                if !query.is_empty() {
                    // Changed files first at equal score: in review, the
                    // file you touched is nearly always the one you meant.
                    scored.sort_by(|a, b| {
                        b.0.cmp(&a.0)
                            .then(b.1.in_changeset.cmp(&a.1.in_changeset))
                            .then(a.1.path.len().cmp(&b.1.path.len()))
                    });
                }
                self.rows = scored
                    .into_iter()
                    .take(search::RESULT_LIMIT)
                    .map(|(_, r)| r)
                    .collect();
                self.note = if self.repo_scope && self.repo_files.is_none() {
                    "Loading the repository file list…".into()
                } else {
                    format!(
                        "{} of {} files · Tab {}",
                        self.rows.len(),
                        self.file_haystack().len(),
                        if self.repo_scope {
                            "back to changed files"
                        } else {
                            "search the whole repo"
                        }
                    )
                };
            }
            FinderMode::Symbols => {
                let query = self.input.trim().to_string();
                let path = self.symbol_path.clone();
                let mut scored: Vec<(i32, FinderRow)> = Vec::new();
                for sym in &self.symbols {
                    let (score, matched) = if query.is_empty() {
                        (0, Vec::new())
                    } else {
                        match search::fuzzy(&query, &sym.name) {
                            Some(v) => v,
                            None => continue,
                        }
                    };
                    scored.push((
                        score,
                        FinderRow {
                            path: path.clone(),
                            line: Some(sym.line),
                            text: sym.name.clone(),
                            matched,
                            range: None,
                            tag: sym.kind,
                            in_changeset: true,
                            pick: None,
                        },
                    ));
                }
                if !query.is_empty() {
                    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
                }
                self.rows = scored.into_iter().map(|(_, r)| r).collect();
                self.note = if self.symbols.is_empty() {
                    "No definitions found in this file.".into()
                } else {
                    format!("{} of {} symbols", self.rows.len(), self.symbols.len())
                };
            }
            FinderMode::Grep => {
                let query = self.input.trim().to_string();
                if query.chars().count() < 2 {
                    self.pending = None;
                    self.note = "Type at least two characters.".into();
                } else {
                    self.pending = Some((query, Instant::now()));
                    self.note = "Searching…".into();
                }
            }
            // A list that already exists: typing narrows it down.
            FinderMode::Refs | FinderMode::Pick => {
                let query = self.input.trim().to_lowercase();
                let total = self.preset.len();
                self.rows = self
                    .preset
                    .iter()
                    .filter(|r| {
                        query.is_empty()
                            || r.text.to_lowercase().contains(&query)
                            || r.path.to_lowercase().contains(&query)
                    })
                    .map(|r| FinderRow {
                        path: r.path.clone(),
                        line: r.line,
                        text: r.text.clone(),
                        matched: r.matched.clone(),
                        range: r.range,
                        tag: r.tag,
                        in_changeset: r.in_changeset,
                        pick: r.pick,
                    })
                    .collect();
                if self.mode == FinderMode::Pick {
                    self.note = "Enter picks the symbol · Esc cancels".into();
                } else if !query.is_empty() {
                    self.note = format!("{} of {total} shown", self.rows.len());
                }
            }
        }
    }
}

/// State of the in-app theme picker (`t` / the 🎨 Theme button). Selecting
/// a row switches the process-wide theme immediately — the sample in the
/// overlay previews it — and Enter persists the choice to the config file;
/// Esc restores `prev`.
///
/// The picker also owns the light/dark switch (`a`), since the two choices
/// belong together: the syntax theme and loupe's own colors have to agree
/// or the diff ends up unreadable, which is the whole reason this exists.
pub struct ThemePicker {
    pub sel: usize,
    pub scroll: usize,
    prev: two_face::theme::EmbeddedThemeName,
    prev_appearance: Appearance,
    /// The row that was selected the last time each appearance was active,
    /// as `[dark, light]`. Without this, `a` twice would not come back:
    /// theme pairing is many-to-one (three Catppuccin flavors share Latte),
    /// so mapping out and back lands on a different theme — which Enter
    /// would then write over the user's actual choice.
    remembered: [Option<usize>; 2],
    /// `wizard::SAMPLE` highlighted with the selected theme.
    pub preview: Vec<HlLine>,
}

impl ThemePicker {
    fn new() -> Self {
        let current = highlight::current_theme();
        ThemePicker {
            sel: highlight::THEMES
                .iter()
                .position(|(_, t)| *t == current)
                .unwrap_or(0),
            scroll: 0,
            prev: current,
            prev_appearance: crate::theme::appearance(),
            remembered: [None, None],
            preview: highlight::highlight("sample.rs", crate::wizard::SAMPLE),
        }
    }

    fn select(&mut self, idx: usize) {
        self.sel = idx.min(highlight::THEMES.len() - 1);
        highlight::set_theme(highlight::THEMES[self.sel].1);
        self.preview = highlight::highlight("sample.rs", crate::wizard::SAMPLE);
    }

    /// Flip light ⇄ dark, moving the selection to the counterpart of the
    /// current theme so the preview stays coherent — or back to whatever
    /// was selected the last time this appearance was active.
    fn toggle_appearance(&mut self) {
        let current = crate::theme::appearance();
        let next = current.other();
        self.remembered[usize::from(current.is_light())] = Some(self.sel);
        crate::theme::set_appearance(next);
        let idx = self.remembered[usize::from(next.is_light())].unwrap_or_else(|| {
            let paired = highlight::for_appearance(highlight::THEMES[self.sel].1, next);
            highlight::THEMES
                .iter()
                .position(|(_, t)| *t == paired)
                .unwrap_or(self.sel)
        });
        self.select(idx);
    }

    /// Whether the user actually moved off the appearance loupe started
    /// with. Toggling twice is not an override, so it must not pin
    /// `appearance` in the config and disable detection everywhere else.
    fn appearance_changed(&self) -> bool {
        crate::theme::appearance() != self.prev_appearance
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonId {
    ViewTree,
    ViewFlat,
    FoldToggle,
    Edit,
    Comment,
    BackToPrs,
    LocalChanges,
    Refresh,
    Help,
    CheckoutYes,
    CheckoutReviewOnly,
    CheckoutCancel,
    CommentPost,
    CommentCancel,
    EditorSave,
    EditorClose,
    /// ⇥ Format in the editor top bar (also Ctrl+T).
    EditorFormat,
    /// 📖 Preview / ✎ Source — the toggle between the rendered markdown
    /// and the text it came from (also the `P` key).
    PreviewToggle,
    /// ✕ on the preview: back to the diff.
    PreviewClose,
    Theme,
    ThemeApply,
    ThemeCancel,
    ThemeRow(usize),
    /// The ☀/🌙 light-dark switch in the theme picker.
    AppearanceToggle,
    /// The PR ⇄ local toggle in the review top bar (also the ` key).
    SwapView,
    /// 🔍 in the review top bar — opens the finder.
    Find,
    /// ⧉ in the review top bar — copies the selected lines.
    Copy,
    /// A result row in the finder.
    FinderRow(usize),
    /// The finder's mode tabs and its changeset/repo scope switch.
    FinderMode(FinderMode),
    FinderScope,
    FinderClose,
    /// The two buttons on the revert confirm prompt.
    RevertYes,
    RevertCancel,
    /// A line of the file-panel right-click menu.
    PathMenuRow(usize),
    /// A line of the blame popup.
    BlameMenuRow(usize),
    /// One row that shows or hides the blame pane.
    BlameToggle,

    // --- the ☰ menu and the lines only it offers
    /// ☰ in the top bar.
    Menu,
    /// A line of the ☰ menu, by index into [`Menu::rows`].
    MenuRow(usize),
    /// One row that flips between split and inline (the toolbar used to
    /// spend two buttons on this).
    ViewToggle,
    /// One row that flips the file panel between tree and flat.
    TreeToggle,
    /// The finder, opened straight into one of its three modes.
    FindGrep,
    FindSymbols,
    /// `/` — the incremental search inside the open diff.
    FindInDiff,
    /// ↺ on the section at the cursor, and on the whole file. Both ask
    /// before they touch anything.
    RevertSection,
    RevertFile,
    /// The idle re-scan switch (see `App::auto_refresh`).
    AutoRefreshToggle,
    Quit,
}

/// Clickable regions recorded during the last draw.
#[derive(Default)]
pub struct HitAreas {
    pub buttons: Vec<(Rect, ButtonId)>,
    /// The PR / LOCAL badge in the review top bar. Right-click copies the
    /// PR link, so a coding agent can be pointed at the same PR.
    pub badge: Rect,
    pub pr_list: Rect,
    pub file_list: Rect,
    pub diff: Rect,
    /// The whole review body (file panel + diff), for resize arithmetic.
    pub review: Rect,
    /// The two border columns between the panels — drag to resize.
    pub divider: Rect,
    /// The blame pane, when it is showing. Zero-sized when it is not.
    pub blame: Rect,
    /// The seam between the blame pane and the diff — the second drag
    /// handle, and zero-sized for the same reason.
    pub blame_divider: Rect,
}

impl HitAreas {
    pub fn button_at(&self, x: u16, y: u16) -> Option<ButtonId> {
        self.buttons
            .iter()
            .find(|(r, _)| contains(*r, x, y))
            .map(|(_, id)| *id)
    }
}

pub fn contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

fn spinner_frame(started: Instant) -> char {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    FRAMES[(started.elapsed().as_millis() / 80) as usize % FRAMES.len()]
}

/// Default width of the file panel, and the limits a resize is clamped to.
pub const FILE_PANEL_DEFAULT: u16 = 34;
pub const FILE_PANEL_MIN: u16 = 16;
/// Columns the diff pane must keep, whatever the file panel is dragged to.
pub const DIFF_MIN_W: u16 = 24;
/// Default width of the blame pane, and the floor a resize is clamped to.
/// The floor still fits the heat bar and a short name; the age and the
/// pull request number drop out above it (see `ui::draw_blame`).
pub const BLAME_DEFAULT: u16 = 30;
pub const BLAME_MIN: u16 = 12;
/// Width of the change bar down the left of the diff — the ↺ marker on each
/// changed section, and the click target that reverts it. Only reserved
/// when reverting is possible at all (see [`App::revert_gutter`]), so a
/// read-only review still gets the full width for code.
pub const REVERT_W: u16 = 2;
/// Lines of context kept between the cursor and the edge of the diff pane.
const SCROLLOFF: usize = 3;
/// Columns one Left/Right key press scrolls the diff body.
const HSCROLL_STEP: i32 = 8;
/// Columns one sideways wheel notch scrolls it — smaller, because a
/// trackpad swipe delivers a stream of them.
const HSCROLL_WHEEL: i32 = 4;

/// How a revert prompt names what is at stake: "3 added and 1 removed
/// lines", in whichever halves are non-zero.
pub fn lines_phrase(adds: usize, dels: usize) -> String {
    let s = |n: usize| if n == 1 { "" } else { "s" };
    match (adds, dels) {
        (0, 0) => "nothing".into(),
        (a, 0) => format!("{a} added line{}", s(a)),
        (0, d) => format!("{d} removed line{}", s(d)),
        (a, d) => format!("{a} added and {d} removed line{}", s(a.max(d))),
    }
}

/// True if `path` itself is a symlink (without following it).
fn is_symlink(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

// ------------------------------------------------------------------ file list

/// One row of the file panel (flat list or tree).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEntry {
    Dir {
        /// Display name; single-child dir chains are compressed ("a/b/c").
        label: String,
        /// Full repo-relative path of the (compressed) directory.
        path: String,
        depth: u16,
    },
    File {
        /// Index into `App::files`.
        idx: usize,
        depth: u16,
    },
}

fn build_tree_entries(files: &[ChangedFile], collapsed: &HashSet<String>) -> Vec<FileEntry> {
    #[derive(Default)]
    struct Node {
        dirs: BTreeMap<String, Node>,
        files: Vec<(String, usize)>,
    }
    let mut root = Node::default();
    for (i, f) in files.iter().enumerate() {
        let mut parts: Vec<&str> = f.path.split('/').collect();
        let base = parts.pop().unwrap_or("").to_string();
        let mut node = &mut root;
        for p in parts {
            node = node.dirs.entry(p.to_string()).or_default();
        }
        node.files.push((base, i));
    }
    fn emit(
        node: &Node,
        prefix: &str,
        depth: u16,
        collapsed: &HashSet<String>,
        out: &mut Vec<FileEntry>,
    ) {
        for (name, child) in &node.dirs {
            // Compress chains of single-subdir folders with no files.
            let mut label = name.clone();
            let mut cur = child;
            while cur.files.is_empty() && cur.dirs.len() == 1 {
                let (n2, c2) = cur.dirs.iter().next().expect("len checked");
                label.push('/');
                label.push_str(n2);
                cur = c2;
            }
            let path = if prefix.is_empty() {
                label.clone()
            } else {
                format!("{prefix}/{label}")
            };
            out.push(FileEntry::Dir {
                label,
                path: path.clone(),
                depth,
            });
            if !collapsed.contains(&path) {
                emit(cur, &path, depth + 1, collapsed, out);
            }
        }
        let mut fs = node.files.clone();
        fs.sort();
        for (_, idx) in fs {
            out.push(FileEntry::File { idx, depth });
        }
    }
    let mut out = Vec::new();
    emit(&root, "", 0, collapsed, &mut out);
    out
}

// ------------------------------------------------------------------ jobs

/// A modal background task; its result arrives as an [`Outcome`].
pub struct ForegroundJob {
    rx: Receiver<Result<Outcome>>,
    pub label: String,
    pub cancellable: bool,
    /// Startup auto-open chain: on cancel/failure, fall back to the PR list.
    fallback_to_list: bool,
    started: Instant,
}

/// Fire-and-forget viewed-state sync; only failures are reported.
/// A fire-and-forget sync behind an optimistic local change. On failure the
/// change is rolled back; a staging sync also brings back a fresh read of
/// the index.
struct BgJob {
    rx: Receiver<Result<Option<HashMap<String, StageState>>>>,
    kind: BgKind,
}

enum BgKind {
    /// GitHub "viewed" checkbox; `viewed` is what we set it to.
    Viewed { path: String, viewed: bool },
    /// `git add` / unstage; `before` is the state to restore on failure.
    Stage { path: String, before: StageState },
    /// A plain re-read of the index after something changed the working
    /// tree under it. Nothing to roll back if it fails.
    Rescan,
    /// `gh pr view --web` from the blame popup. Nothing to roll back —
    /// it either opened a browser or it says why it could not.
    OpenPr { number: u64 },
}

/// Everything that belongs to one side of the PR ⇄ local toggle. Swapping
/// stashes the current side here and restores the other one, so flipping
/// back is instant — no reload, no loading screen. `entries` and `display`
/// are cheap derivations and get rebuilt on restore instead of stored.
struct Workspace {
    local: bool,
    local_branch: Option<String>,
    pr: Option<PrDetail>,
    checked_out: bool,
    merge_base: String,
    files: Vec<ChangedFile>,
    viewed: HashSet<String>,
    stage: HashMap<String, StageState>,
    file_cursor: usize,
    file_scroll: usize,
    collapsed_dirs: HashSet<String>,
    diff: Option<FileDiff>,
    collapse_unchanged: bool,
    expanded_folds: HashSet<usize>,
    old_content: Option<String>,
    new_content: Option<String>,
    old_hl: Vec<HlLine>,
    new_hl: Vec<HlLine>,
    differs_from_head: bool,
    diff_scroll: usize,
    diff_cursor: usize,
    diff_hscroll: usize,
    selection: Option<Selection>,
}

/// A non-modal background refresh (swap-back re-check of the restored side).
/// Unlike a [`ForegroundJob`] it never blocks input and never shows the
/// loading screen — its result is applied in place when it lands.
struct QuietJob {
    rx: Receiver<Result<QuietOutcome>>,
    label: String,
    started: Instant,
    /// True when the idle timer started this, not the reader. An automatic
    /// refresh that finds nothing says nothing — otherwise the status line
    /// repeats "up to date" every few seconds forever.
    auto: bool,
}

pub struct PrRefreshData {
    detail: PrDetail,
    merge_base: String,
    files: Vec<ChangedFile>,
    viewed: HashSet<String>,
}

pub enum QuietOutcome {
    /// Fresh PR metadata + file list (the head may have moved).
    Pr(Box<PrRefreshData>),
    /// Fresh scan of uncommitted changes.
    Local(Box<LocalOpenedData>),
    /// A file reloaded in place, keeping the scroll position.
    File(Box<FileLoadedData>),
}

pub struct PrOpenedData {
    repo: String,
    repo_root: Option<PathBuf>,
    detail: PrDetail,
    checked_out: bool,
    merge_base: String,
    files: Vec<ChangedFile>,
    viewed: HashSet<String>,
    /// Opened automatically for the current branch (vs picked from the list).
    auto: bool,
}

pub struct FileLoadedData {
    idx: usize,
    path: String,
    old: Option<String>,
    new: Option<String>,
    old_hl: Vec<HlLine>,
    new_hl: Vec<HlLine>,
    differs: bool,
    diff: FileDiff,
}

pub struct EditorSavedData {
    path: String,
    content: String,
    differs: bool,
    diff: FileDiff,
    new_hl: Vec<HlLine>,
}

pub struct LocalOpenedData {
    /// Checked-out branch, or None on detached HEAD.
    branch: Option<String>,
    /// HEAD commit (the old side of every local diff); None in a repository
    /// with no commits yet — every file then shows as fully added.
    head: Option<String>,
    files: Vec<ChangedFile>,
    /// Which of them are staged (the file panel's + / ✓ column).
    stage: HashMap<String, StageState>,
}

/// A finished revert: what it undid, and the file reloaded from disk when
/// the file it touched is the one on screen.
pub struct RevertedData {
    /// "3 lines in src/app.rs" — the message half.
    what: String,
    /// Whether the working tree still has the file (false when it was one
    /// the change had created).
    gone: bool,
    /// A whole file, rather than one section of it — the local file list
    /// has to be rescanned either way, since the file just left it.
    whole_file: bool,
    file: Option<Box<FileLoadedData>>,
}

pub enum Outcome {
    BranchPr(Option<u64>),
    /// Changes thrown away — one section, or a whole file.
    Reverted(Box<RevertedData>),
    LocalOpened(Box<LocalOpenedData>),
    Prs {
        repo: String,
        prs: Vec<PrSummary>,
    },
    PrOpened(Box<PrOpenedData>),
    FileLoaded(Box<FileLoadedData>),
    CommentPosted {
        path: String,
        lo: usize,
        hi: usize,
    },
    EditorSaved(Box<EditorSavedData>),
    ExternalOpened(Box<ExternalFile>),
    /// A file outside the changeset was written; nothing else changed.
    ExternalSaved(String),
    /// Answers from a language server.
    Locations(Box<LocationsData>),
    Hover(Box<HoverData>),
}

/// A file opened from a search result (or a jump to a definition) that
/// isn't part of the changeset. There is no diff to show for it — it is
/// just a file — so it opens in the editor, over the review, and closing
/// it leaves the review exactly as it was.
pub struct ExternalFile {
    path: String,
    abs_path: PathBuf,
    content: String,
    /// Line to land on.
    line: Option<usize>,
    /// The working tree is on another branch, so this text came from the
    /// commit under review and must not be written back over the file.
    read_only: bool,
    /// Open it in the markdown preview rather than the editor.
    preview: bool,
}

/// One place a symbol is used, with the source line to show for it.
pub struct Place {
    loc: lsp::Loc,
    text: String,
}

pub struct LocationsData {
    action: LspAction,
    word: String,
    places: Vec<Place>,
}

pub struct HoverData {
    word: String,
    text: Option<String>,
}

/// Read a file the way the current review reads files: from the commit
/// under review, falling back to the working tree.
fn read_source(root: &std::path::Path, rev: Option<&str>, path: &str) -> Option<String> {
    let from_disk =
        || gitops::safe_repo_path(root, path).and_then(|p| std::fs::read_to_string(p).ok());
    match rev {
        Some(rev) => gitops::show_file(rev, path).or_else(from_disk),
        None => from_disk(),
    }
}

/// What the editor is asking its language server for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorRequest {
    Complete,
    Hover,
    Definition,
    Format,
}

impl EditorRequest {
    /// Whether the user asked for this by pressing a key — completion
    /// also fires on its own while typing, and an unavailable server
    /// must not shout about it once per character.
    fn is_explicit(self) -> bool {
        self != EditorRequest::Complete
    }
}

/// A language-server request made from the editor.
///
/// Like [`SearchJob`] and unlike a [`ForegroundJob`], this never blocks
/// input — you must be able to keep typing while a completion is in
/// flight — and a result from a keystroke you have already typed past is
/// dropped by generation number rather than applied late.
struct EditorJob {
    rx: Receiver<Result<EditorOutcome>>,
    gen: u64,
}

enum EditorOutcome {
    Completions(Vec<lsp::Completion>),
    Hover {
        word: String,
        text: Option<String>,
    },
    Definition {
        word: String,
        locs: Vec<lsp::Loc>,
    },
    /// `None` when the server does not format this language.
    Formatted(Option<Vec<lsp::TextEdit>>),
}

/// A finder query running in the background. Unlike every other job this
/// one is *replaceable*: each keystroke can start a new one, and results
/// from a query the user has already typed past are dropped by generation
/// number rather than being applied late.
struct SearchJob {
    rx: Receiver<Result<SearchOutcome>>,
    gen: u64,
    started: Instant,
}

enum SearchOutcome {
    Grep {
        hits: Vec<search::Hit>,
        truncated: bool,
        query: String,
    },
    /// The repository's file list, for whole-repo file matching.
    Files(Vec<String>),
    /// A language server's answer for the open file's symbols, replacing
    /// the pattern-matched ones already on screen.
    Symbols(Vec<lsp::Sym>),
}

/// Incremental search within the open diff (`/`), the vim-shaped half of
/// the feature: a prompt in the status bar, matches highlighted in place,
/// `n`/`N` to step. Kept separate from [`Finder`] because it searches
/// *what is on screen* rather than what is on disk.
#[derive(Default)]
pub struct Find {
    pub query: String,
    /// True while the prompt is open and taking keystrokes.
    pub typing: bool,
    /// Where the cursor was when the prompt opened, so Esc can undo the
    /// incremental jumping around.
    origin: (usize, usize),
    /// Diff rows containing a match, ascending.
    pub rows: Vec<usize>,
    /// Index into `rows` of the current match.
    pub at: usize,
}

impl Find {
    pub fn active(&self) -> bool {
        !self.query.is_empty()
    }
}

// ------------------------------------------------------------------ app

pub struct App {
    pub should_quit: bool,
    mode: LaunchMode,
    /// Configured upstream GitHub org: PR operations target
    /// `<org>/<repo-name>` instead of the clone's own owner.
    org: Option<String>,
    pub screen: Screen,
    pub repo: Option<String>,
    pub repo_root: PathBuf,

    /// True while reviewing local uncommitted changes instead of a PR
    /// (`pr` is None then; commenting and viewed-sync are off).
    pub local: bool,
    /// Branch name shown in the local-review top bar.
    pub local_branch: Option<String>,

    pub prs: Vec<PrSummary>,
    pub pr_cursor: usize,
    pub pr_scroll: usize,

    pub pr: Option<PrDetail>,
    pub checked_out: bool,
    pub merge_base: String,
    pub files: Vec<ChangedFile>,
    /// Paths marked as viewed (mirrors the GitHub PR page checkmarks).
    /// PR review only — local review stages files instead.
    pub viewed: HashSet<String>,
    /// Index state per path, for local-changes review.
    pub stage: HashMap<String, StageState>,
    pub file_cursor: usize,
    pub file_scroll: usize,
    pub tree_view: bool,
    pub collapsed_dirs: HashSet<String>,
    pub entries: Vec<FileEntry>,
    /// Width of the file panel in columns; dragged by the divider, seeded
    /// from the `file_panel_width` config key.
    pub file_panel_w: u16,
    /// Which divider is being dragged, if any.
    dragging: Dragging,

    pub diff: Option<FileDiff>,
    /// Visible diff lines for the current view/fold state.
    pub display: Vec<DisplayEntry>,
    pub collapse_unchanged: bool,
    pub expanded_folds: HashSet<usize>,
    pub old_content: Option<String>,
    pub new_content: Option<String>,
    pub old_hl: Vec<HlLine>,
    pub new_hl: Vec<HlLine>,
    /// True when the working-tree file no longer matches the PR head commit —
    /// review comments would misanchor, so commenting is blocked until the
    /// user pushes or reloads.
    pub differs_from_head: bool,
    pub diff_scroll: usize,
    /// Cursor row, as an index into `display`. Keyboard motions move it,
    /// clicking a line puts it there, and the renderer underlines it.
    pub diff_cursor: usize,
    /// Line-visual mode (`V`): motions extend the selection.
    pub select_mode: bool,
    /// True after a bare `g`, waiting for the second one of `gg`.
    pending_g: bool,
    /// Horizontal offset of the diff *body* in columns; the line-number
    /// gutters stay pinned.
    pub diff_hscroll: usize,
    pub view: ViewMode,
    pub selection: Option<Selection>,
    drag_select: bool,
    last_click: Option<(Instant, u16, u16)>,

    pub editor: Option<Editor>,
    /// The markdown preview, when one is open. It shares the pane with
    /// the diff and the editor, and never coexists with the editor —
    /// `P` swaps one for the other on the same file.
    pub preview: Option<Preview>,
    /// True when loupe was launched to read one file (`loupe md <path>`)
    /// and there is no review beside it: the preview takes the whole
    /// window and quitting is the only way out.
    pub preview_only: bool,
    pub overlay: Overlay,

    pub status: String,
    pub status_err: bool,
    pub job: Option<ForegroundJob>,
    bg_jobs: Vec<BgJob>,
    /// The other side of the PR ⇄ local toggle, kept loaded for an instant
    /// swap back. Always the opposite of the current `local` flag.
    stash: Option<Box<Workspace>>,
    /// In-flight silent refresh of the current side (see [`QuietJob`]).
    quiet: Option<QuietJob>,
    /// A quiet result that landed while a modal job was running; applied
    /// once the modal job finishes so the two can't fight over the state.
    /// The flag is the job's `auto` (see [`QuietJob`]).
    pending_quiet: Option<(QuietOutcome, bool)>,
    /// When the last key or mouse event arrived. The idle re-scan waits
    /// for a gap here, so it never lands mid-drag or mid-keystroke.
    last_input: Instant,
    /// When the last automatic re-scan started, for the [`RESCAN_MIN_GAP`]
    /// floor.
    last_auto_rescan: Instant,
    /// `auto_refresh` in the config: the idle re-scan of local changes.
    /// Also toggled from the ☰ menu, for this session.
    pub auto_refresh: bool,
    /// Language servers, started on demand and shared with worker
    /// threads (see [`crate::lsp`]).
    pub lsp: Lsp,
    /// `language_servers` in the config; off means loupe never starts one.
    pub lsp_enabled: bool,
    /// (display row, char column) of the last click on a diff line — an
    /// unambiguous "this symbol" for the request that follows it.
    click_word: Option<(usize, usize)>,
    /// In-flight editor request (see [`EditorJob`]).
    editor_job: Option<EditorJob>,
    editor_gen: u64,
    /// When the buffer last changed, for the idle push to the server.
    editor_touched: Option<Instant>,
    /// Diagnostics version last seen, so a redraw only happens when the
    /// servers actually said something new.
    diag_seen: u64,
    /// Run the language server's formatter on save (`format_on_save`).
    pub format_on_save: bool,
    /// True while a format is running as the first half of a save.
    format_then_save: bool,
    /// In-flight finder query (see [`SearchJob`]).
    search_job: Option<SearchJob>,
    /// Bumped on every new query; a result whose generation is stale is
    /// dropped instead of overwriting fresher rows.
    search_gen: u64,
    /// Incremental in-diff search (`/`, `n`, `N`).
    pub find: Find,
    /// A line to jump to once the file finishes loading (set when a
    /// search result names a file that isn't open yet).
    pending_jump: Option<usize>,
    /// Error to re-surface once a fallback PR-list load finishes.
    post_load_err: Option<String>,
    /// One-shot note prepended to the next file-loaded status message.
    auto_open_note: Option<String>,

    // --- the blame pane (see [`crate::blame`])
    /// Show the blame pane between the file panel and the diff. `blame`
    /// in the config seeds it; `B` and the ☰ menu flip it.
    pub blame_on: bool,
    /// Its width in columns, dragged by the second divider and seeded
    /// from the `blame_width` config key.
    pub blame_w: u16,
    /// Blame of the new side of the open file, and of the old side. Both
    /// are None until the job lands, and for a file with no history.
    pub blame_new: Option<Blame>,
    pub blame_old: Option<Blame>,
    /// What `blame_new` is the blame *of* — the file in the changeset,
    /// or the standalone file the editor has open. An answer for
    /// anything else is dropped.
    blame_path: Option<String>,
    /// In-flight blame read (see [`BlameJob`]).
    blame_job: Option<BlameJob>,
    /// In-flight GitHub lookup of the pull requests behind the commits
    /// whose subject did not name one. Kept in its own slot so opening
    /// another file cannot drop an answer that is already paid for.
    blame_pr_job: Option<Receiver<Result<HashMap<String, PrRef>>>>,
    /// Bumped whenever the pane's subject changes; an answer with a stale
    /// generation is dropped rather than drawn over the wrong file.
    blame_gen: u64,
    /// Commit hash → pull request, for the whole session. Filled from
    /// commit subjects first and from GitHub second, so a second file
    /// sharing the same history costs nothing.
    pub blame_prs: HashMap<String, PrRef>,
    /// Hashes already asked about, so a commit GitHub knows nothing about
    /// is not asked about once per file for the rest of the session.
    blame_asked: HashSet<String>,
    /// Commits belonging to the change under review — the lines this pull
    /// request (or this unpushed branch) moved.
    pub blame_change_set: HashSet<String>,
    /// `git config user.email`, lowercased, so the pane can tell the
    /// reader's own commits apart. None when git has none set.
    pub blame_me: Option<String>,
    /// `owner/name` read from the origin remote. Local review never
    /// resolves a repository — it has no reason to talk to GitHub — so
    /// without this the pane could never link a commit to its pull
    /// request in exactly the review that most wants the link.
    blame_origin: Option<String>,
    /// True once the change set and the email have been read for this
    /// review; they are per-review, not per-file.
    blame_ctx: bool,
    /// True once a blame job has answered for the open file. A file with
    /// no history answers with nothing, so "no blame" and "not asked
    /// yet" have to be two different states or a silent refresh would
    /// re-blame it every few seconds forever.
    blame_done: bool,
    /// `blame_pr_lookup` in the config: may loupe ask GitHub which pull
    /// request a blamed commit belongs to.
    pub blame_pr_lookup: bool,

    pub layout: HitAreas,
}

/// An in-flight blame read. Blame is deliberately *not* part of the file
/// load: a `git blame` on a long file can take a second, and paying that
/// on every file open would undo the load latency the diff pipeline is
/// built around. So the pane fills in a moment after the diff does.
struct BlameJob {
    rx: Receiver<Box<BlameData>>,
    gen: u64,
}

pub struct BlameData {
    /// The file this is the blame of, checked against the open one before
    /// it is applied.
    path: String,
    new: Option<Blame>,
    old: Option<Blame>,
    /// Read on the first blame of a review, and None on every one after
    /// it — this describes the review, not the file.
    ctx: Option<BlameCtx>,
}

/// What the pane needs to know once per review rather than once per file.
pub struct BlameCtx {
    /// Commits belonging to the change under review.
    change_set: HashSet<String>,
    /// `git config user.email`, lowercased.
    me: Option<String>,
    /// `owner/name` from the origin remote, for a pull request link when
    /// the review itself did not resolve a repository.
    origin: Option<String>,
}

impl App {
    pub fn new(mode: LaunchMode, org: Option<String>) -> Self {
        App {
            should_quit: false,
            mode,
            org: org.filter(|o| !o.trim().is_empty()),
            screen: Screen::PrList,
            repo: None,
            repo_root: PathBuf::from("."),
            local: false,
            local_branch: None,
            prs: Vec::new(),
            pr_cursor: 0,
            pr_scroll: 0,
            pr: None,
            checked_out: false,
            merge_base: String::new(),
            files: Vec::new(),
            viewed: HashSet::new(),
            stage: HashMap::new(),
            file_cursor: 0,
            file_scroll: 0,
            tree_view: true,
            collapsed_dirs: HashSet::new(),
            entries: Vec::new(),
            file_panel_w: FILE_PANEL_DEFAULT,
            dragging: Dragging::None,
            diff: None,
            display: Vec::new(),
            collapse_unchanged: true,
            expanded_folds: HashSet::new(),
            old_content: None,
            new_content: None,
            old_hl: Vec::new(),
            new_hl: Vec::new(),
            differs_from_head: false,
            diff_scroll: 0,
            diff_cursor: 0,
            select_mode: false,
            pending_g: false,
            diff_hscroll: 0,
            view: ViewMode::SideBySide,
            selection: None,
            drag_select: false,
            last_click: None,
            editor: None,
            preview: None,
            preview_only: false,
            overlay: Overlay::None,
            status: String::new(),
            status_err: false,
            job: None,
            bg_jobs: Vec::new(),
            stash: None,
            quiet: None,
            pending_quiet: None,
            last_input: Instant::now(),
            last_auto_rescan: Instant::now(),
            auto_refresh: true,
            lsp: Lsp::default(),
            lsp_enabled: true,
            click_word: None,
            editor_job: None,
            editor_gen: 0,
            editor_touched: None,
            diag_seen: 0,
            format_on_save: false,
            format_then_save: false,
            search_job: None,
            search_gen: 0,
            find: Find::default(),
            pending_jump: None,
            post_load_err: None,
            auto_open_note: None,
            blame_on: false,
            blame_w: BLAME_DEFAULT,
            blame_new: None,
            blame_old: None,
            blame_path: None,
            blame_job: None,
            blame_pr_job: None,
            blame_gen: 0,
            blame_prs: HashMap::new(),
            blame_asked: HashSet::new(),
            blame_change_set: HashSet::new(),
            blame_me: None,
            blame_origin: None,
            blame_ctx: false,
            blame_done: false,
            blame_pr_lookup: true,
            layout: HitAreas::default(),
        }
    }

    fn ok(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_err = false;
    }

    fn err(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_err = true;
    }

    // ------------------------------------------------------------- job engine

    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// True while either divider is being dragged (the idle re-scan and
    /// the status bar only care that a drag is in flight).
    pub fn resizing(&self) -> bool {
        self.dragging != Dragging::None
    }

    /// Which divider is being dragged — the renderer accents that seam
    /// and leaves the other one alone.
    pub fn dragging(&self) -> Dragging {
        self.dragging
    }

    /// Spinner frame + label + cancellability for the status bar.
    pub fn spinner(&self) -> Option<(char, &str, bool)> {
        self.job
            .as_ref()
            .map(|j| (spinner_frame(j.started), j.label.as_str(), j.cancellable))
    }

    /// True while a silent background refresh is running (keeps the main
    /// loop ticking so its spinner animates and its result lands promptly).
    pub fn refreshing(&self) -> bool {
        self.quiet.is_some()
    }

    /// Push the editor buffer to its language server once typing pauses.
    ///
    /// Debounced, and deliberately best-effort: `sync_open` never blocks
    /// and never starts a server, so a slow or missing one costs a
    /// skipped tick rather than a stutter between keystrokes.
    fn sync_editor_buffer(&mut self) -> bool {
        if !self.lsp_enabled {
            return false;
        }
        let Some(touched) = self.editor_touched else {
            return false;
        };
        if touched.elapsed() < SYNC_DEBOUNCE {
            return false;
        }
        let Some(editor) = &self.editor else {
            self.editor_touched = None;
            return false;
        };
        let text = editor.content();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&text, &mut hasher);
        let hash = std::hash::Hasher::finish(&hasher);
        if hash == editor.synced {
            self.editor_touched = None;
            return false;
        }
        let path = editor.path.clone();
        let root = self.repo_root.clone();
        if self.lsp.sync_open(&root, &path, &text) {
            self.editor_touched = None;
            if let Some(editor) = &mut self.editor {
                editor.synced = hash;
            }
        }
        false
    }

    /// Collect anything the servers have pushed — diagnostics, mostly —
    /// and hand the current file's to the editor.
    fn poll_diagnostics(&mut self) -> bool {
        if !self.lsp_enabled || self.editor.is_none() {
            return false;
        }
        let version = self.lsp.poll();
        if version == self.diag_seen {
            return false;
        }
        self.diag_seen = version;
        let root = self.repo_root.clone();
        let Some(path) = self.editor.as_ref().map(|e| e.path.clone()) else {
            return false;
        };
        let list = self.lsp.diagnostics(&root, &path);
        if let Some(editor) = &mut self.editor {
            if editor.diagnostics == list {
                return false;
            }
            editor.diagnostics = list;
        }
        true
    }

    /// True while a finder query is running or waiting out its debounce.
    /// The main loop treats this like `busy()` for timing purposes only —
    /// input stays live throughout.
    pub fn searching(&self) -> bool {
        self.search_job.is_some()
            || matches!(&self.overlay, Overlay::Finder(f) if f.pending.is_some())
    }

    /// Spinner frame for a running query — a whole-repo grep on a large
    /// tree is fast, but not instant, and silence reads as breakage.
    pub fn search_spinner(&self) -> Option<char> {
        self.search_job.as_ref().map(|j| spinner_frame(j.started))
    }

    /// Spinner frame + label for a silent refresh — informational only,
    /// input stays live.
    pub fn quiet_spinner(&self) -> Option<(char, &str)> {
        self.quiet
            .as_ref()
            .map(|q| (spinner_frame(q.started), q.label.as_str()))
    }

    fn spawn<F>(&mut self, label: impl Into<String>, cancellable: bool, fallback: bool, work: F)
    where
        F: FnOnce() -> Result<Outcome> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(work());
        });
        self.job = Some(ForegroundJob {
            rx,
            label: label.into(),
            cancellable,
            fallback_to_list: fallback,
            started: Instant::now(),
        });
    }

    /// Cancel the running foreground job (if it allows it). The worker thread
    /// keeps running to completion but its result is dropped.
    pub fn cancel_job(&mut self) {
        let Some(job) = self.job.take() else { return };
        if !job.cancellable {
            self.job = Some(job);
            return;
        }
        self.err(format!("✗ Cancelled — {}.", job.label));
        if job.fallback_to_list && self.screen == Screen::PrList {
            self.spawn_load_prs();
        }
    }

    /// Apply any finished background work. Called every main-loop tick.
    /// Returns true when something was applied and a redraw is needed.
    pub fn poll_jobs(&mut self) -> bool {
        let mut changed = false;
        // Viewed-state syncs: report failures, revert the optimistic flip.
        let mut i = 0;
        while i < self.bg_jobs.len() {
            let res = match self.bg_jobs[i].rx.try_recv() {
                Ok(r) => Some(r),
                Err(TryRecvError::Disconnected) => {
                    Some(Err(anyhow::anyhow!("sync task ended unexpectedly")))
                }
                Err(TryRecvError::Empty) => None,
            };
            match res {
                None => i += 1,
                Some(r) => {
                    let bg = self.bg_jobs.remove(i);
                    match (r, bg.kind) {
                        // Staging succeeded: adopt the fresh index read (it
                        // knows about partial staging, which the optimistic
                        // guess can't).
                        (Ok(Some(states)), _) => {
                            self.stage = states;
                            changed = true;
                        }
                        (Ok(None), _) => {}
                        (Err(e), BgKind::Viewed { path, viewed }) => {
                            if viewed {
                                self.viewed.remove(&path);
                            } else {
                                self.viewed.insert(path.clone());
                            }
                            self.err(format!("Viewed sync for {path} failed: {e:#}"));
                            changed = true;
                        }
                        (Err(e), BgKind::Stage { path, before }) => {
                            self.stage.insert(path.clone(), before);
                            self.err(format!("Staging {path} failed: {e:#}"));
                            changed = true;
                        }
                        // A failed re-read leaves the icons as they were:
                        // cosmetic, and not worth a scary message.
                        (Err(_), BgKind::Rescan) => {}
                        (Err(e), BgKind::OpenPr { number }) => {
                            self.err(format!("Couldn't open PR #{number}: {e:#}"));
                            changed = true;
                        }
                    }
                }
            }
        }

        // The blame pane fills in after the diff it sits beside.
        if self.poll_blame() {
            changed = true;
        }

        // A previewed file that something else rewrote — the plan file an
        // agent is still writing — is re-rendered where it stands.
        if self.poll_preview_reload() {
            changed = true;
        }

        // Keep the language server's copy of the buffer current, and pick
        // up whatever it has pushed back.
        if self.sync_editor_buffer() {
            changed = true;
        }
        if self.poll_diagnostics() {
            changed = true;
        }
        if let Some(job) = &self.editor_job {
            match job.rx.try_recv() {
                Ok(r) => {
                    let gen = job.gen;
                    self.editor_job = None;
                    match r {
                        Ok(o) => self.apply_editor_outcome(gen, o),
                        Err(e) => self.err(format!("{e:#}")),
                    }
                    changed = true;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.editor_job = None;
                    changed = true;
                }
            }
        }

        // A debounced query whose quiet period has elapsed becomes a job.
        if self.maybe_spawn_search() {
            changed = true;
        }
        if let Some(job) = &self.search_job {
            match job.rx.try_recv() {
                Ok(r) => {
                    let gen = job.gen;
                    self.search_job = None;
                    match r {
                        Ok(o) => self.apply_search(gen, o),
                        Err(e) => {
                            if let Overlay::Finder(f) = &mut self.overlay {
                                f.rows.clear();
                                f.note = format!("Search failed: {e:#}");
                            }
                        }
                    }
                    changed = true;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.search_job = None;
                    changed = true;
                }
            }
        }

        // Silent refreshes: applied in place, never modal. If a modal job is
        // mid-flight the result is parked until it finishes, so the two can't
        // interleave their writes to the same state.
        if let Some(q) = &self.quiet {
            match q.rx.try_recv() {
                Ok(r) => {
                    let auto = q.auto;
                    self.quiet = None;
                    match r {
                        Ok(o) if self.job.is_some() => self.pending_quiet = Some((o, auto)),
                        Ok(o) => self.apply_quiet(o, auto),
                        // A failed refresh never disturbs what's on screen.
                        // An automatic one fails silently too: the reader
                        // never asked, so a scary line would be noise.
                        Err(e) => {
                            if !auto {
                                self.err(format!("Background refresh failed: {e:#}"));
                            }
                        }
                    }
                    changed = true;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.quiet = None;
                    changed = true;
                }
            }
        }

        // Nothing in flight and the reader has gone quiet: check the tree.
        if self.maybe_auto_rescan() {
            changed = true;
        }

        let Some(job) = &self.job else { return changed };
        let res = match job.rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return changed,
            Err(TryRecvError::Disconnected) => {
                Err(anyhow::anyhow!("background task ended unexpectedly"))
            }
        };
        let job = self.job.take().expect("job checked above");
        match res {
            Ok(outcome) => self.apply(outcome),
            Err(e) => {
                self.err(format!("{e:#}"));
                if job.fallback_to_list && self.screen == Screen::PrList {
                    // Keep the failure visible after the list loads.
                    self.post_load_err = Some(self.status.clone());
                    self.spawn_load_prs();
                }
            }
        }
        // A quiet result that arrived mid-job can go on now.
        if self.job.is_none() {
            if let Some((o, auto)) = self.pending_quiet.take() {
                self.apply_quiet(o, auto);
            }
        }
        true
    }

    /// Start an idle re-scan of the working tree when the reader has been
    /// still long enough.
    ///
    /// This is the answer to an agent editing files under an open review:
    /// the diff on screen goes stale the moment something else writes to
    /// the tree, and nothing in a terminal tells loupe that it happened.
    /// Only local review polls — a pull request lives on GitHub, and
    /// polling that on a timer spends API calls for a head commit that
    /// moves a few times a day.
    ///
    /// Every condition below means "the reader is in the middle of
    /// something": a modal job, an overlay, the editor, a live selection,
    /// a drag. The re-scan waits rather than pulling the ground out.
    fn maybe_auto_rescan(&mut self) -> bool {
        if !self.should_auto_rescan() {
            return false;
        }
        self.last_auto_rescan = Instant::now();
        self.spawn_quiet_local(true);
        true
    }

    /// The decision behind [`Self::maybe_auto_rescan`], kept separate so it
    /// can be tested without starting a `git status`.
    fn should_auto_rescan(&self) -> bool {
        if !self.auto_refresh || !self.local || self.screen != Screen::Review {
            return false;
        }
        if self.job.is_some() || self.quiet.is_some() || self.pending_quiet.is_some() {
            return false;
        }
        if self.editor.is_some() || !matches!(self.overlay, Overlay::None) {
            return false;
        }
        if self.selection.is_some() || self.drag_select || self.resizing() {
            return false;
        }
        self.last_input.elapsed() >= IDLE_BEFORE_RESCAN
            && self.last_auto_rescan.elapsed() >= RESCAN_MIN_GAP
    }

    fn apply(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::BranchPr(Some(number)) => self.spawn_open_pr(number, true, true),
            Outcome::BranchPr(None) => {
                // Reached from launch (already on the list) or from the
                // PR ⇄ local toggle when the branch has no open PR — the
                // list is the place to pick one either way.
                self.screen = Screen::PrList;
                self.spawn_load_prs();
            }
            Outcome::LocalOpened(data) => {
                let d = *data;
                // Entering local review on top of an open PR: stash the PR
                // side first so ` swaps straight back to it.
                if !self.local && self.pr.is_some() {
                    let ws = self.save_workspace();
                    self.stash = Some(Box::new(ws));
                }
                self.local = true;
                self.local_branch = d.branch;
                self.pr = None;
                // The working tree IS the review target, so editing works.
                self.checked_out = true;
                // Old side of every diff: HEAD (empty in a commitless repo —
                // show_file then yields None and files render as added).
                self.merge_base = d.head.unwrap_or_default();
                self.reset_blame_review();
                self.files = d.files;
                self.viewed.clear();
                self.stage = d.stage;
                self.differs_from_head = false;
                self.screen = Screen::Review;
                self.file_cursor = 0;
                self.file_scroll = 0;
                self.selection = None;
                self.editor = None;
                self.collapsed_dirs.clear();
                self.rebuild_entries();
                if self.files.is_empty() {
                    self.diff = None;
                    self.display.clear();
                    self.ok(
                        "Working tree clean — no uncommitted changes. Press b for pull requests.",
                    );
                } else {
                    let n = self.files.len();
                    let what = if n == 1 { "file" } else { "files" };
                    self.auto_open_note = Some(format!(
                        "Reviewing uncommitted changes vs HEAD ({n} {what}; b for the PR list)"
                    ));
                    self.spawn_load_file(0);
                }
            }
            Outcome::Prs { repo, prs } => {
                self.repo = Some(repo.clone());
                self.prs = prs;
                self.pr_cursor = 0;
                self.pr_scroll = 0;
                if self.prs.is_empty() {
                    self.ok(format!("{repo}: no open pull requests."));
                } else {
                    self.ok(format!(
                        "{repo}: {} open PRs — click one to review.",
                        self.prs.len()
                    ));
                }
                if let Some(e) = self.post_load_err.take() {
                    self.err(e);
                }
            }
            Outcome::PrOpened(data) => {
                let d = *data;
                let number = d.detail.number;
                // Opening a PR from local review: stash the local side so
                // ` swaps straight back to it. Opening a (different) PR
                // from PR mode keeps whatever local stash already exists.
                if self.local {
                    let ws = self.save_workspace();
                    self.stash = Some(Box::new(ws));
                }
                self.local = false;
                self.local_branch = None;
                self.repo = Some(d.repo);
                if let Some(root) = d.repo_root {
                    self.repo_root = root;
                }
                self.checked_out = d.checked_out;
                self.merge_base = d.merge_base;
                self.reset_blame_review();
                self.files = d.files;
                self.viewed = d.viewed;
                if d.auto {
                    self.auto_open_note = Some(format!(
                        "Opened PR #{number} for branch “{}” (b for the PR list)",
                        d.detail.head_ref_name
                    ));
                }
                self.pr = Some(d.detail);
                self.screen = Screen::Review;
                self.file_cursor = 0;
                self.file_scroll = 0;
                self.selection = None;
                self.editor = None;
                self.collapsed_dirs.clear();
                self.rebuild_entries();
                if self.files.is_empty() {
                    self.diff = None;
                    self.display.clear();
                    self.ok(format!("PR #{number} has no changed files."));
                } else {
                    self.spawn_load_file(0);
                }
            }
            Outcome::FileLoaded(data) => {
                let d = *data;
                // A silent refresh can reorder `files` while a load is in
                // flight — trust the path, not the captured index.
                self.file_cursor =
                    if self.files.get(d.idx).map(|f| f.path.as_str()) == Some(d.path.as_str()) {
                        d.idx
                    } else {
                        self.files
                            .iter()
                            .position(|f| f.path == d.path)
                            .unwrap_or(d.idx.min(self.files.len().saturating_sub(1)))
                    };
                self.old_content = d.old;
                self.new_content = d.new;
                self.old_hl = d.old_hl;
                self.new_hl = d.new_hl;
                self.differs_from_head = d.differs;
                self.diff = Some(d.diff);
                self.expanded_folds.clear();
                self.rebuild_display();
                // A search result names a line; opening the file normally
                // lands on its first change instead.
                match self.pending_jump.take() {
                    Some(line) => self.jump_to_line(line),
                    None => {
                        self.diff_cursor = self.first_change_display();
                        self.diff_scroll = self.diff_cursor.saturating_sub(3);
                    }
                }
                self.diff_hscroll = 0;
                self.select_mode = false;
                self.selection = None;
                self.editor = None;
                // Switching files from the preview stays in the preview
                // when the next file is one it can render, and drops to
                // the diff when it is not: the pane follows what the
                // reader was doing, not what they last clicked.
                let keep_preview = self.preview.take().is_some() && markdown::is_markdown(&d.path);
                self.recompute_matches();
                self.reveal_current_file();
                // The blame of the file that just landed, on its own job.
                self.spawn_blame(self.file_cursor);
                // Start this language's server in the background, so the
                // first gd / gr / K doesn't pay for the handshake.
                if self.lsp_enabled {
                    if let (Some(file), Some(text)) = (
                        self.files.get(self.file_cursor),
                        self.new_content.as_deref(),
                    ) {
                        self.lsp.warm(&self.repo_root, &file.path, text);
                    }
                }
                let mode = if self.checked_out {
                    "editable"
                } else {
                    "read-only (branch not checked out)"
                };
                let msg = format!("{} — {}", d.path, mode);
                match self.auto_open_note.take() {
                    Some(note) => self.ok(format!("{note} · {msg}")),
                    None => self.ok(msg),
                }
                if keep_preview {
                    self.preview_current_file();
                }
            }
            Outcome::CommentPosted { path, lo, hi } => {
                self.overlay = Overlay::None;
                self.selection = None;
                let range = if lo == hi {
                    format!("line {hi}")
                } else {
                    format!("lines {lo}–{hi}")
                };
                self.ok(format!("✔ Comment posted on {path} {range}."));
            }
            Outcome::EditorSaved(data) => {
                let d = *data;
                if let Some(ed) = &mut self.editor {
                    ed.dirty = false;
                    ed.discard_armed = false;
                }
                self.new_content = Some(d.content);
                self.new_hl = d.new_hl;
                self.selection = None;
                self.differs_from_head = d.differs;
                self.diff = Some(d.diff);
                self.rebuild_display();
                self.ok(format!(
                    "✔ Saved {} — diff updated. Commit & push when ready.",
                    d.path
                ));
            }
            Outcome::Reverted(data) => self.apply_reverted(*data),
            Outcome::ExternalOpened(data) => self.apply_external(*data),
            Outcome::ExternalSaved(path) => {
                if let Some(ed) = &mut self.editor {
                    ed.dirty = false;
                    ed.discard_armed = false;
                }
                self.ok(format!("✔ Saved {path} (not part of this change)."));
            }
            Outcome::Locations(data) => self.apply_locations(*data),
            Outcome::Hover(data) => {
                let d = *data;
                match d.text {
                    Some(text) => {
                        self.ok(format!("{} — Esc closes.", d.word));
                        self.overlay = Overlay::Hover(Box::new(HoverPanel {
                            word: d.word,
                            lines: text.lines().map(str::to_string).collect(),
                        }));
                    }
                    None => self.err(format!("Nothing known about {}.", d.word)),
                }
            }
        }
    }

    /// Open a file that is not part of the changeset, in the editor.
    ///
    /// Deliberately *not* a diff: there is nothing to diff it against,
    /// and a side-by-side view of a file against itself is two identical
    /// columns. The review underneath is untouched, so closing the editor
    /// puts the reader back exactly where they were with no reload.
    fn apply_external(&mut self, d: ExternalFile) {
        // A markdown file opens as a document. That is the whole point of
        // reaching for the finder: a plan file or a review write-up is
        // meant to be read, and `P` gets to its source when it is not.
        if d.preview {
            let mut pv = Preview::new(&d.path, d.abs_path.clone(), &d.content);
            pv.standalone = true;
            pv.mtime = preview::mtime_of(&d.abs_path);
            if let Some(line) = d.line {
                pv.go_to_source(line);
            }
            self.preview = Some(pv);
            self.blame_new = None;
            self.blame_old = None;
            self.ok(format!(
                "📖 {} — P shows the source, Esc goes back to the diff.",
                d.path
            ));
            return;
        }
        let mut editor = Editor::new(&d.path, d.abs_path, &d.content);
        editor.standalone = true;
        editor.read_only = d.read_only;
        if let Some(line) = d.line {
            editor.jump_to_line(line);
        }
        self.editor = Some(editor);
        // The pane follows the editor: a file outside the changeset has
        // no old side, so only the one blame is read.
        self.spawn_blame_external(d.path.clone(), d.read_only);
        let where_ = match d.line {
            Some(line) => format!(" at line {line}"),
            None => String::new(),
        };
        if d.read_only {
            self.ok(format!(
                "👁 {}{where_} — not in this change, and the branch isn't checked out, so it's read-only. Esc goes back.",
                d.path
            ));
        } else {
            self.ok(format!(
                "{}{where_} — not part of this change. Edit and Ctrl+S if you want; Esc goes back.",
                d.path
            ));
        }
    }

    // ------------------------------------------------------------- spawners

    /// First action after launch, by mode. Auto (the default): review
    /// uncommitted local changes if there are any; with a clean tree, fall
    /// through to the PR flow — if the checked-out branch has an open PR
    /// (the usual case when running inside a per-branch worktree), open it
    /// directly, otherwise show the PR list.
    pub fn start(&mut self) {
        // Deserialize the syntax/theme assets while the first git/gh call
        // runs, so the first file open doesn't pay that cost.
        highlight::warm();
        if !gitops::is_repo() {
            self.err("Not inside a git repository — run loupe from a repo clone.");
            return;
        }
        if let Some(root) = gitops::repo_root() {
            self.repo_root = root;
        }
        match self.mode {
            LaunchMode::Local => self.spawn_open_local(true),
            LaunchMode::Auto => self.spawn_open_local(false),
            LaunchMode::Pr => {
                let org = self.org.clone();
                self.spawn(
                    "Checking the current branch for an open PR".to_string(),
                    true,
                    true,
                    move || {
                        Ok(Outcome::BranchPr(github::pr_for_current_branch(
                            org.as_deref(),
                        )))
                    },
                )
            }
        }
    }

    /// Scan the working tree for uncommitted changes and open them for
    /// review. With `force` the (possibly empty) local review always opens;
    /// otherwise a clean tree falls through to the PR flow.
    pub fn spawn_open_local(&mut self, force: bool) {
        let root = self.repo_root.clone();
        let org = self.org.clone();
        self.spawn(
            "Scanning for uncommitted changes",
            true,
            !force,
            move || {
                let files = gitops::local_changes(&root)?;
                if files.is_empty() && !force {
                    return Ok(Outcome::BranchPr(github::pr_for_current_branch(
                        org.as_deref(),
                    )));
                }
                Ok(Outcome::LocalOpened(Box::new(LocalOpenedData {
                    branch: gitops::current_branch(),
                    head: gitops::head_oid(),
                    files,
                    // Cosmetic: a status read that fails just shows everything
                    // as unstaged rather than sinking the whole scan.
                    stage: gitops::stage_states(&root).unwrap_or_default(),
                })))
            },
        );
    }

    fn spawn_load_prs(&mut self) {
        let repo = self.repo.clone();
        let org = self.org.clone();
        self.spawn("Loading open pull requests", true, false, move || {
            let repo = match repo {
                Some(r) => r,
                None => github::resolve_repo(org.as_deref())?,
            };
            let prs = github::list_open_prs(&repo)?;
            Ok(Outcome::Prs { repo, prs })
        });
    }

    fn spawn_open_pr(&mut self, number: u64, checkout: bool, auto: bool) {
        let repo = self.repo.clone();
        let org = self.org.clone();
        let label = if auto {
            match gitops::current_branch() {
                Some(b) => format!("Opening PR #{number} (current branch “{b}”)"),
                None => format!("Opening PR #{number}"),
            }
        } else if checkout {
            format!("Checking out & opening PR #{number}")
        } else {
            format!("Opening PR #{number} (read-only)")
        };
        self.spawn(label, true, auto, move || {
            let repo = match repo {
                Some(r) => r,
                None => github::resolve_repo(org.as_deref())?,
            };
            let detail = github::pr_detail(&repo, number)?;
            let repo_root = gitops::repo_root();
            // Make sure both endpoint commits exist locally (best effort) —
            // fetched from whichever remote actually hosts the PR, which
            // with an upstream org configured may not be `origin`.
            let source = gitops::fetch_source(&repo);
            let _ = gitops::fetch_pr(&source, &detail.base_ref_name, number);
            if checkout
                && gitops::current_branch().as_deref() != Some(detail.head_ref_name.as_str())
            {
                github::checkout_pr(&repo, number)?;
            }
            let merge_base = gitops::merge_base(&detail.base_ref_oid, &detail.head_ref_oid);
            let files = github::changed_files(&repo, number)?;
            // Viewed state is cosmetic — don't fail the open over it.
            let viewed = github::viewed_files(&repo, number).unwrap_or_default();
            Ok(Outcome::PrOpened(Box::new(PrOpenedData {
                repo,
                repo_root,
                detail,
                checked_out: checkout,
                merge_base,
                files,
                viewed,
                auto,
            })))
        });
    }

    /// What a file load needs from the app, captured before the worker
    /// thread starts (both the modal and the quiet load use this).
    fn file_load_ctx(&self) -> (String, String, bool, bool, PathBuf) {
        (
            self.merge_base.clone(),
            self.pr
                .as_ref()
                .map(|p| p.head_ref_oid.clone())
                .unwrap_or_default(),
            self.checked_out,
            self.local,
            self.repo_root.clone(),
        )
    }

    fn spawn_load_file(&mut self, idx: usize) {
        let Some(file) = self.files.get(idx).cloned() else {
            return;
        };
        let ctx = self.file_load_ctx();
        self.spawn(format!("Loading {}", file.path), true, false, move || {
            load_file_data(idx, file, ctx).map(|d| Outcome::FileLoaded(Box::new(d)))
        });
    }

    // -------------------------------------------------------- blame pane

    /// Start the blame of file `idx`, throwing away whatever the pane was
    /// showing.
    ///
    /// Blame is its own job rather than part of `load_file_data` on
    /// purpose: `git blame` on a long file costs about as much as the
    /// whole rest of the load, and paying that on every file open would
    /// undo the open latency the diff pipeline is built around. The pane
    /// fills in a moment after the diff, and says so while it waits.
    fn spawn_blame(&mut self, idx: usize) {
        let Some(file) = self.files.get(idx).cloned() else {
            self.clear_blame();
            return;
        };
        // New side: the working tree when that is what is under review —
        // which is what marks uncommitted lines as uncommitted — and the
        // head commit otherwise, because the file on disk then belongs to
        // some other branch.
        let rev = if self.checked_out {
            None
        } else {
            Some(
                self.pr
                    .as_ref()
                    .map(|p| p.head_ref_oid.clone())
                    .unwrap_or_default(),
            )
        };
        let old_path = (file.status != "added" && !self.merge_base.is_empty())
            .then(|| file.old_path().to_string());
        let new_path = (file.status != "removed").then(|| file.path.clone());
        self.start_blame(file.path, new_path, rev, old_path);
    }

    /// Blame a file the change never touches — one the editor opened from
    /// a search result or a jump to a definition. There is no old side to
    /// compare it against, so only one blame is read.
    fn spawn_blame_external(&mut self, path: String, read_only: bool) {
        // A read-only editor is showing the commit, not the working tree
        // (the tree belongs to another branch), so blame the same place.
        let rev = read_only.then(|| self.merge_base.clone());
        self.start_blame(path.clone(), Some(path), rev, None);
    }

    /// Throw away what the pane is showing, with nothing on the way to
    /// replace it.
    fn clear_blame(&mut self) {
        // Bumping the generation is what makes the answer already in
        // flight land on the floor instead of on the wrong file.
        self.blame_gen = self.blame_gen.wrapping_add(1);
        self.blame_job = None;
        self.blame_new = None;
        self.blame_old = None;
        self.blame_path = None;
        self.blame_done = false;
    }

    /// The one worker behind every blame read. `new_path` and `old_path`
    /// are None when that side does not exist — a file this change added
    /// has no old side, one it removed has no new one.
    fn start_blame(
        &mut self,
        subject: String,
        new_path: Option<String>,
        rev: Option<String>,
        old_path: Option<String>,
    ) {
        self.clear_blame();
        if !self.blame_on || self.screen != Screen::Review {
            return;
        }
        self.blame_path = Some(subject.clone());
        let gen = self.blame_gen;
        let root = self.repo_root.clone();
        let merge_base = self.merge_base.clone();
        let head = self
            .pr
            .as_ref()
            .map(|p| p.head_ref_oid.clone())
            .unwrap_or_default();
        let local = self.local;
        // The change set and the reader's email describe the review, not
        // the file, so they are read once and carried on the first answer.
        let want_ctx = !self.blame_ctx;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            // The two sides are independent `git blame` calls and each
            // costs about as much as the other; run them together, the
            // way the load job already runs the two highlights.
            let (new, old) = thread::scope(|s| {
                let old_side = s.spawn(|| {
                    let path = old_path?;
                    blame::blame_file(&root, Some(&merge_base), &path)
                });
                let new = new_path
                    .as_deref()
                    .and_then(|path| blame::blame_file(&root, rev.as_deref(), path));
                (new, old_side.join().unwrap_or(None))
            });
            let ctx = want_ctx.then(|| BlameCtx {
                change_set: blame::change_set(&root, &merge_base, &head, local),
                me: blame::my_email(&root),
                origin: gitops::origin_repo(),
            });
            let _ = tx.send(Box::new(BlameData {
                path: subject,
                new,
                old,
                ctx,
            }));
        });
        self.blame_job = Some(BlameJob { rx, gen });
    }

    /// Apply a finished blame read, and a finished pull request lookup.
    /// Returns true when the pane changed and a redraw is needed.
    fn poll_blame(&mut self) -> bool {
        let mut changed = false;
        if let Some(job) = &self.blame_job {
            match job.rx.try_recv() {
                Ok(data) => {
                    let stale = job.gen != self.blame_gen;
                    self.blame_job = None;
                    if !stale {
                        changed |= self.apply_blame(*data);
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.blame_job = None,
            }
        }
        if let Some(rx) = &self.blame_pr_job {
            match rx.try_recv() {
                Ok(res) => {
                    self.blame_pr_job = None;
                    // A failed lookup is not worth a message: the pane
                    // still shows the author, the age and any number the
                    // commit subject named.
                    if let Ok(map) = res {
                        changed |= !map.is_empty();
                        self.blame_prs.extend(map);
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.blame_pr_job = None,
            }
        }
        changed
    }

    fn apply_blame(&mut self, data: BlameData) -> bool {
        // The generation guard already covers a file switch; this catches
        // the case where the same generation names a different file
        // because the changed-file list moved under a refresh.
        if self.blame_path.as_deref() != Some(data.path.as_str()) {
            return false;
        }
        if let Some(ctx) = data.ctx {
            self.blame_change_set = ctx.change_set;
            self.blame_me = ctx.me;
            self.blame_origin = ctx.origin;
            self.blame_ctx = true;
        }
        self.blame_new = data.new;
        self.blame_old = data.old;
        self.blame_done = true;
        self.seed_blame_prs();
        self.spawn_blame_pulls();
        true
    }

    /// Every distinct commit the open file is blamed on, both sides.
    fn blame_commits(&self) -> Vec<Arc<blame::Commit>> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for side in [self.blame_new.as_ref(), self.blame_old.as_ref()]
            .into_iter()
            .flatten()
        {
            for c in side.commits() {
                if seen.insert(c.sha.clone()) {
                    out.push(c);
                }
            }
        }
        out
    }

    /// Fill the session's hash → pull request map from what the commit
    /// subjects already say. Free, offline, and right for a squash or a
    /// merge commit, which is most of a GitHub repository's history.
    fn seed_blame_prs(&mut self) {
        let found: Vec<(String, u64, String)> = self
            .blame_commits()
            .iter()
            .filter(|c| !self.blame_prs.contains_key(&c.sha))
            .filter_map(|c| c.pr.map(|n| (c.sha.clone(), n, c.summary.clone())))
            .collect();
        for (sha, number, title) in found {
            let url = self.pr_link(number);
            self.blame_prs.insert(sha, PrRef { number, title, url });
        }
    }

    /// Ask GitHub about the commits whose subject named no pull request.
    ///
    /// One batched GraphQL call, and only for hashes never asked about
    /// before — a commit GitHub says nothing about must not be asked
    /// again once per file for the rest of the session. A lookup already
    /// in flight is left to finish; the next file open picks up the rest.
    fn spawn_blame_pulls(&mut self) {
        if !self.blame_pr_lookup || self.blame_pr_job.is_some() {
            return;
        }
        let Some(repo) = self.blame_repo() else {
            return;
        };
        let shas: Vec<String> = self
            .blame_commits()
            .iter()
            .filter(|c| {
                !c.uncommitted()
                    && !self.blame_prs.contains_key(&c.sha)
                    && !self.blame_asked.contains(&c.sha)
            })
            .map(|c| c.sha.clone())
            .collect();
        if shas.is_empty() {
            return;
        }
        self.blame_asked.extend(shas.iter().cloned());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(github::pulls_for_commits(&repo, &shas));
        });
        self.blame_pr_job = Some(rx);
    }

    /// A web link for pull request `n` in the repository under review.
    ///
    /// The open pull request's own url is the template when there is one,
    /// so a GitHub Enterprise host stays right; github.com is the
    /// fallback, the same one [`App::pr_url`] already falls back to.
    fn pr_link(&self, n: u64) -> String {
        let from_pr = self
            .pr
            .as_ref()
            .map(|p| p.url.as_str())
            .filter(|u| !u.is_empty())
            .and_then(|url| {
                url.rfind("/pull/")
                    .map(|cut| format!("{}/pull/{n}", &url[..cut]))
            });
        from_pr.unwrap_or_else(|| match self.blame_repo() {
            Some(r) => format!("https://github.com/{r}/pull/{n}"),
            None => String::new(),
        })
    }

    /// Forget everything blame knows that belongs to *this review* rather
    /// than to this file: the change set, the reader's email, and the
    /// lines on screen. Called whenever the review itself changes — a
    /// different pull request, a swap to local changes, a refetch — so
    /// the pane can never colour a commit "in this change" against the
    /// wrong change.
    fn reset_blame_review(&mut self) {
        self.blame_change_set.clear();
        self.blame_me = None;
        self.blame_origin = None;
        self.blame_ctx = false;
        self.clear_blame();
    }

    /// Show or hide the pane. Turning it on blames the open file at once,
    /// so the answer is on its way before the reader looks for it.
    pub fn toggle_blame(&mut self) {
        self.blame_on = !self.blame_on;
        if self.blame_on {
            self.blame_w = self.clamp_blame_w(self.blame_w);
            self.file_panel_w = self.clamp_panel_w(self.file_panel_w);
            self.spawn_blame(self.file_cursor);
            self.ok("Blame on — click a row for the commit behind it. B hides it again.");
        } else {
            self.clear_blame();
            self.ok("Blame off.");
        }
    }

    fn spawn_post_comment(&mut self) {
        let Overlay::Comment(draft) = &self.overlay else {
            return;
        };
        let body = draft.textarea.lines().join("\n");
        if body.trim().is_empty() {
            self.err("Comment is empty — write something or press Esc to cancel.");
            return;
        }
        let (repo, number, commit) = match (&self.repo, &self.pr) {
            (Some(r), Some(p)) => (r.clone(), p.number, p.head_ref_oid.clone()),
            _ => return,
        };
        let side = match draft.side {
            Side::Left => CommentSide::Left,
            Side::Right => CommentSide::Right,
        };
        let (path, lo, hi) = (draft.path.clone(), draft.lo, draft.hi);
        self.spawn(
            format!("Posting comment to PR #{number}"),
            false,
            false,
            move || {
                let start = if lo != hi { Some(lo) } else { None };
                github::post_review_comment(&repo, number, &commit, &path, &body, side, hi, start)?;
                Ok(Outcome::CommentPosted { path, lo, hi })
            },
        );
    }

    fn spawn_save_editor(&mut self) {
        let Some(editor) = &self.editor else { return };
        if editor.read_only {
            self.err(
                "This file is read-only — it came from the commit under review, not your working tree. Reopen the PR with “Checkout & review” to edit.",
            );
            return;
        }
        let content = editor.content();
        let abs_path = editor.abs_path.clone();
        let path = editor.path.clone();
        // A file that isn't part of the changeset has no diff to refresh:
        // write it and stop, rather than recomputing the open file's diff
        // against someone else's content.
        let standalone = editor.standalone;
        let head_oid = self
            .pr
            .as_ref()
            .map(|p| p.head_ref_oid.clone())
            .unwrap_or_default();
        let old = self.old_content.clone();
        let local = self.local;
        self.spawn(format!("Saving {path}"), false, false, move || {
            // Re-check at write time: the file could have been swapped for a
            // symlink since the editor was opened.
            if is_symlink(&abs_path) {
                anyhow::bail!(
                    "{} is a symlink — refusing to write through it",
                    abs_path.display()
                );
            }
            std::fs::write(&abs_path, &content)?;
            if standalone {
                return Ok(Outcome::ExternalSaved(path));
            }
            // Line numbers may have shifted relative to the PR head, so
            // commenting stays blocked until the change is pushed. (Local
            // review has no PR head — nothing to compare against.)
            let differs = if local {
                false
            } else {
                let head = gitops::show_file(&head_oid, &path);
                head.as_deref() != Some(content.as_str())
            };
            let diff = FileDiff::compute(old.as_deref(), Some(&content));
            let new_hl = highlight::highlight(&path, &content);
            Ok(Outcome::EditorSaved(Box::new(EditorSavedData {
                path,
                content,
                differs,
                diff,
                new_hl,
            })))
        });
    }

    // ------------------------------------------------------------- reverting

    /// Whether putting changes back is possible at all. The working tree has
    /// to be the thing under review: a PR opened read-only is someone else's
    /// commit, and there is nothing of ours on disk to undo. An open editor
    /// owns the file until it is closed, so it hides the offer too.
    pub fn can_revert(&self) -> bool {
        self.checked_out && self.screen == Screen::Review && self.editor.is_none()
    }

    /// Columns the change bar takes off the left of the diff pane — zero
    /// when there is nothing to revert, so nothing but the feature pays for
    /// it. Every piece of diff geometry measures from
    /// [`Self::diff_body`], which subtracts this.
    pub fn revert_gutter(&self) -> u16 {
        if self.can_revert() && self.diff.is_some() {
            REVERT_W
        } else {
            0
        }
    }

    /// The diff pane minus the change bar: the part that actually holds the
    /// two panes (or the inline body).
    pub fn diff_body(&self) -> Rect {
        let r = self.layout.diff;
        let g = self.revert_gutter().min(r.width);
        Rect {
            x: r.x + g,
            width: r.width - g,
            ..r
        }
    }

    /// The section of the diff a display row belongs to, if any. Fold
    /// banners and context lines belong to none — there is nothing there to
    /// put back.
    pub fn section_on_row(&self, display_row: usize) -> Option<(usize, usize)> {
        let entry = *self.display.get(display_row)?;
        if matches!(
            entry,
            DisplayEntry::Fold { .. } | DisplayEntry::Unfold { .. }
        ) {
            return None;
        }
        self.diff.as_ref()?.section_at(self.entry_row(entry))
    }

    /// What the change bar draws on a display row: `Some(true)` on the first
    /// row of a section (the ↺ marker), `Some(false)` further down it, None
    /// where nothing changed.
    pub fn change_bar(&self, display_row: usize) -> Option<bool> {
        let here = self.section_on_row(display_row)?;
        let above = display_row
            .checked_sub(1)
            .and_then(|i| self.section_on_row(i));
        Some(above != Some(here))
    }

    /// Ask before reverting one section of the open diff, named by the
    /// display row the click landed on. Deliberately not tied to the cursor:
    /// the marker you click is the change that goes.
    pub fn ask_revert_section(&mut self, display_row: usize) {
        if !self.revert_allowed() {
            return;
        }
        let Some((start, end)) = self.section_on_row(display_row) else {
            self.err("No change on that line — ↺ marks each one you can put back.");
            return;
        };
        let Some((adds, dels, deletes)) = self.diff.as_ref().map(|d| {
            let (adds, dels) = d.section_counts((start, end));
            let gone = d
                .revert_section(
                    (start, end),
                    self.old_content.as_deref(),
                    self.new_content.as_deref(),
                )
                .is_none();
            (adds, dels, gone)
        }) else {
            return;
        };
        let Some(path) = self.files.get(self.file_cursor).map(|f| f.path.clone()) else {
            return;
        };
        self.overlay = Overlay::Revert(Box::new(RevertPrompt {
            target: RevertTarget::Section { start, end },
            path,
            adds,
            dels,
            deletes,
        }));
    }

    /// Ask before throwing away every change in one file.
    pub fn ask_revert_file(&mut self, idx: usize) {
        if !self.revert_allowed() {
            return;
        }
        let Some(file) = self.files.get(idx) else {
            return;
        };
        self.overlay = Overlay::Revert(Box::new(RevertPrompt {
            target: RevertTarget::File { idx },
            path: file.path.clone(),
            adds: file.additions as usize,
            dels: file.deletions as usize,
            deletes: file.status == "added",
        }));
    }

    /// Shared guard: says why not, rather than doing nothing.
    fn revert_allowed(&mut self) -> bool {
        if self.editor.is_some() {
            self.err("Close the editor first (Ctrl+S to save, Esc to close) before reverting.");
            return false;
        }
        if !self.can_revert() {
            self.err(
                "This review is read-only — reopen the PR with “Checkout & review” to change files.",
            );
            return false;
        }
        true
    }

    /// Go ahead with the prompt that is open.
    pub fn confirm_revert(&mut self) {
        let Overlay::Revert(prompt) = &self.overlay else {
            return;
        };
        let target = prompt.target;
        self.overlay = Overlay::None;
        self.spawn_revert(target);
    }

    /// Do the revert on a worker thread, then reload the file it touched.
    ///
    /// A section is written straight to the working tree (the index is left
    /// exactly as it was — half a file is not something to stage behind
    /// someone's back); a whole file goes through git, which puts the index
    /// back too so the file stops showing as changed.
    fn spawn_revert(&mut self, target: RevertTarget) {
        let root = self.repo_root.clone();
        let rev = Some(self.merge_base.clone()).filter(|r| !r.is_empty());
        let ctx = self.file_load_ctx();
        let open_idx = self.file_cursor;
        match target {
            RevertTarget::File { idx } => {
                let Some(file) = self.files.get(idx).cloned() else {
                    return;
                };
                let path = file.path.clone();
                let previous = file.previous.clone();
                let gone = file.status == "added";
                // Reloading only makes sense for the file on screen.
                let reload = (idx == open_idx).then(|| file.clone());
                self.spawn(format!("Reverting {path}"), false, false, move || {
                    gitops::revert_path(&root, rev.as_deref(), &path)?;
                    // A rename has two halves: the new path goes, the
                    // original comes back.
                    if let Some(prev) = previous {
                        gitops::revert_path(&root, rev.as_deref(), &prev)?;
                    }
                    let file = match reload {
                        Some(f) => Some(Box::new(load_file_data(open_idx, f, ctx)?)),
                        None => None,
                    };
                    Ok(Outcome::Reverted(Box::new(RevertedData {
                        what: format!("every change in {path}"),
                        gone,
                        whole_file: true,
                        file,
                    })))
                });
            }
            RevertTarget::Section { start, end } => {
                let Some(file) = self.files.get(open_idx).cloned() else {
                    return;
                };
                let Some((adds, dels, rebuilt)) = self.diff.as_ref().map(|d| {
                    let (adds, dels) = d.section_counts((start, end));
                    let rebuilt = d.revert_section(
                        (start, end),
                        self.old_content.as_deref(),
                        self.new_content.as_deref(),
                    );
                    (adds, dels, rebuilt)
                }) else {
                    return;
                };
                let Some(abs_path) = gitops::safe_repo_path(&self.repo_root, &file.path) else {
                    self.err(format!("Refusing to write to “{}”.", file.path));
                    return;
                };
                let path = file.path.clone();
                // What the diff on screen was computed from. If the file has
                // moved on since, the row model is stale and writing it back
                // would clobber whatever else happened.
                let expected = self.new_content.clone();
                let what = format!("{} in {path}", lines_phrase(adds, dels));
                let gone = rebuilt.is_none();
                self.spawn(format!("Reverting a change in {path}"), false, false, move || {
                    if is_symlink(&abs_path) {
                        anyhow::bail!(
                            "{} is a symlink — refusing to write through it",
                            abs_path.display()
                        );
                    }
                    let on_disk = std::fs::read_to_string(&abs_path).ok();
                    if on_disk != expected {
                        anyhow::bail!(
                            "{path} changed on disk since it was loaded — press r to reload it first"
                        );
                    }
                    match &rebuilt {
                        Some(text) => std::fs::write(&abs_path, text)?,
                        // The whole of a file the change created: undoing it
                        // means the file goes.
                        None => std::fs::remove_file(&abs_path)?,
                    }
                    let file = Box::new(load_file_data(open_idx, file, ctx)?);
                    Ok(Outcome::Reverted(Box::new(RevertedData {
                        what,
                        gone,
                        whole_file: false,
                        file: Some(file),
                    })))
                });
            }
        }
    }

    /// Put a reloaded file back on screen without moving the reader, the way
    /// a silent refresh does — a revert changes a few lines, not the place
    /// you were reading.
    fn apply_reverted(&mut self, d: RevertedData) {
        let mut cleaned = false;
        if let Some(file) = d.file {
            let f = *file;
            let idx = f.idx;
            if self.files.get(idx).map(|x| x.path.as_str()) == Some(f.path.as_str()) {
                self.old_content = f.old;
                self.new_content = f.new;
                self.old_hl = f.old_hl;
                self.new_hl = f.new_hl;
                self.differs_from_head = f.differs;
                // Counts in the file panel come from the list, which was
                // built before the revert — keep them honest.
                if let Some(entry) = self.files.get_mut(idx) {
                    entry.additions = f.diff.additions as u64;
                    entry.deletions = f.diff.deletions as u64;
                }
                cleaned = f.diff.additions == 0 && f.diff.deletions == 0;
                self.diff = Some(f.diff);
                self.expanded_folds.clear();
                self.selection = None;
                self.select_mode = false;
                self.rebuild_display();
                let last = self.display.len().saturating_sub(1);
                self.diff_cursor = self.diff_cursor.min(last);
                self.diff_scroll = self.diff_scroll.min(last);
                self.diff_hscroll = self.diff_hscroll.min(self.max_hscroll());
                self.recompute_matches();
            }
        }
        let tail = if d.gone {
            " — the file is gone; there was no earlier version of it."
        } else if d.whole_file {
            " — it is back to the version it was changed from."
        } else {
            " — the rest of the file is untouched."
        };
        self.ok(format!("↩ Reverted {}{tail}", d.what));
        // Local review lists what is uncommitted, so a file that is clean
        // again has to leave the list — and a whole-file revert always
        // leaves it, including one done from a row that isn't open.
        if self.local && (cleaned || d.gone || d.whole_file) {
            self.spawn_quiet_local(false);
        } else if self.local {
            // A section went back in the working tree only, which can turn
            // a staged file into a partly staged one: re-read the index so
            // the icon column doesn't lie about it.
            self.refresh_stage_states();
        }
    }

    /// Flip a file's viewed checkbox: local state immediately, GitHub sync in
    /// the background (reverted with an error message if the sync fails).
    /// The file panel's leading icon: "viewed" for a PR, "staged" for local
    /// changes.
    pub fn stage_state(&self, path: &str) -> StageState {
        self.stage.get(path).copied().unwrap_or_default()
    }

    /// How many files are fully staged (for the panel title).
    pub fn staged_count(&self) -> usize {
        self.files
            .iter()
            .filter(|f| self.stage_state(&f.path) == StageState::Staged)
            .count()
    }

    /// Re-read the index in the background and adopt it wholesale — the
    /// cheap half of a rescan, for when the working tree moved but the file
    /// list did not.
    fn refresh_stage_states(&mut self) {
        let root = self.repo_root.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(gitops::stage_states(&root).map(Some));
        });
        self.bg_jobs.push(BgJob {
            rx,
            kind: BgKind::Rescan,
        });
    }

    /// What the icon column does: stage/unstage locally, mark viewed on a PR.
    pub fn toggle_file_mark(&mut self, idx: usize) {
        if self.local {
            self.toggle_stage(idx);
        } else {
            self.toggle_viewed(idx);
        }
    }

    /// `git add` the file, or take it back out of the index if all of it is
    /// already staged. The diff itself is working-tree vs HEAD, so what's on
    /// screen doesn't change — only the icon does.
    pub fn toggle_stage(&mut self, idx: usize) {
        let Some(file) = self.files.get(idx) else {
            return;
        };
        let path = file.path.clone();
        let previous = file.previous.clone();
        let before = self.stage_state(&path);
        let add = before != StageState::Staged;
        // Optimistic: the real state (partial staging included) comes back
        // with the rescan.
        self.stage.insert(
            path.clone(),
            if add {
                StageState::Staged
            } else {
                StageState::Unstaged
            },
        );
        if add {
            self.ok(format!("✚ Staged {path} — click again to unstage."));
        } else {
            self.ok(format!(
                "↩ Unstaged {path} — the working tree is untouched."
            ));
        }
        let root = self.repo_root.clone();
        let (tx, rx) = mpsc::channel();
        {
            let path = path.clone();
            thread::spawn(move || {
                let prev = previous.as_deref();
                let res = if add {
                    gitops::stage_file(&root, &path, prev)
                } else {
                    gitops::unstage_file(&root, &path, prev)
                };
                // Re-read the index either way: the truth is cheap to get
                // and knows about partial staging.
                let _ = tx.send(res.and_then(|()| gitops::stage_states(&root).map(Some)));
            });
        }
        self.bg_jobs.push(BgJob {
            rx,
            kind: BgKind::Stage { path, before },
        });
    }

    pub fn toggle_viewed(&mut self, idx: usize) {
        let Some(file) = self.files.get(idx) else {
            return;
        };
        let path = file.path.clone();
        let mark = !self.viewed.contains(&path);
        if mark {
            self.viewed.insert(path.clone());
        } else {
            self.viewed.remove(&path);
        }
        // Local review: the checkbox is a session-local reading aid —
        // there is no PR to sync it to.
        let Some(pr) = &self.pr else {
            if mark {
                self.ok(format!("✓ {path} marked viewed."));
            } else {
                self.ok(format!("☐ {path} unmarked."));
            }
            return;
        };
        let id = pr.id.clone();
        let (tx, rx) = mpsc::channel();
        {
            let path = path.clone();
            thread::spawn(move || {
                let _ = tx.send(github::set_file_viewed(&id, &path, mark).map(|()| None));
            });
        }
        if mark {
            self.ok(format!("✓ {path} marked viewed — syncing to GitHub."));
        } else {
            self.ok(format!("☐ {path} unmarked — syncing to GitHub."));
        }
        self.bg_jobs.push(BgJob {
            rx,
            kind: BgKind::Viewed { path, viewed: mark },
        });
    }

    // ------------------------------------------------------- theme picker

    pub fn open_theme_picker(&mut self) {
        self.overlay = Overlay::ThemePicker(ThemePicker::new());
        self.ok("Pick a theme — j/k or click to preview, Enter to keep it, Esc to cancel.");
    }

    /// Keep the selected theme: save it to the global config and re-highlight
    /// whatever is open so the whole UI reflects it.
    fn apply_theme_pick(&mut self) {
        let Overlay::ThemePicker(tp) = &self.overlay else {
            return;
        };
        let name = highlight::theme_key(highlight::THEMES[tp.sel].1);
        let appearance = crate::theme::appearance();
        // The two appearances have their own theme slot, so saving a light
        // theme never clobbers the dark one (or the other way round).
        let mut pairs = vec![(crate::config::theme_key_for(appearance), name)];
        let pinned = tp.appearance_changed();
        if pinned {
            pairs.push(("appearance", appearance.key()));
        }
        self.overlay = Overlay::None;
        // Re-highlighting the open file replaces the status message when it
        // lands, so route the confirmation through the prepended note.
        let reload = self.screen == Screen::Review && self.diff.is_some();
        match crate::config::save_global(&pairs) {
            Ok(path) => {
                let note = if pinned {
                    format!(" ({} colors pinned)", appearance.key())
                } else {
                    String::new()
                };
                let msg = format!("✔ Theme “{name}”{note} — saved to {}", path.display());
                if reload {
                    self.auto_open_note = Some(msg);
                } else {
                    self.ok(format!("{msg}."));
                }
            }
            Err(e) => self.err(format!(
                "Theme “{name}” applied for this session, but saving it failed: {e:#}"
            )),
        }
        // Recompute the highlights of the open file under the new theme.
        if reload {
            self.spawn_load_file(self.file_cursor);
        }
        // The preview caches its rendered lines with the colors they were
        // built from, so it has to be told the palette moved.
        if let Some(pv) = &mut self.preview {
            pv.restyle();
        }
    }

    fn cancel_theme_pick(&mut self) {
        if let Overlay::ThemePicker(tp) = &self.overlay {
            highlight::set_theme(tp.prev);
            crate::theme::set_appearance(tp.prev_appearance);
            self.overlay = Overlay::None;
            if let Some(pv) = &mut self.preview {
                pv.restyle();
            }
            self.ok("Theme unchanged.");
        }
    }

    // ------------------------------------------------------- derived state

    pub fn rebuild_entries(&mut self) {
        self.entries = if self.tree_view {
            build_tree_entries(&self.files, &self.collapsed_dirs)
        } else {
            (0..self.files.len())
                .map(|idx| FileEntry::File { idx, depth: 0 })
                .collect()
        };
        let max = self.entries.len().saturating_sub(1);
        if self.file_scroll > max {
            self.file_scroll = max;
        }
    }

    pub fn rebuild_display(&mut self) {
        self.display = match &self.diff {
            Some(d) => d.display(
                self.view == ViewMode::SideBySide,
                self.collapse_unchanged,
                &self.expanded_folds,
            ),
            None => Vec::new(),
        };
    }

    /// The underlying diff row a display entry points at (for anchoring
    /// scroll position across view/fold changes).
    fn entry_row(&self, entry: DisplayEntry) -> usize {
        match entry {
            DisplayEntry::Fold { start, .. } | DisplayEntry::Unfold { start, .. } => start,
            DisplayEntry::Line(i) => {
                if self.view == ViewMode::SideBySide {
                    i
                } else {
                    self.diff.as_ref().map(|d| d.inline[i].row).unwrap_or(0)
                }
            }
        }
    }

    fn first_change_display(&self) -> usize {
        let Some(diff) = &self.diff else { return 0 };
        let sbs = self.view == ViewMode::SideBySide;
        self.display
            .iter()
            .position(|e| match e {
                DisplayEntry::Line(i) => {
                    let row = if sbs { *i } else { diff.inline[*i].row };
                    diff.rows[row].kind != RowKind::Context
                }
                DisplayEntry::Fold { .. } | DisplayEntry::Unfold { .. } => false,
            })
            .unwrap_or(0)
    }

    // ----------------------------------------------------------------- keys

    pub fn handle_key(&mut self, key: KeyEvent) {
        self.last_input = Instant::now();
        // A foreground job is modal: only cancel/quit get through.
        if self.job.is_some() {
            match key.code {
                KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Esc => self.cancel_job(),
                KeyCode::Char('q') => self.should_quit = true,
                _ => {}
            }
            return;
        }

        // Overlays capture keys first.
        match &mut self.overlay {
            Overlay::Help => {
                self.overlay = Overlay::None;
                return;
            }
            Overlay::CheckoutPrompt(n) => {
                let n = *n;
                match key.code {
                    KeyCode::Char('c') | KeyCode::Enter => {
                        self.overlay = Overlay::None;
                        self.spawn_open_pr(n, true, false);
                    }
                    KeyCode::Char('o') => {
                        self.overlay = Overlay::None;
                        self.spawn_open_pr(n, false, false);
                    }
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = Overlay::None,
                    _ => {}
                }
                return;
            }
            Overlay::Revert(_) => {
                match key.code {
                    KeyCode::Enter | KeyCode::Char('y') => self.confirm_revert(),
                    KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                        self.overlay = Overlay::None;
                        self.ok("Left alone — nothing was reverted.");
                    }
                    _ => {}
                }
                return;
            }
            Overlay::PathMenu(menu) => {
                let last = menu.items.len().saturating_sub(1);
                let mut close = false;
                let hit = match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        menu.sel = menu.sel.saturating_sub(1);
                        None
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        menu.sel = (menu.sel + 1).min(last);
                        None
                    }
                    KeyCode::Enter => Some(menu.sel),
                    KeyCode::Esc | KeyCode::Char('q') => {
                        close = true;
                        None
                    }
                    // Each line has its own letter, so the common case is
                    // one key press rather than move-then-confirm.
                    KeyCode::Char(c) => menu.items.iter().position(|it| it.key == c),
                    _ => None,
                };
                if close {
                    self.overlay = Overlay::None;
                } else if let Some(i) = hit {
                    self.path_menu_copy(i);
                }
                return;
            }
            Overlay::BlameMenu(menu) => {
                let last = menu.items.len().saturating_sub(1);
                let mut close = false;
                let hit = match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        menu.sel = menu.sel.saturating_sub(1);
                        None
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        menu.sel = (menu.sel + 1).min(last);
                        None
                    }
                    KeyCode::Enter => Some(menu.sel),
                    KeyCode::Esc | KeyCode::Char('q') => {
                        close = true;
                        None
                    }
                    // Each line has its own letter, so following a commit
                    // is one key press rather than move-then-confirm.
                    KeyCode::Char(c) => menu.items.iter().position(|it| it.key == c),
                    _ => None,
                };
                if close {
                    self.overlay = Overlay::None;
                } else if let Some(i) = hit {
                    self.blame_menu_run(i);
                }
                return;
            }
            Overlay::Menu(menu) => {
                let mut close = false;
                let hit = match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        menu.sel = menu.next_selectable(menu.sel, -1);
                        None
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        menu.sel = menu.next_selectable(menu.sel, 1);
                        None
                    }
                    KeyCode::Enter | KeyCode::Char(' ') => Some(menu.sel),
                    KeyCode::Esc => {
                        close = true;
                        None
                    }
                    // The hint column is the key that does the same thing
                    // outside the menu, so pressing it here runs the line.
                    // A menu open should never be a reason to learn a
                    // second set of keys.
                    KeyCode::Char(c) => {
                        let want = c.to_string();
                        let found = menu.rows.iter().position(
                            |r| matches!(r, MenuRow::Item(it) if it.enabled && it.hint == want),
                        );
                        // No line owns this key: close and let it through
                        // to the review, rather than swallowing it.
                        if found.is_none() {
                            close = true;
                        }
                        found
                    }
                    _ => None,
                };
                if let Some(i) = hit {
                    self.menu_activate(i);
                    return;
                }
                if close {
                    self.overlay = Overlay::None;
                    // `q` on an open menu means quit, the same as it does
                    // everywhere else.
                    if key.code == KeyCode::Char('q') {
                        self.should_quit = true;
                    }
                }
                return;
            }
            Overlay::Comment(draft) => {
                match (key.code, key.modifiers) {
                    (KeyCode::Esc, _) => self.overlay = Overlay::None,
                    (KeyCode::Char('s'), KeyModifiers::CONTROL)
                    | (KeyCode::Enter, KeyModifiers::CONTROL) => {
                        self.spawn_post_comment();
                    }
                    _ => {
                        draft.textarea.input(key);
                    }
                }
                return;
            }
            Overlay::ThemePicker(tp) => {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => tp.select(tp.sel.saturating_sub(1)),
                    KeyCode::Down | KeyCode::Char('j') => tp.select(tp.sel + 1),
                    KeyCode::PageUp => tp.select(tp.sel.saturating_sub(8)),
                    KeyCode::PageDown => tp.select(tp.sel + 8),
                    KeyCode::Home => tp.select(0),
                    KeyCode::End => tp.select(usize::MAX),
                    KeyCode::Char('a') => tp.toggle_appearance(),
                    KeyCode::Enter => self.apply_theme_pick(),
                    KeyCode::Esc | KeyCode::Char('q') => self.cancel_theme_pick(),
                    _ => {}
                }
                return;
            }
            Overlay::Finder(_) => {
                self.finder_key(key);
                return;
            }
            Overlay::Hover(_) => {
                self.overlay = Overlay::None;
                return;
            }
            Overlay::None => {}
        }

        // The `/` prompt owns every keystroke while it is open.
        if self.find.typing {
            self.find_key(key);
            return;
        }

        // Preview mode. Reading keys only: everything that changes the
        // document happens in the source view, one `P` away.
        if self.preview.is_some() {
            let page = self.preview.as_ref().map(|p| p.page()).unwrap_or(10);
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            if self.pending_g {
                self.pending_g = false;
                if let (KeyCode::Char('g'), Some(pv)) = (key.code, &mut self.preview) {
                    pv.scroll_to_top();
                    return;
                }
            }
            match key.code {
                KeyCode::Char('P') | KeyCode::Char('e') | KeyCode::Char('i') => {
                    self.toggle_preview();
                }
                KeyCode::Esc => self.close_preview(),
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Char('r') => self.reload_preview(),
                KeyCode::Char('g') => self.pending_g = true,
                KeyCode::Char('m') => self.open_menu_from_key(),
                KeyCode::Char('t') => self.open_theme_picker(),
                KeyCode::Char('?') => self.overlay = Overlay::Help,
                KeyCode::Char('p') if ctrl => self.open_finder(FinderMode::Files),
                KeyCode::Char('<') => self.resize_file_panel(-2),
                KeyCode::Char('>') => self.resize_file_panel(2),
                KeyCode::Char(']') if !self.preview_only => self.step_file(1),
                KeyCode::Char('[') if !self.preview_only => self.step_file(-1),
                _ => {
                    let Some(pv) = &mut self.preview else { return };
                    match key.code {
                        KeyCode::Down | KeyCode::Char('j') => pv.scroll_rows(1),
                        KeyCode::Up | KeyCode::Char('k') => pv.scroll_rows(-1),
                        KeyCode::Char('d') if ctrl => pv.scroll_rows(page / 2),
                        KeyCode::Char('u') if ctrl => pv.scroll_rows(-page / 2),
                        KeyCode::Char('f') if ctrl => pv.scroll_rows(page),
                        KeyCode::Char('b') if ctrl => pv.scroll_rows(-page),
                        KeyCode::PageDown | KeyCode::Char(' ') => pv.scroll_rows(page),
                        KeyCode::PageUp => pv.scroll_rows(-page),
                        KeyCode::Home => pv.scroll_to_top(),
                        KeyCode::Char('G') | KeyCode::End => pv.scroll_to_bottom(),
                        // Section-at-a-time, the way `}` walks hunks in
                        // the diff. A long plan file is read this way.
                        // Past the last heading, `}` runs on to the end of
                        // the document rather than doing nothing.
                        KeyCode::Char('}') | KeyCode::Tab if !pv.jump_heading(true) => {
                            pv.scroll_to_bottom();
                        }
                        KeyCode::Char('{') | KeyCode::BackTab if !pv.jump_heading(false) => {
                            pv.scroll_to_top();
                        }
                        _ => {}
                    }
                }
            }
            return;
        }

        // Editor mode.
        if self.editor.is_some() {
            // The completion popup owns a few keys while it is open, and
            // gives them straight back when it closes.
            if self.editor.as_ref().is_some_and(|e| e.completion.is_some()) {
                let handled = match (key.code, key.modifiers) {
                    (KeyCode::Esc, _) => {
                        if let Some(e) = &mut self.editor {
                            e.completion = None;
                        }
                        self.ok("Completion dismissed.");
                        true
                    }
                    (KeyCode::Tab, _) | (KeyCode::Enter, _) => {
                        let accepted = self.editor.as_mut().and_then(|e| e.accept_completion());
                        match accepted {
                            Some(label) => self.ok(format!("Inserted {label}.")),
                            None => self.err("Nothing selected."),
                        }
                        true
                    }
                    (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                        if let Some(c) = self.editor.as_mut().and_then(|e| e.completion.as_mut()) {
                            c.move_sel(-1);
                        }
                        true
                    }
                    (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                        if let Some(c) = self.editor.as_mut().and_then(|e| e.completion.as_mut()) {
                            c.move_sel(1);
                        }
                        true
                    }
                    _ => false,
                };
                if handled {
                    return;
                }
            }

            let editor = self.editor.as_mut().expect("checked above");
            match (key.code, key.modifiers) {
                (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                    // Format first when asked to: saving the unformatted
                    // text and reformatting after would write twice.
                    if self.format_on_save && !self.editor.as_ref().is_some_and(|e| e.read_only) {
                        self.format_then_save = true;
                        self.spawn_editor_request(EditorRequest::Format);
                    } else {
                        self.spawn_save_editor();
                    }
                }
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                    let target = editor.copy_target();
                    match target {
                        Some((text, what)) => match clipboard::copy(&text) {
                            Ok(via) => self.ok(format!("⧉ Copied {what} via {via}.")),
                            Err(e) => self.err(format!("Couldn't copy: {e:#}")),
                        },
                        None => self.err("Nothing to copy."),
                    }
                }
                // Language-server keys. These are Ctrl-based because the
                // editor takes plain characters as text — `K` and `gd`
                // from the diff view would just type letters here — and
                // Ctrl+G/T/] are the ones tui-textarea leaves free.
                (KeyCode::Char('g'), KeyModifiers::CONTROL) => {
                    self.spawn_editor_request(EditorRequest::Hover);
                }
                (KeyCode::Char(']'), KeyModifiers::CONTROL) => {
                    self.spawn_editor_request(EditorRequest::Definition);
                }
                (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
                    self.ok("Formatting…");
                    self.spawn_editor_request(EditorRequest::Format);
                }
                (KeyCode::Char(' '), KeyModifiers::CONTROL) => {
                    self.spawn_editor_request(EditorRequest::Complete);
                }
                // Back to the rendered document. Alt rather than Ctrl:
                // Ctrl+P is tui-textarea's cursor-up, and taking it away
                // would break moving around the buffer.
                (KeyCode::Char('p'), KeyModifiers::ALT)
                | (KeyCode::Char('P'), KeyModifiers::ALT) => {
                    self.toggle_preview();
                }
                // tui-textarea's own undo is Ctrl+U; Ctrl+Z is what
                // everyone reaches for, so both work. A format is undone
                // whole rather than in the two steps it took to apply.
                (KeyCode::Char('z'), KeyModifiers::CONTROL)
                | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                    if editor.undo_format() {
                        self.ok("Formatting undone.");
                    } else {
                        editor.textarea.undo();
                    }
                    if let Some(e) = &mut self.editor {
                        e.dirty = true;
                    }
                    self.editor_touched = Some(Instant::now());
                }
                // Paging must bypass tui-textarea's handler: its internal
                // PageUp/PageDown call TextArea::scroll, which would desync
                // our shadow viewport (and with it, mouse hit-testing).
                (KeyCode::PageDown, _) | (KeyCode::Char('v'), KeyModifiers::CONTROL) => {
                    let page = editor.page();
                    editor.scroll_lines(page);
                }
                (KeyCode::PageUp, _) | (KeyCode::Char('v'), KeyModifiers::ALT) => {
                    let page = editor.page();
                    editor.scroll_lines(-page);
                }
                (KeyCode::Esc, _) => {
                    if editor.dirty && !editor.discard_armed {
                        editor.discard_armed = true;
                        self.err("Unsaved changes — Esc again to discard, Ctrl+S to save.");
                    } else {
                        self.close_editor();
                    }
                }
                _ => {
                    let modified = editor.textarea.input(key);
                    if modified {
                        editor.dirty = true;
                        editor.discard_armed = false;
                        editor.touched();
                    }
                    let typed = match key.code {
                        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                            Some(c)
                        }
                        _ => None,
                    };
                    if modified {
                        // The server's copy is now stale; the idle tick
                        // will push the new text.
                        self.editor_touched = Some(Instant::now());
                    }
                    // Narrow an open popup to what has been typed since,
                    // or open one when a word (or a `.`) starts.
                    let open = self.editor.as_ref().is_some_and(|e| e.completion.is_some());
                    if open {
                        if let Some(e) = &mut self.editor {
                            e.update_completion();
                        }
                    }
                    if let Some(c) = typed {
                        let still_open =
                            self.editor.as_ref().is_some_and(|e| e.completion.is_some());
                        if !still_open && (c == '.' || c == ':' || c == '>') {
                            self.spawn_editor_request(EditorRequest::Complete);
                        }
                    }
                }
            }
            return;
        }

        match self.screen {
            Screen::PrList => match key.code {
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Char('r') => self.spawn_load_prs(),
                KeyCode::Char('l') => self.spawn_open_local(true),
                KeyCode::Char('`') => self.toggle_workspace(),
                KeyCode::Char('m') => self.open_menu_from_key(),
                KeyCode::Char('t') => self.open_theme_picker(),
                KeyCode::Char('?') => self.overlay = Overlay::Help,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.pr_cursor = self.pr_cursor.saturating_sub(1);
                    self.ensure_pr_visible();
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if self.pr_cursor + 1 < self.prs.len() {
                        self.pr_cursor += 1;
                    }
                    self.ensure_pr_visible();
                }
                KeyCode::Enter => {
                    if let Some(pr) = self.prs.get(self.pr_cursor) {
                        self.overlay = Overlay::CheckoutPrompt(pr.number);
                    }
                }
                _ => {}
            },
            Screen::Review => {
                // `g` is the one prefix key: gg jumps to the top. Any other
                // key cancels the pending g and is handled normally.
                let pending_g = std::mem::take(&mut self.pending_g);
                if pending_g {
                    match key.code {
                        KeyCode::Char('g') => {
                            self.cursor_to(0);
                            return;
                        }
                        // gd / gr: the language server's answers, under
                        // the keys vim users already reach for.
                        KeyCode::Char('d') => {
                            self.lsp_action(LspAction::Definition);
                            return;
                        }
                        KeyCode::Char('r') => {
                            self.lsp_action(LspAction::References);
                            return;
                        }
                        // Anything else cancels the prefix and is handled
                        // normally below.
                        _ => {}
                    }
                }
                let page = self.diff_page() as i32;
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    // --- motions (they move the cursor; the view follows)
                    KeyCode::Up | KeyCode::Char('k') => self.cursor_by(-1),
                    KeyCode::Down | KeyCode::Char('j') => self.cursor_by(1),
                    KeyCode::Char('d') if ctrl => self.cursor_by(page / 2),
                    KeyCode::Char('u') if ctrl => self.cursor_by(-page / 2),
                    KeyCode::Char('f') if ctrl => self.cursor_by(page),
                    KeyCode::Char('b') if ctrl => self.cursor_by(-page),
                    KeyCode::PageDown => self.cursor_by(page),
                    KeyCode::PageUp => self.cursor_by(-page),
                    KeyCode::Char('g') => self.pending_g = true,
                    KeyCode::Char('G') | KeyCode::End => {
                        self.cursor_to(self.display.len().saturating_sub(1))
                    }
                    KeyCode::Home => {
                        self.diff_hscroll = 0;
                        self.cursor_to(0);
                    }
                    // Top / middle / bottom of what's on screen.
                    KeyCode::Char('H') => self.cursor_to(self.diff_scroll),
                    KeyCode::Char('M') => self.cursor_to(self.diff_scroll + self.diff_page() / 2),
                    KeyCode::Char('L') => {
                        self.cursor_to(self.diff_scroll + self.diff_page().saturating_sub(1))
                    }
                    KeyCode::Char('}') => self.jump_hunk(true),
                    KeyCode::Char('{') => self.jump_hunk(false),
                    // --- search
                    KeyCode::Char('/') => self.start_find(),
                    // vim's "what is this?" key.
                    KeyCode::Char('K') => self.lsp_action(LspAction::Hover),
                    KeyCode::Char('n') => self.goto_match(true),
                    KeyCode::Char('N') => self.goto_match(false),
                    KeyCode::Char('p') if ctrl => self.open_finder(FinderMode::Files),
                    KeyCode::Char('#') => self.open_finder(FinderMode::Grep),
                    KeyCode::Char('@') => self.open_finder(FinderMode::Symbols),
                    // --- scrolling that leaves the cursor where it is
                    KeyCode::Char('e') if ctrl => {
                        self.scroll_diff(1);
                        self.clamp_cursor_to_view();
                    }
                    KeyCode::Char('y') if ctrl => {
                        self.scroll_diff(-1);
                        self.clamp_cursor_to_view();
                    }
                    // --- horizontal
                    KeyCode::Left | KeyCode::Char('h') => self.scroll_diff_h(-HSCROLL_STEP),
                    KeyCode::Right | KeyCode::Char('l') => self.scroll_diff_h(HSCROLL_STEP),
                    KeyCode::Char('0') => self.scroll_diff_h(i32::MIN / 2),
                    KeyCode::Char('$') => self.scroll_diff_h(i32::MAX / 2),
                    // --- selection and actions on the cursor row
                    KeyCode::Char('V') => self.toggle_select_mode(),
                    // Copy what is selected (or the cursor line). Ctrl+C
                    // does nothing else here — `q` is how you quit.
                    KeyCode::Char('y') => self.yank(),
                    KeyCode::Char('c') if ctrl => self.yank(),
                    KeyCode::Enter | KeyCode::Char(' ') => {
                        let idx = self.diff_cursor;
                        if !self.toggle_fold_row(idx) {
                            self.err("Nothing to fold here — V selects lines, e edits.");
                        }
                    }
                    // Esc unwinds one layer at a time, saying which each
                    // time: selection, then search, then out.
                    KeyCode::Esc if self.selection.is_some() => self.clear_selection(),
                    KeyCode::Esc if self.find.active() => self.clear_find(),
                    // --- everything that was already bound
                    KeyCode::Char('q') => self.should_quit = true,
                    KeyCode::Esc | KeyCode::Char('b') => self.back_to_pr_list(),
                    KeyCode::Char('v') => self.toggle_view(),
                    KeyCode::Char('z') => self.toggle_fold(),
                    KeyCode::Char('B') => self.toggle_blame(),
                    KeyCode::Char('x') => self.toggle_file_mark(self.file_cursor),
                    // Put changes back: the section at the cursor, or (with
                    // shift) the whole file. Both ask first.
                    KeyCode::Char('u') => self.ask_revert_section(self.diff_cursor),
                    KeyCode::Char('U') => self.ask_revert_file(self.file_cursor),
                    KeyCode::Char('e') | KeyCode::Char('i') => {
                        let line = match self.cursor_pos() {
                            Some((Side::Right, n)) => Some(n),
                            _ => None,
                        };
                        self.open_editor(line);
                    }
                    // Render this file as a document. Shift, because `p`
                    // belongs to search and the diff takes bare letters.
                    KeyCode::Char('P') => self.toggle_preview(),
                    KeyCode::Char('c') => self.open_comment(),
                    KeyCode::Char('`') => self.toggle_workspace(),
                    KeyCode::Char('r') => self.refresh_review(),
                    KeyCode::Char('m') => self.open_menu_from_key(),
                    KeyCode::Char('t') => self.open_theme_picker(),
                    KeyCode::Char('?') => self.overlay = Overlay::Help,
                    KeyCode::Char('<') => self.resize_file_panel(-2),
                    KeyCode::Char('>') => self.resize_file_panel(2),
                    // `n`/`p` used to do this too; they belong to search
                    // now, and `]`/`[` is the convention anyway.
                    KeyCode::Char(']') => self.step_file(1),
                    KeyCode::Char('[') => self.step_file(-1),
                    _ => {}
                }
            }
        }
    }

    fn back_to_pr_list(&mut self) {
        self.screen = Screen::PrList;
        self.diff = None;
        self.display.clear();
        self.selection = None;
        // After an auto-opened PR the list was never fetched.
        if self.prs.is_empty() {
            self.spawn_load_prs();
        }
    }

    // ---------------------------------------------------------------- mouse

    pub fn handle_mouse(&mut self, m: MouseEvent) {
        self.last_input = Instant::now();
        if self.job.is_some() {
            return;
        }
        let (x, y) = (m.column, m.row);

        // Overlays.
        match &mut self.overlay {
            Overlay::Help => {
                if matches!(m.kind, MouseEventKind::Down(_)) {
                    self.overlay = Overlay::None;
                }
                return;
            }
            Overlay::CheckoutPrompt(n) => {
                let n = *n;
                if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
                    match self.layout.button_at(x, y) {
                        Some(ButtonId::CheckoutYes) => {
                            self.overlay = Overlay::None;
                            self.spawn_open_pr(n, true, false);
                        }
                        Some(ButtonId::CheckoutReviewOnly) => {
                            self.overlay = Overlay::None;
                            self.spawn_open_pr(n, false, false);
                        }
                        Some(ButtonId::CheckoutCancel) => self.overlay = Overlay::None,
                        _ => {}
                    }
                }
                return;
            }
            Overlay::Revert(_) => {
                if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
                    match self.layout.button_at(x, y) {
                        Some(ButtonId::RevertYes) => self.confirm_revert(),
                        Some(ButtonId::RevertCancel) => {
                            self.overlay = Overlay::None;
                            self.ok("Left alone — nothing was reverted.");
                        }
                        _ => {}
                    }
                }
                return;
            }
            Overlay::Comment(_) => {
                if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
                    match self.layout.button_at(x, y) {
                        Some(ButtonId::CommentPost) => self.spawn_post_comment(),
                        Some(ButtonId::CommentCancel) => self.overlay = Overlay::None,
                        _ => {}
                    }
                }
                return;
            }
            Overlay::ThemePicker(tp) => {
                match m.kind {
                    MouseEventKind::ScrollDown => tp.select(tp.sel + 1),
                    MouseEventKind::ScrollUp => tp.select(tp.sel.saturating_sub(1)),
                    MouseEventKind::Down(MouseButton::Left) => match self.layout.button_at(x, y) {
                        Some(ButtonId::ThemeRow(i)) => {
                            if let Overlay::ThemePicker(tp) = &mut self.overlay {
                                tp.select(i);
                            }
                        }
                        Some(ButtonId::AppearanceToggle) => {
                            if let Overlay::ThemePicker(tp) = &mut self.overlay {
                                tp.toggle_appearance();
                            }
                        }
                        Some(ButtonId::ThemeApply) => self.apply_theme_pick(),
                        Some(ButtonId::ThemeCancel) => self.cancel_theme_pick(),
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::Finder(_) => {
                self.finder_mouse(m, x, y);
                return;
            }
            Overlay::Hover(_) => {
                if matches!(m.kind, MouseEventKind::Down(_)) {
                    self.overlay = Overlay::None;
                }
                return;
            }
            Overlay::PathMenu(_) => {
                // A click anywhere else closes it, which is what every
                // other context menu does.
                if matches!(m.kind, MouseEventKind::Down(_)) {
                    match self.layout.button_at(x, y) {
                        Some(ButtonId::PathMenuRow(i)) => self.path_menu_copy(i),
                        _ => self.overlay = Overlay::None,
                    }
                }
                return;
            }
            Overlay::BlameMenu(_) => {
                if matches!(m.kind, MouseEventKind::Down(_)) {
                    match self.layout.button_at(x, y) {
                        Some(ButtonId::BlameMenuRow(i)) => self.blame_menu_run(i),
                        _ => self.overlay = Overlay::None,
                    }
                }
                return;
            }
            Overlay::Menu(_) => {
                match m.kind {
                    // The wheel over an open menu scrolls the menu, not the
                    // diff behind it.
                    MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                        let step: isize = if m.kind == MouseEventKind::ScrollDown {
                            1
                        } else {
                            -1
                        };
                        if let Overlay::Menu(menu) = &mut self.overlay {
                            for _ in 0..3 {
                                menu.sel = menu.next_selectable(menu.sel, step);
                            }
                        }
                    }
                    MouseEventKind::Down(_) => match self.layout.button_at(x, y) {
                        Some(ButtonId::MenuRow(i)) => self.menu_activate(i),
                        // Clicking ☰ again puts the menu away.
                        _ => self.overlay = Overlay::None,
                    },
                    _ => {}
                }
                return;
            }
            Overlay::None => {}
        }

        // Preview mode: the file panel still switches files, because a
        // rendered document has nothing unsaved to lose.
        if self.preview.is_some() {
            if self.divider_mouse(m, x, y) {
                return;
            }
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    match self.layout.button_at(x, y) {
                        Some(ButtonId::Menu) => {
                            self.open_menu(x, y);
                            return;
                        }
                        Some(id) if self.activate(id) => return,
                        _ => {}
                    }
                    if contains(self.layout.file_list, x, y) {
                        self.file_list_click(x, y);
                        return;
                    }
                    if let Some(pv) = &mut self.preview {
                        pv.on_click(x, y);
                    }
                }
                MouseEventKind::Down(MouseButton::Right) => {
                    if contains(self.layout.file_list, x, y) {
                        self.open_path_menu(x, y);
                    }
                }
                MouseEventKind::ScrollUp => {
                    if contains(self.layout.file_list, x, y) {
                        self.file_scroll = self.file_scroll.saturating_sub(3);
                    } else if let Some(pv) = &mut self.preview {
                        pv.scroll_rows(-preview::WHEEL_ROWS);
                    }
                }
                MouseEventKind::ScrollDown => {
                    if contains(self.layout.file_list, x, y) {
                        let h = self.layout.file_list.height as usize;
                        let max = self.entries.len().saturating_sub(h.max(1));
                        self.file_scroll = (self.file_scroll + 3).min(max);
                    } else if let Some(pv) = &mut self.preview {
                        pv.scroll_rows(preview::WHEEL_ROWS);
                    }
                }
                _ => {}
            }
            return;
        }

        // Editor mode: top-bar buttons, the file list, and the editor surface.
        if self.editor.is_some() {
            // The panel divider stays draggable with the editor open.
            if self.divider_mouse(m, x, y) {
                return;
            }
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    match self.layout.button_at(x, y) {
                        Some(ButtonId::Menu) => {
                            self.open_menu(x, y);
                            return;
                        }
                        Some(id) if self.activate(id) => return,
                        _ => {}
                    }
                    if contains(self.layout.file_list, x, y) {
                        self.err("Close the editor first (Ctrl+S to save, Esc to close) before switching files.");
                        return;
                    }
                    if let Some(ed) = &mut self.editor {
                        ed.on_click(x, y);
                    }
                }
                // Copying a path does not switch files, so the menu still
                // works while the editor holds the file panel.
                MouseEventKind::Down(MouseButton::Right) => {
                    if contains(self.layout.file_list, x, y) {
                        self.open_path_menu(x, y);
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if let Some(ed) = &mut self.editor {
                        ed.on_drag(x, y);
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    if let Some(ed) = &mut self.editor {
                        ed.on_release();
                    }
                }
                MouseEventKind::ScrollUp => {
                    if contains(self.layout.file_list, x, y) {
                        self.file_scroll = self.file_scroll.saturating_sub(3);
                    } else if let Some(ed) = &mut self.editor {
                        ed.scroll_lines(-3);
                    }
                }
                MouseEventKind::ScrollDown => {
                    if contains(self.layout.file_list, x, y) {
                        let h = self.layout.file_list.height as usize;
                        let max = self.entries.len().saturating_sub(h.max(1));
                        self.file_scroll = (self.file_scroll + 3).min(max);
                    } else if let Some(ed) = &mut self.editor {
                        ed.scroll_lines(3);
                    }
                }
                _ => {}
            }
            return;
        }

        match self.screen {
            Screen::PrList => self.mouse_pr_list(m, x, y),
            Screen::Review => self.mouse_review(m, x, y),
        }
    }

    fn mouse_pr_list(&mut self, m: MouseEvent, x: u16, y: u16) {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                match self.layout.button_at(x, y) {
                    Some(ButtonId::Menu) => {
                        self.open_menu(x, y);
                        return;
                    }
                    Some(id) if self.activate(id) => return,
                    _ => {}
                }
                let r = self.layout.pr_list;
                if contains(r, x, y) {
                    let idx = self.pr_scroll + (y - r.y) as usize;
                    if idx < self.prs.len() {
                        self.pr_cursor = idx;
                        self.overlay = Overlay::CheckoutPrompt(self.prs[idx].number);
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                self.pr_scroll = self.pr_scroll.saturating_sub(3);
            }
            MouseEventKind::ScrollDown => {
                let h = self.layout.pr_list.height as usize;
                let max = self.prs.len().saturating_sub(h.max(1));
                self.pr_scroll = (self.pr_scroll + 3).min(max);
            }
            _ => {}
        }
    }

    fn mouse_review(&mut self, m: MouseEvent, x: u16, y: u16) {
        if self.divider_mouse(m, x, y) {
            return;
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                match self.layout.button_at(x, y) {
                    Some(ButtonId::Menu) => {
                        self.open_menu(x, y);
                        return;
                    }
                    Some(id) if self.activate(id) => return,
                    _ => {}
                }
                let fl = self.layout.file_list;
                if contains(fl, x, y) {
                    self.file_list_click(x, y);
                    return;
                }
                if contains(self.layout.blame, x, y) {
                    self.open_blame_menu(x, y);
                    return;
                }
                let dr = self.layout.diff;
                if contains(dr, x, y) {
                    self.diff_click(x, y);
                }
            }
            // Dragging past the top or bottom edge scrolls and keeps
            // selecting, so a block bigger than the screen is one gesture.
            MouseEventKind::Drag(MouseButton::Left) if self.drag_select => {
                self.drag_to(x, y);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag_select = false;
            }
            // Right-click on the file panel asks about the row's path;
            // anywhere else it drops the diff selection.
            MouseEventKind::Down(MouseButton::Right) => {
                if contains(self.layout.badge, x, y) {
                    self.copy_pr_link();
                } else if contains(self.layout.file_list, x, y) {
                    self.open_path_menu(x, y);
                } else if contains(self.layout.blame, x, y) {
                    self.open_blame_menu(x, y);
                } else {
                    self.clear_selection();
                }
            }
            // Sideways wheel. A horizontal trackpad swipe (or a tilt wheel)
            // arrives as ScrollLeft/ScrollRight; otherwise it is the normal
            // wheel with a modifier held. Which modifier survives depends on
            // the terminal — Shift is the convention, but plenty of them
            // swallow it or rewrite it — so any modifier counts.
            MouseEventKind::ScrollLeft => self.scroll_diff_h(-HSCROLL_WHEEL),
            MouseEventKind::ScrollRight => self.scroll_diff_h(HSCROLL_WHEEL),
            MouseEventKind::ScrollUp if !m.modifiers.is_empty() => {
                self.scroll_diff_h(-HSCROLL_WHEEL);
            }
            MouseEventKind::ScrollDown if !m.modifiers.is_empty() => {
                self.scroll_diff_h(HSCROLL_WHEEL);
            }
            MouseEventKind::ScrollUp => {
                if contains(self.layout.file_list, x, y) {
                    self.file_scroll = self.file_scroll.saturating_sub(3);
                } else {
                    self.scroll_diff(-3);
                    self.clamp_cursor_to_view();
                }
            }
            MouseEventKind::ScrollDown => {
                if contains(self.layout.file_list, x, y) {
                    let h = self.layout.file_list.height as usize;
                    let max = self.entries.len().saturating_sub(h.max(1));
                    self.file_scroll = (self.file_scroll + 3).min(max);
                } else {
                    self.scroll_diff(3);
                    self.clamp_cursor_to_view();
                }
            }
            _ => {}
        }
    }

    /// Divider drag: press on the two border columns between the panels and
    /// move to resize. Returns true when the event belonged to the divider
    /// (including every move of an in-flight drag, wherever the pointer is).
    /// Extend the selection to the pointer, character by character.
    ///
    /// The side is pinned to wherever the drag *started*: dragging from
    /// the old pane into the new one keeps selecting old-side text, which
    /// is the only reading that makes sense — the two sides are different
    /// documents.
    fn drag_to(&mut self, x: u16, y: u16) {
        let Some(sel) = self.selection else { return };
        let side = sel.side;
        let r = self.layout.diff;
        if y < r.y {
            self.scroll_diff(-1);
        } else if y >= r.y + r.height {
            self.scroll_diff(1);
        }
        let r = self.layout.diff;
        let offset = y.saturating_sub(r.y) as usize;
        let row = (self.diff_scroll + offset).min(
            self.diff_scroll
                .saturating_add(r.height.saturating_sub(1) as usize)
                .min(self.display.len().saturating_sub(1)),
        );
        // Past the last line, the selection runs to the end of the file
        // rather than stopping dead where the text does.
        let line = match self.line_on_row(row, side) {
            Some(line) => line,
            None => return,
        };
        let len = self
            .side_content(side)
            .and_then(|c| c.lines().nth(line.saturating_sub(1)))
            .map(|t| t.chars().count())
            .unwrap_or(0);
        // Left of the pane's text is the start of the line, not "no
        // column" — dragging out to the left selects to the line start.
        let col = self
            .diff_col_at(x, side)
            .and_then(|c| self.char_index(side, line, c))
            .unwrap_or(0)
            .min(len);
        let Some(sel) = &mut self.selection else {
            return;
        };
        sel.end = Pos::new(line, col);
        // Any drag at all means the user is choosing characters, not lines.
        sel.linewise = false;
    }

    /// The file line shown on a display row for one side — the vertical
    /// half of hit-testing, without the side-from-x guessing that
    /// `diff_pos_at` does (a drag already knows its side).
    fn line_on_row(&self, row: usize, side: Side) -> Option<usize> {
        let diff = self.diff.as_ref()?;
        let DisplayEntry::Line(i) = *self.display.get(row)? else {
            return None;
        };
        let row = match self.view {
            ViewMode::SideBySide => diff.rows.get(i)?,
            ViewMode::Inline => {
                let entry = diff.inline.get(i)?;
                if entry.side != side {
                    return None;
                }
                diff.rows.get(entry.row)?
            }
        };
        match side {
            Side::Left => row.old_ln,
            Side::Right => row.new_ln,
        }
    }

    /// Both panel dividers. They behave identically — press to start a
    /// drag, double-click to reset — so one handler drives both and
    /// [`Dragging`] names which one is in flight.
    fn divider_mouse(&mut self, m: MouseEvent, x: u16, y: u16) -> bool {
        let which = if contains(self.layout.divider, x, y) {
            Dragging::FilePanel
        } else if contains(self.layout.blame_divider, x, y) {
            Dragging::BlamePane
        } else {
            Dragging::None
        };
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if which != Dragging::None => {
                let name = self.panel_name(which);
                // Double-click puts it back to the default width.
                if self.double_click(x, y) {
                    self.dragging = Dragging::None;
                    let w = self.reset_panel(which);
                    self.ok(format!("{name} reset to {w} columns."));
                } else {
                    self.dragging = which;
                    self.ok(format!(
                        "Drag to resize the {} — double-click the divider to reset.",
                        name.to_lowercase()
                    ));
                }
                true
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging != Dragging::None => {
                self.drag_panel_to(x);
                true
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging != Dragging::None => {
                let which = self.dragging;
                self.dragging = Dragging::None;
                let (name, w) = (self.panel_name(which), self.panel_width(which));
                self.ok(format!("{name} {w} columns."));
                true
            }
            _ => false,
        }
    }

    fn panel_name(&self, which: Dragging) -> &'static str {
        match which {
            Dragging::BlamePane => "Blame pane",
            _ => "File panel",
        }
    }

    fn panel_width(&self, which: Dragging) -> u16 {
        match which {
            Dragging::BlamePane => self.blame_w,
            _ => self.file_panel_w,
        }
    }

    fn reset_panel(&mut self, which: Dragging) -> u16 {
        match which {
            Dragging::BlamePane => {
                self.blame_w = self.clamp_blame_w(BLAME_DEFAULT);
                self.blame_w
            }
            _ => {
                self.file_panel_w = self.clamp_panel_w(FILE_PANEL_DEFAULT);
                self.file_panel_w
            }
        }
    }

    /// Put the divider being dragged under column `x`. Each panel's width
    /// is measured from the left edge of whatever sits before it, so the
    /// seam lands where the pointer is rather than where the arithmetic
    /// of a fixed layout would put it.
    fn drag_panel_to(&mut self, x: u16) {
        match self.dragging {
            Dragging::FilePanel => {
                let left = self.layout.review.x;
                // The divider's left column is the panel's last column.
                let w = x.saturating_sub(left).saturating_add(1);
                self.file_panel_w = self.clamp_panel_w(w);
            }
            Dragging::BlamePane => {
                let left = self.layout.review.x + self.file_panel_w;
                let w = x.saturating_sub(left).saturating_add(1);
                self.blame_w = self.clamp_blame_w(w);
            }
            Dragging::None => {}
        }
    }

    /// True when this press lands on the same spot as the last one, quickly.
    fn double_click(&mut self, x: u16, y: u16) -> bool {
        let now = Instant::now();
        let double = matches!(
            self.last_click,
            Some((t, lx, ly))
                if now.duration_since(t).as_millis() < 400 && lx.abs_diff(x) <= 1 && ly == y
        );
        self.last_click = Some((now, x, y));
        double
    }

    /// A click inside the file panel: checkbox toggles viewed, a directory
    /// row collapses/expands, a file row opens the file.
    fn file_list_click(&mut self, x: u16, y: u16) {
        let fl = self.layout.file_list;
        let vis = self.file_scroll + (y - fl.y) as usize;
        let Some(entry) = self.entries.get(vis).cloned() else {
            return;
        };
        match entry {
            FileEntry::Dir { path, .. } => {
                if !self.collapsed_dirs.remove(&path) {
                    self.collapsed_dirs.insert(path);
                }
                self.rebuild_entries();
            }
            FileEntry::File { idx, depth } => {
                // The icon plus its trailing space: a 4-column target.
                let cb_start = fl.x + depth;
                // ↺ at the far right of the row, where the counts end.
                let revert_from = fl.x + fl.width.saturating_sub(REVERT_W);
                if x >= cb_start && x < cb_start + 4 {
                    self.toggle_file_mark(idx);
                } else if self.can_revert() && x >= revert_from {
                    self.ask_revert_file(idx);
                } else if idx != self.file_cursor {
                    self.spawn_load_file(idx);
                }
            }
        }
    }

    /// Absolute path of a repo-relative path. The repo root is a relative
    /// path until a repository is opened, so it is resolved first; the
    /// file itself is not, because a deleted file has nothing on disk to
    /// resolve and its path is still worth copying.
    fn abs_path(&self, rel: &str) -> Option<PathBuf> {
        let root =
            std::fs::canonicalize(&self.repo_root).unwrap_or_else(|_| self.repo_root.clone());
        gitops::safe_repo_path(&root, rel)
    }

    /// Open the right-click menu for the file-panel row at `y`.
    fn open_path_menu(&mut self, x: u16, y: u16) {
        let fl = self.layout.file_list;
        let vis = self.file_scroll + (y - fl.y) as usize;
        let Some(entry) = self.entries.get(vis) else {
            return;
        };
        let (path, is_dir) = match entry {
            FileEntry::Dir { path, .. } => (path.clone(), true),
            FileEntry::File { idx, .. } => match self.files.get(*idx) {
                Some(f) => (f.path.clone(), false),
                None => return,
            },
        };
        let mut items = vec![PathMenuItem {
            key: 'r',
            label: "Copy relative path",
            text: path.clone(),
        }];
        // A path git cannot express as a plain relative path has no
        // absolute form worth handing out, so that line is left off.
        if let Some(abs) = self.abs_path(&path) {
            items.push(PathMenuItem {
                key: 'f',
                label: "Copy full path",
                text: abs.to_string_lossy().into_owned(),
            });
        }
        self.overlay = Overlay::PathMenu(Box::new(PathMenu {
            path,
            is_dir,
            items,
            sel: 0,
            anchor: (x, y),
        }));
    }

    /// The commit the blame pane shows on display row `row`.
    ///
    /// Which side is blamed follows what the row shows. An inline row
    /// names its own side. A split row prefers the new side and falls
    /// back to the old one, which is the only side a purely removed row
    /// has — and the side that answers "what am I deleting?".
    pub fn blame_for_row(&self, row: usize) -> Option<Arc<blame::Commit>> {
        let DisplayEntry::Line(i) = *self.display.get(row)? else {
            return None;
        };
        let diff = self.diff.as_ref()?;
        let (side, ln) = match self.view {
            ViewMode::Inline => {
                let entry = diff.inline.get(i)?;
                let row = diff.rows.get(entry.row)?;
                let ln = match entry.side {
                    Side::Left => row.old_ln,
                    Side::Right => row.new_ln,
                };
                (entry.side, ln)
            }
            ViewMode::SideBySide => {
                let row = diff.rows.get(i)?;
                match row.new_ln {
                    Some(n) => (Side::Right, Some(n)),
                    None => (Side::Left, row.old_ln),
                }
            }
        };
        let blame = match side {
            Side::Right => self.blame_new.as_ref(),
            Side::Left => self.blame_old.as_ref(),
        }?;
        blame.at(ln?).cloned()
    }

    /// The repository the pane links pull requests in: the one under
    /// review when there is one, and the origin remote otherwise.
    pub fn blame_repo(&self) -> Option<String> {
        self.repo.clone().or_else(|| self.blame_origin.clone())
    }

    /// True while the blame pane is waiting for its job — the pane says
    /// so rather than looking like a file with no history.
    pub fn blame_loading(&self) -> bool {
        self.blame_job.is_some()
    }

    /// True when `commit` was written by the reader.
    pub fn blame_is_mine(&self, commit: &blame::Commit) -> bool {
        self.blame_me
            .as_deref()
            .is_some_and(|me| me == commit.author_email.to_lowercase())
    }

    /// Open the popup for the blame row at screen position (x, y).
    fn open_blame_menu(&mut self, x: u16, y: u16) {
        let r = self.layout.blame;
        let row = self.diff_scroll + y.saturating_sub(r.y) as usize;
        let Some(commit) = self.blame_for_row(row) else {
            self.err(if self.blame_job.is_some() {
                "Blame is still loading."
            } else {
                "Nothing is blamed on that row."
            });
            return;
        };
        let pr = self.blame_prs.get(&commit.sha).cloned();
        let mut items = Vec::new();
        if let Some(pr) = &pr {
            items.push(BlameMenuItem {
                key: 'o',
                label: format!("Open pull request #{} in the browser", pr.number),
                action: BlameAction::OpenPr(pr.number),
            });
            if !pr.url.is_empty() {
                items.push(BlameMenuItem {
                    key: 'y',
                    label: "Copy the pull request link".into(),
                    action: BlameAction::Copy(pr.url.clone()),
                });
            }
        }
        // There is no hash to copy for a line that is not committed yet.
        if !commit.uncommitted() {
            items.push(BlameMenuItem {
                key: 'c',
                label: "Copy the commit hash".into(),
                action: BlameAction::Copy(commit.sha.clone()),
            });
        }
        let in_change = self.blame_change_set.contains(&commit.sha);
        let mine = self.blame_is_mine(&commit);
        self.overlay = Overlay::BlameMenu(Box::new(BlameMenu {
            commit,
            pr,
            in_change,
            mine,
            items,
            sel: 0,
            anchor: (x, y),
        }));
    }

    /// Run line `i` of the blame popup and close it.
    fn blame_menu_run(&mut self, i: usize) {
        let Overlay::BlameMenu(menu) = &self.overlay else {
            return;
        };
        let Some(item) = menu.items.get(i) else {
            return;
        };
        let action = item.action.clone();
        self.overlay = Overlay::None;
        match action {
            BlameAction::Copy(text) => match clipboard::copy(&text) {
                Ok(via) => self.ok(format!("⧉ Copied {text} to the clipboard via {via}.")),
                Err(e) => self.err(format!("Couldn't copy: {e:#}")),
            },
            BlameAction::OpenPr(number) => {
                let Some(repo) = self.blame_repo() else {
                    self.err("No GitHub repository for this review — nothing to open.");
                    return;
                };
                // `gh` blocks while it starts a browser; fire and forget,
                // so the review stays responsive either way.
                let (tx, rx) = mpsc::channel();
                thread::spawn(move || {
                    let _ = tx.send(github::open_pr_web(&repo, number).map(|()| None));
                });
                self.bg_jobs.push(BgJob {
                    rx,
                    kind: BgKind::OpenPr { number },
                });
                self.ok(format!("Opening PR #{number} in your browser…"));
            }
        }
    }

    /// Copy line `i` of the right-click menu and close it.
    fn path_menu_copy(&mut self, i: usize) {
        let Overlay::PathMenu(menu) = &self.overlay else {
            return;
        };
        let Some(item) = menu.items.get(i) else {
            return;
        };
        let text = item.text.clone();
        self.overlay = Overlay::None;
        match clipboard::copy(&text) {
            Ok(via) => self.ok(format!("⧉ Copied {text} to the clipboard via {via}.")),
            Err(e) => self.err(format!("Couldn't copy: {e:#}")),
        }
    }

    // ------------------------------------------------------------- ☰ menu

    /// Build the ☰ menu for whatever is on screen right now.
    ///
    /// The toolbar shows the two or three things that fit; this is the rest
    /// of the tool. It is rebuilt on every open rather than cached, so a
    /// line's enabled state and its on/off mark always describe the state
    /// the reader is actually looking at.
    fn build_menu(&self) -> Vec<MenuRow> {
        let item = |label: String, hint: &'static str, id: ButtonId| {
            MenuRow::Item(MenuItem {
                label,
                hint,
                id,
                enabled: true,
                checked: None,
            })
        };
        let switch = |label: String, hint: &'static str, id: ButtonId, on: bool| {
            MenuRow::Item(MenuItem {
                label,
                hint,
                id,
                enabled: true,
                checked: Some(on),
            })
        };
        let maybe = |label: String, hint: &'static str, id: ButtonId, enabled: bool| {
            MenuRow::Item(MenuItem {
                label,
                hint,
                id,
                enabled,
                checked: None,
            })
        };

        let mut rows = Vec::new();

        if self.screen == Screen::PrList {
            rows.push(MenuRow::Heading("GO"));
            rows.push(item("⎇  Local changes".into(), "l", ButtonId::LocalChanges));
            rows.push(item("⟳  Refresh the list".into(), "r", ButtonId::Refresh));
            rows.push(MenuRow::Heading("SETTINGS"));
            rows.push(item("🎨 Theme".into(), "t", ButtonId::Theme));
            rows.push(item("?  Help".into(), "?", ButtonId::Help));
            rows.push(item("✕  Quit".into(), "q", ButtonId::Quit));
            return rows;
        }

        if self.preview.is_some() {
            rows.push(MenuRow::Heading("PREVIEW"));
            rows.push(item(
                "✎  Show the source".into(),
                "P",
                ButtonId::PreviewToggle,
            ));
            rows.push(item("⟳  Reload from disk".into(), "r", ButtonId::Refresh));
            if !self.preview_only {
                rows.push(item(
                    "✕  Back to the diff".into(),
                    "Esc",
                    ButtonId::PreviewClose,
                ));
            }
            rows.push(MenuRow::Heading("SETTINGS"));
            rows.push(item("🎨 Theme".into(), "t", ButtonId::Theme));
            rows.push(item("?  Help".into(), "?", ButtonId::Help));
            rows.push(item("✕  Quit".into(), "q", ButtonId::Quit));
            return rows;
        }

        if self.editor.is_some() {
            rows.push(MenuRow::Heading("EDITOR"));
            rows.push(item("💾 Save".into(), "Ctrl+S", ButtonId::EditorSave));
            rows.push(item("⇥  Format".into(), "Ctrl+T", ButtonId::EditorFormat));
            if markdown::is_markdown(self.editor.as_ref().map(|e| e.path.as_str()).unwrap_or("")) {
                rows.push(item(
                    "📖 Preview the markdown".into(),
                    "Alt+P",
                    ButtonId::PreviewToggle,
                ));
            }
            rows.push(item(
                "✕  Close the editor".into(),
                "Esc",
                ButtonId::EditorClose,
            ));
            rows.push(MenuRow::Heading("SETTINGS"));
            rows.push(item("🎨 Theme".into(), "t", ButtonId::Theme));
            rows.push(item("?  Help".into(), "?", ButtonId::Help));
            return rows;
        }

        let has_sel = self.selection.is_some();
        let split = self.view == ViewMode::SideBySide;

        rows.push(MenuRow::Heading("VIEW"));
        rows.push(item(
            if split {
                "≡  Switch to inline".into()
            } else {
                "◫  Switch to split".into()
            },
            "v",
            ButtonId::ViewToggle,
        ));
        rows.push(switch(
            "⇕  Fold unchanged lines".into(),
            "z",
            ButtonId::FoldToggle,
            self.collapse_unchanged,
        ));
        rows.push(switch(
            "🌲 Tree file panel".into(),
            "",
            ButtonId::TreeToggle,
            self.tree_view,
        ));
        rows.push(switch(
            "👤 Blame column".into(),
            "B",
            ButtonId::BlameToggle,
            self.blame_on,
        ));

        rows.push(MenuRow::Heading("FIND"));
        rows.push(item("Go to a file".into(), "Ctrl+P", ButtonId::Find));
        rows.push(item(
            "Search the repository".into(),
            "#",
            ButtonId::FindGrep,
        ));
        rows.push(item(
            "Symbols in this file".into(),
            "@",
            ButtonId::FindSymbols,
        ));
        rows.push(item("Search this diff".into(), "/", ButtonId::FindInDiff));

        rows.push(MenuRow::Heading("ACTIONS"));
        rows.push(item("✎  Edit this file".into(), "e", ButtonId::Edit));
        rows.push(maybe(
            "📖 Preview the markdown".into(),
            "P",
            ButtonId::PreviewToggle,
            self.can_preview(),
        ));
        if !self.local {
            rows.push(maybe(
                "💬 Comment on the selection".into(),
                "c",
                ButtonId::Comment,
                has_sel,
            ));
        }
        rows.push(maybe(
            "⧉  Copy the selection".into(),
            "y",
            ButtonId::Copy,
            has_sel,
        ));
        let revert = self.can_revert();
        rows.push(maybe(
            "↺  Revert this section".into(),
            "u",
            ButtonId::RevertSection,
            revert,
        ));
        rows.push(maybe(
            "↺  Revert the whole file".into(),
            "U",
            ButtonId::RevertFile,
            revert,
        ));

        rows.push(MenuRow::Heading("GO"));
        rows.push(item("⟳  Refresh now".into(), "r", ButtonId::Refresh));
        rows.push(item(
            if self.local {
                "⇄  Swap to the pull request".into()
            } else {
                "⇄  Swap to local changes".into()
            },
            "`",
            ButtonId::SwapView,
        ));
        rows.push(item(
            "←  Pull request list".into(),
            "b",
            ButtonId::BackToPrs,
        ));

        rows.push(MenuRow::Heading("SETTINGS"));
        rows.push(item("🎨 Theme".into(), "t", ButtonId::Theme));
        // Only local review polls, so the switch belongs to local review.
        if self.local {
            rows.push(switch(
                "⟳  Refresh while idle".into(),
                "",
                ButtonId::AutoRefreshToggle,
                self.auto_refresh,
            ));
        }
        rows.push(item("?  Help".into(), "?", ButtonId::Help));
        rows.push(item("✕  Quit".into(), "q", ButtonId::Quit));
        rows
    }

    /// Open the ☰ menu under the button at (`x`, `y`).
    pub fn open_menu(&mut self, x: u16, y: u16) {
        let rows = self.build_menu();
        let mut menu = Menu {
            rows,
            sel: 0,
            scroll: 0,
            anchor: (x, y),
        };
        menu.sel = menu.first_selectable();
        self.overlay = Overlay::Menu(Box::new(menu));
    }

    /// `m`: open the ☰ menu where the ☰ button is, so it lands in the same
    /// place whether the mouse or the keyboard asked for it.
    pub fn open_menu_from_key(&mut self) {
        let anchor = self
            .layout
            .buttons
            .iter()
            .find(|(_, id)| *id == ButtonId::Menu)
            .map(|(r, _)| (r.x, r.y))
            .unwrap_or((0, 0));
        self.open_menu(anchor.0, anchor.1);
    }

    /// Run the ☰ menu line at `i` and close the menu.
    fn menu_activate(&mut self, i: usize) {
        let Overlay::Menu(menu) = &self.overlay else {
            return;
        };
        let Some(MenuRow::Item(item)) = menu.rows.get(i) else {
            return;
        };
        if !item.enabled {
            return;
        }
        let id = item.id;
        self.overlay = Overlay::None;
        self.activate(id);
    }

    /// The one dispatch table behind every toolbar button and every ☰ menu
    /// line. Returns false for an id this does not own (an overlay's own
    /// buttons handle those themselves).
    pub fn activate(&mut self, id: ButtonId) -> bool {
        match id {
            ButtonId::ViewToggle => self.toggle_view(),
            ButtonId::ViewTree => self.set_tree_view(true),
            ButtonId::ViewFlat => self.set_tree_view(false),
            ButtonId::TreeToggle => self.set_tree_view(!self.tree_view),
            ButtonId::FoldToggle => self.toggle_fold(),
            ButtonId::BlameToggle => self.toggle_blame(),
            ButtonId::Find => self.open_finder(FinderMode::Files),
            ButtonId::FindGrep => self.open_finder(FinderMode::Grep),
            ButtonId::FindSymbols => self.open_finder(FinderMode::Symbols),
            ButtonId::FindInDiff => self.start_find(),
            ButtonId::Edit => self.open_editor(None),
            ButtonId::PreviewToggle => self.toggle_preview(),
            ButtonId::PreviewClose => self.close_preview(),
            ButtonId::Comment => self.open_comment(),
            ButtonId::Copy => self.yank(),
            ButtonId::RevertSection => self.ask_revert_section(self.diff_cursor),
            ButtonId::RevertFile => self.ask_revert_file(self.file_cursor),
            ButtonId::Refresh if self.preview.is_some() => self.reload_preview(),
            ButtonId::Refresh => match self.screen {
                Screen::PrList => self.spawn_load_prs(),
                Screen::Review => self.refresh_review(),
            },
            ButtonId::SwapView => self.toggle_workspace(),
            ButtonId::BackToPrs => self.back_to_pr_list(),
            ButtonId::LocalChanges => self.spawn_open_local(true),
            ButtonId::Theme => self.open_theme_picker(),
            ButtonId::AutoRefreshToggle => {
                self.auto_refresh = !self.auto_refresh;
                if self.auto_refresh {
                    // Start the clock now, not from whenever it was last on.
                    self.last_auto_rescan = Instant::now();
                    self.ok("⟳ Idle refresh on — local changes re-scan while you read.");
                } else {
                    self.ok("⟳ Idle refresh off — press r (or ⟳) to re-scan.");
                }
            }
            ButtonId::Help => self.overlay = Overlay::Help,
            ButtonId::Quit => self.should_quit = true,
            ButtonId::EditorSave => self.spawn_save_editor(),
            ButtonId::EditorClose => self.request_close_editor(),
            ButtonId::EditorFormat => {
                self.ok("Formatting…");
                self.spawn_editor_request(EditorRequest::Format);
            }
            _ => return false,
        }
        true
    }

    fn diff_click(&mut self, x: u16, y: u16) {
        // Clicking a fold row expands it.
        let r = self.layout.diff;
        let vis = self.diff_scroll + (y - r.y) as usize;
        // The change bar down the left edge: anywhere on a section's bar
        // offers to put that section back.
        if x < r.x + self.revert_gutter() {
            self.ask_revert_section(vis);
            return;
        }
        if self.toggle_fold_row(vis) {
            return;
        }
        // Keyboard and mouse share one cursor.
        if vis < self.display.len() {
            self.diff_cursor = vis;
        }

        let double = self.double_click(x, y);

        let Some((side, line)) = self.diff_pos_at(x, y) else {
            return;
        };
        // Remember which word was clicked: it makes the next gd / gr / K
        // unambiguous without needing a column cursor.
        self.click_word = self.diff_char_at(x, side, line).map(|c| (vis, c));
        if double && side == Side::Right {
            self.open_editor(Some(line));
            return;
        }
        // A click selects the whole line — the shape commenting wants.
        // Dragging from here switches to exactly the characters covered
        // (see the Drag arm in `mouse_review`).
        let col = self.diff_char_at(x, side, line).unwrap_or(0);
        self.selection = Some(Selection {
            side,
            anchor: Pos::new(line, col),
            end: Pos::new(line, col),
            linewise: true,
        });
        self.select_mode = false;
        self.drag_select = true;
        let side_name = if side == Side::Right { "new" } else { "old" };
        self.ok(format!(
            "Line {line} selected ({side_name}) — drag for text · y copies · c comments"
        ));
    }

    /// Which pane the pointer is over. Inline view has only one.
    fn pane_at(&self, x: u16) -> Side {
        let r = self.diff_body();
        let lw = (r.width as usize).saturating_sub(1) / 2;
        if self.view == ViewMode::Inline || (x as usize) < r.x as usize + lw + 1 {
            Side::Left
        } else {
            Side::Right
        }
    }

    /// Display column within one pane's body, horizontal scroll included —
    /// the horizontal half of the mapping [`Self::diff_pos_at`] does
    /// vertically. `None` when the pointer is left of that body.
    ///
    /// The pane is passed in rather than derived from `x` because the two
    /// callers want different things: a click reads the pane it landed on,
    /// while a drag stays with the pane it started in even when the
    /// pointer wanders across the divider.
    fn diff_col_at(&self, x: u16, pane: Side) -> Option<usize> {
        let r = self.diff_body();
        let lw = (r.width as usize).saturating_sub(1) / 2;
        // Gutters: 5 columns per side, 12 inline.
        let body_x = match (self.view, pane) {
            (ViewMode::Inline, _) => r.x as usize + 12,
            (ViewMode::SideBySide, Side::Left) => r.x as usize + 5,
            (ViewMode::SideBySide, Side::Right) => r.x as usize + lw + 1 + 5,
        };
        Some((x as usize).checked_sub(body_x)? + self.diff_hscroll)
    }

    /// Char index in a file line, for a display column (tabs expanded).
    fn char_index(&self, side: Side, line: usize, col: usize) -> Option<usize> {
        let text = self.side_content(side)?.lines().nth(line.checked_sub(1)?)?;
        Some(crate::diff::char_at_col(text, col))
    }

    /// Char index under the pointer, for a click.
    fn diff_char_at(&self, x: u16, side: Side, line: usize) -> Option<usize> {
        let col = self.diff_col_at(x, self.pane_at(x))?;
        self.char_index(side, line, col)
    }

    /// Map a screen position inside the diff area to (side, file line).
    fn diff_pos_at(&self, x: u16, y: u16) -> Option<(Side, usize)> {
        let diff = self.diff.as_ref()?;
        let r = self.layout.diff;
        if !contains(r, x, y) {
            return None;
        }
        let vis = self.diff_scroll + (y - r.y) as usize;
        let DisplayEntry::Line(i) = *self.display.get(vis)? else {
            return None;
        };
        match self.view {
            ViewMode::SideBySide => {
                let row = diff.rows.get(i)?;
                let body = self.diff_body();
                let mid = body.x + body.width / 2;
                let clicked_left = x < mid;
                use crate::diff::RowKind::*;
                if clicked_left {
                    match row.kind {
                        Removed | Modified => Some((Side::Left, row.old_ln?)),
                        Context => Some((Side::Right, row.new_ln?)),
                        Added => Some((Side::Right, row.new_ln?)),
                    }
                } else {
                    match row.kind {
                        Removed => Some((Side::Left, row.old_ln?)),
                        _ => Some((Side::Right, row.new_ln?)),
                    }
                }
            }
            ViewMode::Inline => {
                let entry = diff.inline.get(i)?;
                let row = &diff.rows[entry.row];
                match entry.side {
                    Side::Left => Some((Side::Left, row.old_ln?)),
                    Side::Right => Some((Side::Right, row.new_ln?)),
                }
            }
        }
    }

    // -------------------------------------------------------------- actions

    fn toggle_view(&mut self) {
        let next = match self.view {
            ViewMode::SideBySide => ViewMode::Inline,
            ViewMode::Inline => ViewMode::SideBySide,
        };
        self.set_view(next);
    }

    fn set_view(&mut self, mode: ViewMode) {
        if self.view == mode {
            return;
        }
        // Keep roughly the same spot on screen: anchor on the visible row.
        let anchor = self.anchors();
        self.view = mode;
        self.rebuild_display();
        self.restore_anchors(anchor);
        // Gutter widths differ between the views: a carried-over horizontal
        // offset would land somewhere arbitrary.
        self.diff_hscroll = 0;
    }

    fn set_tree_view(&mut self, tree: bool) {
        if self.tree_view == tree {
            return;
        }
        self.tree_view = tree;
        self.rebuild_entries();
        self.reveal_current_file();
    }

    /// The Fold button / `z`. Folding is on with some sections expanded →
    /// fold those back first (that is what "fold" means from there); only
    /// once everything is folded does the toggle flip to the full file.
    fn toggle_fold(&mut self) {
        if self.collapse_unchanged && !self.expanded_folds.is_empty() {
            let n = self.expanded_folds.len();
            self.refold_with_anchor();
            let s = if n == 1 { "section" } else { "sections" };
            self.ok(format!(
                "Re-folded {n} expanded {s} — press z again for the full file."
            ));
            return;
        }
        self.set_collapse(!self.collapse_unchanged);
    }

    /// Drop every expanded fold, keeping the current row on screen.
    fn refold_with_anchor(&mut self) {
        let anchor = self.anchors();
        self.expanded_folds.clear();
        self.rebuild_display();
        self.restore_anchors(anchor);
    }

    /// Diff rows the scroll top and the cursor sit on. Folding and view
    /// switches renumber `display`, so both are restored by row, not index.
    fn anchors(&self) -> (usize, usize) {
        let row_at = |i: usize| self.display.get(i).map(|e| self.entry_row(*e)).unwrap_or(0);
        (row_at(self.diff_scroll), row_at(self.diff_cursor))
    }

    fn restore_anchors(&mut self, (scroll, cursor): (usize, usize)) {
        let s = self.row_position(scroll);
        let c = self.row_position(cursor);
        self.diff_scroll = s;
        self.diff_cursor = c;
    }

    /// First display row at or after diff row `anchor`.
    fn row_position(&self, anchor: usize) -> usize {
        self.display
            .iter()
            .position(|e| self.entry_row(*e) >= anchor)
            .unwrap_or(0)
    }

    // ------------------------------------------------------------- cursor

    /// (side, file line) under the cursor — the same mapping a click on the
    /// right-hand pane uses, so keyboard and mouse agree.
    pub fn cursor_pos(&self) -> Option<(Side, usize)> {
        let diff = self.diff.as_ref()?;
        let DisplayEntry::Line(i) = *self.display.get(self.diff_cursor)? else {
            return None;
        };
        match self.view {
            ViewMode::SideBySide => {
                let row = diff.rows.get(i)?;
                match row.kind {
                    RowKind::Removed => Some((Side::Left, row.old_ln?)),
                    _ => Some((Side::Right, row.new_ln?)),
                }
            }
            ViewMode::Inline => {
                let entry = diff.inline.get(i)?;
                let row = &diff.rows[entry.row];
                match entry.side {
                    Side::Left => Some((Side::Left, row.old_ln?)),
                    Side::Right => Some((Side::Right, row.new_ln?)),
                }
            }
        }
    }

    /// Put the cursor on a display row and scroll enough to show it.
    fn cursor_to(&mut self, idx: usize) {
        if self.display.is_empty() {
            self.diff_cursor = 0;
            return;
        }
        self.diff_cursor = idx.min(self.display.len() - 1);
        self.extend_selection();
        self.ensure_cursor_visible();
    }

    fn cursor_by(&mut self, delta: i32) {
        let next = (self.diff_cursor as i64 + delta as i64).max(0) as usize;
        self.cursor_to(next);
    }

    /// Scroll so the cursor stays on screen, keeping SCROLLOFF lines of
    /// context where the file allows it.
    fn ensure_cursor_visible(&mut self) {
        let h = self.diff_page();
        let off = SCROLLOFF.min(h.saturating_sub(1) / 2);
        let max = self.display.len().saturating_sub(h);
        let cur = self.diff_cursor;
        if cur < self.diff_scroll + off {
            self.diff_scroll = cur.saturating_sub(off);
        } else if cur + off + 1 > self.diff_scroll + h {
            self.diff_scroll = (cur + off + 1).saturating_sub(h);
        }
        self.diff_scroll = self.diff_scroll.min(max);
    }

    /// After a view-only scroll (wheel, Ctrl+E/Ctrl+Y) drag the cursor along
    /// so it never sits off screen — the way vim does.
    fn clamp_cursor_to_view(&mut self) {
        if self.display.is_empty() {
            return;
        }
        let h = self.diff_page();
        let last = self.display.len() - 1;
        let hi = (self.diff_scroll + h).saturating_sub(1).min(last);
        self.diff_cursor = self.diff_cursor.clamp(self.diff_scroll.min(hi), hi);
        self.extend_selection();
    }

    /// The cursor row's line number on one specific side, if it has one.
    /// A selection anchored on the old side keeps extending across modified
    /// rows this way, even though the cursor's own side reads as "new".
    fn cursor_line_on(&self, side: Side) -> Option<usize> {
        let diff = self.diff.as_ref()?;
        let DisplayEntry::Line(i) = *self.display.get(self.diff_cursor)? else {
            return None;
        };
        let row = match self.view {
            ViewMode::SideBySide => diff.rows.get(i)?,
            ViewMode::Inline => {
                let entry = diff.inline.get(i)?;
                // In inline view each screen row belongs to one side.
                if entry.side != side {
                    return None;
                }
                diff.rows.get(entry.row)?
            }
        };
        match side {
            Side::Left => row.old_ln,
            Side::Right => row.new_ln,
        }
    }

    /// In line-visual mode, motions drag the selection's far end along.
    fn extend_selection(&mut self) {
        if !self.select_mode {
            return;
        }
        let Some(side) = self.selection.map(|s| s.side) else {
            return;
        };
        if let Some(line) = self.cursor_line_on(side) {
            if let Some(sel) = &mut self.selection {
                sel.end = Pos::new(line, 0);
                sel.linewise = true;
            }
        }
    }

    /// `V`: start (or end) a line selection at the cursor.
    fn toggle_select_mode(&mut self) {
        if self.select_mode {
            self.select_mode = false;
            self.ok("Selection finished — c comments, Esc clears.");
            return;
        }
        let Some((side, line)) = self.cursor_pos() else {
            self.err("Nothing selectable on this row.");
            return;
        };
        self.selection = Some(Selection::lines(side, line, line));
        self.select_mode = true;
        let which = if side == Side::Right { "new" } else { "old" };
        self.ok(format!(
            "Selecting {which} line {line} — j/k extends, c comments, Esc cancels."
        ));
    }

    fn clear_selection(&mut self) {
        self.select_mode = false;
        self.selection = None;
        self.ok("Selection cleared.");
    }

    /// True when this display row is part of a change (not context, not a
    /// fold banner).
    fn entry_changed(&self, idx: usize) -> bool {
        let Some(diff) = &self.diff else { return false };
        match self.display.get(idx) {
            Some(DisplayEntry::Line(i)) => {
                let row = if self.view == ViewMode::SideBySide {
                    *i
                } else {
                    diff.inline[*i].row
                };
                diff.rows[row].kind != RowKind::Context
            }
            _ => false,
        }
    }

    /// `{` / `}`: the first row of the next (or previous) run of changes.
    fn jump_hunk(&mut self, forward: bool) {
        let starts = |i: usize| self.entry_changed(i) && (i == 0 || !self.entry_changed(i - 1));
        let found = if forward {
            (self.diff_cursor + 1..self.display.len()).find(|i| starts(*i))
        } else {
            (0..self.diff_cursor).rev().find(|i| starts(*i))
        };
        match found {
            Some(i) => {
                self.cursor_to(i);
                self.ok(if forward {
                    "Next change."
                } else {
                    "Previous change."
                });
            }
            None => self.err(if forward {
                "No more changes below — ] for the next file."
            } else {
                "No more changes above — [ for the previous file."
            }),
        }
    }

    /// Expand or fold the banner row at `idx`; true when it was one.
    fn toggle_fold_row(&mut self, idx: usize) -> bool {
        match self.display.get(idx).copied() {
            Some(DisplayEntry::Fold { start, count }) => {
                let anchor = self.anchors();
                self.expanded_folds.insert(start);
                self.rebuild_display();
                self.restore_anchors(anchor);
                self.ok(format!(
                    "Expanded {count} unchanged lines — click the header (or Enter) to fold again."
                ));
                true
            }
            Some(DisplayEntry::Unfold { start, count }) => {
                let anchor = self.anchors();
                self.expanded_folds.remove(&start);
                self.rebuild_display();
                self.restore_anchors(anchor);
                self.ok(format!("Folded {count} unchanged lines."));
                true
            }
            _ => false,
        }
    }

    fn set_collapse(&mut self, on: bool) {
        if self.collapse_unchanged == on {
            return;
        }
        let anchor = self.anchors();
        self.collapse_unchanged = on;
        // Turning folding back on means *all* of it: sections expanded
        // before are folded again, not left open.
        if on {
            self.expanded_folds.clear();
        }
        self.rebuild_display();
        self.restore_anchors(anchor);
        if on {
            self.ok("Unchanged sections collapsed — click a fold (or press z) to expand.");
        } else {
            self.ok("Showing the full file.");
        }
    }

    fn diff_page(&self) -> usize {
        (self.layout.diff.height as usize).max(1)
    }

    fn scroll_diff(&mut self, delta: i32) {
        let len = self.display.len();
        let h = self.diff_page();
        let max = len.saturating_sub(h);
        let cur = self.diff_scroll as i64 + delta as i64;
        self.diff_scroll = cur.clamp(0, max as i64) as usize;
    }

    /// Columns available for line *text* in the diff pane — what the
    /// horizontal offset is measured against. The gutters don't scroll.
    pub fn diff_body_w(&self) -> usize {
        let w = self.diff_body().width as usize;
        match self.view {
            // Two panes split by one divider column, each with a 5-column
            // line-number gutter.
            ViewMode::SideBySide => (w.saturating_sub(1) / 2).saturating_sub(5),
            // Inline gutter: "%4d %4d %c " — 12 columns.
            ViewMode::Inline => w.saturating_sub(12),
        }
    }

    /// Furthest the diff body can scroll sideways: the widest line, less
    /// what already fits.
    pub fn max_hscroll(&self) -> usize {
        self.diff
            .as_ref()
            .map(|d| d.max_width.saturating_sub(self.diff_body_w()))
            .unwrap_or(0)
    }

    fn scroll_diff_h(&mut self, delta: i32) {
        let max = self.max_hscroll();
        let cur = self.diff_hscroll as i64 + delta as i64;
        let next = cur.clamp(0, max as i64) as usize;
        if next == self.diff_hscroll {
            return;
        }
        self.diff_hscroll = next;
        if next == 0 {
            self.ok("Back at column 1.");
        } else {
            self.ok(format!(
                "Column {} of {} — ←/→ or Shift+wheel to scroll, Home to reset.",
                next + 1,
                self.diff.as_ref().map(|d| d.max_width).unwrap_or(0),
            ));
        }
    }

    /// Widen (`delta` > 0) or narrow the file panel, in columns.
    fn resize_file_panel(&mut self, delta: i32) {
        let cur = self.file_panel_w as i32 + delta;
        let next = self.clamp_panel_w(cur.max(0) as u16);
        if next == self.file_panel_w {
            return;
        }
        self.file_panel_w = next;
        self.ok(format!(
            "File panel {next} columns — drag the divider, or < / >."
        ));
    }

    /// Columns the blame pane takes right now — zero when it is off, and
    /// zero when the terminal is too narrow to carry three panes. The
    /// layout, the resize clamps and the mouse arithmetic all measure
    /// from this, so they cannot disagree about whether it is there.
    pub fn blame_gutter(&self) -> u16 {
        if !self.blame_on || self.screen != Screen::Review {
            return 0;
        }
        let need = FILE_PANEL_MIN + BLAME_MIN + DIFF_MIN_W;
        if self.layout.review.width < need {
            return 0;
        }
        self.clamp_blame_w(self.blame_w)
    }

    /// Keep the panel wide enough to be useful and the diff pane alive.
    /// The blame pane, when it is showing, is not available to either.
    pub fn clamp_panel_w(&self, w: u16) -> u16 {
        let taken = if self.blame_on {
            self.blame_w.max(BLAME_MIN)
        } else {
            0
        };
        let avail = self.layout.review.width.saturating_sub(taken);
        let hi = avail.saturating_sub(DIFF_MIN_W).max(FILE_PANEL_MIN);
        w.clamp(FILE_PANEL_MIN, hi)
    }

    /// The same, for the blame pane: it may not take the file panel's
    /// floor, and it may not take the diff's.
    pub fn clamp_blame_w(&self, w: u16) -> u16 {
        let avail = self
            .layout
            .review
            .width
            .saturating_sub(self.file_panel_w.max(FILE_PANEL_MIN));
        let hi = avail.saturating_sub(DIFF_MIN_W).max(BLAME_MIN);
        w.clamp(BLAME_MIN, hi)
    }

    /// Expand any collapsed dirs hiding the open file, then scroll to it.
    fn reveal_current_file(&mut self) {
        if let Some(file) = self.files.get(self.file_cursor) {
            let path = &file.path;
            let before = self.collapsed_dirs.len();
            self.collapsed_dirs
                .retain(|d| !path.starts_with(&format!("{d}/")));
            if self.collapsed_dirs.len() != before {
                self.rebuild_entries();
            }
        }
        self.ensure_file_visible();
    }

    fn ensure_file_visible(&mut self) {
        let Some(pos) = self
            .entries
            .iter()
            .position(|e| matches!(e, FileEntry::File { idx, .. } if *idx == self.file_cursor))
        else {
            return;
        };
        let h = (self.layout.file_list.height as usize).max(1);
        if pos < self.file_scroll {
            self.file_scroll = pos;
        } else if pos >= self.file_scroll + h {
            self.file_scroll = pos + 1 - h;
        }
    }

    fn ensure_pr_visible(&mut self) {
        let h = (self.layout.pr_list.height as usize).max(1);
        if self.pr_cursor < self.pr_scroll {
            self.pr_scroll = self.pr_cursor;
        } else if self.pr_cursor >= self.pr_scroll + h {
            self.pr_scroll = self.pr_cursor + 1 - h;
        }
    }

    fn step_file(&mut self, delta: i32) {
        if self.files.is_empty() {
            return;
        }
        let cur = self.file_cursor as i32 + delta;
        let idx = cur.clamp(0, self.files.len() as i32 - 1) as usize;
        if idx != self.file_cursor {
            self.spawn_load_file(idx);
        }
    }

    // ---------------------------------------------------------------- finder

    /// Paths of the changeset under review — the default search scope.
    fn changeset_paths(&self) -> Vec<String> {
        self.files.iter().map(|f| f.path.clone()).collect()
    }

    /// The commit a search should read. In local review that's the working
    /// tree (`None`); in PR review it's the head commit, because the tree
    /// on disk may be a different branch entirely — and even when it isn't,
    /// the commit is what the diff on screen is showing.
    fn search_rev(&self) -> Option<String> {
        if self.local {
            return None;
        }
        self.pr
            .as_ref()
            .map(|p| p.head_ref_oid.clone())
            .filter(|oid| !oid.is_empty())
    }

    /// Definitions in the open file, for the `@` mode.
    fn current_symbols(&self) -> (Vec<search::Symbol>, String) {
        let Some(file) = self.files.get(self.file_cursor) else {
            return (Vec::new(), String::new());
        };
        let content = self.new_content.as_ref().or(self.old_content.as_ref());
        let syms = content
            .map(|c| search::symbols(&file.path, c))
            .unwrap_or_default();
        (syms, file.path.clone())
    }

    pub fn open_finder(&mut self, mode: FinderMode) {
        if self.editor.is_some() {
            self.err("Close the editor first (Ctrl+S saves, Esc closes).");
            return;
        }
        let (symbols, symbol_path) = self.current_symbols();
        let finder = Finder::new(mode, self.changeset_paths(), symbols, symbol_path);
        self.overlay = Overlay::Finder(Box::new(finder));
        self.refresh_finder();
        if mode == FinderMode::Symbols {
            // Pattern matches show instantly; the language server's
            // better answer replaces them when it arrives.
            self.spawn_lsp_symbols();
        }
        self.ok(match mode {
            FinderMode::Files => "Type to filter files · # searches inside them · @ lists symbols",
            FinderMode::Grep => "Type to search inside every changed file · Tab widens to the repo",
            FinderMode::Symbols => "Type to jump to a definition in this file",
            FinderMode::Refs | FinderMode::Pick => "Type to filter · Enter chooses",
        });
    }

    /// Recompute the finder's rows, and fetch the repository file list if
    /// the scope now needs one.
    fn refresh_finder(&mut self) {
        let need_files = {
            let Overlay::Finder(f) = &mut self.overlay else {
                return;
            };
            f.rebuild();
            f.mode == FinderMode::Files && f.repo_scope && f.repo_files.is_none()
        };
        if need_files && self.search_job.is_none() {
            self.spawn_repo_files();
        }
    }

    /// Start the debounced query if its quiet period has passed.
    fn maybe_spawn_search(&mut self) -> bool {
        let ready = match &self.overlay {
            Overlay::Finder(f) => match &f.pending {
                Some((q, at)) if at.elapsed() >= SEARCH_DEBOUNCE => {
                    Some((q.clone(), f.repo_scope, f.regex))
                }
                _ => None,
            },
            _ => None,
        };
        let Some((query, repo_scope, regex)) = ready else {
            return false;
        };
        if let Overlay::Finder(f) = &mut self.overlay {
            f.pending = None;
        }
        self.spawn_grep(query, repo_scope, regex);
        true
    }

    fn spawn_grep(&mut self, query: String, repo_scope: bool, regex: bool) {
        self.search_gen += 1;
        let gen = self.search_gen;
        let req = search::GrepRequest {
            root: self.repo_root.clone(),
            query: query.clone(),
            rev: self.search_rev(),
            paths: if repo_scope {
                Vec::new()
            } else {
                self.changeset_paths()
            },
            regex,
            // Only a working-tree search can see files git doesn't know
            // about yet — which is exactly when they matter.
            untracked: self.local,
        };
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(
                search::grep(&req).map(|(hits, truncated)| SearchOutcome::Grep {
                    hits,
                    truncated,
                    query,
                }),
            );
        });
        self.search_job = Some(SearchJob {
            rx,
            gen,
            started: Instant::now(),
        });
    }

    fn spawn_repo_files(&mut self) {
        self.search_gen += 1;
        let gen = self.search_gen;
        let root = self.repo_root.clone();
        let rev = self.search_rev();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(search::list_files(&root, rev.as_deref()).map(SearchOutcome::Files));
        });
        self.search_job = Some(SearchJob {
            rx,
            gen,
            started: Instant::now(),
        });
    }

    /// Apply a finished query — unless the user has typed past it.
    fn apply_search(&mut self, gen: u64, outcome: SearchOutcome) {
        if gen != self.search_gen {
            return;
        }
        let Overlay::Finder(f) = &mut self.overlay else {
            return;
        };
        match outcome {
            SearchOutcome::Grep {
                hits,
                truncated,
                query,
            } => {
                let total = hits.len();
                f.rows = hits
                    .into_iter()
                    .map(|h| {
                        let in_changeset = f.changeset.contains(&h.path);
                        FinderRow {
                            line: Some(h.line),
                            text: h.text.trim_end().to_string(),
                            range: (h.len > 0).then_some((h.col, h.col + h.len)),
                            tag: if h.definition {
                                "def"
                            } else if in_changeset {
                                "changed"
                            } else {
                                ""
                            },
                            path: h.path,
                            matched: Vec::new(),
                            in_changeset,
                            pick: None,
                        }
                    })
                    .collect();
                // Definitions first, then hits in files under review. A
                // stable sort keeps git's path ordering inside each group.
                f.rows.sort_by_key(|r| (r.tag != "def", !r.in_changeset));
                f.sel = 0;
                f.scroll = 0;
                let scope = if f.repo_scope {
                    "the repository"
                } else {
                    "changed files"
                };
                f.note = if total == 0 {
                    format!(
                        "No match for “{query}” in {scope}.{}",
                        if f.repo_scope {
                            ""
                        } else {
                            " Tab searches the whole repo."
                        }
                    )
                } else {
                    format!(
                        "{total}{} match{} in {scope} · Tab {}",
                        if truncated { "+" } else { "" },
                        if total == 1 { "" } else { "es" },
                        if f.repo_scope {
                            "back to changed files"
                        } else {
                            "widens to the repo"
                        }
                    )
                };
            }
            SearchOutcome::Files(list) => {
                f.repo_files = Some(list);
                f.rebuild();
            }
            SearchOutcome::Symbols(syms) => {
                // The language server knows more than the pattern matcher
                // ever will — take its answer over ours.
                let path = f.symbol_path.clone();
                f.symbols = syms
                    .into_iter()
                    .map(|s| search::Symbol {
                        line: s.line,
                        kind: s.kind,
                        text: match &s.container {
                            Some(c) if !c.is_empty() => format!("{c}.{}", s.name),
                            _ => s.name.clone(),
                        },
                        name: s.name,
                    })
                    .collect();
                let n = f.symbols.len();
                f.rebuild();
                f.note = format!("{n} symbols in {path} · from the language server");
            }
        }
    }

    fn finder_key(&mut self, key: KeyEvent) {
        // Anything that closes the overlay is decided up front, so the
        // borrow of the finder is over before it runs.
        match key.code {
            KeyCode::Esc => {
                self.overlay = Overlay::None;
                self.ok("Search closed.");
                return;
            }
            KeyCode::Enter => {
                // In "which symbol?" mode the row picks a target for a
                // pending request rather than a place to go.
                let chosen = match &self.overlay {
                    Overlay::Finder(f) => f.rows.get(f.sel).and_then(|r| r.pick).and_then(|i| {
                        f.pending_action
                            .map(|action| (action, f.targets.get(i).cloned()))
                    }),
                    _ => None,
                };
                if let Some((action, target)) = chosen {
                    self.overlay = Overlay::None;
                    match target {
                        Some(t) => self.spawn_lsp(action, t),
                        None => self.err("That symbol is no longer available."),
                    }
                    return;
                }
                let pick = match &self.overlay {
                    Overlay::Finder(f) => f.rows.get(f.sel).map(|r| (r.path.clone(), r.line)),
                    _ => None,
                };
                match pick {
                    Some((path, line)) => self.open_hit(path, line),
                    None => self.err("No results — nothing to open."),
                }
                return;
            }
            _ => {}
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let mut changed = false;
        {
            let Overlay::Finder(f) = &mut self.overlay else {
                return;
            };
            let page = FINDER_ROWS as i32;
            match (key.code, ctrl) {
                (KeyCode::Up, _) | (KeyCode::Char('p'), true) => f.move_sel(-1),
                (KeyCode::Down, _) | (KeyCode::Char('n'), true) => f.move_sel(1),
                (KeyCode::PageUp, _) => f.move_sel(-page),
                (KeyCode::PageDown, _) => f.move_sel(page),
                (KeyCode::Left, _) => f.cursor = f.cursor.saturating_sub(1),
                (KeyCode::Right, _) => f.cursor = (f.cursor + 1).min(f.input.chars().count()),
                (KeyCode::Home, _) => f.cursor = 0,
                (KeyCode::End, _) => f.cursor = f.input.chars().count(),
                (KeyCode::Char('u'), true) => {
                    f.input.clear();
                    f.cursor = 0;
                    changed = true;
                }
                (KeyCode::Char('w'), true) => {
                    f.delete_word();
                    changed = true;
                }
                (KeyCode::Char('r'), true) => {
                    f.regex = !f.regex;
                    changed = true;
                }
                (KeyCode::Tab, _) => {
                    f.repo_scope = !f.repo_scope;
                    changed = true;
                }
                (KeyCode::Backspace, _) => {
                    // Rubbing out the prefix returns to plain file matching.
                    if f.input.is_empty() && f.mode != FinderMode::Files {
                        f.mode = FinderMode::Files;
                    } else {
                        f.backspace();
                    }
                    changed = true;
                }
                (KeyCode::Char(c), false) => {
                    // A prefix character only means "switch mode" as the
                    // very first thing typed — after that it's just text.
                    match c {
                        '#' if f.input.is_empty() => f.mode = FinderMode::Grep,
                        '@' if f.input.is_empty() => f.mode = FinderMode::Symbols,
                        _ => f.insert(c),
                    }
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            self.refresh_finder();
        }
    }

    fn finder_mouse(&mut self, m: MouseEvent, x: u16, y: u16) {
        match m.kind {
            MouseEventKind::ScrollDown => {
                if let Overlay::Finder(f) = &mut self.overlay {
                    f.move_sel(3);
                }
            }
            MouseEventKind::ScrollUp => {
                if let Overlay::Finder(f) = &mut self.overlay {
                    f.move_sel(-3);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => match self.layout.button_at(x, y) {
                Some(ButtonId::FinderRow(i)) => {
                    let pick = match &mut self.overlay {
                        Overlay::Finder(f) => {
                            f.sel = i.min(f.rows.len().saturating_sub(1));
                            f.rows.get(f.sel).map(|r| (r.path.clone(), r.line))
                        }
                        _ => None,
                    };
                    if let Some((path, line)) = pick {
                        self.open_hit(path, line);
                    }
                }
                Some(ButtonId::FinderMode(mode)) => {
                    if let Overlay::Finder(f) = &mut self.overlay {
                        f.mode = mode;
                    }
                    self.refresh_finder();
                }
                Some(ButtonId::FinderScope) => {
                    if let Overlay::Finder(f) = &mut self.overlay {
                        f.repo_scope = !f.repo_scope;
                    }
                    self.refresh_finder();
                }
                Some(ButtonId::FinderClose) => self.overlay = Overlay::None,
                _ => {}
            },
            _ => {}
        }
    }

    /// Go to a search result: the file if it's under review, the editor
    /// if it isn't.
    fn open_hit(&mut self, path: String, line: Option<usize>) {
        self.overlay = Overlay::None;
        // Already looking at it.
        if self.files.get(self.file_cursor).map(|f| f.path.as_str()) == Some(path.as_str())
            && self.diff.is_some()
        {
            if let Some(l) = line {
                self.jump_to_line(l);
            }
            return;
        }
        if let Some(idx) = self.files.iter().position(|f| f.path == path) {
            self.pending_jump = line;
            self.spawn_load_file(idx);
            return;
        }
        self.spawn_open_external(path, line);
    }

    /// Open a file that is not part of the changeset — the other half of
    /// a search that can reach the whole repository, and where a jump to a
    /// definition in untouched code lands.
    ///
    /// The working tree is preferred, because that is the copy an edit
    /// would change. When the branch under review isn't checked out the
    /// tree belongs to some other branch, so the commit's text is shown
    /// instead and the editor refuses to write it back.
    fn spawn_open_external(&mut self, path: String, line: Option<usize>) {
        if self.editor.is_some() {
            self.err("Close the editor first (Ctrl+S saves, Esc closes).");
            return;
        }
        // A preview holds nothing unsaved, so it simply gives way.
        self.preview = None;
        let root = self.repo_root.clone();
        let rev = self.search_rev();
        // In local review the working tree *is* what is under review.
        let tree_is_review = self.local || self.checked_out;
        let label = format!("Opening {path}");
        self.spawn(label, true, false, move || {
            let Some(abs_path) = gitops::safe_repo_path(&root, &path) else {
                anyhow::bail!("Refusing to open “{path}” — unsafe path.");
            };
            // Same rule as the diff editor: never open (and so never save)
            // through a symlink, which a PR could have planted.
            if is_symlink(&abs_path) {
                anyhow::bail!("“{path}” is a symlink — refusing to open through it.");
            }
            let on_disk = tree_is_review
                .then(|| std::fs::read_to_string(&abs_path).ok())
                .flatten();
            let (content, read_only) = match on_disk {
                Some(text) => (text, false),
                None => {
                    let from_commit = match &rev {
                        Some(rev) => gitops::show_file(rev, &path),
                        None => gitops::head_oid().and_then(|oid| gitops::show_file(&oid, &path)),
                    };
                    match from_commit {
                        Some(text) => (text, true),
                        None => anyhow::bail!("Cannot read {path}."),
                    }
                }
            };
            Ok(Outcome::ExternalOpened(Box::new(ExternalFile {
                preview: markdown::is_markdown(&path),
                path,
                abs_path,
                content,
                line,
                read_only,
            })))
        });
    }

    // ------------------------------------------------- the editor's server

    /// Ask the editor's language server something, off the UI thread.
    /// Any in-flight request is superseded — the newest question is the
    /// only one whose answer still matters.
    fn spawn_editor_request(&mut self, what: EditorRequest) {
        if !self.lsp_enabled {
            if what.is_explicit() {
                self.err("Language servers are off (`language_servers = false` in your config).");
            }
            return;
        }
        let Some(editor) = &self.editor else { return };
        if lsp::Lsp::supports(&editor.path).is_none() {
            if what.is_explicit() {
                self.err(format!(
                    "No language server for {} — loupe drives TypeScript, Go and Rust.",
                    editor.path
                ));
            }
            return;
        }
        let text = editor.content();
        let path = editor.path.clone();
        let at = editor.cursor_pos();
        let word = editor.word_at_cursor();
        let lsp = self.lsp.clone();
        let root = self.repo_root.clone();
        self.editor_gen += 1;
        let gen = self.editor_gen;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = match what {
                EditorRequest::Complete => lsp
                    .complete(&root, &path, &text, at)
                    .map(EditorOutcome::Completions),
                EditorRequest::Hover => lsp
                    .hover(&root, &path, &text, at)
                    .map(|text| EditorOutcome::Hover { word, text }),
                EditorRequest::Definition => lsp
                    .definition(&root, &path, &text, at)
                    .map(|locs| EditorOutcome::Definition { word, locs }),
                EditorRequest::Format => lsp
                    .format(&root, &path, &text, crate::diff::TAB_WIDTH)
                    .map(EditorOutcome::Formatted),
            };
            let _ = tx.send(result);
        });
        self.editor_job = Some(EditorJob { rx, gen });
    }

    fn apply_editor_outcome(&mut self, gen: u64, outcome: EditorOutcome) {
        // Typed past it: the answer is about text that no longer exists.
        if gen != self.editor_gen {
            return;
        }
        match outcome {
            EditorOutcome::Completions(items) => {
                let n = items.len();
                let Some(editor) = &mut self.editor else {
                    return;
                };
                if !editor.open_completion(items) && n > 0 {
                    // Everything was filtered out by what was typed while
                    // the request was in flight.
                    self.ok("No matching completions.");
                }
            }
            EditorOutcome::Hover { word, text } => match text {
                Some(text) => {
                    self.ok(format!("{word} — Esc closes."));
                    self.overlay = Overlay::Hover(Box::new(HoverPanel {
                        word,
                        lines: text.lines().map(str::to_string).collect(),
                    }));
                }
                None => self.err(format!("Nothing known about {word}.")),
            },
            EditorOutcome::Definition { word, locs } => {
                let Some(loc) = locs.into_iter().next() else {
                    self.err(format!("No definition found for {word}."));
                    return;
                };
                // Jumping inside the open file keeps the buffer (and any
                // unsaved edits); anywhere else has to leave it first.
                let same = self.editor.as_ref().is_some_and(|e| e.path == loc.path);
                if same {
                    if let Some(editor) = &mut self.editor {
                        editor.jump_to_line(loc.line);
                    }
                    self.ok(format!("{word} — line {}.", loc.line));
                    return;
                }
                if self.editor.as_ref().is_some_and(|e| e.dirty) {
                    self.err(format!(
                        "{word} is defined in {} — save (Ctrl+S) or discard first.",
                        loc.path
                    ));
                    return;
                }
                self.editor = None;
                self.open_hit(loc.path, Some(loc.line));
            }
            EditorOutcome::Formatted(edits) => {
                let Some(edits) = edits else {
                    self.err("This language server does not format.".to_string());
                    return;
                };
                let pending = std::mem::take(&mut self.format_then_save);
                let changed = self
                    .editor
                    .as_mut()
                    .map(|e| e.apply_edits(&edits))
                    .unwrap_or(false);
                if pending {
                    // Formatting was a step on the way to saving.
                    self.spawn_save_editor();
                } else if changed {
                    self.ok("Formatted — Ctrl+Z puts it back.");
                } else {
                    self.ok("Already formatted.");
                }
            }
        }
    }

    // ------------------------------------------------------------ clipboard

    /// The lines a copy would take: the selection if there is one,
    /// otherwise the cursor row. Returns the text and a description of
    /// what it was, for the status line.
    fn copy_target(&self) -> Option<(String, String)> {
        // Nothing selected: the cursor line, whole.
        let sel = match self.selection {
            Some(sel) => sel,
            None => {
                let (side, line) = self.cursor_pos()?;
                Selection::lines(side, line, line)
            }
        };
        // The text comes from the side the selection is on — which is what
        // makes copying *deleted* code work at all: the old side's text is
        // still here, and it is what gets copied.
        let content = self.side_content(sel.side)?;
        let text = sel.text(content);
        if text.is_empty() {
            return None;
        }
        let (lo, hi) = sel.range();
        let which = if sel.side == Side::Left {
            "removed"
        } else {
            "new"
        };
        let what = if !sel.linewise {
            let chars = text.chars().filter(|c| *c != '\n').count();
            format!("{chars} characters ({which} side)")
        } else if lo == hi {
            format!("line {lo} ({which} side)")
        } else {
            format!("{} lines ({which} side)", hi + 1 - lo)
        };
        Some((text, what))
    }

    /// Right-click on the PR badge: put the PR link on the clipboard.
    /// A reviewer who spots something here usually hands the PR to a
    /// coding agent next, and that agent wants the URL.
    pub fn copy_pr_link(&mut self) {
        let Some(url) = self.pr_url() else {
            self.err("No PR link here — this is a local-changes review.");
            return;
        };
        match clipboard::copy(&url) {
            Ok(via) => self.ok(format!("⧉ Copied {url} to the clipboard via {via}.")),
            Err(e) => self.err(format!("Couldn't copy: {e:#}")),
        }
    }

    /// The web URL of the PR under review, if a PR is under review.
    /// `gh pr view` reports it, which keeps GitHub Enterprise hosts
    /// correct; the owner/name form is the fallback for a PR opened
    /// before that field existed.
    pub fn pr_url(&self) -> Option<String> {
        let pr = self.pr.as_ref()?;
        if !pr.url.is_empty() {
            return Some(pr.url.clone());
        }
        let repo = self.repo.as_ref()?;
        Some(format!("https://github.com/{repo}/pull/{}", pr.number))
    }

    /// `y` / Ctrl+C / the ⧉ Copy button.
    pub fn yank(&mut self) {
        let Some((text, what)) = self.copy_target() else {
            self.err("Nothing to copy — click or drag over the lines you want, or press V.");
            return;
        };
        match clipboard::copy(&text) {
            Ok(via) => self.ok(format!("⧉ Copied {what} to the clipboard via {via}.")),
            Err(e) => self.err(format!("Couldn't copy: {e:#}")),
        }
    }

    // ------------------------------------------------------ language servers

    /// The full text of the file on one side of the diff — what gets
    /// handed to the language server, so it analyses what is on screen
    /// rather than what happens to be on disk.
    fn side_content(&self, side: Side) -> Option<&String> {
        match side {
            Side::Right => self.new_content.as_ref(),
            Side::Left => self.old_content.as_ref(),
        }
    }

    /// Symbols on the cursor row that could be asked about. A click on a
    /// word wins outright — that is an unambiguous "this one".
    fn targets_on_cursor_row(&self) -> Vec<Target> {
        let Some(file) = self.files.get(self.file_cursor) else {
            return Vec::new();
        };
        let Some((side, line)) = self.cursor_pos() else {
            return Vec::new();
        };
        let Some(content) = self.side_content(side) else {
            return Vec::new();
        };
        let Some(text) = content.lines().nth(line.saturating_sub(1)) else {
            return Vec::new();
        };
        let make = |col: usize, word: String| Target {
            path: file.path.clone(),
            line,
            col: col + 1,
            word,
            side,
        };
        // A word the mouse landed on, if the click was on this very row.
        if let Some((idx, char_col)) = self.click_word {
            if idx == self.diff_cursor {
                if let Some((start, word)) = search::word_at(text, char_col) {
                    return vec![make(start, word)];
                }
            }
        }
        search::identifiers(text)
            .into_iter()
            .map(|(col, word)| make(col, word))
            .collect()
    }

    /// `gd` / `gr` / `K`. One symbol on the line goes straight through;
    /// several open the picker rather than guessing.
    pub fn lsp_action(&mut self, action: LspAction) {
        if !self.lsp_enabled {
            self.err("Language servers are off (`language_servers = false` in your config).");
            return;
        }
        let targets = self.targets_on_cursor_row();
        match targets.len() {
            0 => self.err(format!(
                "No symbol on this line to find the {}.",
                action.verb()
            )),
            1 => self.spawn_lsp(action, targets[0].clone()),
            _ => self.open_pick(action, targets),
        }
    }

    /// Ask which symbol on the line was meant, then do the thing.
    fn open_pick(&mut self, action: LspAction, targets: Vec<Target>) {
        let (symbols, symbol_path) = self.current_symbols();
        let mut finder = Finder::new(
            FinderMode::Pick,
            self.changeset_paths(),
            symbols,
            symbol_path,
        );
        finder.preset = targets
            .iter()
            .enumerate()
            .map(|(i, t)| FinderRow {
                path: t.path.clone(),
                line: None,
                text: t.word.clone(),
                matched: Vec::new(),
                range: None,
                tag: "",
                in_changeset: true,
                pick: Some(i),
            })
            .collect();
        finder.targets = targets;
        finder.pending_action = Some(action);
        finder.mode = FinderMode::Pick;
        self.overlay = Overlay::Finder(Box::new(finder));
        self.refresh_finder();
        self.ok(format!(
            "Several symbols on this line — pick the one to find the {}.",
            action.verb()
        ));
    }

    fn spawn_lsp(&mut self, action: LspAction, target: Target) {
        let Some(text) = self.side_content(target.side).cloned() else {
            self.err("This side of the diff has no content to analyse.");
            return;
        };
        if lsp::Lsp::supports(&target.path).is_none() {
            self.err(format!(
                "No language server for {} — {} works in TypeScript, Go and Rust.",
                target.path,
                match action {
                    LspAction::Hover => "hover",
                    LspAction::Definition => "go to definition",
                    LspAction::References => "find references",
                }
            ));
            return;
        }
        let lsp = self.lsp.clone();
        let root = self.repo_root.clone();
        let rev = self.search_rev();
        let at = (target.line, target.col);
        let word = target.word.clone();
        let path = target.path.clone();
        let label = format!("Finding the {} {}", action.verb(), word);
        self.spawn(label, true, false, move || match action {
            LspAction::Hover => {
                let text = lsp.hover(&root, &path, &text, at)?;
                Ok(Outcome::Hover(Box::new(HoverData { word, text })))
            }
            LspAction::Definition | LspAction::References => {
                let locs = if action == LspAction::Definition {
                    lsp.definition(&root, &path, &text, at)?
                } else {
                    lsp.references(&root, &path, &text, at)?
                };
                // The positions alone would make an unreadable list; read
                // the line each one points at, once per file.
                let mut cache: HashMap<String, Option<String>> = HashMap::new();
                let places = locs
                    .into_iter()
                    .take(search::RESULT_LIMIT)
                    .map(|loc| {
                        let content = cache
                            .entry(loc.path.clone())
                            .or_insert_with(|| read_source(&root, rev.as_deref(), &loc.path));
                        let line = content
                            .as_ref()
                            .and_then(|c| c.lines().nth(loc.line.saturating_sub(1)))
                            .unwrap_or("");
                        // The server counts columns in UTF-16 units; the
                        // rest of loupe counts chars.
                        let loc = lsp::Loc {
                            col: lsp::char_column(line, loc.col.saturating_sub(1)) + 1,
                            ..loc
                        };
                        Place {
                            loc,
                            text: line.trim_end().to_string(),
                        }
                    })
                    .collect();
                Ok(Outcome::Locations(Box::new(LocationsData {
                    action,
                    word,
                    places,
                })))
            }
        });
    }

    /// Ask the language server for this file's symbols, replacing the
    /// pattern-matched ones in the open finder when the answer lands.
    fn spawn_lsp_symbols(&mut self) {
        if !self.lsp_enabled {
            return;
        }
        let Some(file) = self.files.get(self.file_cursor).cloned() else {
            return;
        };
        let Some(spec) = lsp::Lsp::supports(&file.path) else {
            return;
        };
        // Don't start a job that can only end in "not installed".
        if lsp::which(spec.cmd).is_none() {
            if let Overlay::Finder(f) = &mut self.overlay {
                f.note = format!("{} · pattern matching ({} not installed)", f.note, spec.cmd);
            }
            return;
        }
        let Some(text) = self
            .new_content
            .clone()
            .or_else(|| self.old_content.clone())
        else {
            return;
        };
        self.search_gen += 1;
        let gen = self.search_gen;
        let lsp = self.lsp.clone();
        let root = self.repo_root.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(
                lsp.symbols(&root, &file.path, &text)
                    .map(SearchOutcome::Symbols),
            );
        });
        self.search_job = Some(SearchJob {
            rx,
            gen,
            started: Instant::now(),
        });
    }

    /// Show the answer to `gd` / `gr`.
    fn apply_locations(&mut self, d: LocationsData) {
        let n = d.places.len();
        if n == 0 {
            self.err(format!(
                "No {} {} found. If the server just started it may still be indexing — try again.",
                d.action.verb(),
                d.word
            ));
            return;
        }
        // One answer is not a list: go there.
        if n == 1 && d.action == LspAction::Definition {
            let place = &d.places[0];
            let (path, line) = (place.loc.path.clone(), place.loc.line);
            self.open_hit(path, Some(line));
            return;
        }
        let changeset = self.changeset_paths();
        let (symbols, symbol_path) = self.current_symbols();
        let mut finder = Finder::new(FinderMode::Refs, changeset.clone(), symbols, symbol_path);
        finder.preset = d
            .places
            .iter()
            .map(|p| {
                let in_changeset = changeset.contains(&p.loc.path);
                FinderRow {
                    path: p.loc.path.clone(),
                    line: Some(p.loc.line),
                    text: p.text.clone(),
                    matched: Vec::new(),
                    range: search::find_ranges(&p.text, &d.word).first().copied(),
                    tag: if in_changeset { "changed" } else { "" },
                    in_changeset,
                    pick: None,
                }
            })
            .collect();
        finder.note = format!(
            "{n} {} {} · Enter opens · type to filter",
            if n == 1 { "place uses" } else { "places use" },
            d.word
        );
        self.overlay = Overlay::Finder(Box::new(finder));
        self.refresh_finder();
        // rebuild() overwrites the note only when a filter is typed.
        if let Overlay::Finder(f) = &mut self.overlay {
            f.note = format!(
                "{n} {} “{}” · Enter opens · type to filter",
                if d.action == LspAction::Definition {
                    "definitions of"
                } else if n == 1 {
                    "reference to"
                } else {
                    "references to"
                },
                d.word
            );
        }
        self.ok(format!("{n} results for {}.", d.word));
    }

    // ------------------------------------------------------- jumping to a line

    /// Display row showing diff row `row`, if it is currently visible.
    fn display_index_of_row(&self, row: usize) -> Option<usize> {
        match self.view {
            ViewMode::SideBySide => self
                .display
                .iter()
                .position(|e| matches!(e, DisplayEntry::Line(i) if *i == row)),
            ViewMode::Inline => {
                let diff = self.diff.as_ref()?;
                // Prefer the new side of a modified row — that's the one a
                // search result's line number refers to.
                let entry = diff
                    .inline
                    .iter()
                    .position(|e| e.row == row && e.side == Side::Right)
                    .or_else(|| diff.inline.iter().position(|e| e.row == row))?;
                self.display
                    .iter()
                    .position(|e| matches!(e, DisplayEntry::Line(i) if *i == entry))
            }
        }
    }

    /// Put the cursor on a diff row, expanding the fold hiding it first.
    /// Landing *inside* a collapsed section is the whole reason this isn't
    /// a plain `cursor_to`.
    fn reveal_row(&mut self, row: usize) {
        if self.display_index_of_row(row).is_none() {
            if let Some(start) = self.diff.as_ref().and_then(|d| d.fold_start_for(row)) {
                self.expanded_folds.insert(start);
                self.rebuild_display();
            }
        }
        let Some(idx) = self.display_index_of_row(row) else {
            return;
        };
        self.diff_cursor = idx;
        self.center_cursor();
        self.extend_selection();
    }

    /// Scroll so the cursor sits a third of the way down — near enough to
    /// the middle to show context, high enough to read forward.
    fn center_cursor(&mut self) {
        let h = self.diff_page();
        let max = self.display.len().saturating_sub(h);
        self.diff_scroll = self.diff_cursor.saturating_sub(h / 3).min(max);
        self.ensure_cursor_visible();
    }

    /// Jump to a file line number (new side first, then old).
    pub fn jump_to_line(&mut self, line: usize) {
        let row = {
            let Some(diff) = &self.diff else { return };
            diff.rows
                .iter()
                .position(|r| r.new_ln == Some(line))
                .or_else(|| diff.rows.iter().position(|r| r.old_ln == Some(line)))
        };
        match row {
            Some(row) => self.reveal_row(row),
            None => self.err(format!(
                "Line {line} isn't in this view — it may only exist on the other side of the diff."
            )),
        }
    }

    // -------------------------------------------------- in-diff search (`/`)

    pub fn start_find(&mut self) {
        if self.diff.is_none() {
            self.err("Open a file first — / searches the diff you're reading.");
            return;
        }
        self.find.typing = true;
        self.find.origin = (self.diff_scroll, self.diff_cursor);
        self.find.query.clear();
        self.find.rows.clear();
        self.ok("/");
    }

    /// Keystrokes while the `/` prompt is open.
    fn find_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                let (scroll, cursor) = self.find.origin;
                self.find.typing = false;
                self.find.query.clear();
                self.find.rows.clear();
                self.diff_scroll = scroll;
                self.diff_cursor = cursor;
                self.ok("Search cancelled.");
            }
            KeyCode::Enter => {
                self.find.typing = false;
                if self.find.query.is_empty() {
                    self.ok("Search cleared.");
                } else if self.find.rows.is_empty() {
                    self.err(format!(
                        "No match for “{}” in this file — # searches every file.",
                        self.find.query
                    ));
                } else {
                    let n = self.find.rows.len();
                    self.ok(format!(
                        "{n} match{} — n / N step through them, Esc clears.",
                        if n == 1 { "" } else { "es" }
                    ));
                }
            }
            KeyCode::Backspace => {
                self.find.query.pop();
                self.refresh_find();
            }
            KeyCode::Char(c) => {
                self.find.query.push(c);
                self.refresh_find();
            }
            _ => {}
        }
    }

    /// Re-scan for matches and jump to the first one at or after where the
    /// search started, so the view follows the query as it's typed.
    fn refresh_find(&mut self) {
        self.recompute_matches();
        let (_, origin_cursor) = self.find.origin;
        let from = self
            .display
            .get(origin_cursor)
            .map(|e| self.entry_row(*e))
            .unwrap_or(0);
        let next =
            self.find
                .rows
                .iter()
                .position(|r| *r >= from)
                .or(if self.find.rows.is_empty() {
                    None
                } else {
                    Some(0)
                });
        match next {
            Some(i) => {
                self.find.at = i;
                let row = self.find.rows[i];
                self.reveal_row(row);
            }
            None => {
                let (scroll, cursor) = self.find.origin;
                self.diff_scroll = scroll;
                self.diff_cursor = cursor;
            }
        }
    }

    /// Rows of the open diff containing the search text. Recomputed
    /// whenever the query or the file changes — never per frame.
    pub fn recompute_matches(&mut self) {
        self.find.rows.clear();
        self.find.at = 0;
        if self.find.query.is_empty() {
            return;
        }
        let query = self.find.query.clone();
        let Some(diff) = &self.diff else { return };
        let mut rows = Vec::new();
        for (i, row) in diff.rows.iter().enumerate() {
            let hit = [row.old_text.as_deref(), row.new_text.as_deref()]
                .into_iter()
                .flatten()
                .any(|t| !search::find_ranges(t, &query).is_empty());
            if hit {
                rows.push(i);
            }
        }
        self.find.rows = rows;
    }

    /// `n` / `N`: step to the next or previous match, wrapping.
    pub fn goto_match(&mut self, forward: bool) {
        if !self.find.active() {
            self.err("No search yet — press / to search this file, # to search every file.");
            return;
        }
        if self.find.rows.is_empty() {
            self.err(format!("No match for “{}” in this file.", self.find.query));
            return;
        }
        let cur = self
            .display
            .get(self.diff_cursor)
            .map(|e| self.entry_row(*e))
            .unwrap_or(0);
        let (idx, wrapped) = if forward {
            match self.find.rows.iter().position(|r| *r > cur) {
                Some(i) => (i, false),
                None => (0, true),
            }
        } else {
            match self.find.rows.iter().rposition(|r| *r < cur) {
                Some(i) => (i, false),
                None => (self.find.rows.len() - 1, true),
            }
        };
        self.find.at = idx;
        let row = self.find.rows[idx];
        self.reveal_row(row);
        self.ok(format!(
            "Match {} of {}{}",
            idx + 1,
            self.find.rows.len(),
            if wrapped { " (wrapped)" } else { "" }
        ));
    }

    fn clear_find(&mut self) {
        self.find.query.clear();
        self.find.rows.clear();
        self.find.at = 0;
        self.ok("Search cleared.");
    }

    pub fn open_comment(&mut self) {
        if self.local {
            self.err("Reviewing local changes — commenting needs an open pull request (b for the PR list).");
            return;
        }
        // No explicit selection: comment on the row the cursor is on.
        let sel = match self.selection {
            Some(sel) => sel,
            None => match self.cursor_pos() {
                Some((side, line)) => Selection::lines(side, line, line),
                None => {
                    self.err("Put the cursor on a line first — click it, or move with j/k.");
                    return;
                }
            },
        };
        if self.differs_from_head {
            self.err(
                "Local file differs from the PR head on GitHub — comments would land on the wrong lines. Commit & push your edits (or press r to reload) first.",
            );
            return;
        }
        let Some(file) = self.files.get(self.file_cursor) else {
            return;
        };
        let (lo, hi) = sel.range();
        let mut textarea = TextArea::default();
        textarea.set_placeholder_text("Write your review comment (Markdown supported)…");
        self.overlay = Overlay::Comment(Box::new(CommentDraft {
            textarea,
            path: file.path.clone(),
            side: sel.side,
            lo,
            hi,
        }));
    }

    // ------------------------------------------------ the markdown preview

    /// `P`, the 📖 button and the ☰ line: move between the rendered
    /// document and the text it came from.
    ///
    /// Which way it goes depends on what is open. From the diff or the
    /// editor it renders; from the preview it opens the source in the
    /// editor, on the line that was at the top of the pane. Nothing is
    /// lost either way — the source view is the editor, so an edit made
    /// there and a toggle back shows the change immediately, saved or not.
    pub fn toggle_preview(&mut self) {
        if self.preview.is_some() {
            self.preview_to_source();
        } else if self.editor.is_some() {
            self.source_to_preview();
        } else {
            self.preview_current_file();
        }
    }

    /// Render the changeset file under the file cursor.
    fn preview_current_file(&mut self) {
        let Some(file) = self.files.get(self.file_cursor).cloned() else {
            self.err("No file to preview.");
            return;
        };
        if !markdown::is_markdown(&file.path) {
            self.err(format!(
                "{} is not markdown — P previews .md files (Ctrl+P finds one).",
                file.path
            ));
            return;
        }
        if file.status == "removed" {
            self.err("This file is deleted in the change — there is nothing to render.");
            return;
        }
        let Some(abs_path) = gitops::safe_repo_path(&self.repo_root, &file.path) else {
            self.err(format!("Refusing to open “{}” — unsafe path.", file.path));
            return;
        };
        // The new side is already in memory from the diff load; disk is
        // only consulted when it is not (a file loaded before the diff).
        let content = match &self.new_content {
            Some(c) => c.clone(),
            None => match std::fs::read_to_string(&abs_path) {
                Ok(c) => c,
                Err(e) => {
                    self.err(format!("Cannot read {}: {e}", file.path));
                    return;
                }
            },
        };
        let mut pv = Preview::new(&file.path, abs_path.clone(), &content);
        pv.mtime = preview::mtime_of(&abs_path);
        // Land on the line the diff cursor is on, so previewing a hunk
        // shows the part of the document that hunk changed.
        if let Some((Side::Right, n)) = self.cursor_pos() {
            pv.go_to_source(n);
        }
        self.preview = Some(pv);
        self.ok(format!(
            "📖 {} — P shows the source, e edits it, Esc goes back to the diff.",
            file.path
        ));
    }

    /// Render whatever the editor currently holds, saved or not.
    fn source_to_preview(&mut self) {
        let Some(ed) = &self.editor else { return };
        if !markdown::is_markdown(&ed.path) {
            self.err(format!("{} is not markdown — nothing to preview.", ed.path));
            return;
        }
        let path = ed.path.clone();
        let abs_path = ed.abs_path.clone();
        let standalone = ed.standalone;
        let dirty = ed.dirty;
        let line = ed.cursor_pos().0;
        let content = ed.content();
        if dirty {
            // The buffer is what the reader wants to see; the file on disk
            // is a different document until they save.
            self.ok("📖 Previewing unsaved text — Ctrl+S in the source view writes it.");
        } else {
            self.ok(format!("📖 {path} — P goes back to the source."));
        }
        self.editor = None;
        let mut pv = Preview::new(&path, abs_path.clone(), &content);
        pv.standalone = standalone;
        pv.from_editor = true;
        pv.from_buffer = dirty;
        pv.mtime = preview::mtime_of(&abs_path);
        pv.go_to_source(line);
        self.preview = Some(pv);
        // The blame pane cannot follow a rendered document: one source
        // line is any number of rows, or none. It comes back with the
        // source view.
        self.blame_new = None;
        self.blame_old = None;
    }

    /// Open the source of what the preview is showing, in the editor.
    fn preview_to_source(&mut self) {
        let Some(pv) = self.preview.take() else {
            return;
        };
        let line = pv.source_line();
        // A file in the changeset goes through the same door the diff's
        // `e` key uses, so the checkout and symlink guards still apply.
        if !pv.standalone && !self.preview_only {
            self.open_editor(Some(line));
            if self.editor.is_none() {
                // A guard refused: put the reader back where they were
                // rather than into an empty pane.
                self.preview = Some(pv);
            }
            return;
        }
        let mut ed = Editor::new(&pv.path, pv.abs_path.clone(), &pv.src);
        ed.standalone = true;
        ed.dirty = pv.from_buffer;
        ed.jump_to_line(line);
        self.editor = Some(ed);
        if !self.preview_only {
            self.spawn_blame_external(pv.path.clone(), false);
        }
        self.ok(format!(
            "✎ {} — Ctrl+S saves, P shows the preview again.",
            pv.path
        ));
    }

    /// `r` in the preview: re-read the file now, without waiting for the
    /// idle check to notice it moved.
    pub fn reload_preview(&mut self) {
        let Some(pv) = &self.preview else { return };
        let abs = pv.abs_path.clone();
        let path = pv.path.clone();
        match std::fs::read_to_string(&abs) {
            Ok(text) => {
                let mtime = preview::mtime_of(&abs);
                if let Some(pv) = &mut self.preview {
                    pv.reload(&text);
                    pv.mtime = mtime;
                    pv.from_buffer = false;
                }
                self.ok(format!("📖 {path} reloaded."));
            }
            Err(e) => self.err(format!("Cannot re-read {path}: {e}")),
        }
    }

    /// Esc / ✕: leave the preview. In `loupe md` there is nothing behind
    /// it, so that is the way out of loupe.
    pub fn close_preview(&mut self) {
        if self.preview_only {
            self.should_quit = true;
            return;
        }
        let was = self.preview.take();
        if was.is_some() {
            // The blame pane goes back to the file under review.
            self.spawn_blame(self.file_cursor);
            self.ok("Back to the diff.");
        }
    }

    /// True when the pane is showing a rendered document.
    pub fn previewing(&self) -> bool {
        self.preview.is_some()
    }

    /// Whether a 📖 toggle makes sense right now — the file in front of
    /// the reader is markdown.
    pub fn can_preview(&self) -> bool {
        if self.preview.is_some() {
            return true;
        }
        match &self.editor {
            Some(ed) => markdown::is_markdown(&ed.path),
            None => self
                .files
                .get(self.file_cursor)
                .is_some_and(|f| markdown::is_markdown(&f.path) && f.status != "removed"),
        }
    }

    /// Re-read the previewed file when something else rewrites it — the
    /// case this pane exists for, since the plan file being read is often
    /// one an agent is still writing.
    fn poll_preview_reload(&mut self) -> bool {
        let Some(pv) = &self.preview else {
            return false;
        };
        // Unsaved editor text is the reader's own; disk does not win.
        if pv.from_buffer {
            return false;
        }
        let now = preview::mtime_of(&pv.abs_path);
        if now.is_none() || now == pv.mtime {
            return false;
        }
        let Ok(text) = std::fs::read_to_string(&pv.abs_path) else {
            return false;
        };
        let Some(pv) = &mut self.preview else {
            return false;
        };
        if text == pv.src {
            // The timestamp moved but the bytes did not (a rewrite with
            // the same content). Adopt the new time and stay quiet.
            pv.mtime = now;
            return false;
        }
        pv.mtime = now;
        pv.reload(&text);
        let path = pv.path.clone();
        self.ok(format!("📖 {path} changed on disk — reloaded."));
        true
    }

    /// Open a markdown file by path, with no review behind it
    /// (`loupe md <path>`). The path may be anywhere on this machine.
    pub fn start_preview_only(&mut self, path: &std::path::Path) {
        self.preview_only = true;
        self.screen = Screen::Review;
        let abs_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let content = match std::fs::read_to_string(&abs_path) {
            Ok(c) => c,
            Err(e) => {
                self.err(format!("Cannot read {}: {e}", path.display()));
                String::new()
            }
        };
        // The whole path is the name here: there is no repository to show
        // it relative to.
        let name = abs_path.to_string_lossy().to_string();
        let mut pv = Preview::new(&name, abs_path.clone(), &content);
        pv.standalone = true;
        pv.mtime = preview::mtime_of(&abs_path);
        self.preview = Some(pv);
        if !self.status_err {
            self.ok("📖 P shows the source · Ctrl+S saves · q quits.");
        }
    }

    pub fn open_editor(&mut self, jump_line: Option<usize>) {
        if !self.checked_out {
            self.err("Editing needs the PR branch checked out — reopen the PR and pick “Checkout & review”.");
            return;
        }
        let Some(file) = self.files.get(self.file_cursor) else {
            return;
        };
        if file.status == "removed" {
            self.err("This file is deleted in the PR — nothing to edit.");
            return;
        }
        // Read fresh from disk so external edits are picked up. The PR data
        // is untrusted: refuse paths that would escape the repository, and
        // refuse to edit through a symlink (a PR can add one pointing at any
        // file on this machine — saving would overwrite its target).
        let Some(abs_path) = gitops::safe_repo_path(&self.repo_root, &file.path) else {
            self.err(format!("Refusing to open “{}” — unsafe path.", file.path));
            return;
        };
        if is_symlink(&abs_path) {
            self.err(format!(
                "“{}” is a symlink — refusing to edit through it.",
                file.path
            ));
            return;
        }
        let content = match std::fs::read_to_string(&abs_path) {
            Ok(c) => c,
            Err(_) => match &self.new_content {
                Some(c) => c.clone(),
                None => {
                    self.err(format!("Cannot read {} from the working tree.", file.path));
                    return;
                }
            },
        };
        let mut editor = Editor::new(&file.path, abs_path, &content);
        let target = jump_line.or_else(|| {
            self.selection
                .filter(|s| s.side == Side::Right)
                .map(|s| s.range().0)
        });
        if let Some(line) = target {
            editor.jump_to_line(line);
        }
        self.editor = Some(editor);
        self.ok("Editing new side — Ctrl+S saves to your working tree and refreshes the diff.");
    }

    pub fn request_close_editor(&mut self) {
        if let Some(ed) = &mut self.editor {
            if ed.dirty && !ed.discard_armed {
                ed.discard_armed = true;
                self.err(
                    "Unsaved changes — click ✕ / press Esc again to discard, or Ctrl+S to save.",
                );
                return;
            }
        }
        self.close_editor();
    }

    fn close_editor(&mut self) {
        let standalone = self.editor.as_ref().is_some_and(|e| e.standalone);
        self.editor = None;
        if standalone {
            // The review was never disturbed — there is nothing to reload,
            // and reloading would throw away the reader's place. The pane
            // does have to go back to the file under review, though.
            self.spawn_blame(self.file_cursor);
            self.ok("Back to the diff.");
            return;
        }
        // Reload the file so the diff reflects whatever is on disk now.
        self.spawn_load_file(self.file_cursor);
    }

    // -------------------------------------------------- PR ⇄ local toggle

    /// The ` key / ⇄ button: flip between the PR review and the local
    /// uncommitted-changes review without losing either. The side swapped
    /// to is shown instantly from the stash and then silently re-checked in
    /// the background (fresh commits on the PR, fresh edits locally), so
    /// there is never a loading screen on the way back.
    pub fn toggle_workspace(&mut self) {
        if self.editor.is_some() {
            self.err("Close the editor first (Ctrl+S saves, Esc closes) before swapping views.");
            return;
        }
        // Any in-flight silent refresh belonged to the side being left.
        self.quiet = None;
        self.pending_quiet = None;
        match self.stash.take() {
            Some(ws) if ws.local != self.local => {
                let cur = self.save_workspace();
                self.stash = Some(Box::new(cur));
                let to_local = ws.local;
                self.restore_workspace(*ws);
                if to_local {
                    self.ok("⎇ Local changes — rescanning in the background.");
                    self.spawn_quiet_local(false);
                } else {
                    let n = self.pr.as_ref().map(|p| p.number).unwrap_or(0);
                    self.ok(format!("PR #{n} — checking GitHub for updates."));
                    self.spawn_quiet_pr(false);
                }
            }
            stash => {
                // Nothing stashed for the other side yet (a same-side stash
                // would be a bug; leave it be) — load it the long way once.
                self.stash = stash;
                if self.local {
                    let org = self.org.clone();
                    self.spawn("Finding a pull request to swap to", true, true, move || {
                        Ok(Outcome::BranchPr(github::pr_for_current_branch(
                            org.as_deref(),
                        )))
                    });
                } else {
                    self.spawn_open_local(true);
                }
            }
        }
    }

    /// Move every side-specific field out of the app (leaving defaults
    /// behind — the caller overwrites them right after).
    fn save_workspace(&mut self) -> Workspace {
        Workspace {
            local: self.local,
            local_branch: self.local_branch.take(),
            pr: self.pr.take(),
            checked_out: self.checked_out,
            merge_base: std::mem::take(&mut self.merge_base),
            files: std::mem::take(&mut self.files),
            viewed: std::mem::take(&mut self.viewed),
            stage: std::mem::take(&mut self.stage),
            file_cursor: self.file_cursor,
            file_scroll: self.file_scroll,
            collapsed_dirs: std::mem::take(&mut self.collapsed_dirs),
            diff: self.diff.take(),
            collapse_unchanged: self.collapse_unchanged,
            expanded_folds: std::mem::take(&mut self.expanded_folds),
            old_content: self.old_content.take(),
            new_content: self.new_content.take(),
            old_hl: std::mem::take(&mut self.old_hl),
            new_hl: std::mem::take(&mut self.new_hl),
            differs_from_head: self.differs_from_head,
            diff_scroll: self.diff_scroll,
            diff_cursor: self.diff_cursor,
            diff_hscroll: self.diff_hscroll,
            selection: self.selection.take(),
        }
    }

    fn restore_workspace(&mut self, ws: Workspace) {
        self.local = ws.local;
        self.local_branch = ws.local_branch;
        self.pr = ws.pr;
        self.checked_out = ws.checked_out;
        self.merge_base = ws.merge_base;
        self.reset_blame_review();
        self.files = ws.files;
        self.viewed = ws.viewed;
        self.stage = ws.stage;
        self.file_cursor = ws.file_cursor;
        self.file_scroll = ws.file_scroll;
        self.collapsed_dirs = ws.collapsed_dirs;
        self.diff = ws.diff;
        self.collapse_unchanged = ws.collapse_unchanged;
        self.expanded_folds = ws.expanded_folds;
        self.old_content = ws.old_content;
        self.new_content = ws.new_content;
        self.old_hl = ws.old_hl;
        self.new_hl = ws.new_hl;
        self.differs_from_head = ws.differs_from_head;
        self.selection = ws.selection;
        self.select_mode = false;
        self.drag_select = false;
        self.pending_g = false;
        self.editor = None;
        self.screen = Screen::Review;
        self.rebuild_entries();
        self.rebuild_display();
        // The view mode is shared between the sides, so a stored position
        // can be out of range if it changed while stashed — clamp.
        let last = self.display.len().saturating_sub(1);
        self.diff_cursor = ws.diff_cursor.min(last);
        self.diff_scroll = ws.diff_scroll.min(last);
        self.diff_hscroll = ws.diff_hscroll.min(self.max_hscroll());
        self.ensure_file_visible();
    }

    /// ⟳ in the review top bar, and the `r` key: re-read everything the
    /// review is built from, without a loading screen.
    ///
    /// It re-scans the changed-file list and then reloads the open file
    /// through the same silent path the swap uses, so the scroll position,
    /// the cursor and the folds all survive. Use it when an agent (or a
    /// second terminal) has been writing to the tree and the idle re-scan
    /// has not caught up yet.
    pub fn refresh_review(&mut self) {
        if self.editor.is_some() {
            self.err("Close the editor first (Ctrl+S saves, Esc closes) before refreshing.");
            return;
        }
        if self.quiet.is_some() {
            self.ok("Already refreshing…");
            return;
        }
        // A manual refresh restarts the idle clock: the tree was just read.
        self.last_auto_rescan = Instant::now();
        if self.local {
            self.ok("⟳ Rescanning local changes…");
            self.spawn_quiet_local(false);
        } else {
            self.ok("⟳ Checking GitHub for updates…");
            self.spawn_quiet_pr(false);
        }
    }

    /// Re-fetch the open PR's metadata and file list without blocking.
    fn spawn_quiet_pr(&mut self, auto: bool) {
        let (Some(repo), Some(pr)) = (self.repo.clone(), self.pr.as_ref()) else {
            return;
        };
        let number = pr.number;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send((|| {
                let detail = github::pr_detail(&repo, number)?;
                let source = gitops::fetch_source(&repo);
                let _ = gitops::fetch_pr(&source, &detail.base_ref_name, number);
                let merge_base = gitops::merge_base(&detail.base_ref_oid, &detail.head_ref_oid);
                let files = github::changed_files(&repo, number)?;
                let viewed = github::viewed_files(&repo, number).unwrap_or_default();
                Ok(QuietOutcome::Pr(Box::new(PrRefreshData {
                    detail,
                    merge_base,
                    files,
                    viewed,
                })))
            })());
        });
        self.quiet = Some(QuietJob {
            rx,
            label: format!("Refreshing PR #{number}"),
            started: Instant::now(),
            auto,
        });
    }

    /// Re-scan the working tree for uncommitted changes without blocking.
    fn spawn_quiet_local(&mut self, auto: bool) {
        let root = self.repo_root.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send((|| {
                let files = gitops::local_changes(&root)?;
                Ok(QuietOutcome::Local(Box::new(LocalOpenedData {
                    branch: gitops::current_branch(),
                    head: gitops::head_oid(),
                    files,
                    stage: gitops::stage_states(&root).unwrap_or_default(),
                })))
            })());
        });
        self.quiet = Some(QuietJob {
            rx,
            label: "Rescanning local changes".into(),
            started: Instant::now(),
            auto,
        });
    }

    /// Reload one file in place — same work as [`Self::spawn_load_file`],
    /// but non-modal, and applied without touching the scroll position.
    fn spawn_quiet_file(&mut self, idx: usize, auto: bool) {
        let Some(file) = self.files.get(idx).cloned() else {
            return;
        };
        let label = format!("Refreshing {}", file.path);
        let ctx = self.file_load_ctx();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ =
                tx.send(load_file_data(idx, file, ctx).map(|d| QuietOutcome::File(Box::new(d))));
        });
        self.quiet = Some(QuietJob {
            rx,
            label,
            started: Instant::now(),
            auto,
        });
    }

    /// Keep the cursor on the same file across a list refresh, falling back
    /// to the top when it disappeared (committed away, dropped from the PR).
    fn retarget_file_cursor(&mut self, path: Option<String>) {
        self.file_cursor = path
            .and_then(|p| self.files.iter().position(|f| f.path == p))
            .unwrap_or(0)
            .min(self.files.len().saturating_sub(1));
    }

    /// Apply a finished silent refresh. Each arm re-checks that the app is
    /// still looking at what the refresh was for — a toggle or a new open
    /// since it started just drops the result.
    fn apply_quiet(&mut self, outcome: QuietOutcome, auto: bool) {
        match outcome {
            QuietOutcome::Pr(data) => {
                let d = *data;
                if self.local || self.pr.as_ref().map(|p| p.number) != Some(d.detail.number) {
                    return;
                }
                let number = d.detail.number;
                let changed = self
                    .pr
                    .as_ref()
                    .map(|p| p.head_ref_oid != d.detail.head_ref_oid)
                    .unwrap_or(true)
                    || self.files != d.files;
                let cur = self.files.get(self.file_cursor).map(|f| f.path.clone());
                self.pr = Some(d.detail);
                self.merge_base = d.merge_base;
                // A refresh that found nothing must leave the pane alone;
                // resetting here would blank it with nothing on the way
                // to fill it back in.
                if changed {
                    self.reset_blame_review();
                }
                self.files = d.files;
                self.viewed = d.viewed;
                self.retarget_file_cursor(cur);
                self.rebuild_entries();
                self.reveal_current_file();
                if self.files.is_empty() {
                    self.diff = None;
                    self.display.clear();
                    self.ok(format!("PR #{number} has no changed files."));
                } else if !changed && self.diff.is_some() {
                    if !auto {
                        self.ok(format!("✔ PR #{number} is up to date."));
                    }
                } else {
                    self.spawn_quiet_file(self.file_cursor, auto);
                }
            }
            QuietOutcome::Local(data) => {
                if !self.local {
                    return;
                }
                let d = *data;
                let head = d.head.unwrap_or_default();
                let changed = self.files != d.files || self.merge_base != head;
                self.local_branch = d.branch;
                self.stage = d.stage;

                // Nothing moved. An idle re-scan lands every few seconds,
                // so it must leave the panel exactly as the reader left it
                // — rebuilding the tree here would scroll the file panel
                // back to the cursor while they were browsing it.
                if !changed && self.diff.is_some() {
                    if !auto {
                        self.ok("✔ Local changes are up to date.");
                    }
                    return;
                }

                let cur = self.files.get(self.file_cursor).map(|f| f.path.clone());
                self.merge_base = head;
                self.reset_blame_review();
                self.files = d.files;
                // The local viewed marks are a session-local reading aid;
                // keep them for files that still exist.
                let files = &self.files;
                self.viewed.retain(|p| files.iter().any(|f| &f.path == p));
                self.retarget_file_cursor(cur);
                self.rebuild_entries();
                self.reveal_current_file();
                if self.files.is_empty() {
                    self.diff = None;
                    self.display.clear();
                    self.diff_cursor = 0;
                    self.diff_scroll = 0;
                    // An idle re-scan of an already-clean tree found what
                    // it found last time: stay quiet.
                    if changed || !auto {
                        self.ok("Working tree clean — nothing uncommitted. ` swaps back.");
                    }
                } else {
                    self.spawn_quiet_file(self.file_cursor, auto);
                }
            }
            QuietOutcome::File(data) => {
                let d = *data;
                // Stale if the list moved under it since it was spawned.
                if self.files.get(d.idx).map(|f| f.path.as_str()) != Some(d.path.as_str()) {
                    return;
                }
                let same = self.old_content == d.old && self.new_content == d.new;
                self.file_cursor = d.idx;
                self.old_content = d.old;
                self.new_content = d.new;
                self.old_hl = d.old_hl;
                self.new_hl = d.new_hl;
                self.differs_from_head = d.differs;
                self.diff = Some(d.diff);
                if !same {
                    self.expanded_folds.clear();
                    self.selection = None;
                    self.select_mode = false;
                }
                self.rebuild_display();
                // Unlike a modal load this keeps the reader's place — just
                // clamped, since the content may have shrunk.
                let last = self.display.len().saturating_sub(1);
                self.diff_cursor = self.diff_cursor.min(last);
                self.diff_scroll = self.diff_scroll.min(last);
                self.diff_hscroll = self.diff_hscroll.min(self.max_hscroll());
                // Content that moved invalidates the blame with it: the
                // line numbers the pane is indexed by have shifted. A
                // refresh that moved only the head still needs one,
                // because the change set the pane colours against moved.
                if !same || (self.blame_on && !self.blame_done && self.blame_job.is_none()) {
                    self.spawn_blame(self.file_cursor);
                }
                if !same {
                    self.ok(format!("⟳ {} updated with the latest changes.", d.path));
                } else if !auto {
                    self.ok(format!("✔ {} — up to date.", d.path));
                }
            }
        }
    }
}

/// Load everything the diff view needs for one file: both sides' content,
/// the computed diff, and syntax highlighting. Runs on a worker thread for
/// both the modal load and the silent refresh.
fn load_file_data(
    idx: usize,
    file: ChangedFile,
    (merge_base, head_oid, checked_out, local, root): (String, String, bool, bool, PathBuf),
) -> Result<FileLoadedData> {
    let old = if file.status == "added" || merge_base.is_empty() {
        None
    } else {
        gitops::show_file(&merge_base, file.old_path())
    };
    // The PR-head copy only matters for comment anchoring — local
    // review has no PR to comment on, so skip the lookup.
    let head_content = if local || file.status == "removed" {
        None
    } else {
        gitops::show_file(&head_oid, &file.path)
    };
    let new = if file.status == "removed" {
        None
    } else if checked_out {
        // safe_repo_path rejects API paths that could resolve outside
        // the repository (absolute, `..`, …).
        gitops::safe_repo_path(&root, &file.path)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .or_else(|| head_content.clone())
    } else {
        head_content.clone()
    };
    let differs = if local {
        false
    } else {
        match (&new, &head_content) {
            (Some(a), Some(b)) => a != b,
            (None, None) => false,
            _ => true,
        }
    };
    let diff = FileDiff::compute(old.as_deref(), new.as_deref());
    // Highlighting dominates load time (hundreds of ms per side on
    // large files); do the two sides in parallel.
    let (old_hl, new_hl) = thread::scope(|s| {
        let old_side = s.spawn(|| {
            old.as_deref()
                .map(|c| highlight::highlight(file.old_path(), c))
                .unwrap_or_default()
        });
        let new_hl = new
            .as_deref()
            .map(|c| highlight::highlight(&file.path, c))
            .unwrap_or_default();
        (old_side.join().expect("highlight thread panicked"), new_hl)
    });
    Ok(FileLoadedData {
        idx,
        path: file.path,
        old,
        new,
        old_hl,
        new_hl,
        differs,
        diff,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cf(path: &str) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            status: "modified".into(),
            additions: 1,
            deletions: 0,
            previous: None,
        }
    }

    // ------------------------------------------------- the markdown preview

    /// A review of one markdown file, with its new side already loaded —
    /// the state the `P` key acts on.
    fn md_review_app(dir: &std::path::Path, body: &str) -> App {
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.checked_out = true;
        app.repo_root = dir.to_path_buf();
        app.files = vec![ChangedFile {
            path: "PLAN.md".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 0,
            previous: None,
        }];
        app.rebuild_entries();
        std::fs::write(dir.join("PLAN.md"), body).expect("fixture written");
        app.new_content = Some(body.to_string());
        app
    }

    /// A scratch directory of this test's own, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("loupe-preview-{name}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch directory");
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn p_renders_the_markdown_file_under_the_cursor() {
        let dir = TempDir::new("open");
        let mut app = md_review_app(&dir.0, "# Plan\n\n- one\n");
        app.handle_key(key(KeyCode::Char('P')));
        let pv = app.preview.as_ref().expect("the preview is open");
        assert_eq!(pv.path, "PLAN.md");
        assert!(!pv.standalone, "it is the file under review");
    }

    #[test]
    fn p_refuses_a_file_it_cannot_render() {
        let mut app = folded_app();
        app.handle_key(key(KeyCode::Char('P')));
        assert!(app.preview.is_none());
        assert!(app.status_err, "it says why: {}", app.status);
        assert!(app.status.contains("not markdown"));
    }

    /// The round trip the reader makes to change something they just read:
    /// preview → source → edit → preview, with the edit showing up.
    #[test]
    fn the_preview_and_the_source_swap_places() {
        let dir = TempDir::new("swap");
        let mut app = md_review_app(&dir.0, "# Plan\n\nfirst paragraph.\n");
        app.handle_key(key(KeyCode::Char('P')));
        assert!(app.preview.is_some());

        // P again opens the source in the editor.
        app.handle_key(key(KeyCode::Char('P')));
        assert!(app.preview.is_none());
        let ed = app.editor.as_mut().expect("the editor is open");
        assert_eq!(ed.path, "PLAN.md");
        ed.textarea.move_cursor(tui_textarea::CursorMove::End);
        ed.textarea.insert_str(" edited");
        ed.dirty = true;

        // Alt+P renders the buffer as it stands, saved or not.
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::ALT));
        let pv = app.preview.as_ref().expect("back in the preview");
        assert!(app.editor.is_none());
        assert!(pv.from_buffer, "it is showing unsaved text");
        assert!(pv.src.contains("edited"), "the edit is in the render");
    }

    #[test]
    fn switching_files_keeps_the_preview_only_for_markdown() {
        let dir = TempDir::new("switch");
        let mut app = md_review_app(&dir.0, "# Plan\n");
        app.files.push(ChangedFile {
            path: "NOTES.md".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 0,
            previous: None,
        });
        app.files.push(cf("src/main.rs"));
        app.rebuild_entries();
        std::fs::write(dir.0.join("NOTES.md"), "# Notes\n").unwrap();
        app.handle_key(key(KeyCode::Char('P')));
        assert!(app.preview.is_some());

        // A second markdown file stays in the preview.
        app.apply(Outcome::FileLoaded(Box::new(FileLoadedData {
            idx: 1,
            path: "NOTES.md".into(),
            old: None,
            new: Some("# Notes\n".into()),
            old_hl: Vec::new(),
            new_hl: Vec::new(),
            differs: false,
            diff: FileDiff::compute(None, Some("# Notes\n")),
        })));
        assert_eq!(
            app.preview.as_ref().map(|p| p.path.as_str()),
            Some("NOTES.md")
        );

        // Something it cannot render drops back to the diff.
        app.apply(Outcome::FileLoaded(Box::new(FileLoadedData {
            idx: 2,
            path: "src/main.rs".into(),
            old: None,
            new: Some("fn main() {}\n".into()),
            old_hl: Vec::new(),
            new_hl: Vec::new(),
            differs: false,
            diff: FileDiff::compute(None, Some("fn main() {}\n")),
        })));
        assert!(app.preview.is_none());
    }

    #[test]
    fn esc_leaves_the_preview_for_the_diff() {
        let dir = TempDir::new("esc");
        let mut app = md_review_app(&dir.0, "# Plan\n");
        app.handle_key(key(KeyCode::Char('P')));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.preview.is_none());
        assert!(!app.should_quit);
    }

    #[test]
    fn a_markdown_file_from_the_finder_opens_as_a_document() {
        let mut app = folded_app();
        app.apply_external(ExternalFile {
            path: "docs/notes.md".into(),
            abs_path: "/repo/docs/notes.md".into(),
            content: "# Notes\n".into(),
            line: None,
            read_only: false,
            preview: true,
        });
        let pv = app.preview.as_ref().expect("the preview is open");
        assert!(pv.standalone, "it is not part of the changeset");
        assert!(app.editor.is_none(), "the editor stayed out of the way");
    }

    /// The case the pane exists for: an agent rewrites the plan file while
    /// it is on screen.
    #[test]
    fn a_rewritten_file_is_picked_up() {
        let dir = TempDir::new("reload");
        let mut app = md_review_app(&dir.0, "# Plan\n\nSTEP 1.\n");
        app.handle_key(key(KeyCode::Char('P')));
        assert!(app
            .preview
            .as_ref()
            .is_some_and(|p| p.src.contains("STEP 1")));

        // Nothing has changed, so nothing is re-read.
        assert!(!app.poll_preview_reload());

        // The mtime resolution on some filesystems is coarse, so the
        // recorded time is cleared rather than raced against.
        if let Some(pv) = &mut app.preview {
            pv.mtime = None;
        }
        std::fs::write(dir.0.join("PLAN.md"), "# Plan\n\nSTEP 2.\n").unwrap();
        assert!(app.poll_preview_reload(), "the change was noticed");
        let pv = app.preview.as_ref().expect("still open");
        assert!(pv.src.contains("STEP 2"), "the new text is in the pane");
    }

    #[test]
    fn unsaved_text_is_never_overwritten_by_the_file_on_disk() {
        let dir = TempDir::new("unsaved");
        let mut app = md_review_app(&dir.0, "# Plan\n");
        app.handle_key(key(KeyCode::Char('P')));
        if let Some(pv) = &mut app.preview {
            pv.from_buffer = true;
            pv.mtime = None;
        }
        std::fs::write(dir.0.join("PLAN.md"), "# Something else\n").unwrap();
        assert!(!app.poll_preview_reload(), "the reader's own text wins");
    }

    #[test]
    fn loupe_md_opens_one_file_with_no_review_behind_it() {
        let dir = TempDir::new("standalone");
        let path = dir.0.join("REVIEW.md");
        std::fs::write(&path, "# Review\n\nAll good.\n").unwrap();
        let mut app = App::new(LaunchMode::Auto, None);
        app.start_preview_only(&path);
        assert!(app.preview_only);
        assert!(app.preview.is_some());
        // There is nothing behind the document, so Esc is the way out.
        app.handle_key(key(KeyCode::Esc));
        assert!(app.should_quit);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// The theme picker previews live (selection switches the process-wide
    /// theme) and Esc restores what was active before it opened.
    #[test]
    fn theme_picker_previews_live_and_esc_reverts() {
        let _guard = highlight::test_theme_lock();
        let before = highlight::current_theme();
        let mut app = App::new(LaunchMode::Auto, None);
        app.open_theme_picker();
        let sel0 = match &app.overlay {
            Overlay::ThemePicker(tp) => tp.sel,
            _ => panic!("picker should be open"),
        };
        assert_eq!(highlight::THEMES[sel0].1, before, "starts on current");

        app.handle_key(key(KeyCode::Down));
        let sel1 = match &app.overlay {
            Overlay::ThemePicker(tp) => tp.sel,
            _ => panic!("picker should still be open"),
        };
        assert_eq!(sel1, sel0 + 1);
        assert_eq!(
            highlight::current_theme(),
            highlight::THEMES[sel1].1,
            "moving the selection previews the theme immediately"
        );

        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(highlight::current_theme(), before, "Esc reverts");
    }

    /// Enter persists the selection to the config file named by
    /// $LOUPE_CONFIG, preserving unrelated keys and comments.
    #[test]
    fn theme_picker_enter_saves_to_config() {
        let _guard = highlight::test_theme_lock();
        // Which key the pick lands under depends on the appearance, so pin
        // it rather than inheriting whatever the terminal detected.
        let before_appearance = crate::theme::appearance();
        crate::theme::set_appearance(Appearance::Dark);
        let dir = std::env::temp_dir().join(format!("loupe-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        std::fs::write(&cfg, "# mine\norg = \"acme\"\n").unwrap();
        std::env::set_var("LOUPE_CONFIG", &cfg);

        let before = highlight::current_theme();
        let mut app = App::new(LaunchMode::Auto, None);
        app.open_theme_picker();
        app.handle_key(key(KeyCode::Down));
        let picked = highlight::current_theme();
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(highlight::current_theme(), picked, "Enter keeps the pick");

        let text = std::fs::read_to_string(&cfg).unwrap();
        std::env::remove_var("LOUPE_CONFIG");
        assert!(text.contains("# mine"), "{text}");
        assert!(text.contains("org = \"acme\""), "{text}");
        assert!(
            text.contains(&format!("theme = \"{}\"", highlight::theme_key(picked))),
            "{text}"
        );
        assert!(!app.status_err, "{}", app.status);

        // Leave the process-global look as we found it for other tests.
        highlight::set_theme(before);
        crate::theme::set_appearance(before_appearance);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `a` in the picker flips light ⇄ dark, drags the theme selection to
    /// the counterpart so the two never disagree, and — because the user
    /// overrode detection — pins `appearance` in the config alongside the
    /// theme, under the *light* key.
    #[test]
    fn theme_picker_toggles_appearance() {
        let _guard = highlight::test_theme_lock();
        let before_theme = highlight::current_theme();
        let before_appearance = crate::theme::appearance();
        let dir = std::env::temp_dir().join(format!("loupe-appearance-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        std::fs::write(&cfg, "mode = \"local\"\n").unwrap();
        std::env::set_var("LOUPE_CONFIG", &cfg);

        crate::theme::set_appearance(Appearance::Dark);
        highlight::set_theme(highlight::theme_by_name("gruvbox-dark").unwrap());

        let mut app = App::new(LaunchMode::Auto, None);
        app.open_theme_picker();
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(crate::theme::appearance(), Appearance::Light);
        assert_eq!(
            highlight::theme_key(highlight::current_theme()),
            "gruvbox-light",
            "the selection follows the appearance into the same family"
        );

        app.handle_key(key(KeyCode::Enter));
        let text = std::fs::read_to_string(&cfg).unwrap();
        std::env::remove_var("LOUPE_CONFIG");
        assert!(
            text.contains("light_theme = \"gruvbox-light\""),
            "a light pick goes in the light slot: {text}"
        );
        assert!(
            !text.contains("\ntheme = "),
            "and must not clobber the dark slot: {text}"
        );
        assert!(
            text.contains("appearance = \"light\""),
            "an overridden appearance is pinned: {text}"
        );
        assert!(text.contains("mode = \"local\""), "{text}");

        // Esc, by contrast, puts both halves back.
        crate::theme::set_appearance(Appearance::Dark);
        highlight::set_theme(highlight::theme_by_name("nord").unwrap());
        app.open_theme_picker();
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(crate::theme::appearance(), Appearance::Dark);
        assert_eq!(highlight::theme_key(highlight::current_theme()), "nord");

        highlight::set_theme(before_theme);
        crate::theme::set_appearance(before_appearance);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Toggling `a` twice must land back on the theme you started from,
    /// and must not pin `appearance` in the config. Theme pairing is
    /// many-to-one — three Catppuccin flavors share Latte — so a naive
    /// out-and-back mapping quietly replaces the user's theme, and Enter
    /// then writes the replacement over it.
    #[test]
    fn appearance_round_trip_keeps_the_theme() {
        let _guard = highlight::test_theme_lock();
        let before_theme = highlight::current_theme();
        let before_appearance = crate::theme::appearance();
        let dir = std::env::temp_dir().join(format!("loupe-roundtrip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        std::env::set_var("LOUPE_CONFIG", &cfg);

        // Every theme, including the many-to-one and unpaired ones.
        for (name, theme) in highlight::THEMES {
            let start = if highlight::theme_is_light(*theme) {
                Appearance::Light
            } else {
                Appearance::Dark
            };
            crate::theme::set_appearance(start);
            highlight::set_theme(*theme);
            let mut app = App::new(LaunchMode::Auto, None);
            app.open_theme_picker();
            app.handle_key(key(KeyCode::Char('a')));
            assert_eq!(crate::theme::appearance(), start.other(), "{name}");
            app.handle_key(key(KeyCode::Char('a')));
            assert_eq!(crate::theme::appearance(), start, "{name}");
            assert_eq!(
                highlight::theme_key(highlight::current_theme()),
                *name,
                "toggling twice must come back to {name}"
            );
            app.handle_key(key(KeyCode::Esc));
        }

        // …and Enter after a round trip writes the theme without pinning
        // the appearance, so detection still works on other terminals.
        std::fs::write(&cfg, "").unwrap();
        crate::theme::set_appearance(Appearance::Dark);
        highlight::set_theme(highlight::theme_by_name("dracula").unwrap());
        let mut app = App::new(LaunchMode::Auto, None);
        app.open_theme_picker();
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Enter));
        let text = std::fs::read_to_string(&cfg).unwrap();
        std::env::remove_var("LOUPE_CONFIG");
        assert!(text.contains("theme = \"dracula\""), "{text}");
        assert!(
            !text.contains("appearance ="),
            "a no-op round trip must not pin the appearance: {text}"
        );

        highlight::set_theme(before_theme);
        crate::theme::set_appearance(before_appearance);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tree_groups_files_under_dirs() {
        let files = vec![cf("src/app/x.rs"), cf("src/ui/y.rs"), cf("README.md")];
        let entries = build_tree_entries(&files, &HashSet::new());
        assert_eq!(
            entries,
            vec![
                FileEntry::Dir {
                    label: "src".into(),
                    path: "src".into(),
                    depth: 0
                },
                FileEntry::Dir {
                    label: "app".into(),
                    path: "src/app".into(),
                    depth: 1
                },
                FileEntry::File { idx: 0, depth: 2 },
                FileEntry::Dir {
                    label: "ui".into(),
                    path: "src/ui".into(),
                    depth: 1
                },
                FileEntry::File { idx: 1, depth: 2 },
                FileEntry::File { idx: 2, depth: 0 },
            ]
        );
    }

    /// A right-click on a file row offers both spellings of its path, and
    /// the full one is the relative one under the repo root.
    #[test]
    fn right_click_on_a_file_offers_both_paths() {
        let mut app = App::new(LaunchMode::Local, None);
        app.repo_root = std::env::temp_dir();
        app.files = vec![cf("src/app.rs")];
        app.tree_view = false;
        app.rebuild_entries();
        app.layout.file_list = Rect::new(0, 2, 30, 10);
        app.open_path_menu(4, 2);

        let Overlay::PathMenu(menu) = &app.overlay else {
            panic!("right-click must open the path menu");
        };
        assert_eq!(menu.path, "src/app.rs");
        assert!(!menu.is_dir);
        assert_eq!(menu.items.len(), 2);
        assert_eq!(menu.items[0].text, "src/app.rs");
        let full = &menu.items[1].text;
        assert!(full.starts_with('/'), "{full}");
        assert!(full.ends_with("/src/app.rs"), "{full}");
    }

    /// A local-changes review has no PR behind it, so the badge has no
    /// link to give. Say so instead of copying something wrong.
    #[test]
    fn the_local_badge_has_no_pr_link() {
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.local_branch = Some("feature/x".into());
        assert_eq!(app.pr_url(), None);

        app.layout.badge = Rect::new(0, 0, 10, 1);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 2,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.status_err, "the status line reports it: {}", app.status);
        assert!(app.status.contains("local-changes"), "{}", app.status);
    }

    /// The PR badge falls back to github.com only when `gh` gave no url —
    /// a workspace restored from a stash predates that field.
    #[test]
    fn the_pr_link_falls_back_to_the_repo_name() {
        let mut app = App::new(LaunchMode::Pr, None);
        app.repo = Some("owner/name".into());
        let mut detail = pr_detail(7);
        detail.url = String::new();
        app.pr = Some(detail);
        assert_eq!(
            app.pr_url().as_deref(),
            Some("https://github.com/owner/name/pull/7")
        );
    }

    /// Directory rows are worth copying too — a folder path is what you
    /// hand something that should look at the whole subtree.
    #[test]
    fn right_click_on_a_folder_offers_the_folder_path() {
        let mut app = App::new(LaunchMode::Local, None);
        app.repo_root = std::env::temp_dir();
        app.files = vec![cf("src/ui/panel.rs")];
        app.tree_view = true;
        app.rebuild_entries();
        app.layout.file_list = Rect::new(0, 0, 30, 10);
        app.open_path_menu(2, 0);

        let Overlay::PathMenu(menu) = &app.overlay else {
            panic!("right-click on a folder must open the path menu");
        };
        assert!(menu.is_dir);
        assert_eq!(menu.path, "src/ui");
        assert_eq!(menu.items[0].text, "src/ui");
    }

    /// The menu is a menu: arrows move, Esc closes, nothing is copied.
    #[test]
    fn path_menu_moves_and_closes() {
        let mut app = App::new(LaunchMode::Local, None);
        app.repo_root = std::env::temp_dir();
        app.files = vec![cf("README.md")];
        app.tree_view = false;
        app.rebuild_entries();
        app.layout.file_list = Rect::new(0, 0, 30, 10);
        app.open_path_menu(1, 0);

        app.handle_key(key(KeyCode::Down));
        let Overlay::PathMenu(menu) = &app.overlay else {
            panic!("moving must not close the menu");
        };
        assert_eq!(menu.sel, 1, "Down selects the second line");

        app.handle_key(key(KeyCode::Down));
        let Overlay::PathMenu(menu) = &app.overlay else {
            panic!("the menu must stay open at its last line");
        };
        assert_eq!(menu.sel, 1, "Down stops at the last line");

        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::None), "Esc closes the menu");
    }

    /// A right-click on empty space below the last row does nothing.
    #[test]
    fn right_click_below_the_last_row_opens_nothing() {
        let mut app = App::new(LaunchMode::Local, None);
        app.files = vec![cf("README.md")];
        app.tree_view = false;
        app.rebuild_entries();
        app.layout.file_list = Rect::new(0, 0, 30, 10);
        app.open_path_menu(1, 6);
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn tree_compresses_single_child_chains() {
        let files = vec![cf("a/b/c/deep.rs")];
        let entries = build_tree_entries(&files, &HashSet::new());
        assert_eq!(
            entries,
            vec![
                FileEntry::Dir {
                    label: "a/b/c".into(),
                    path: "a/b/c".into(),
                    depth: 0
                },
                FileEntry::File { idx: 0, depth: 1 },
            ]
        );
    }

    /// 20 identical lines with one change in the middle: two foldable runs.
    fn folded_app() -> App {
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.files = vec![cf("test.rs")];
        app.rebuild_entries();
        let old: String = (1..=20).map(|i| format!("line{i}\n")).collect();
        let new = old.replace("line10\n", "LINE10\n");
        app.diff = Some(FileDiff::compute(Some(&old), Some(&new)));
        app.rebuild_display();
        app.layout.diff = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };
        app
    }

    /// An app parked in local review with the reader long gone, which is
    /// the only state the idle re-scan is allowed to fire from.
    fn idle_local_app() -> App {
        let mut app = folded_app();
        app.local = true;
        let long_ago = Instant::now() - Duration::from_secs(60);
        app.last_input = long_ago;
        app.last_auto_rescan = long_ago;
        app
    }

    /// The whole point of the idle re-scan: an agent writes to the tree
    /// while nobody touches the keyboard, and the diff catches up on its
    /// own.
    #[test]
    fn the_idle_rescan_fires_only_when_the_reader_has_stopped() {
        let mut app = idle_local_app();
        assert!(app.should_auto_rescan(), "idle local review re-scans");

        // A key press just now: wait for the reader to settle again.
        app.last_input = Instant::now();
        assert!(!app.should_auto_rescan(), "a live reader is not idle");

        // Idle again, but the last re-scan was moments ago.
        app.last_input = Instant::now() - Duration::from_secs(60);
        app.last_auto_rescan = Instant::now();
        assert!(!app.should_auto_rescan(), "the minimum gap holds it off");
    }

    /// Every "the reader is in the middle of something" state blocks it.
    /// A re-scan that pulled a selection or an overlay out from under
    /// someone would be worse than a stale diff.
    #[test]
    fn the_idle_rescan_never_interrupts_anything() {
        type Case = (&'static str, Box<dyn Fn(&mut App)>);
        let cases: Vec<Case> = vec![
            (
                "switched off",
                Box::new(|a: &mut App| a.auto_refresh = false),
            ),
            ("a pull request", Box::new(|a: &mut App| a.local = false)),
            (
                "the pr list",
                Box::new(|a: &mut App| a.screen = Screen::PrList),
            ),
            (
                "an overlay",
                Box::new(|a: &mut App| a.overlay = Overlay::Help),
            ),
            (
                "a selection",
                Box::new(|a: &mut App| a.selection = Some(Selection::lines(Side::Right, 1, 1))),
            ),
            ("a drag", Box::new(|a: &mut App| a.drag_select = true)),
            (
                "a resize",
                Box::new(|a: &mut App| a.dragging = Dragging::FilePanel),
            ),
        ];
        for (what, setup) in cases {
            let mut app = idle_local_app();
            setup(&mut app);
            assert!(
                !app.should_auto_rescan(),
                "the idle re-scan must stand down for {what}"
            );
        }
    }

    /// An automatic re-scan that finds nothing says nothing. Without this
    /// the status line would repeat "up to date" every few seconds for as
    /// long as loupe is open.
    #[test]
    fn an_idle_rescan_that_finds_nothing_stays_quiet() {
        let mut app = idle_local_app();
        app.ok("something the reader was told");
        let before = app.status.clone();

        let same = |a: &App| FileLoadedData {
            idx: 0,
            path: "test.rs".into(),
            old: a.old_content.clone(),
            new: a.new_content.clone(),
            old_hl: Vec::new(),
            new_hl: Vec::new(),
            differs: false,
            diff: FileDiff::compute(a.old_content.as_deref(), a.new_content.as_deref()),
        };
        let data = same(&app);
        app.apply_quiet(QuietOutcome::File(Box::new(data)), true);
        assert_eq!(
            app.status, before,
            "an automatic no-op leaves the line alone"
        );

        // The same refresh asked for by hand does report back.
        let data = same(&app);
        app.apply_quiet(QuietOutcome::File(Box::new(data)), false);
        assert!(app.status.contains("up to date"), "{}", app.status);
    }

    /// A change always speaks, however the refresh was started — that is
    /// the reader's only cue that the diff moved under them.
    #[test]
    fn an_idle_rescan_reports_a_real_change() {
        let mut app = idle_local_app();
        app.old_content = Some("a\n".into());
        app.new_content = Some("b\n".into());
        app.apply_quiet(
            QuietOutcome::File(Box::new(FileLoadedData {
                idx: 0,
                path: "test.rs".into(),
                old: Some("a\n".into()),
                new: Some("c\n".into()),
                old_hl: Vec::new(),
                new_hl: Vec::new(),
                differs: false,
                diff: FileDiff::compute(Some("a\n"), Some("c\n")),
            })),
            true,
        );
        assert!(app.status.contains("updated"), "{}", app.status);
        assert_eq!(app.new_content.as_deref(), Some("c\n"));
    }

    /// An idle re-scan that finds the same files must not move the file
    /// panel. It lands every few seconds; scrolling the reader back to the
    /// cursor each time would make the panel unusable.
    #[test]
    fn an_idle_rescan_that_changes_nothing_leaves_the_panel_alone() {
        let mut app = idle_local_app();
        app.files = vec![cf("a.rs"), cf("b.rs"), cf("c.rs")];
        app.rebuild_entries();
        app.file_cursor = 0;
        // The reader scrolled away from the cursor to look at something.
        app.file_scroll = 2;

        app.apply_quiet(
            QuietOutcome::Local(Box::new(LocalOpenedData {
                branch: app.local_branch.clone(),
                head: Some(app.merge_base.clone()),
                files: vec![cf("a.rs"), cf("b.rs"), cf("c.rs")],
                stage: HashMap::new(),
            })),
            true,
        );
        assert_eq!(app.file_scroll, 2, "the panel stayed where it was put");
        assert!(!app.refreshing(), "no file reload when nothing moved");
    }

    /// ⟳ and `r` re-scan without a loading screen, so the reader keeps
    /// their place.
    #[test]
    fn refresh_review_is_never_modal() {
        let mut app = idle_local_app();
        app.diff_scroll = 3;
        app.refresh_review();
        assert!(!app.busy(), "no modal job, no loading screen");
        assert!(app.refreshing(), "the re-scan runs in the background");
        assert_eq!(app.diff_scroll, 3, "the reader keeps their place");
    }

    /// The editor owns the screen: refreshing under it would throw away
    /// unsaved edits, so it says no instead.
    #[test]
    fn refresh_review_refuses_with_the_editor_open() {
        let mut app = idle_local_app();
        app.editor = Some(Editor::new("test.rs", PathBuf::from("test.rs"), "x\n"));
        app.refresh_review();
        assert!(!app.refreshing());
        assert!(app.status_err, "{}", app.status);
    }

    /// The ☰ menu is built from the state it is opened in: no Comment line
    /// in local review, and the swap line names the side you would land on.
    #[test]
    fn the_menu_is_built_for_what_is_on_screen() {
        let labels = |app: &App| -> Vec<String> {
            let Overlay::Menu(menu) = &app.overlay else {
                panic!("the menu did not open");
            };
            menu.rows
                .iter()
                .filter_map(|r| match r {
                    MenuRow::Item(it) => Some(it.label.clone()),
                    MenuRow::Heading(_) => None,
                })
                .collect()
        };

        let mut app = idle_local_app();
        app.open_menu(0, 0);
        let local = labels(&app);
        assert!(!local.iter().any(|l| l.contains("Comment")), "{local:?}");
        assert!(
            local.iter().any(|l| l.contains("Swap to the pull request")),
            "{local:?}"
        );
        assert!(
            local.iter().any(|l| l.contains("Refresh while idle")),
            "the idle switch belongs to local review: {local:?}"
        );

        app.overlay = Overlay::None;
        app.local = false;
        app.open_menu(0, 0);
        let pr = labels(&app);
        assert!(pr.iter().any(|l| l.contains("Comment")), "{pr:?}");
        assert!(
            pr.iter().any(|l| l.contains("Swap to local changes")),
            "{pr:?}"
        );
        assert!(
            !pr.iter().any(|l| l.contains("Refresh while idle")),
            "a pull request never polls: {pr:?}"
        );
    }

    /// A line that cannot do anything right now is drawn but inert, and the
    /// keyboard steps over it rather than landing on it.
    #[test]
    fn the_menu_skips_lines_that_do_not_apply() {
        let mut app = idle_local_app();
        app.open_menu(0, 0);
        let Overlay::Menu(menu) = &app.overlay else {
            panic!("the menu did not open");
        };
        let copy = menu
            .rows
            .iter()
            .position(|r| matches!(r, MenuRow::Item(it) if it.label.contains("Copy")))
            .expect("a Copy line");
        assert!(
            matches!(&menu.rows[copy], MenuRow::Item(it) if !it.enabled),
            "nothing is selected, so Copy is inert"
        );
        // Walking down from the top never stops on it.
        let mut at = menu.first_selectable();
        let mut seen = vec![at];
        loop {
            let next = menu.next_selectable(at, 1);
            if next == at {
                break;
            }
            at = next;
            seen.push(at);
        }
        assert!(!seen.contains(&copy), "the cursor stepped onto a dead line");
        assert!(
            seen.iter()
                .all(|i| matches!(&menu.rows[*i], MenuRow::Item(it) if it.enabled)),
            "every stop is a live line"
        );
    }

    /// Picking a menu line runs it and puts the menu away — the same
    /// dispatch the toolbar buttons use, so the two can never disagree.
    #[test]
    fn a_menu_line_runs_and_closes() {
        let mut app = idle_local_app();
        let before = app.view;
        app.open_menu(0, 0);
        let Overlay::Menu(menu) = &app.overlay else {
            panic!("the menu did not open");
        };
        let toggle = menu
            .rows
            .iter()
            .position(|r| matches!(r, MenuRow::Item(it) if it.id == ButtonId::ViewToggle))
            .expect("a view line");
        app.menu_activate(toggle);
        assert!(matches!(app.overlay, Overlay::None), "the menu closed");
        assert_ne!(app.view, before, "the view flipped");
    }

    /// The idle switch in the menu turns the polling off for the session.
    #[test]
    fn the_menu_switch_turns_the_idle_rescan_off() {
        let mut app = idle_local_app();
        assert!(app.should_auto_rescan());
        app.activate(ButtonId::AutoRefreshToggle);
        assert!(!app.auto_refresh);
        assert!(!app.should_auto_rescan(), "switched off means switched off");
        app.activate(ButtonId::AutoRefreshToggle);
        assert!(app.auto_refresh);
    }

    fn folds(app: &App) -> usize {
        app.display
            .iter()
            .filter(|e| matches!(e, DisplayEntry::Fold { .. }))
            .count()
    }

    /// Expanding a run and then toggling fold puts that run back — the
    /// toggle used to jump straight to the full file and leave the expanded
    /// section open forever.
    #[test]
    fn fold_toggle_refolds_expanded_sections() {
        let mut app = folded_app();
        let baseline = app.display.clone();
        assert_eq!(folds(&app), 2);

        app.expanded_folds.insert(0);
        app.rebuild_display();
        assert_eq!(folds(&app), 1, "the expanded run is no longer folded");

        // First toggle: fold what was expanded, stay in folded mode.
        app.toggle_fold();
        assert!(app.collapse_unchanged);
        assert!(app.expanded_folds.is_empty());
        assert_eq!(app.display, baseline);

        // Second toggle: now it means "show the whole file".
        app.toggle_fold();
        assert!(!app.collapse_unchanged);
        assert_eq!(folds(&app), 0);

        // And back — nothing stays expanded across the round trip.
        app.toggle_fold();
        assert!(app.collapse_unchanged);
        assert_eq!(app.display, baseline);
    }

    /// The header drawn above an expanded run folds just that run.
    #[test]
    fn clicking_the_unfold_header_refolds_one_run() {
        let mut app = folded_app();
        let baseline = app.display.clone();
        let r = app.layout.diff;

        // Click the first fold row to expand it.
        let fold_y = app
            .display
            .iter()
            .position(|e| matches!(e, DisplayEntry::Fold { .. }))
            .expect("a fold to click") as u16;
        app.diff_click(r.x + 10, r.y + fold_y);
        assert_eq!(app.expanded_folds.len(), 1);

        // The run now starts with a click-to-fold header.
        let header_y = app
            .display
            .iter()
            .position(|e| matches!(e, DisplayEntry::Unfold { .. }))
            .expect("an unfold header") as u16;
        app.diff_click(r.x + 10, r.y + header_y);
        assert!(app.expanded_folds.is_empty());
        assert_eq!(app.display, baseline);
    }

    /// Horizontal scrolling is bounded by the widest line and reset by Home.
    #[test]
    fn horizontal_scroll_is_bounded_by_content() {
        let mut app = folded_app();
        app.view = ViewMode::Inline;
        let long = format!("{}\n", "x".repeat(200));
        app.diff = Some(FileDiff::compute(Some("short\n"), Some(&long)));
        app.rebuild_display();

        let body = app.diff_body_w();
        assert_eq!(app.max_hscroll(), 200 - body);

        app.handle_key(KeyEvent::from(KeyCode::Right));
        assert_eq!(app.diff_hscroll, HSCROLL_STEP as usize);
        for _ in 0..500 {
            app.handle_key(KeyEvent::from(KeyCode::Right));
        }
        assert_eq!(
            app.diff_hscroll,
            app.max_hscroll(),
            "cannot scroll past the widest line"
        );
        app.handle_key(KeyEvent::from(KeyCode::Home));
        assert_eq!(app.diff_hscroll, 0);
        assert_eq!(app.diff_scroll, 0);

        // Nothing to scroll when every line fits.
        app.diff = Some(FileDiff::compute(Some("a\n"), Some("b\n")));
        app.rebuild_display();
        assert_eq!(app.max_hscroll(), 0);
        app.handle_key(KeyEvent::from(KeyCode::Right));
        assert_eq!(app.diff_hscroll, 0);
    }

    /// Vim motions move a cursor row (not just the viewport), the view
    /// follows it, and `}` / `{` jump between runs of changes.
    #[test]
    fn vim_motions_move_the_cursor_and_follow_it() {
        let mut app = folded_app();
        app.collapse_unchanged = false;
        app.rebuild_display();
        // 20 rows, one change on row 9; a 6-row window makes scrolling bite.
        app.layout.diff = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 6,
        };
        let key = |c| KeyEvent::from(KeyCode::Char(c));

        app.diff_cursor = 0;
        app.diff_scroll = 0;
        app.handle_key(key('j'));
        app.handle_key(key('j'));
        assert_eq!(app.diff_cursor, 2);
        assert_eq!(app.diff_scroll, 0, "no need to scroll yet");

        // Moving past the window scrolls it, keeping the cursor in view.
        for _ in 0..6 {
            app.handle_key(key('j'));
        }
        assert_eq!(app.diff_cursor, 8);
        assert!(app.diff_scroll > 0, "the view followed the cursor");
        assert!(
            app.diff_cursor >= app.diff_scroll && app.diff_cursor < app.diff_scroll + 6,
            "cursor stays on screen"
        );

        // G / gg are the ends of the file.
        app.handle_key(key('G'));
        assert_eq!(app.diff_cursor, app.display.len() - 1);
        app.handle_key(key('g'));
        app.handle_key(key('g'));
        assert_eq!(app.diff_cursor, 0);
        assert_eq!(app.diff_scroll, 0);

        // A lone g is not a motion, and it does not stay armed.
        app.handle_key(key('g'));
        app.handle_key(key('j'));
        assert_eq!(app.diff_cursor, 1);
        app.handle_key(key('g'));
        assert_eq!(app.diff_cursor, 1, "g alone waits for the second one");

        // } jumps to the change; there is only one, so a second } says so.
        app.handle_key(key('}'));
        assert_eq!(app.diff_cursor, 9, "the modified row");
        assert!(app.entry_changed(app.diff_cursor));
        app.handle_key(key('}'));
        assert!(app.status_err, "no more changes below");
        app.handle_key(key('{'));
        assert_eq!(app.diff_cursor, 9, "still the only change, coming back up");

        // Ctrl+E scrolls without taking the cursor with it, until the
        // cursor would leave the screen.
        app.handle_key(key('g'));
        app.handle_key(key('g'));
        let ctrl_e = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
        app.handle_key(ctrl_e);
        assert_eq!(app.diff_scroll, 1);
        assert_eq!(app.diff_cursor, 1, "dragged along only at the edge");
    }

    /// `V` starts a line selection the motions extend, and `c` comments on
    /// the cursor row when nothing is selected.
    #[test]
    fn visual_line_selection_follows_the_cursor() {
        let mut app = folded_app();
        app.collapse_unchanged = false;
        app.rebuild_display();
        app.layout.diff = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };
        let key = |c| KeyEvent::from(KeyCode::Char(c));

        app.diff_cursor = 2;
        app.handle_key(key('V'));
        let sel = app.selection.expect("V selects the cursor line");
        assert_eq!(sel.range(), (3, 3), "row 2 is file line 3");

        app.handle_key(key('j'));
        app.handle_key(key('j'));
        assert_eq!(app.selection.unwrap().range(), (3, 5), "motions extend it");

        // Moving back up shrinks it again; the anchor holds.
        app.handle_key(key('k'));
        assert_eq!(app.selection.unwrap().range(), (3, 4));

        // Esc clears the selection before it means "back to the PR list".
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.selection.is_none());
        assert!(!app.select_mode);
        assert_eq!(
            app.screen,
            Screen::Review,
            "the first Esc only cleared the selection"
        );

        // A click puts the cursor where the mouse is, so the two agree.
        let r = app.layout.diff;
        app.diff_click(r.x + 10, r.y + 7);
        assert_eq!(app.diff_cursor, 7);
    }

    /// In local review the icon column stages the file instead of marking it
    /// viewed; in PR review it still marks it viewed.
    #[test]
    fn icon_column_stages_in_local_review() {
        let mut app = folded_app();
        app.local = true;
        // Somewhere without a repository: the git call fails on its own
        // thread and is never polled, so only the optimistic state is under
        // test here.
        app.repo_root = std::env::temp_dir();

        assert_eq!(app.stage_state("test.rs"), StageState::Unstaged);
        assert_eq!(app.staged_count(), 0);
        app.toggle_file_mark(0);
        assert_eq!(app.stage_state("test.rs"), StageState::Staged);
        assert_eq!(app.staged_count(), 1);
        assert!(
            app.viewed.is_empty(),
            "local review never touches viewed state"
        );

        // Clicking a fully staged file takes it back out of the index.
        app.toggle_file_mark(0);
        assert_eq!(app.stage_state("test.rs"), StageState::Unstaged);

        // A partly staged file stages the rest rather than unstaging.
        app.stage.insert("test.rs".into(), StageState::Partial);
        app.toggle_file_mark(0);
        assert_eq!(app.stage_state("test.rs"), StageState::Staged);

        // PR review is unchanged: the same column marks files viewed.
        app.local = false;
        app.toggle_file_mark(0);
        assert!(app.viewed.contains("test.rs"));
    }

    /// The wheel scrolls sideways when a modifier is held (whichever one the
    /// terminal passes through) and when the terminal reports a horizontal
    /// wheel directly; a bare wheel still scrolls vertically.
    #[test]
    fn modified_wheel_scrolls_sideways() {
        let mut app = folded_app();
        app.view = ViewMode::Inline;
        app.diff = Some(FileDiff::compute(
            Some("short\n"),
            Some(&format!("{}\n", "x".repeat(200))),
        ));
        app.rebuild_display();
        let wheel = |kind, modifiers| MouseEvent {
            kind,
            column: 60,
            row: 5,
            modifiers,
        };

        for m in [
            KeyModifiers::SHIFT,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL,
        ] {
            app.diff_hscroll = 0;
            app.handle_mouse(wheel(MouseEventKind::ScrollDown, m));
            assert_eq!(
                app.diff_hscroll, HSCROLL_WHEEL as usize,
                "{m:?}+wheel scrolls right"
            );
            app.handle_mouse(wheel(MouseEventKind::ScrollUp, m));
            assert_eq!(app.diff_hscroll, 0, "{m:?}+wheel scrolls back");
        }

        // Terminals that report a horizontal wheel need no modifier.
        app.handle_mouse(wheel(MouseEventKind::ScrollRight, KeyModifiers::NONE));
        assert_eq!(app.diff_hscroll, HSCROLL_WHEEL as usize);
        app.handle_mouse(wheel(MouseEventKind::ScrollLeft, KeyModifiers::NONE));
        assert_eq!(app.diff_hscroll, 0);

        // A bare wheel is still vertical scrolling.
        app.diff_scroll = 0;
        app.layout.diff = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 1,
        };
        app.handle_mouse(wheel(MouseEventKind::ScrollDown, KeyModifiers::NONE));
        assert_eq!(
            app.diff_hscroll, 0,
            "an unmodified wheel must not scroll sideways"
        );
        assert!(
            app.diff_scroll > 0,
            "an unmodified wheel still scrolls down"
        );
    }

    /// Dragging the divider resizes the file panel, within limits that keep
    /// both panes usable.
    #[test]
    fn divider_drag_resizes_within_limits() {
        let mut app = folded_app();
        app.layout.review = Rect {
            x: 0,
            y: 1,
            width: 100,
            height: 40,
        };
        app.layout.divider = Rect {
            x: 33,
            y: 1,
            width: 2,
            height: 40,
        };

        let ev = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), 33, 5));
        assert!(app.resizing());
        app.handle_mouse(ev(MouseEventKind::Drag(MouseButton::Left), 49, 5));
        assert_eq!(app.file_panel_w, 50);

        // Dragged off the right edge: the diff pane keeps its minimum.
        app.handle_mouse(ev(MouseEventKind::Drag(MouseButton::Left), 99, 5));
        assert_eq!(app.file_panel_w, 100 - DIFF_MIN_W);
        // …and off the left edge: the panel keeps its minimum.
        app.handle_mouse(ev(MouseEventKind::Drag(MouseButton::Left), 0, 5));
        assert_eq!(app.file_panel_w, FILE_PANEL_MIN);

        app.handle_mouse(ev(MouseEventKind::Up(MouseButton::Left), 0, 5));
        assert!(!app.resizing());
        // A release outside a drag must not be swallowed by the divider.
        app.handle_mouse(ev(MouseEventKind::Drag(MouseButton::Left), 80, 5));
        assert_eq!(
            app.file_panel_w, FILE_PANEL_MIN,
            "drag without a press does nothing"
        );

        // Keyboard resize honours the same clamps.
        app.handle_key(KeyEvent::from(KeyCode::Char('>')));
        assert_eq!(app.file_panel_w, FILE_PANEL_MIN + 2);
        app.handle_key(KeyEvent::from(KeyCode::Char('<')));
        assert_eq!(app.file_panel_w, FILE_PANEL_MIN);
    }

    fn pr_detail(number: u64) -> PrDetail {
        PrDetail {
            id: "node".into(),
            number,
            title: "a change".into(),
            head_ref_oid: "a".repeat(40),
            base_ref_oid: "b".repeat(40),
            base_ref_name: "main".into(),
            head_ref_name: "feat".into(),
            url: format!("https://github.com/o/r/pull/{number}"),
        }
    }

    fn pr_workspace(number: u64) -> Workspace {
        let old = "x\n".to_string();
        let new = "y\n".to_string();
        Workspace {
            local: false,
            local_branch: None,
            pr: Some(pr_detail(number)),
            checked_out: true,
            merge_base: "c".repeat(40),
            files: vec![cf("other.rs")],
            viewed: HashSet::new(),
            stage: HashMap::new(),
            file_cursor: 0,
            file_scroll: 0,
            collapsed_dirs: HashSet::new(),
            diff: Some(FileDiff::compute(Some(&old), Some(&new))),
            collapse_unchanged: true,
            expanded_folds: HashSet::new(),
            old_content: Some(old),
            new_content: Some(new),
            old_hl: Vec::new(),
            new_hl: Vec::new(),
            differs_from_head: false,
            diff_scroll: 0,
            diff_cursor: 0,
            diff_hscroll: 0,
            selection: None,
        }
    }

    /// The ` toggle swaps the two workspaces in place — no modal job, no
    /// loading screen — and swapping back restores the exact position.
    #[test]
    fn toggle_swaps_workspaces_and_back() {
        let mut app = folded_app();
        app.local = true;
        app.repo = Some("acme/repo".into());
        app.diff_scroll = 2;
        app.diff_cursor = 3;
        app.stash = Some(Box::new(pr_workspace(7)));

        app.toggle_workspace();
        assert!(!app.local, "swapped to the PR side");
        assert!(app.job.is_none(), "the swap itself is instant, not a job");
        assert!(app.refreshing(), "a silent PR refresh starts on swap-back");
        assert_eq!(app.pr.as_ref().map(|p| p.number), Some(7));
        assert_eq!(app.files[0].path, "other.rs");
        assert_eq!(app.screen, Screen::Review);
        let stashed = app.stash.as_ref().expect("local side stashed");
        assert!(stashed.local, "the stash now holds the local side");
        assert_eq!(stashed.diff_scroll, 2);

        app.toggle_workspace();
        assert!(app.local, "swapped back to local");
        assert_eq!(app.files[0].path, "test.rs");
        assert_eq!(app.diff_scroll, 2, "reading position survives the trip");
        assert_eq!(app.diff_cursor, 3);
        assert!(app.diff.is_some(), "diff came from the stash, not a reload");
        assert!(app.refreshing(), "the local side rescans in the background");
    }

    /// Opening a PR while reviewing local changes stashes the local side,
    /// so ` can flip straight back to it later.
    #[test]
    fn opening_a_pr_stashes_the_local_side() {
        let mut app = folded_app();
        app.local = true;
        app.local_branch = Some("feat".into());
        app.apply(Outcome::PrOpened(Box::new(PrOpenedData {
            repo: "acme/repo".into(),
            repo_root: None,
            detail: pr_detail(9),
            checked_out: true,
            merge_base: "c".repeat(40),
            files: vec![cf("pr_file.rs")],
            viewed: HashSet::new(),
            auto: false,
        })));
        assert!(!app.local);
        assert_eq!(app.pr.as_ref().map(|p| p.number), Some(9));
        let stashed = app.stash.as_ref().expect("local side stashed");
        assert!(stashed.local);
        assert_eq!(stashed.files[0].path, "test.rs");
        assert!(stashed.diff.is_some());
    }

    /// A quiet file refresh keeps the reading position when nothing
    /// changed, and re-anchors (dropping the selection) when it did.
    #[test]
    fn quiet_file_refresh_keeps_the_readers_place() {
        let mut app = folded_app();
        app.local = true;
        let old: String = (1..=20).map(|i| format!("line{i}\n")).collect();
        let new = old.replace("line10\n", "LINE10\n");
        app.old_content = Some(old.clone());
        app.new_content = Some(new.clone());
        app.diff_scroll = 3;
        app.diff_cursor = 4;
        app.selection = Some(Selection::lines(Side::Right, 10, 10));

        let reload = |old: &str, new: &str| FileLoadedData {
            idx: 0,
            path: "test.rs".into(),
            old: Some(old.to_string()),
            new: Some(new.to_string()),
            old_hl: Vec::new(),
            new_hl: Vec::new(),
            differs: false,
            diff: FileDiff::compute(Some(old), Some(new)),
        };

        // Identical content: position and selection survive untouched.
        app.apply_quiet(QuietOutcome::File(Box::new(reload(&old, &new))), false);
        assert_eq!(app.diff_scroll, 3);
        assert_eq!(app.diff_cursor, 4);
        assert!(app.selection.is_some());
        assert!(app.status.contains("up to date"), "{}", app.status);

        // Changed content: no jump to the top, but the selection goes —
        // its lines may not exist any more.
        let new2 = new.replace("line3\n", "line3 changed\n");
        app.apply_quiet(QuietOutcome::File(Box::new(reload(&old, &new2))), false);
        assert_eq!(app.new_content.as_deref(), Some(new2.as_str()));
        assert!(app.selection.is_none());
        assert_eq!(app.diff_scroll, 3, "position kept, only clamped");
        assert!(app.status.contains("updated"), "{}", app.status);
    }

    /// A quiet PR refresh that finds nothing new says so and leaves the
    /// loaded diff alone (no chained file reload).
    #[test]
    fn quiet_pr_refresh_up_to_date() {
        let mut app = folded_app();
        app.repo = Some("acme/repo".into());
        app.pr = Some(pr_detail(7));
        app.merge_base = "c".repeat(40);
        app.apply_quiet(
            QuietOutcome::Pr(Box::new(PrRefreshData {
                detail: pr_detail(7),
                merge_base: "c".repeat(40),
                files: vec![cf("test.rs")],
                viewed: HashSet::new(),
            })),
            false,
        );
        assert!(app.status.contains("up to date"), "{}", app.status);
        assert!(!app.refreshing(), "no chained reload when nothing moved");
        assert!(app.diff.is_some());
    }

    /// The toggle refuses to fire with the editor open — unsaved edits
    /// must not be silently stashed away.
    #[test]
    fn toggle_blocked_while_editing() {
        let mut app = folded_app();
        app.local = true;
        app.stash = Some(Box::new(pr_workspace(7)));
        app.editor = Some(Editor::new("test.rs", PathBuf::from("test.rs"), "x\n"));
        app.toggle_workspace();
        assert!(app.local, "still on the local side");
        assert!(app.stash.is_some(), "stash untouched");
        assert!(app.status_err, "{}", app.status);
    }

    #[test]
    fn collapsed_dir_hides_subtree() {
        let files = vec![cf("src/app/x.rs"), cf("README.md")];
        // "src" holds a single child dir and no files, so its row is the
        // compressed "src/app" — that path is also the collapse key.
        let mut collapsed = HashSet::new();
        collapsed.insert("src/app".to_string());
        let entries = build_tree_entries(&files, &collapsed);
        assert_eq!(
            entries,
            vec![
                FileEntry::Dir {
                    label: "src/app".into(),
                    path: "src/app".into(),
                    depth: 0
                },
                FileEntry::File { idx: 1, depth: 0 },
            ]
        );
    }

    // ---------------------------------------------------------------- search

    fn finder_of(app: &App) -> &Finder {
        match &app.overlay {
            Overlay::Finder(f) => f,
            _ => panic!("the finder should be open"),
        }
    }

    fn type_into_finder(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
    }

    #[test]
    fn finder_filters_files_and_prefixes_switch_mode() {
        let mut app = folded_app();
        app.files = vec![cf("src/app.rs"), cf("src/ui/render.rs"), cf("README.md")];
        app.rebuild_entries();
        app.new_content = Some("fn alpha() {}\nfn beta() {}\n".into());

        app.open_finder(FinderMode::Files);
        assert_eq!(finder_of(&app).rows.len(), 3, "everything, unfiltered");

        type_into_finder(&mut app, "app");
        let f = finder_of(&app);
        assert_eq!(f.mode, FinderMode::Files);
        assert_eq!(
            f.rows[0].path, "src/app.rs",
            "the best fuzzy match sorts first"
        );
        assert!(
            !f.rows[0].matched.is_empty(),
            "matched positions come back for highlighting"
        );

        // Backspacing to empty and typing `@` switches to symbols; the
        // definitions come from the open file.
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Backspace));
        }
        app.handle_key(key(KeyCode::Char('@')));
        let f = finder_of(&app);
        assert_eq!(f.mode, FinderMode::Symbols);
        assert_eq!(f.rows.len(), 2, "two fns in the open file");
        type_into_finder(&mut app, "bet");
        let f = finder_of(&app);
        assert_eq!(f.rows.len(), 1);
        assert_eq!(f.rows[0].text, "beta");
        assert_eq!(f.rows[0].line, Some(2));

        // Backspace on an empty input walks back out of the mode.
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Backspace));
        }
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(finder_of(&app).mode, FinderMode::Files);

        // `#` needs a subprocess, so it only arms the debounce here.
        app.handle_key(key(KeyCode::Char('#')));
        type_into_finder(&mut app, "al");
        assert_eq!(finder_of(&app).mode, FinderMode::Grep);
        assert!(app.searching(), "a grep query is pending");

        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn grep_results_put_definitions_first() {
        let mut app = folded_app();
        app.files = vec![cf("a.ts"), cf("b.ts")];
        app.open_finder(FinderMode::Grep);
        let gen = app.search_gen + 1;
        app.search_gen = gen;
        app.apply_search(
            gen,
            SearchOutcome::Grep {
                query: "handleClick".into(),
                truncated: false,
                hits: vec![
                    search::Hit {
                        path: "b.ts".into(),
                        line: 9,
                        text: "  handleClick();".into(),
                        col: 2,
                        len: 11,
                        definition: false,
                    },
                    search::Hit {
                        path: "vendor/z.ts".into(),
                        line: 3,
                        text: "const handleClick = () => {}".into(),
                        col: 6,
                        len: 11,
                        definition: true,
                    },
                ],
            },
        );
        let f = finder_of(&app);
        assert_eq!(f.rows[0].tag, "def", "the definition floats to the top");
        assert_eq!(f.rows[0].path, "vendor/z.ts");
        assert!(!f.rows[0].in_changeset);
        assert_eq!(f.rows[1].tag, "changed");
        assert_eq!(f.rows[1].range, Some((2, 13)));

        // A result from a query the user has already typed past is dropped.
        app.search_gen += 1;
        let stale = gen;
        app.apply_search(
            stale,
            SearchOutcome::Grep {
                query: "old".into(),
                truncated: false,
                hits: Vec::new(),
            },
        );
        assert_eq!(finder_of(&app).rows.len(), 2, "stale results are ignored");
    }

    #[test]
    fn jumping_to_a_line_expands_the_fold_hiding_it() {
        let mut app = folded_app();
        // Line 3 is inside the first folded run (rows 0..6).
        assert!(folds(&app) > 0);
        let before = app.display.len();
        app.jump_to_line(3);
        assert!(
            app.display.len() > before,
            "the fold covering the target line was expanded"
        );
        let row = match app.display[app.diff_cursor] {
            DisplayEntry::Line(i) => i,
            other => panic!("cursor should be on a line, got {other:?}"),
        };
        assert_eq!(app.diff.as_ref().unwrap().rows[row].new_ln, Some(3));
    }

    #[test]
    fn slash_search_highlights_steps_and_wraps() {
        let mut app = folded_app();
        app.handle_key(key(KeyCode::Char('/')));
        assert!(app.find.typing);
        type_into_finder(&mut app, "line1");
        // line1, line10 (as LINE10 — smart case matches), line11..line19
        assert!(app.find.rows.len() > 2, "matches found while typing");
        app.handle_key(key(KeyCode::Enter));
        assert!(!app.find.typing);
        assert!(app.find.active());

        let first = app.find.at;
        app.handle_key(key(KeyCode::Char('n')));
        assert_ne!(app.find.at, first, "n steps to the next match");
        // Walk off the end and confirm it wraps rather than stopping.
        for _ in 0..app.find.rows.len() {
            app.handle_key(key(KeyCode::Char('n')));
        }
        assert!(app.status.contains("Match"));

        // Esc clears the search rather than leaving the review.
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.find.active());
        assert_eq!(app.screen, Screen::Review);

        // Cancelling mid-type puts the reader back where they started.
        app.diff_cursor = 5;
        app.handle_key(key(KeyCode::Char('/')));
        type_into_finder(&mut app, "line19");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.diff_cursor, 5, "Esc restores the original position");
        assert!(app.find.query.is_empty());
    }

    #[test]
    fn n_and_p_no_longer_step_files() {
        let mut app = folded_app();
        app.files = vec![cf("a.rs"), cf("b.rs")];
        app.rebuild_entries();
        // `n` with no search says so instead of loading the next file.
        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.file_cursor, 0);
        assert!(app.job.is_none(), "no file load was started");
        assert!(app.status_err && app.status.contains('/'));
    }

    /// A file whose language has no server, so the flow can be tested
    /// without starting a subprocess.
    fn app_with_line(path: &str, line: &str) -> App {
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.files = vec![cf(path)];
        app.rebuild_entries();
        let content = format!("first\n{line}\nlast\n");
        // A file against itself: every row is context.
        app.diff = Some(FileDiff::compute(Some(&content), Some(&content)));
        app.new_content = Some(content.clone());
        app.old_content = Some(content);
        app.collapse_unchanged = false;
        app.rebuild_display();
        app.layout.diff = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };
        app.diff_cursor = 1; // the interesting line
        app
    }

    #[test]
    fn a_clicked_word_wins_over_guessing() {
        let mut app = app_with_line("notes.md", "alpha = beta(gamma)");
        // Nothing clicked: every identifier on the line is a candidate.
        let all = app.targets_on_cursor_row();
        assert_eq!(
            all.iter().map(|t| t.word.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "beta", "gamma"]
        );
        assert_eq!(all[1].line, 2, "line numbers are 1-based");
        assert_eq!(all[1].col, 9, "columns are 1-based char offsets");

        // A click on "beta" (char 8 of the line) settles it.
        app.click_word = Some((app.diff_cursor, 9));
        let picked = app.targets_on_cursor_row();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].word, "beta");

        // A click recorded on a *different* row doesn't leak into this one.
        app.click_word = Some((0, 1));
        assert_eq!(app.targets_on_cursor_row().len(), 3);
    }

    #[test]
    fn an_ambiguous_line_asks_which_symbol() {
        let mut app = app_with_line("notes.md", "alpha = beta(gamma)");
        app.lsp_action(LspAction::References);
        let f = finder_of(&app);
        assert_eq!(f.mode, FinderMode::Pick);
        assert_eq!(f.rows.len(), 3);
        assert_eq!(f.pending_action, Some(LspAction::References));
        assert_eq!(f.rows[0].pick, Some(0));

        // Typing filters the picker, and Enter runs the pending request
        // against the row that survived — which for a language with no
        // server means an explanation rather than a subprocess.
        type_into_finder(&mut app, "gam");
        assert_eq!(finder_of(&app).rows.len(), 1);
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.job.is_none(), "no server was started for a .md file");
        assert!(
            app.status_err && app.status.contains("No language server"),
            "got {:?}",
            app.status
        );
    }

    #[test]
    fn one_symbol_on_the_line_needs_no_picker() {
        let mut app = app_with_line("notes.md", "    handleClick");
        app.lsp_action(LspAction::Hover);
        assert!(
            matches!(app.overlay, Overlay::None),
            "a single candidate goes straight through"
        );
        assert!(app.status.contains("No language server"));

        // An empty line has nothing to ask about, and says so differently.
        app.diff_cursor = 0;
        let mut app2 = app_with_line("notes.md", "   ");
        app2.lsp_action(LspAction::Definition);
        assert!(app2.status.contains("No symbol on this line"));
    }

    #[test]
    fn references_land_in_the_finder_with_their_source_lines() {
        let mut app = app_with_line("a.ts", "handleClick()");
        app.files = vec![cf("a.ts")];
        app.rebuild_entries();
        app.apply_locations(LocationsData {
            action: LspAction::References,
            word: "handleClick".into(),
            places: vec![
                Place {
                    loc: lsp::Loc {
                        path: "a.ts".into(),
                        line: 2,
                        col: 1,
                    },
                    text: "handleClick()".into(),
                },
                Place {
                    loc: lsp::Loc {
                        path: "vendor/b.ts".into(),
                        line: 40,
                        col: 7,
                    },
                    text: "  const x = handleClick;".into(),
                },
            ],
        });
        let f = finder_of(&app);
        assert_eq!(f.mode, FinderMode::Refs);
        assert_eq!(f.rows.len(), 2);
        assert!(f.note.contains("references to"), "note: {}", f.note);
        assert_eq!(f.rows[0].tag, "changed", "a hit under review is marked");
        assert_eq!(f.rows[1].tag, "");
        assert_eq!(
            f.rows[1].range,
            Some((12, 23)),
            "the word is highlighted inside the source line"
        );

        // A lone definition skips the list entirely and just goes there.
        app.apply_locations(LocationsData {
            action: LspAction::Definition,
            word: "handleClick".into(),
            places: vec![Place {
                loc: lsp::Loc {
                    path: "a.ts".into(),
                    line: 2,
                    col: 1,
                },
                text: "handleClick()".into(),
            }],
        });
        assert!(matches!(app.overlay, Overlay::None));

        // Nothing found says why, rather than showing an empty list.
        app.apply_locations(LocationsData {
            action: LspAction::References,
            word: "handleClick".into(),
            places: Vec::new(),
        });
        assert!(app.status_err && app.status.contains("indexing"));
    }

    #[test]
    fn language_servers_can_be_turned_off() {
        let mut app = app_with_line("a.ts", "handleClick()");
        app.lsp_enabled = false;
        app.lsp_action(LspAction::Definition);
        assert!(app.status_err && app.status.contains("language_servers = false"));
        assert!(app.job.is_none());
    }

    /// A one-file review at a known size, so mouse coordinates in a test
    /// mean something. Side-by-side with the diff pane at column 34: its
    /// left body starts after a 5-column gutter (39), the right pane after
    /// the divider at 34 + 32 + 1, plus its own gutter (72).
    fn mouse_app(old: &str, new: &str) -> App {
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.files = vec![cf("a.ts")];
        app.rebuild_entries();
        app.old_content = Some(old.to_string());
        app.new_content = Some(new.to_string());
        app.diff = Some(FileDiff::compute(Some(old), Some(new)));
        app.collapse_unchanged = false;
        app.rebuild_display();
        app.layout.diff = Rect {
            x: 34,
            y: 1,
            width: 66,
            height: 20,
        };
        app.layout.review = Rect {
            x: 0,
            y: 1,
            width: 100,
            height: 20,
        };
        app
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// The ask this feature exists for: drag across part of a line and get
    /// exactly those characters, from whichever side was dragged.
    #[test]
    fn dragging_selects_characters_not_whole_lines() {
        let old = "const alpha = compute(one);\nconst beta = compute(two);\nconst gamma = 3;\n";
        let new = "const alpha = compute(one);\nconst beta = RENAMED(two);\nconst gamma = 3;\n";
        let mut app = mouse_app(old, new);
        // Line 2 is the second row of the pane, at y = 1 + 1.
        let left = |col: usize| 39 + col as u16;
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), left(6), 2));

        // A press alone still selects the whole line — that is what
        // commenting wants, and what clicking has always done.
        let sel = app.selection.expect("a selection");
        assert!(sel.linewise);
        assert_eq!(sel.side, Side::Left);
        assert_eq!(app.copy_target().unwrap().0, "const beta = compute(two);");

        // Dragging turns it into exactly the characters covered.
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), left(10), 2));
        let sel = app.selection.unwrap();
        assert!(!sel.linewise);
        let (text, what) = app.copy_target().unwrap();
        assert_eq!(text, "beta");
        assert!(
            what.contains("4 characters") && what.contains("removed"),
            "{what}"
        );

        // Across lines, it runs to the end of the first and into the next.
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), left(11), 3));
        assert_eq!(
            app.copy_target().unwrap().0,
            "beta = compute(two);\nconst gamma"
        );

        // The renderer paints precisely that range — the highlight and the
        // clipboard read the same function, so they cannot drift apart.
        let sel = app.selection.unwrap();
        assert_eq!(sel.cols_on(Side::Left, 2, 26), Some((6, 26)));
        assert_eq!(sel.cols_on(Side::Left, 3, 16), Some((0, 11)));
        assert_eq!(sel.cols_on(Side::Right, 2, 26), None, "wrong side");

        // Releasing ends the drag; a later move doesn't keep selecting.
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), left(1), 4));
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), left(1), 4));
        assert_eq!(app.selection.unwrap().end, Pos::new(3, 11));

        // Commenting still works off the line span, unchanged.
        assert_eq!(app.selection.unwrap().range(), (2, 3));
    }

    #[test]
    fn a_drag_on_the_new_side_stays_on_the_new_side() {
        let old = "let a = 1;\nlet b = 2;\n";
        let new = "let a = 1;\nlet B = 22;\n";
        let mut app = mouse_app(old, new);
        let right = |col: usize| 72 + col as u16;
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), right(4), 2));
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), right(10), 2));
        let sel = app.selection.unwrap();
        assert_eq!(sel.side, Side::Right);
        assert_eq!(app.copy_target().unwrap().0, "B = 22");

        // Dragging back across the divider into the left pane keeps
        // selecting new-side text: the two panes are different documents,
        // so a selection that spanned both would copy nonsense.
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 40, 2));
        assert_eq!(app.selection.unwrap().side, Side::Right);
        assert!(app.copy_target().unwrap().0.starts_with("let"));
    }

    /// The point of copying: getting the *removed* text out, which is the
    /// side that no longer exists anywhere on disk.
    #[test]
    fn copying_takes_the_lines_from_the_selected_side() {
        let mut app = folded_app();
        let old: String = (1..=20).map(|i| format!("line{i}\n")).collect();
        let new = old.replace("line10\n", "LINE10\n");
        app.old_content = Some(old);
        app.new_content = Some(new);

        // A selection on the old side copies what was deleted.
        app.selection = Some(Selection::lines(Side::Left, 9, 11));
        let (text, what) = app.copy_target().expect("something to copy");
        assert_eq!(text, "line9\nline10\nline11");
        assert!(
            what.contains("3 lines") && what.contains("removed"),
            "{what}"
        );

        // The new side of the same range is the replacement text.
        app.selection = Some(Selection::lines(Side::Right, 10, 10));
        let (text, what) = app.copy_target().unwrap();
        assert_eq!(text, "LINE10");
        assert!(what.contains("line 10") && what.contains("new"), "{what}");

        // With no selection it falls back to the cursor row, so `y` always
        // means something.
        app.selection = None;
        app.jump_to_line(10);
        let (text, what) = app.copy_target().expect("the cursor row");
        assert_eq!(text, "LINE10");
        assert!(what.contains("line 10"), "{what}");

        // A fold banner is not a line — say so rather than copying blank.
        app.cursor_to(0);
        assert!(matches!(app.display[0], DisplayEntry::Fold { .. }));
        assert!(app.copy_target().is_none());

        // And with nothing loaded at all it says so rather than copying "".
        let mut empty = App::new(LaunchMode::Local, None);
        empty.screen = Screen::Review;
        assert!(empty.copy_target().is_none());
        empty.yank();
        assert!(empty.status_err && empty.status.contains("Nothing to copy"));
    }

    /// Two changed sections in one small file, with folding off so every
    /// row is on screen: rows 0 a, 1 b→B, 2-4 context, 5 f→F1, 6 +F2, 7 g.
    fn revert_app() -> App {
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        // Local review always has the working tree under it.
        app.checked_out = true;
        app.files = vec![cf("a.txt")];
        app.rebuild_entries();
        let old = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "a\nB\nc\nd\ne\nF1\nF2\ng\n";
        app.old_content = Some(old.into());
        app.new_content = Some(new.into());
        app.diff = Some(FileDiff::compute(Some(old), Some(new)));
        app.collapse_unchanged = false;
        app.rebuild_display();
        app.layout.diff = Rect {
            x: 34,
            y: 1,
            width: 66,
            height: 20,
        };
        app.layout.file_list = Rect {
            x: 1,
            y: 1,
            width: 32,
            height: 10,
        };
        app.layout.review = Rect {
            x: 0,
            y: 1,
            width: 100,
            height: 20,
        };
        app
    }

    /// The change bar marks the first row of every section, and clicking it
    /// asks before anything is thrown away.
    #[test]
    fn the_change_bar_marks_each_section_and_asks_before_reverting() {
        let mut app = revert_app();
        let bars: Vec<Option<bool>> = (0..app.display.len()).map(|i| app.change_bar(i)).collect();
        assert_eq!(
            bars,
            vec![
                None,
                Some(true),
                None,
                None,
                None,
                Some(true),
                Some(false),
                None
            ],
            "↺ on the first row of each section, a plain bar below it"
        );

        // Clicking the bar beside the second section (display row 5) offers
        // to revert exactly that section — the cursor is somewhere else
        // entirely, and that must not matter.
        app.diff_cursor = 0;
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 35, 6));
        let Overlay::Revert(prompt) = &app.overlay else {
            panic!("expected a confirm prompt, status: {}", app.status);
        };
        assert_eq!(prompt.target, RevertTarget::Section { start: 5, end: 7 });
        assert_eq!(prompt.path, "a.txt");
        assert_eq!((prompt.adds, prompt.dels), (2, 1));
        assert!(!prompt.deletes);
        assert!(app.selection.is_none(), "the bar is not the diff body");

        // Esc backs out and says so; nothing was touched.
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.status.contains("Left alone"), "{}", app.status);

        // The bar beside an unchanged line has nothing to offer.
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 34, 1));
        assert!(matches!(app.overlay, Overlay::None));
        assert!(
            app.status_err && app.status.contains("No change"),
            "{}",
            app.status
        );

        // `u` works off the cursor for keyboard users.
        app.diff_cursor = 1;
        app.handle_key(key(KeyCode::Char('u')));
        assert!(matches!(
            &app.overlay,
            Overlay::Revert(p) if p.target == RevertTarget::Section { start: 1, end: 2 }
        ));
    }

    /// A read-only review has no working tree to put back: no change bar, no
    /// stolen clicks, and a reason rather than silence.
    #[test]
    fn a_read_only_review_offers_no_revert() {
        let mut app = revert_app();
        app.local = false;
        app.checked_out = false;
        assert_eq!(app.revert_gutter(), 0);
        assert!(!app.can_revert());

        // The columns the change bar would have taken belong to the diff
        // again: this click selects a line instead of opening a prompt.
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 35, 6));
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.selection.is_some(), "the click reached the diff");

        app.handle_key(key(KeyCode::Char('u')));
        assert!(matches!(app.overlay, Overlay::None));
        assert!(
            app.status_err && app.status.contains("read-only"),
            "{}",
            app.status
        );
    }

    /// The file list's ↺ column reverts the whole file, and says plainly
    /// when that means deleting it.
    #[test]
    fn the_file_list_offers_a_whole_file_revert() {
        let mut app = revert_app();
        let fl = app.layout.file_list;
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            fl.x + fl.width - 1,
            fl.y,
        ));
        let Overlay::Revert(prompt) = &app.overlay else {
            panic!("expected a confirm prompt, status: {}", app.status);
        };
        assert_eq!(prompt.target, RevertTarget::File { idx: 0 });
        assert!(!prompt.deletes);

        // A file the change created has nothing to go back to.
        app.overlay = Overlay::None;
        app.files = vec![ChangedFile {
            path: "new.txt".into(),
            status: "added".into(),
            additions: 3,
            deletions: 0,
            previous: None,
        }];
        app.rebuild_entries();
        app.ask_revert_file(0);
        let Overlay::Revert(prompt) = &app.overlay else {
            panic!("expected a confirm prompt");
        };
        assert!(prompt.deletes, "a new file is deleted, not emptied");
        assert_eq!((prompt.adds, prompt.dels), (3, 0));

        // Clicking the same column while the review is read-only explains
        // itself instead of doing nothing.
        app.overlay = Overlay::None;
        app.checked_out = false;
        app.ask_revert_file(0);
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.status_err, "{}", app.status);
    }

    /// End to end: confirm the prompt, let the worker run, and the file on
    /// disk has exactly that one section put back — the rest of the change
    /// is still there.
    #[test]
    fn reverting_a_section_rewrites_only_that_section_on_disk() {
        let dir = std::env::temp_dir().join(format!("loupe-revert-app-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let new = "a\nB\nc\nd\ne\nF1\nF2\ng\n";
        std::fs::write(dir.join("a.txt"), new).unwrap();

        let mut app = revert_app();
        app.repo_root = dir.clone();
        app.diff_scroll = 2;
        app.diff_cursor = 5;
        app.ask_revert_section(5);
        app.confirm_revert();
        for _ in 0..300 {
            if app.job.is_none() {
                break;
            }
            app.poll_jobs();
            thread::sleep(Duration::from_millis(10));
        }
        assert!(app.job.is_none(), "the revert finished");
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "a\nB\nc\nd\ne\nf\ng\n",
            "the second section went back; the first one stayed"
        );
        assert!(app.status.contains("Reverted"), "{}", app.status);
        // The panel's counts are re-read from the diff that just landed, so
        // the two can't disagree.
        let diff = app.diff.as_ref().expect("the file was reloaded");
        assert_eq!(app.files[0].additions as usize, diff.additions);
        assert_eq!(app.files[0].deletions as usize, diff.deletions);
        // And the reader is still roughly where they were.
        assert!(app.diff_scroll <= 2 && app.diff_cursor <= 5);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A revert that finds the file changed underneath it stops rather than
    /// writing a stale row model over someone else's edit.
    #[test]
    fn a_stale_diff_refuses_to_write() {
        let dir = std::env::temp_dir().join(format!("loupe-revert-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // What is on disk is not what the diff on screen was built from.
        std::fs::write(dir.join("a.txt"), "something else entirely\n").unwrap();

        let mut app = revert_app();
        app.repo_root = dir.clone();
        app.ask_revert_section(1);
        app.confirm_revert();
        for _ in 0..300 {
            if app.job.is_none() {
                break;
            }
            app.poll_jobs();
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "something else entirely\n",
            "the file is untouched"
        );
        assert!(
            app.status_err && app.status.contains("changed on disk"),
            "{}",
            app.status
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_outside_the_change_opens_in_the_editor() {
        let mut app = folded_app();
        app.files = vec![cf("test.rs"), cf("other.rs")];
        app.rebuild_entries();
        app.file_cursor = 1;
        app.diff_scroll = 4;
        let files_before = app.files.len();
        let diff_before = app.display.len();

        app.apply_external(ExternalFile {
            path: "vendor/untouched.ts".into(),
            abs_path: "/repo/vendor/untouched.ts".into(),
            content: "one\ntwo\nthree\nfour\n".into(),
            line: Some(3),
            read_only: false,
            preview: false,
        });
        let ed = app.editor.as_ref().expect("the editor is open");
        assert!(ed.standalone, "it is not the changeset file");
        assert!(!ed.read_only);
        assert_eq!(ed.path, "vendor/untouched.ts");
        assert_eq!(ed.textarea.cursor().0, 2, "landed on line 3");

        // The review underneath is untouched — that is the whole point of
        // using the editor instead of a diff of the file against itself.
        assert_eq!(app.files.len(), files_before);
        assert_eq!(app.file_cursor, 1);
        assert_eq!(app.diff_scroll, 4);
        assert_eq!(app.display.len(), diff_before);

        // So closing it needs no reload and loses nothing.
        app.handle_key(key(KeyCode::Esc));
        assert!(app.editor.is_none());
        assert!(app.job.is_none(), "no reload was needed");
        assert_eq!(app.file_cursor, 1);
        assert_eq!(app.diff_scroll, 4);
    }

    #[test]
    fn a_file_from_a_commit_opens_read_only() {
        let mut app = folded_app();
        app.apply_external(ExternalFile {
            path: "vendor/x.ts".into(),
            abs_path: "/repo/vendor/x.ts".into(),
            content: "one\n".into(),
            line: None,
            read_only: true,
            preview: false,
        });
        assert!(app.editor.as_ref().unwrap().read_only);
        assert!(app.status.contains("read-only"));
        // Saving is refused rather than writing another branch's file.
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(app.job.is_none(), "no write job was started");
        assert!(app.status_err && app.status.contains("read-only"));
    }

    #[test]
    fn opening_a_hit_outside_the_changeset_opens_a_file() {
        let mut app = folded_app();
        app.files = vec![cf("test.rs")];
        app.rebuild_entries();
        app.open_finder(FinderMode::Grep);
        // A path that is not under review: this has to open the file,
        // not diff it against a base it has no place in.
        app.open_hit("vendor/elsewhere.ts".into(), Some(12));
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.job.is_some(), "a load job was started");
        assert!(app.job.as_ref().unwrap().label.contains("elsewhere.ts"));

        // A path that *is* under review loads the file and remembers the
        // line to land on.
        app.job = None;
        app.file_cursor = 0;
        app.diff = None;
        app.open_hit("test.rs".into(), Some(7));
        assert_eq!(app.pending_jump, Some(7));
    }

    // ------------------------------------------------------------ blame

    fn commit(sha: &str, author: &str) -> Arc<blame::Commit> {
        Arc::new(blame::Commit {
            sha: sha.into(),
            author: author.into(),
            author_email: format!("{}@test", author.to_lowercase()),
            author_time: blame::now() - 60 * 60 * 24,
            summary: format!("subject (#{})", sha.len()),
            pr: None,
        })
    }

    fn blame_of(commits: &[Arc<blame::Commit>]) -> Blame {
        Blame {
            lines: commits
                .iter()
                .map(|c| blame::BlameLine { commit: c.clone() })
                .collect(),
        }
    }

    /// A review of one modified line, with both sides blamed.
    fn blamed_app() -> App {
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.checked_out = true;
        app.files = vec![cf("f.txt")];
        app.rebuild_entries();
        app.collapse_unchanged = false;
        app.diff = Some(FileDiff::compute(Some("a\nb\n"), Some("a\nB\n")));
        app.rebuild_display();
        app.blame_on = true;
        app.blame_new = Some(blame_of(&[commit("new1", "Ann"), commit("new2", "Bob")]));
        app.blame_old = Some(blame_of(&[commit("old1", "Ann"), commit("old2", "Cid")]));
        app
    }

    /// Which side a row is blamed on follows what the row shows: an
    /// inline row names its own side, and a split row prefers the new
    /// side, falling back to the old one for a purely removed line.
    #[test]
    fn a_row_is_blamed_on_the_side_it_shows() {
        let mut app = blamed_app();

        app.view = ViewMode::Inline;
        app.rebuild_display();
        // Rows: context line 1 (new), removed line 2 (old), added line 2.
        assert_eq!(app.blame_for_row(0).unwrap().author, "Ann");
        assert_eq!(
            app.blame_for_row(1).unwrap().author,
            "Cid",
            "a removed line is blamed on the old side"
        );
        assert_eq!(app.blame_for_row(2).unwrap().author, "Bob");

        app.view = ViewMode::SideBySide;
        app.rebuild_display();
        // Rows: context, then the modification — the new side wins.
        assert_eq!(app.blame_for_row(0).unwrap().author, "Ann");
        assert_eq!(app.blame_for_row(1).unwrap().author, "Bob");
        assert!(app.blame_for_row(99).is_none(), "past the end");
    }

    /// A pure deletion has no new side to prefer, so the split view falls
    /// back to the old one — the only side that can say what is going.
    #[test]
    fn a_deleted_line_is_blamed_on_the_old_side_in_split_view() {
        let mut app = blamed_app();
        app.view = ViewMode::SideBySide;
        app.diff = Some(FileDiff::compute(Some("a\nb\n"), Some("a\n")));
        app.rebuild_display();
        assert_eq!(app.blame_for_row(1).unwrap().author, "Cid");
    }

    /// Clicking a blame row opens the commit behind it, with the ways to
    /// follow it. The pull request line is there only when one is known.
    #[test]
    fn clicking_a_blame_row_opens_the_commit() {
        let mut app = blamed_app();
        app.view = ViewMode::Inline;
        app.rebuild_display();
        app.layout.blame = Rect::new(20, 3, 30, 10);
        app.repo = Some("acme/tool".into());
        app.blame_prs.insert(
            "new2".into(),
            PrRef {
                number: 412,
                title: "Fix the parser".into(),
                url: "https://github.com/acme/tool/pull/412".into(),
            },
        );

        // Row 2 of the diff is the added line, blamed on "new2".
        app.open_blame_menu(22, 5);
        let Overlay::BlameMenu(menu) = &app.overlay else {
            panic!("a click on the pane must open the commit");
        };
        assert_eq!(menu.commit.author, "Bob");
        assert_eq!(menu.pr.as_ref().unwrap().number, 412);
        let keys: Vec<char> = menu.items.iter().map(|it| it.key).collect();
        assert_eq!(keys, vec!['o', 'y', 'c'], "open, copy link, copy hash");
        assert_eq!(
            menu.items[1].action,
            BlameAction::Copy("https://github.com/acme/tool/pull/412".into())
        );

        // A commit with no pull request offers only the hash.
        app.overlay = Overlay::None;
        app.open_blame_menu(22, 3);
        let Overlay::BlameMenu(menu) = &app.overlay else {
            panic!("still opens");
        };
        assert_eq!(menu.commit.author, "Ann");
        assert!(menu.pr.is_none());
        assert_eq!(menu.items.len(), 1, "only the hash is on offer");
    }

    /// The pane is off until asked for, and the ☰ menu shows the same
    /// state the key sets.
    #[test]
    fn the_blame_switch_is_off_until_asked_for() {
        let mut app = blamed_app();
        app.blame_on = false;
        assert_eq!(app.blame_gutter(), 0);

        app.handle_key(key(KeyCode::Char('B')));
        assert!(app.blame_on);
        let on = app
            .build_menu()
            .iter()
            .any(|r| matches!(r, MenuRow::Item(it) if it.id == ButtonId::BlameToggle && it.checked == Some(true)));
        assert!(on, "the menu line shows the pane is on");

        app.handle_key(key(KeyCode::Char('B')));
        assert!(!app.blame_on);
        assert!(
            app.blame_new.is_none(),
            "and the pane's contents go with it"
        );
    }

    /// The commit subjects are the free half of the pull request lookup;
    /// only what they miss is worth a GitHub call.
    #[test]
    fn subjects_seed_the_pull_request_map_before_github_is_asked() {
        let mut app = blamed_app();
        app.repo = Some("acme/tool".into());
        let mut named = commit("abc", "Ann");
        Arc::get_mut(&mut named).unwrap().pr = Some(77);
        app.blame_new = Some(blame_of(&[named, commit("def", "Bob")]));

        app.seed_blame_prs();
        assert_eq!(app.blame_prs["abc"].number, 77);
        assert_eq!(
            app.blame_prs["abc"].url,
            "https://github.com/acme/tool/pull/77"
        );
        assert!(
            !app.blame_prs.contains_key("def"),
            "a subject with no number is left for the lookup"
        );
    }

    /// A GitHub Enterprise host has to survive: the open pull request's
    /// own url is the template, not github.com.
    #[test]
    fn pull_request_links_keep_the_host_of_the_review() {
        let mut app = blamed_app();
        app.repo = Some("acme/tool".into());
        assert_eq!(app.pr_link(9), "https://github.com/acme/tool/pull/9");

        app.pr = Some(PrDetail {
            url: "https://git.acme.example/acme/tool/pull/3".into(),
            ..pr_detail(3)
        });
        assert_eq!(app.pr_link(9), "https://git.acme.example/acme/tool/pull/9");
    }
}

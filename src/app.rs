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
use crate::conflict::{Conflicted, Resolution};
use crate::ctx;
use crate::diff::{DisplayEntry, FileDiff, Layer, Pos, RowKind, Selection, Side};
use crate::editor::Editor;
use crate::github::{
    self, ChangedFile, CommentSide, PrDetail, PrRef, PrSummary, ReviewComment, Verdict,
};
use crate::gitops::{self, Commit, MergeOp, StageState, Stash, StashScope, Tracking};
use crate::highlight::{self, HlLine};
use crate::linter;
use crate::lsp::{self, Lsp};
use crate::markdown;
use crate::pins::{self, Pin, Pins};
use crate::preview::{self, Preview};
use crate::search;
use crate::theme::Appearance;
use anyhow::Result;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
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

/// Which list the file panel is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelMode {
    /// The files this change touches. The panel loupe has always had.
    Changes,
    /// Every file in the repository.
    Files,
    /// The commits on this branch the upstream does not have. Open one to
    /// see its files; open a file to read that commit's diff.
    Commits,
    /// The stack the open pull request belongs to, drawn bottom-first as
    /// the ladder it is. Only reachable while there is a stack to draw.
    Stack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    SideBySide,
    Inline,
}

/// How many layers of the change the diff draws.
///
/// A branch under review has two steps in it, not one: what the remote
/// already has, and what the working tree has done since. Every diff tool
/// shows the sum of the two and calls it the change, which answers "what
/// does this branch do" and cannot answer "am I rewriting what I already
/// wrote". These three settings are that second question, and `s` walks
/// between them. See [`crate::diff::Layer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layers {
    /// One layer: the change as a whole, the way every diff tool draws
    /// it. What loupe has always shown.
    #[default]
    Off,
    /// Only what the working tree has changed since that middle version,
    /// with the lines it had already changed marked. The quiet view:
    /// your own newer edits, and which of them are second attempts.
    Mine,
    /// All three versions at once: the base branch on the left, the
    /// working tree on the right, and each row painted by the step that
    /// wrote it.
    Full,
}

impl Layers {
    /// The next setting `s` moves to.
    pub fn next(self) -> Self {
        match self {
            Layers::Off => Layers::Full,
            Layers::Full => Layers::Mine,
            Layers::Mine => Layers::Off,
        }
    }

    /// What the status line and the ☰ menu call it.
    pub fn label(self) -> &'static str {
        match self {
            Layers::Off => "off",
            Layers::Full => "the whole stack",
            Layers::Mine => "only what is new",
        }
    }

    pub fn on(self) -> bool {
        self != Layers::Off
    }
}

pub struct CommentDraft {
    pub textarea: TextArea<'static>,
    pub path: String,
    pub side: Side,
    pub lo: usize,
    pub hi: usize,
}

// ----------------------------------------------------------- the review box

/// The review composer at the foot of the file panel: a summary to say
/// something about the pull request as a whole, and the verdict to send
/// with it.
///
/// Inline comments answer "this line is wrong". This answers "should this
/// be merged" — which is the half of a review the diff pane has no place
/// for, and the half GitHub needs before anything is actually said.
pub struct ReviewBox {
    pub textarea: TextArea<'static>,
    /// What the button will send.
    pub verdict: Verdict,
    /// True while the verdict dropdown is open under the ▾.
    pub picking: bool,
    /// Which line of that dropdown is highlighted.
    pub pick: usize,
    /// True while the box has the keyboard, so typing goes here rather
    /// than to the diff.
    pub focused: bool,
}

impl ReviewBox {
    fn new() -> Self {
        let mut textarea = TextArea::default();
        // Short on purpose: the box is as wide as the file panel, and a
        // placeholder that does not fit is cut off rather than wrapped.
        textarea.set_placeholder_text("Summary of this review…");
        ReviewBox {
            textarea,
            verdict: Verdict::Comment,
            picking: false,
            pick: 0,
            focused: false,
        }
    }

    /// The summary as it stands.
    pub fn body(&self) -> String {
        self.textarea.lines().join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.body().trim().is_empty()
    }

    fn clear(&mut self) {
        let mut fresh = ReviewBox::new();
        fresh.verdict = self.verdict;
        *self = fresh;
    }
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
    /// How to resolve one merge conflict, or the whole conflicted file.
    ConflictMenu(Box<ConflictMenu>),
    /// "Send this review?" — everything that is about to go to GitHub,
    /// listed, before any of it does.
    ReviewConfirm(Box<ReviewPrompt>),
    /// The verdict list under the review box's ▾.
    VerdictMenu,
    /// The fixes and refactors a language server offers here.
    CodeActions(Box<CodeActionMenu>),
    /// One problem, laid out: the claim, then each reason under the one
    /// it explains. See [`crate::explain`].
    Problem(Box<ProblemPanel>),
    /// "Open a file by path" (`Ctrl+O`) — one line to type or paste a path
    /// into. It is the way in for a terminal that cannot report a drop,
    /// and for a path an agent just printed.
    OpenPath(Box<OpenPathBox>),
}

/// What the one-line path box is asking for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PathBoxKind {
    /// `Ctrl+O`: open whatever path is typed.
    #[default]
    Open,
    /// A new file in this directory. Empty for the repository root.
    NewFile { dir: String },
    /// A new name for a file that exists.
    Rename { from: String },
    /// A new name for the symbol under the editor's cursor.
    RenameSymbol { word: String },
    /// What to call the stash about to be made. Empty is allowed here and
    /// nowhere else: git writes its own `WIP on <branch>` name when you
    /// give it none, so Enter on an empty box is a real answer.
    StashName { scope: StashScope },
}

/// The fixes and refactors on offer, and which is selected.
pub struct CodeActionMenu {
    pub actions: Vec<lsp::CodeAction>,
    pub sel: usize,
}

/// The one-line path box behind `Ctrl+O`.
#[derive(Default)]
pub struct OpenPathBox {
    /// What the box is asking for.
    pub kind: PathBoxKind,
    /// What has been typed so far.
    pub input: String,
    /// Caret position, counted in characters rather than bytes so a path
    /// with an accent in it still edits one character at a time.
    pub caret: usize,
}

impl OpenPathBox {
    fn insert(&mut self, text: &str) {
        let at = self.byte_at(self.caret);
        self.input.insert_str(at, text);
        self.caret += text.chars().count();
    }

    fn backspace(&mut self) {
        if self.caret == 0 {
            return;
        }
        let end = self.byte_at(self.caret);
        let start = self.byte_at(self.caret - 1);
        self.input.replace_range(start..end, "");
        self.caret -= 1;
    }

    fn delete(&mut self) {
        let len = self.input.chars().count();
        if self.caret >= len {
            return;
        }
        let start = self.byte_at(self.caret);
        let end = self.byte_at(self.caret + 1);
        self.input.replace_range(start..end, "");
    }

    fn move_caret(&mut self, delta: i32) {
        let len = self.input.chars().count() as i32;
        self.caret = (self.caret as i32 + delta).clamp(0, len) as usize;
    }

    fn byte_at(&self, chars: usize) -> usize {
        self.input
            .char_indices()
            .nth(chars)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }
}

/// What a review submit is about to send. Captured when the prompt opens,
/// so the sentence on screen and the request cannot describe two different
/// things.
pub struct ReviewPrompt {
    pub number: u64,
    pub verdict: Verdict,
    pub body: String,
    pub comments: Vec<ReviewComment>,
    /// The pull request head moved after these comments were written, so
    /// they may no longer point at lines that exist.
    pub stale: bool,
}

// --------------------------------------------------------------- conflicts

/// What a line of the conflict menu does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictAction {
    /// Keep one side of the conflict under the cursor.
    Take(Resolution),
    /// Keep one side of every conflict in the file at once.
    TakeAll(Resolution),
    /// Resolve the whole path from the index instead of from the marker
    /// text — the answer for a conflict markers cannot describe.
    TakeSide { ours: bool },
    /// Open the raw file, markers and all, in the editor.
    EditByHand,
    /// `git add` the file, telling git the conflict is settled.
    MarkResolved,
}

pub struct ConflictItem {
    /// The key that runs this line on its own.
    pub key: char,
    pub label: String,
    /// The second, greyed line under the label. Empty for none.
    pub note: String,
    pub act: ConflictAction,
}

/// The resolve menu for a conflict: which side to keep, and the ways to
/// settle the file as a whole.
pub struct ConflictMenu {
    pub path: String,
    /// The conflict the menu was opened on, as an index into the parsed
    /// file. `None` when it was opened away from one — the whole-file
    /// lines are then all it offers.
    pub hunk: Option<usize>,
    /// Heading above the lines — which conflict of how many, or the file.
    pub title: String,
    pub items: Vec<ConflictItem>,
    pub sel: usize,
    /// The cell that was clicked. The popup is drawn next to it.
    pub anchor: (u16, u16),
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
    /// What the border says. The ☰ menu says `☰ Menu`; a right-click
    /// menu names the word it is about, so the reader can see that the
    /// click landed on the symbol they meant.
    pub title: String,
    /// Flip above the anchor when there is no room below it. False for
    /// the ☰ menu, which has to leave its own button visible; true for a
    /// menu opened at the pointer, which has nothing to keep in view.
    pub flip: bool,
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
    pub action: PathAction,
}

/// What a line of the file-panel menu does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathAction {
    /// Put a path on the clipboard.
    Copy(String),
    /// Ask for a name, then create an empty file beside this row.
    NewFile,
    /// Ask for a new name for this file.
    Rename,
    /// Offer to delete it. Choosing this only *asks* — see
    /// [`PathAction::DeleteConfirmed`].
    Delete,
    /// Delete it, having asked. This is the only line loupe puts behind a
    /// second press, because it is the only one that destroys a file.
    DeleteConfirmed,
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

/// One problem, opened to be read rather than glanced at.
///
/// The gutter mark says something is wrong, the margin says what, and
/// the status bar says it again for the line the cursor is on. This is
/// for the messages none of those can hold: a type mismatch whose real
/// answer is three levels down inside the sentence.
pub struct ProblemPanel {
    pub rows: Vec<crate::explain::Row>,
    /// `typescript(2322)` — who complained and what they call it.
    pub code: Option<String>,
    pub severity: u8,
    pub line: usize,
    /// How many problems the line has, so the panel can say when it is
    /// showing the worst of several.
    pub of: usize,
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
    /// Every problem the language server found in the open file.
    /// Arrived at the same way `Refs` is.
    Problems,
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
            FinderMode::Refs | FinderMode::Pick | FinderMode::Problems => "",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            FinderMode::Files => "Go to file",
            FinderMode::Grep => "Find in files",
            FinderMode::Symbols => "Symbols in this file",
            FinderMode::Refs => "References",
            FinderMode::Problems => "Problems",
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

/// How long typing has to pause before suggestions are fetched.
///
/// Shorter than the buffer sync above, because the popup is what the
/// typist is waiting for rather than something happening behind them.
/// Long enough that typing a ten-letter name costs one round trip and
/// not ten.
const SUGGEST_DEBOUNCE: Duration = Duration::from_millis(90);

/// How many characters of a word to wait for before suggesting on the
/// strength of the word alone.
///
/// One. A trigger character (`.`, `::`) suggests on its own with no
/// prefix at all; this is the rule for an ordinary name being typed,
/// where a popup on the empty string would just be the whole scope.
const SUGGEST_AFTER: usize = 1;

/// How long typing has to pause before the linter runs.
///
/// Longer than either of the above: this costs a process, not a pipe
/// write, and a lint warning is worth having a moment later than a
/// compiler error.
const LINT_DEBOUNCE: Duration = Duration::from_millis(700);

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
            FinderMode::Refs | FinderMode::Pick | FinderMode::Problems => {
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
/// Which syntax theme goes with each appearance.
///
/// Kept on the app rather than only in `Startup`, because the appearance
/// can change while loupe is running — a system that turns dark at seven
/// takes the terminal with it — and the matching theme has to be
/// reachable without going back to the config file.
#[derive(Default, Clone, Copy)]
pub struct Themes {
    pub dark: Option<two_face::theme::EmbeddedThemeName>,
    pub light: Option<two_face::theme::EmbeddedThemeName>,
    /// `--theme`: overrides both, for this session only.
    pub session: Option<two_face::theme::EmbeddedThemeName>,
}

impl Themes {
    /// The theme to use on `appearance`.
    ///
    /// A slot that is set wins. An empty slot takes the counterpart of
    /// the other one — so Gruvbox stays Gruvbox across the change rather
    /// than falling back to a default nobody chose — and an empty pair
    /// takes the default for that appearance.
    pub fn for_appearance(&self, appearance: Appearance) -> two_face::theme::EmbeddedThemeName {
        if let Some(theme) = self.session {
            return theme;
        }
        let (own, other) = match appearance {
            Appearance::Dark => (self.dark, self.light),
            Appearance::Light => (self.light, self.dark),
        };
        own.unwrap_or_else(|| match other {
            Some(theme) => highlight::for_appearance(theme, appearance),
            None => highlight::default_theme(appearance),
        })
    }
}

pub struct ThemePicker {
    pub sel: usize,
    pub scroll: usize,
    prev: two_face::theme::EmbeddedThemeName,
    prev_appearance: Appearance,
    /// What the picker will write for `appearance`: follow the terminal,
    /// or pin one. It starts on whatever is already in force, so the row
    /// answers "why is it stuck light?" without anybody going to look.
    pub auto: bool,
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
    fn new(auto: bool) -> Self {
        let current = highlight::current_theme();
        ThemePicker {
            sel: highlight::THEMES
                .iter()
                .position(|(_, t)| *t == current)
                .unwrap_or(0),
            scroll: 0,
            prev: current,
            prev_appearance: crate::theme::appearance(),
            auto,
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

    /// Pin the appearance the picker is showing, rather than following
    /// the terminal.
    fn pin_appearance(&mut self) {
        self.auto = false;
    }

    /// Follow the terminal again, and move the preview to whatever it
    /// says right now — the whole point is to see the answer.
    fn follow_terminal(&mut self, found: Appearance) {
        self.auto = true;
        if crate::theme::appearance() != found {
            self.toggle_appearance();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonId {
    ViewTree,
    /// Copy the block that tells an agent what is on screen.
    CopyContext,
    ViewFlat,
    /// The Change/Files/Commits toggle on the file panel's bottom border.
    PanelChanges,
    PanelFiles,
    PanelCommits,
    /// The panel toggle row's fourth button, and the ☰ line: the stack
    /// the open pull request belongs to.
    PanelStack,
    /// The `▾` the panel toggle row collapses into when it has no room
    /// for a button per section. Opens the sections menu.
    PanelMenu,
    /// Editor commands that have no button of their own, reached from ☰.
    EditorDefinition,
    EditorHover,
    EditorFind,
    EditorComment,
    EditorReferences,
    EditorRename,
    EditorActions,
    EditorSignature,
    /// Copy the selection, or the cursor line — the right-click menu's
    /// version of the editor's Ctrl+C.
    EditorCopy,
    /// Walk the problems the language server found in this file.
    EditorNextProblem,
    EditorPrevProblem,
    /// All of them at once, as a list to pick from.
    EditorProblems,
    /// Lay one problem out and read it properly.
    EditorExplain,
    BufferNext,
    BufferPrev,
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
    /// The Auto switch beside it: follow the terminal instead of pinning.
    AppearanceAuto,
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
    /// A line of the conflict resolve menu.
    ConflictMenuRow(usize),

    // --- the review box at the foot of the file panel
    /// The summary text box: a click gives it the keyboard.
    ReviewBody,
    /// The button itself — sends whatever verdict it is showing.
    ReviewSubmit,
    /// The ▾ beside it, which opens the verdict list.
    ReviewVerdict,
    /// A line of that list.
    VerdictRow(usize),
    /// One of the three verdicts, chosen from the ☰ menu.
    SetVerdict(Verdict),
    /// The held-comment count, which offers to throw them away.
    ReviewDiscard,
    /// The two buttons on the submit prompt.
    ReviewYes,
    ReviewCancel,
    /// The two buttons on an inline comment draft.
    CommentHold,
    /// One row that shows or hides the blame pane.
    BlameToggle,

    // --- the pinned-file tab row (see [`crate::pins`])
    /// A tab in the row: a click opens the file it holds.
    PinTab(usize),
    /// One of the open buffers, in the same row after the pins.
    BufferTab(usize),
    /// The ✕ on one of those tabs.
    BufferClose(usize),
    /// The ✕ on one tab, which unpins it.
    PinClose(usize),
    /// 📌 in the top bar, and the ☰ row beside it: pin the file in front
    /// of the reader, or unpin it when it is already pinned.
    PinToggle,
    /// The ☰ row that opens the path box (`Ctrl+O`).
    PinOpenPath,
    /// The two buttons on that box.
    OpenPathGo,
    OpenPathCancel,

    // --- the ☰ menu and the lines only it offers
    /// ☰ in the top bar.
    Menu,
    /// A line of the ☰ menu, by index into [`Menu::rows`].
    MenuRow(usize),
    /// One row that flips between split and inline (the toolbar used to
    /// spend two buttons on this).
    ViewToggle,
    /// ☰ → the layers line: one layer of the change, or three.
    LayersCycle,
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
    /// ⚑ — the resolve menu for the conflict at the cursor, and for the
    /// whole conflicted file.
    ResolveConflict,
    ResolveFile,
    /// The idle re-scan switch (see `App::auto_refresh`).
    AutoRefreshToggle,
    /// ✚ all / ↩ all — move a whole staging section across, and the
    /// menu's line for the one file at the cursor.
    StageFile,
    StageAll,
    UnstageAll,
    /// 📦 in the ☰ menu: the stash menu.
    StashMenu,
    /// Make a stash of this scope. The name box opens first.
    StashPush(StashScope),
    /// One stash in the list: a click opens what can be done with it.
    StashPick(usize),
    StashApply(usize),
    StashPop(usize),
    StashDrop(usize),
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
    /// The review composer at the foot of the file panel. Zero-sized when
    /// it is not drawn.
    pub review_box: Rect,
    /// The blame pane, when it is showing. Zero-sized when it is not.
    pub blame: Rect,
    /// The row of pinned-file tabs. Zero-sized when nothing is pinned,
    /// which is also when it takes no height off the window.
    pub pin_row: Rect,
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

/// Whether this key means "find in what I am reading".
///
/// `Ctrl+F` is the one that always arrives. `Cmd+F` is what a mac reader
/// reaches for, so it is accepted too — but most terminals keep Cmd+F for
/// their own find bar and never forward it, and none report it at all
/// without the kitty keyboard protocol. It costs one line to honour the
/// ones that do.
pub fn is_find_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('f') | KeyCode::Char('F'))
        && (key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::SUPER))
        && !key.modifiers.contains(KeyModifiers::SHIFT)
}

/// The same key with shift: find across every file, rather than in this
/// one. It is the other half of the pair every editor has.
pub fn is_find_all_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('F') | KeyCode::Char('f'))
        && (key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::SUPER))
        && key.modifiers.contains(KeyModifiers::SHIFT)
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

/// Width of the action at the right-hand end of a staging heading — `✚ all`
/// and `↩ all`, and the click target that runs them.
pub const SECTION_ACTION_W: u16 = 6;

/// How old the repository listing may get before coming back to the file
/// panel re-reads it. Long, because the read costs about 93 ms on a
/// 16,761-file repository and nothing on screen goes wrong while it is
/// out of date — a file added a minute ago simply is not listed yet.
const REPO_LISTING_MAX_AGE: Duration = Duration::from_secs(60);

/// How often the terminal is asked what its background is now.
///
/// A reader whose system turns dark in the evening wants loupe to follow
/// without being restarted. Half a minute is soon enough not to be
/// noticed and rare enough to cost nothing.
const APPEARANCE_POLL: Duration = Duration::from_secs(30);

/// How long the reader has to have been idle before that question is
/// asked. See [`App::wants_appearance_check`].
const APPEARANCE_IDLE: Duration = Duration::from_secs(5);

/// How old the commit list may get before coming back to the Commits
/// panel re-reads it. A commit lands when somebody runs `git commit`,
/// which loupe never does, so the list is only ever stale by somebody
/// else's doing.
const COMMIT_LIST_MAX_AGE: Duration = Duration::from_secs(30);

/// How many paths the file tree will hold whole before it starts reading
/// one directory at a time instead.
///
/// Measured, the tree costs 48 ms to build at 400,000 paths and 2 µs to
/// emit collapsed, so the build is not the problem — the memory is. Every
/// path is a `String` and every directory a `BTreeMap` node, and a
/// repository this size is one where somebody opens two folders and never
/// looks at the rest.
///
/// Below this the whole tree is faster: expanding costs 2 µs from the
/// cache against about 10 ms for a `read_dir` and its subprocess. Above
/// it, that trade turns over.
const MAX_EAGER_PATHS: usize = 200_000;
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

/// The shortest run of typed characters that is worth testing as a
/// dropped path. `/a/b.md` is 7, and nothing a reader types on purpose
/// gets near the real bar — an absolute path to a file that exists.
const MIN_TYPED_DROP: usize = 6;

/// How long to wait for the rest of a path that is being typed in one
/// character at a time. A long path, or a slow link, can straddle two
/// reads, and half a path is not a drop.
pub const TYPED_DROP_GAP: Duration = Duration::from_millis(30);

/// The characters an absolute path can start with, once a terminal has
/// had its way with it: the path itself, a home-relative path, either
/// quote, a backslash escape, or a `file://` URL.
fn starts_like_path(text: &str) -> bool {
    matches!(text.chars().next(), Some('/' | '~' | '\'' | '"' | '\\'))
        || "file://".starts_with(&text[..text.len().min(7)])
}

/// True when the batch so far could be the beginning of a path being
/// typed in, and is therefore worth waiting a moment to finish.
///
/// Ordinary typing never reaches here: one key press is not a path, and
/// two characters only arrive together when something wrote them
/// together.
pub fn partial_typed_path(events: &[Event]) -> bool {
    match typed_text(events) {
        Some(text) => text.len() >= 2 && starts_like_path(&text),
        None => false,
    }
}

/// The text of a batch that is nothing but plain typed characters, or
/// `None` when it holds anything else.
fn typed_text(events: &[Event]) -> Option<String> {
    let mut text = String::new();
    for ev in events {
        match ev {
            Event::Key(k) if k.kind == KeyEventKind::Release => {}
            Event::Key(k) => {
                // A modifier means a command, not a character.
                if k.modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    return None;
                }
                match k.code {
                    KeyCode::Char(c) => text.push(c),
                    // Some terminals finish a dropped path with a return.
                    KeyCode::Enter if !text.is_empty() => {}
                    _ => return None,
                }
            }
            // These say nothing either way, and a resize in the middle of
            // a drop should not break it.
            Event::Resize(..) | Event::FocusGained | Event::FocusLost => {}
            _ => return None,
        }
    }
    (!text.is_empty()).then_some(text)
}

/// The path a run of plain characters spells, when they arrived together
/// and name a file on disk — a file dropped on a terminal that does not
/// use bracketed paste.
///
/// The bar is deliberately high: every event in the batch has to be an
/// unmodified printable character, and what they spell has to be an
/// absolute path to a file that exists. No run of loupe's own keys can
/// clear that, so nothing a reader actually types is taken for a drop.
fn typed_path_burst(events: &[Event]) -> Option<String> {
    let text = typed_text(events)?;
    if text.trim().len() < MIN_TYPED_DROP {
        return None;
    }
    pins::dropped_paths(&text).is_some().then_some(text)
}

/// True if `path` itself is a symlink (without following it).
fn is_symlink(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

// ------------------------------------------------------------------ file list

/// What a file row in the panel points at.
///
/// A row in the change under review carries an index into [`App::files`],
/// and the diff, the stage mark, the viewed mark and the revert all key
/// off it. A row that is only a file in the repository carries an index
/// into [`App::repo_paths`] and has none of those — asking it for a
/// `ChangedFile` gives `None`, which is the whole reason this is an enum
/// rather than the bare index it used to be.
///
/// Both arms are indices rather than owned paths on purpose. Rows are
/// emitted on every collapse toggle, so a `String` here would be one
/// allocation per visible row per click. The lists already own the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowSrc {
    Changed(usize),
    Path(usize),
}

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
        src: RowSrc,
        depth: u16,
    },
    /// The heading above the conflicted files, which are listed first in
    /// both the flat and the tree view. A merge conflict blocks the
    /// commit, so it never sits inside a folder the reader has to open.
    ConflictHeading {
        count: usize,
    },
    /// The heading above the staged or the unstaged half of a local
    /// review. It carries the count, the fold arrow, and the action that
    /// moves the whole section across.
    StageHeading {
        section: StageSection,
        count: usize,
        collapsed: bool,
    },
    /// One commit in the Commits panel. `idx` indexes [`App::commits`].
    CommitRow {
        idx: usize,
        open: bool,
    },
    /// One file inside an open commit.
    CommitFileRow {
        commit: usize,
        file: usize,
    },
    /// What the Commits panel says when it has no commits to list: still
    /// reading, nothing to push, or no upstream to measure against.
    CommitNote(String),
    /// One pull request in the Stack panel. `idx` indexes the stack's
    /// `entries`, which are held bottom-first; the panel draws them the
    /// other way up, because a stack is talked about with its trunk at
    /// the bottom.
    StackRow {
        idx: usize,
    },
    /// The branch the whole stack lands on, drawn under the bottom of the
    /// ladder. Not a target — there is no pull request to open.
    StackTrunk,
    /// What the Stack panel says when there is no stack to draw.
    StackNote(String),
}

/// Which half of the local file panel a heading names.
///
/// A pull request has no index, so it has no sections: the split is local
/// review only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StageSection {
    /// Files with something in the index — what `git commit` would take.
    Staged,
    /// Files whose change is only in the working tree.
    Unstaged,
}

impl StageSection {
    /// What the heading says.
    pub fn title(self) -> &'static str {
        match self {
            StageSection::Staged => "STAGED",
            StageSection::Unstaged => "UNSTAGED",
        }
    }

    /// The right-hand action on the heading: stage the whole section, or
    /// take the whole section back out.
    pub fn action_label(self) -> &'static str {
        match self {
            StageSection::Staged => " ↩ all",
            StageSection::Unstaged => " ✚ all",
        }
    }
}

/// One file open as a diff, and everything about how it is being read.
///
/// The same bundle the PR ⇄ local swap parks (see [`Workspace`]), for the
/// same reason: coming back to a file has to land the reader exactly
/// where they left it — same scroll, same cursor, same folds, same
/// selection — or the tab is a bookmark rather than a place.
pub struct DiffTab {
    pub path: String,
    /// The [`App::theme_gen`] its highlights were computed under. A tab
    /// parked before the terminal turned dark comes back in the old
    /// theme's colours unless it is re-highlighted first.
    theme_gen: u64,
    /// The commit it came out of, when it came from the Commits panel.
    open_commit: Option<OpenCommit>,
    conflict: Option<ConflictView>,
    diff: Option<FileDiff>,
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

/// The commit whose diff the pane is showing, and which of its files.
///
/// It carries the commit's own labels rather than an index into
/// [`App::commits`]: the list is re-read while a commit is open, and a
/// title that renumbered itself under the reader would be worse than one
/// that stayed put.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCommit {
    pub oid: String,
    pub short: String,
    pub subject: String,
    /// The commit's first parent — the old side of its diff. Empty for a
    /// root commit, which has none, so every file in it reads as added.
    pub parent: String,
    /// Index into that commit's own file list.
    pub file: usize,
}

/// What one read of the commit list came back with.
pub(crate) struct CommitsData {
    pub(crate) base: Option<String>,
    pub(crate) commits: Vec<Commit>,
}

/// One directory's contents, read on a worker thread.
struct DirRead {
    dir: String,
    files: Vec<String>,
    dirs: Vec<String>,
}

/// The names directly inside one directory: files first, then
/// subdirectories.
///
/// A symlinked directory counts as a file. Following one would let a link
/// pointing at an ancestor draw a tree with no bottom, and nothing here
/// needs to look inside it.
fn read_one_dir(abs: &Path) -> Result<(Vec<String>, Vec<String>)> {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(abs)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // `file_type` does not follow the link, which is the point.
        match entry.file_type() {
            Ok(t) if t.is_dir() => dirs.push(name),
            Ok(_) => files.push(name),
            Err(_) => continue,
        }
    }
    files.sort();
    dirs.sort();
    Ok((files, dirs))
}

/// The changeset's directory tree, built once per file list.
///
/// Building this is the expensive half of the file panel and emitting the
/// rows is the cheap half — on a 400,000-path tree the build costs 48 ms
/// and the emit costs about a microsecond. A collapse toggle only changes
/// which rows are visible, so it must never pay for the build. Hence the
/// split: [`TreeNodes::build`] runs when `App::files` changes, and
/// [`TreeNodes::emit`] runs on every toggle.
#[derive(Default)]
pub struct TreeNodes {
    root: Node,
    /// How many files this was built from. `rebuild_entries` asserts
    /// against it in debug builds, so a new path that changes the file
    /// list without rebuilding the tree fails a test rather than
    /// quietly drawing the previous change's rows.
    built_from: usize,
}

#[derive(Default)]
struct Node {
    /// A directory git would not walk into — a nested repository, or one
    /// whose whole contents are ignored. Its children cost a `read_dir`,
    /// and only when somebody asks for them.
    stub: bool,
    dirs: BTreeMap<String, Node>,
    /// (base name, what the row points at), sorted at build time. Sorting
    /// here rather than in `emit` is what lets `emit` borrow the vector
    /// instead of cloning it once per directory.
    files: Vec<(String, RowSrc)>,
}

impl TreeNodes {
    fn build(files: &[ChangedFile]) -> TreeNodes {
        let mut tree = TreeNodes::default();
        for (i, f) in files.iter().enumerate() {
            // Conflicted files are listed above the tree, not inside it
            // (see `App::rebuild_entries`), so the tree never shows them
            // twice.
            if f.conflicted {
                continue;
            }
            tree.insert(&f.path, RowSrc::Changed(i));
        }
        tree.finish(files.len());
        tree
    }

    /// The whole repository, from the two `git ls-files` calls.
    ///
    /// A stub is a directory git would not walk into, so it goes in as a
    /// directory with nothing under it. Reading what is inside costs a
    /// `read_dir` and happens when somebody expands it.
    fn from_listing(listing: &search::RepoListing) -> TreeNodes {
        let mut tree = TreeNodes::default();
        if listing.paths.len() > MAX_EAGER_PATHS {
            // Too big to hold whole. Put in the top level and make every
            // directory a stub, so the panel is usable immediately and each
            // folder costs one `read_dir` when somebody opens it.
            for (i, path) in listing.paths.iter().enumerate() {
                match path.split_once('/') {
                    Some((dir, _)) => tree.dir_at(dir),
                    None => tree.insert(path, RowSrc::Path(i)),
                }
            }
        } else {
            for (i, path) in listing.paths.iter().enumerate() {
                tree.insert(path, RowSrc::Path(i));
            }
        }
        for (dir, _) in &listing.stubs {
            tree.dir_at(dir);
        }
        tree.finish(listing.paths.len());
        tree
    }

    fn insert(&mut self, path: &str, src: RowSrc) {
        let mut parts: Vec<&str> = path.split('/').collect();
        let base = parts.pop().unwrap_or("").to_string();
        let mut node = &mut self.root;
        for p in parts {
            node = node.dirs.entry(p.to_string()).or_default();
        }
        node.files.push((base, src));
    }

    /// Make sure a directory exists in the tree, with nothing in it, and
    /// mark it as one whose contents have not been read.
    fn dir_at(&mut self, path: &str) {
        self.node_at(path).stub = true;
    }

    fn node_at(&mut self, path: &str) -> &mut Node {
        let mut node = &mut self.root;
        for p in path.split('/') {
            node = node.dirs.entry(p.to_string()).or_default();
        }
        node
    }

    /// Drop one file from the tree.
    ///
    /// The path keeps its slot in `repo_paths` — the indices on screen
    /// point into it, and closing the gap would repoint every row after
    /// this one at the wrong file. Nothing reaches the slot once the tree
    /// has forgotten it.
    fn remove(&mut self, path: &str) -> bool {
        let Some((dir, base)) = path.rsplit_once('/') else {
            let before = self.root.files.len();
            self.root.files.retain(|(name, _)| name != path);
            return self.root.files.len() != before;
        };
        let mut node = &mut self.root;
        for p in dir.split('/') {
            match node.dirs.get_mut(p) {
                Some(child) => node = child,
                None => return false,
            }
        }
        let before = node.files.len();
        node.files.retain(|(name, _)| name != base);
        node.files.len() != before
    }

    /// Re-sort one directory's files, after a read added to them.
    fn resort_at(&mut self, path: &str) {
        self.node_at(path).files.sort_by(|a, b| a.0.cmp(&b.0));
    }

    /// Whether this directory's contents are still unread.
    fn is_stub(&self, path: &str) -> bool {
        let mut node = &self.root;
        for p in path.split('/') {
            match node.dirs.get(p) {
                Some(child) => node = child,
                None => return false,
            }
        }
        node.stub
    }

    fn finish(&mut self, built_from: usize) {
        fn sort(node: &mut Node) {
            node.files.sort_by(|a, b| a.0.cmp(&b.0));
            for child in node.dirs.values_mut() {
                sort(child);
            }
        }
        sort(&mut self.root);
        self.built_from = built_from;
    }

    /// Every directory path this tree would draw, which is what
    /// "everything collapsed" has to hold.
    ///
    /// It has to come from the same walk that emits the rows: single-child
    /// directory chains are compressed into one row with a joined label,
    /// and a collapse set built from raw path prefixes would not match any
    /// of them.
    fn all_dirs(&self) -> HashSet<String> {
        let mut rows = Vec::new();
        walk(
            &self.root,
            "",
            0,
            &HashSet::new(),
            &|_| true,
            false,
            &mut rows,
        );
        rows.into_iter()
            .filter_map(|e| match e {
                FileEntry::Dir { path, .. } => Some(path),
                _ => None,
            })
            .collect()
    }

    fn emit(&self, collapsed: &HashSet<String>, out: &mut Vec<FileEntry>) {
        walk(&self.root, "", 0, collapsed, &|_| true, false, out);
    }

    /// The same rows, with only the files `keep` accepts.
    ///
    /// This is how one tree serves both halves of a local review: the
    /// staged and the unstaged sections are the same tree emitted twice
    /// under different filters, so nothing has to be rebuilt when a file
    /// moves across. A directory left with nothing in it is dropped
    /// rather than drawn empty.
    fn emit_where(
        &self,
        collapsed: &HashSet<String>,
        keep: &dyn Fn(RowSrc) -> bool,
        out: &mut Vec<FileEntry>,
    ) {
        walk(&self.root, "", 0, collapsed, keep, true, out);
    }
}

/// Whether anything under this node passes the filter — the question a
/// collapsed directory has to answer, because its rows are never emitted.
fn node_keeps(node: &Node, keep: &dyn Fn(RowSrc) -> bool) -> bool {
    node.files.iter().any(|(_, src)| keep(*src))
        || node.dirs.values().any(|child| node_keeps(child, keep))
}

fn walk(
    node: &Node,
    prefix: &str,
    depth: u16,
    collapsed: &HashSet<String>,
    keep: &dyn Fn(RowSrc) -> bool,
    // Drop a directory the filter left with nothing in it. Off for the
    // panel that lists the repository, where a directory git would not
    // walk into is legitimately empty until somebody opens it.
    prune: bool,
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
        // A collapsed directory emits no rows to count, so it is asked
        // directly; an open one is walked and rolled back when the walk
        // turns out to have added nothing.
        if collapsed.contains(&path) {
            if !prune || node_keeps(cur, keep) {
                out.push(FileEntry::Dir { label, path, depth });
            }
            continue;
        }
        let mark = out.len();
        out.push(FileEntry::Dir {
            label,
            path: path.clone(),
            depth,
        });
        walk(cur, &path, depth + 1, collapsed, keep, prune, out);
        if prune && out.len() == mark + 1 {
            out.truncate(mark);
        }
    }
    for (_, src) in &node.files {
        if keep(*src) {
            out.push(FileEntry::File { src: *src, depth });
        }
    }
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
    /// The whole index at once. `before` is the map to restore on
    /// failure, because every file moved rather than one.
    StageAll {
        before: HashMap<String, StageState>,
        add: bool,
    },
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
    /// The merge in progress and the upstream drift. Both describe the
    /// working tree, so only the local side of the swap ever carries them.
    merge_op: Option<MergeOp>,
    tracking: Option<Tracking>,
    conflict: Option<ConflictView>,
    pr: Option<PrDetail>,
    checked_out: bool,
    /// The stack, which belongs to the pull request side of the swap.
    stack: Option<github::Stack>,
    merge_base: String,
    /// The two sides always travel together: a pull request's stack is
    /// written against its base branch, and local review's against
    /// `origin/HEAD`, so the swap has to carry the right one.
    stack_base: String,
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
    /// The reader is sitting and waiting for this one — it is the file
    /// they just clicked, not a refresh going on behind them.
    ///
    /// The main loop waits on input for 80 ms at a time while a quiet job
    /// runs, which is right for a refresh: it costs nothing and the
    /// spinner still turns. It is wrong for a file read that finishes in
    /// under two milliseconds, because the reader then sits looking at
    /// the old file for the rest of the 80. This asks for the next frame
    /// instead.
    eager: bool,
}

pub struct PrRefreshData {
    detail: PrDetail,
    merge_base: String,
    files: Vec<ChangedFile>,
    viewed: HashSet<String>,
    /// Re-read every refresh: a stack changes under you when somebody
    /// merges the pull request below yours, which is the moment you most
    /// want to be told.
    stack: Option<github::Stack>,
}

pub enum QuietOutcome {
    /// Fresh PR metadata + file list (the head may have moved).
    Pr(Box<PrRefreshData>),
    /// Fresh scan of uncommitted changes.
    Local(Box<LocalOpenedData>),
    /// A file reloaded in place, keeping the scroll position.
    File(Box<FileLoadedData>),
    /// A file the file tree named, read and ready for the editor. Quiet
    /// rather than modal so a double click on the tree survives the read
    /// — see [`App::spawn_open_tree_file`].
    External(Box<ExternalFile>),
}

pub struct PrOpenedData {
    repo: String,
    repo_root: Option<PathBuf>,
    detail: PrDetail,
    checked_out: bool,
    merge_base: String,
    files: Vec<ChangedFile>,
    viewed: HashSet<String>,
    /// The stack this pull request is part of, when it is in one.
    stack: Option<github::Stack>,
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
    /// The merge conflict this file holds, when it holds one. The two
    /// sides above are then our version and their version rather than the
    /// base and the working tree (see [`load_file_data`]).
    conflict: Option<ConflictView>,
}

/// A conflicted file, ready to draw: the parsed markers, and the conflict
/// each line of each side belongs to.
pub struct ConflictView {
    pub file: Arc<Conflicted>,
    /// The conflict owning each 0-based line of the old (ours) side.
    pub old_owner: Vec<Option<usize>>,
    /// The same for the new (theirs) side.
    pub new_owner: Vec<Option<usize>>,
}

impl ConflictView {
    fn build(file: Conflicted) -> (Self, String, String) {
        let sides = file.sides();
        (
            ConflictView {
                file: Arc::new(file),
                old_owner: sides.ours_owner,
                new_owner: sides.theirs_owner,
            },
            sides.ours,
            sides.theirs,
        )
    }

    /// The conflict a diff row belongs to, from whichever side names one.
    fn owner(&self, old_ln: Option<usize>, new_ln: Option<usize>) -> Option<usize> {
        let pick = |owner: &[Option<usize>], ln: Option<usize>| {
            ln.and_then(|n| owner.get(n - 1).copied().flatten())
        };
        pick(&self.new_owner, new_ln).or_else(|| pick(&self.old_owner, old_ln))
    }
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
    /// The merge, rebase, or cherry-pick in progress, when there is one.
    merge_op: Option<MergeOp>,
    /// How far the branch has drifted from the branch it tracks.
    tracking: Option<Tracking>,
    /// What the branch's whole change is written against, for the stacked
    /// diff. See [`gitops::default_base`].
    stack_base: Option<String>,
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
    /// A whole review went up: the verdict, and how many inline comments
    /// went with it.
    ReviewSubmitted {
        verdict: Verdict,
        count: usize,
    },
    EditorSaved(Box<EditorSavedData>),
    ExternalOpened(Box<ExternalFile>),
    /// A file outside the changeset was written; nothing else changed.
    ExternalSaved(String),
    /// A stash was made, applied, or dropped. The working tree is a
    /// different shape afterwards, so a fresh local scan comes with it.
    Stashed {
        note: String,
        local: Box<LocalOpenedData>,
    },
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
    /// This file is to become the peek tab. It rides with the file
    /// rather than sitting in a field on `App`, because it describes
    /// this read and nothing else — and because the tab it replaces must
    /// not be touched until the read lands.
    peek: bool,
}

/// One tab in the row, as the draw needs it.
pub struct BufferTab {
    /// The file the tab holds. The row is addressed by position and not
    /// every open buffer is in it — a pinned file already has a tab of
    /// its own — so the position alone no longer finds the buffer.
    pub path: String,
    /// The file name, or the parent and the name when two tabs share one.
    pub label: String,
    /// Unsaved work in it. The tab carries a dot while this is true.
    pub dirty: bool,
    /// The buffer on screen. There is at most one.
    pub active: bool,
    /// The peek tab: one click opened it and the next click replaces it.
    /// The tab draws in italics to say so. See [`App::peek_path`].
    pub peek: bool,
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

/// Turn a language server's positions into rows somebody can read.
///
/// The positions alone would make an unreadable list, so the line each one
/// points at is read, once per file. The server counts columns in UTF-16
/// units and the rest of loupe counts chars, so the column is converted
/// here rather than at every use.
fn build_places(root: &Path, rev: Option<&str>, locs: Vec<lsp::Loc>) -> Vec<Place> {
    let mut cache: HashMap<String, Option<String>> = HashMap::new();
    locs.into_iter()
        .take(search::RESULT_LIMIT)
        .map(|loc| {
            let content = cache
                .entry(loc.path.clone())
                .or_insert_with(|| read_source(root, rev, &loc.path));
            let line = content
                .as_ref()
                .and_then(|c| c.lines().nth(loc.line.saturating_sub(1)))
                .unwrap_or("");
            let loc = lsp::Loc {
                col: lsp::char_column(line, loc.col.saturating_sub(1)) + 1,
                ..loc
            };
            Place {
                loc,
                text: line.trim_end().to_string(),
            }
        })
        .collect()
}

/// Read a file the way the current review reads files: from the commit
/// under review, falling back to the working tree.
fn read_source(root: &std::path::Path, rev: Option<&str>, path: &str) -> Option<String> {
    let from_disk =
        || gitops::safe_repo_path(root, path).and_then(|p| std::fs::read_to_string(p).ok());
    match rev {
        Some(rev) => gitops::show_file(root, rev, path).or_else(from_disk),
        None => from_disk(),
    }
}

/// What the editor is asking its language server for.
#[derive(Clone, PartialEq, Eq)]
enum EditorRequest {
    /// Suggestions here. Carries the character that asked for them, when
    /// a character did — a server answers a `.` differently from a
    /// request the reader made by hand.
    Complete(Option<char>),
    Hover,
    Definition,
    /// Rename the symbol under the cursor everywhere, to this name.
    Rename(String),
    /// The fixes and refactors on offer where the cursor is.
    CodeActions,
    /// The signature of the call the cursor is inside.
    SignatureHelp,
    /// Every place a symbol is used. `gr` has answered this in the diff
    /// view since language servers arrived; the editor could not.
    References,
    Format,
}

impl EditorRequest {
    /// Whether the user asked for this by pressing a key — completion
    /// also fires on its own while typing, and an unavailable server
    /// must not shout about it once per character.
    fn is_explicit(&self) -> bool {
        !matches!(self, EditorRequest::Complete(_))
    }

    /// What the status bar says while this request is in flight, or
    /// `None` for one the reader did not ask for.
    fn waiting_for(&self, word: &str) -> Option<String> {
        let about = |verb: &str| {
            Some(if word.is_empty() {
                format!("{verb}…")
            } else {
                format!("{verb} {word}…")
            })
        };
        match self {
            EditorRequest::Complete(_) => None,
            EditorRequest::Definition => about("Finding the definition of"),
            EditorRequest::References => about("Finding every use of"),
            EditorRequest::Hover => about("Asking what is"),
            EditorRequest::SignatureHelp => Some("Asking for the signature…".into()),
            EditorRequest::CodeActions => Some("Asking what is on offer here…".into()),
            EditorRequest::Rename(to) => Some(format!("Renaming {word} to {to}…")),
            EditorRequest::Format => Some("Formatting…".into()),
        }
    }
}

/// A language-server request made from the editor.
///
/// Like [`SearchJob`] and unlike a [`ForegroundJob`], this never blocks
/// input — you must be able to keep typing while a completion is in
/// flight — and a result from a keystroke you have already typed past is
/// dropped by generation number rather than applied late.
/// A linter run over the open buffer.
struct LintJob {
    rx: Receiver<anyhow::Result<Option<Vec<lsp::Diagnostic>>>>,
    gen: u64,
    /// Which file it is about, so an answer for a buffer the reader has
    /// clicked away from is dropped rather than painted over the new one.
    path: String,
}

struct EditorJob {
    rx: Receiver<Result<EditorOutcome>>,
    gen: u64,
    /// When it was asked, for the spinner frame.
    started: Instant,
    /// What to say while it runs. `None` for a request the reader did
    /// not ask for — completion fires on its own while typing, and a
    /// spinner per character would be noise.
    label: Option<String>,
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
    /// Every place a symbol is used, ready for the same list the diff
    /// view shows.
    References(Box<LocationsData>),
    /// A rename's edits, per file.
    Renamed {
        to: String,
        edits: Vec<(String, Vec<lsp::TextEdit>)>,
    },
    CodeActions(Vec<lsp::CodeAction>),
    Signature(Option<lsp::Signature>),
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
    /// Where the context provider publishes what is on screen, when
    /// loupe managed to bind its socket. See [`crate::ctx`].
    pub context: Option<ctx::Shared>,

    /// True while reviewing local uncommitted changes instead of a PR
    /// (`pr` is None then; commenting and viewed-sync are off).
    pub local: bool,
    /// Branch name shown in the local-review top bar.
    pub local_branch: Option<String>,
    /// The merge, rebase, or cherry-pick git is in the middle of. Local
    /// review only — it is a property of the working tree.
    pub merge_op: Option<MergeOp>,
    /// Commits ahead of and behind the tracked branch, for the top bar.
    /// None when the branch tracks nothing loupe can resolve.
    pub tracking: Option<Tracking>,
    /// The conflict of the open file, when it has one. While this is set
    /// the diff pane shows our version against their version, and the
    /// change bar resolves conflicts instead of reverting sections.
    pub conflict: Option<ConflictView>,

    pub prs: Vec<PrSummary>,
    pub pr_cursor: usize,
    pub pr_scroll: usize,

    pub pr: Option<PrDetail>,
    pub checked_out: bool,
    /// Inline comments written but held back, waiting to go up as one
    /// review (see [`App::add_to_review`]). Persisted between runs.
    pub pending: Vec<ReviewComment>,
    /// The PR head the held comments were written against. A head that
    /// moves under them would anchor them to lines that no longer exist,
    /// so the submit prompt says so rather than letting GitHub refuse the
    /// whole review.
    pending_commit: Option<String>,
    /// The review composer at the foot of the file panel.
    pub review: ReviewBox,
    /// True once ✕ Discard has been asked for and not yet confirmed. Any
    /// other action clears it, so the second press has to be deliberate.
    discard_armed: bool,
    pub merge_base: String,
    /// The commit the branch's whole change is written against — the
    /// bottom of the stacked diff. The same thing as `merge_base` under a
    /// pull request; local review diffs against HEAD instead, so it reads
    /// this from `origin/HEAD` when it opens. Empty when the repository
    /// cannot say, which is what turns [`Layers`] off.
    pub stack_base: String,
    /// How many layers of the change the diff draws. See [`Layers`].
    pub layers: Layers,
    /// The stack the open pull request belongs to, when it is in one.
    /// `None` for local review, for a pull request on its own, and on a
    /// server that does not have stacks.
    pub stack: Option<github::Stack>,
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
    /// The staging sections the reader folded away. Local review only —
    /// a pull request has no index, so it has no sections. This is a view
    /// preference rather than review state, so it stays on the app across
    /// a swap to the pull request and back.
    pub collapsed_sections: HashSet<StageSection>,
    /// What `git stash list` said when the stash menu was last opened.
    /// Read there rather than kept fresh: a stash nobody is looking at
    /// costs nothing to not know about.
    pub stashes: Vec<Stash>,

    // --- the Commits panel
    /// The commits the upstream does not have, newest first.
    pub commits: Vec<Commit>,
    /// The ref the list is measured against — the branch's upstream, or
    /// the default branch when it tracks nothing.
    pub commits_base: Option<String>,
    /// Files of each commit, read the first time it is opened and kept.
    /// A commit's files never change, so nothing here goes stale.
    pub commit_files: HashMap<String, Vec<ChangedFile>>,
    /// Which commits are open in the panel.
    pub open_commits: HashSet<String>,
    /// What the panel says when it has no commits to list.
    pub commits_note: Option<String>,
    /// When the list was last read, so coming back to the panel does not
    /// re-read it on every visit.
    commits_read: Option<Instant>,
    commits_job: Option<Receiver<Result<CommitsData>>>,
    /// The commit file the diff pane is showing. `None` means the diff is
    /// the change under review — the working tree, or the pull request.
    pub open_commit: Option<OpenCommit>,
    pub entries: Vec<FileEntry>,
    /// The directory tree behind `entries`, cached so a collapse toggle
    /// emits rows without rebuilding it. See [`TreeNodes`].
    tree: TreeNodes,
    /// Every path in the repository, for rows the change does not touch.
    /// [`RowSrc::Path`] indexes into it. Empty until the listing lands.
    pub repo_paths: Vec<String>,
    /// Path to its index in `files`, so a row in the repository list can
    /// find the change behind it without scanning. Rebuilt with the tree.
    changed_by_path: HashMap<String, usize>,
    /// Directories git is ignoring whole. What is inside one inherits the
    /// answer, which a position in `repo_paths` cannot give — a directory
    /// is not itself a path in that list.
    repo_ignored_dirs: HashSet<String>,
    /// Whether each path in `repo_paths` is one git ignores, drawn dim.
    /// Parallel to `repo_paths` rather than a split point, because reading
    /// inside an ignored directory appends its children to the end and
    /// they inherit its answer, not the end of the list's.
    repo_ignored: Vec<bool>,
    /// The repository tree, built once from the listing.
    file_tree: TreeNodes,
    /// Which list the panel is showing.
    pub panel: PanelMode,
    /// Collapse state for Files mode. Kept apart from `collapsed_dirs`
    /// because the two lists hold different folders: collapsing `src` in
    /// one view must not collapse it in the other, where the reader never
    /// touched it.
    collapsed_files: HashSet<String>,
    /// The in-flight repository listing.
    repo_job: Option<Receiver<Result<search::RepoListing>>>,
    /// The in-flight read of one directory the listing stopped at.
    dir_job: Option<Receiver<Result<DirRead>>>,
    /// When the repository listing last landed, for the staleness check on
    /// the way back into Files mode.
    repo_listed: Option<Instant>,
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
    /// The last mouse press: when, where, and how many presses in a row
    /// it made on that spot. The run drives double- and triple-click.
    last_click: Option<(Instant, u16, u16, u8)>,

    pub editor: Option<Editor>,
    /// Set by a `q` that was refused for unsaved work; a second `q`
    /// goes through. Cleared by anything else, so it never carries over
    /// into a later, unrelated keypress.
    quit_armed: bool,
    /// Buffers that are open but not in front of the reader.
    ///
    /// `editor` stays what it has always been — the buffer on screen —
    /// rather than becoming an index into a list. Every one of the 81
    /// places that reads it still means "the buffer the reader is looking
    /// at", which is what they all meant before, so none of them had to
    /// change. Opening another file parks this one here; coming back
    /// swaps them.
    pub parked: Vec<Editor>,
    /// Follow the terminal's own background while loupe runs.
    ///
    /// True when nothing pinned the appearance — no `--light`/`--dark`,
    /// no `appearance = "light" | "dark"` in the config. A system that
    /// turns dark at seven takes the terminal with it, and the reader
    /// should not have to restart loupe (or go and change a setting) to
    /// follow.
    pub appearance_auto: bool,
    /// Which syntax theme goes with each appearance.
    pub themes: Themes,
    /// When the terminal was last asked about its background.
    appearance_asked: Instant,
    /// Bumped every time the appearance changes. A parked diff's
    /// highlights were computed under the theme of its own generation, so
    /// one that does not match is re-highlighted when it comes forward.
    theme_gen: u64,
    /// Diffs the reader has open other than the one on screen.
    ///
    /// The same shape as [`Self::parked`], and for the same reason: the
    /// diff on screen stays in the fields it has always been in, so every
    /// place that reads `self.diff` still means "the diff the reader is
    /// looking at". Opening another one parks this one here; coming back
    /// swaps them.
    parked_diffs: Vec<DiffTab>,
    /// Set by a double click in the file panel: the diff about to land
    /// keeps its tab instead of becoming the one the next click replaces.
    keep_tab: bool,
    /// The tab row, by path, in the reader's own order.
    ///
    /// One list for every kind of tab. A file is in the row while it is
    /// open as a diff, as a buffer, or as both, and it keeps its place
    /// through every move between them: pressing `e` on a diff and `Esc`
    /// back again must not shuffle the tab under the pointer.
    ///
    /// Storage lives elsewhere — [`Self::parked`] for buffers,
    /// [`Self::parked_diffs`] for diffs — and neither is ever in row
    /// order. This is the only thing that is, and a drag is the only
    /// thing that permutes it.
    tab_order: Vec<String>,
    /// The place a closed peek tab left, for the file that replaces it.
    ///
    /// Walking a tree means opening a great many files to look at one,
    /// and each one lands in the same tab. Without this the replacement
    /// would append instead, and the row would grow a tab to the right on
    /// every click.
    tab_hole: Option<usize>,
    /// The tab a drag picked up, by path. A path rather than an index
    /// because the drag is what changes the indices.
    tab_drag: Option<String>,
    /// The **peek tab**: the file one click opened, which the next click
    /// replaces. There is at most one.
    ///
    /// Walking a file tree means opening a great many files to look at
    /// one of them, and a tab per look fills the row with files the
    /// reader never wanted. So a click peeks: it puts the file in this
    /// tab, the next click puts the next file in the same tab, and a
    /// double click keeps it. The row draws a peek in italics, which is
    /// the only warning the reader gets that it is about to go.
    ///
    /// "Peek" and not "preview": a preview in loupe is the rendered
    /// markdown document behind `P`, and one word for two things in the
    /// same tab row would be a word nobody could follow.
    ///
    /// Read it through [`App::peek_path`], never directly — a buffer the
    /// reader has typed into is theirs, and that rule lives there.
    peek: Option<String>,
    /// The markdown preview, when one is open. It shares the pane with
    /// the diff and the editor, and never coexists with the editor —
    /// `P` swaps one for the other on the same file.
    pub preview: Option<Preview>,
    /// True when loupe was launched to read one file (`loupe md <path>`)
    /// and there is no review beside it: the preview takes the whole
    /// window and quitting is the only way out.
    pub preview_only: bool,
    /// Files pinned to the tab row at the top of the window, and which
    /// of them is open (see [`crate::pins`]).
    pub pins: Pins,
    /// Set while a pinned tab loads a file the change touches: the tab
    /// wants the rendered document, and the file has to land first.
    pin_wants_preview: bool,
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
    /// A suggestion request waiting out [`SUGGEST_DEBOUNCE`], and the
    /// character that asked for it. See [`App::maybe_suggest`].
    completion_due: Option<(Instant, Option<char>)>,
    /// Run linters over the buffer as it is edited (`linters` in the
    /// config).
    pub lint_enabled: bool,
    /// When the buffer last changed, for the lint debounce.
    lint_touched: Option<Instant>,
    /// Hash of the text last handed to the linter, so an unchanged
    /// buffer costs no process.
    linted: Option<u64>,
    lint_job: Option<LintJob>,
    lint_gen: u64,
    /// Whether a linter that cannot run has already said so. It says it
    /// once, not once per pause in typing.
    lint_complained: bool,
    editor_gen: u64,
    /// When the buffer last changed, for the idle push to the server.
    editor_touched: Option<Instant>,
    /// Diagnostics version last seen, so a redraw only happens when the
    /// servers actually said something new.
    diag_seen: u64,
    /// Run the language server's formatter on save (`format_on_save`).
    pub format_on_save: bool,
    /// Open the completion popup as a name is typed, not only on
    /// `Ctrl+Space` (`suggest_while_typing`). On by default: an editor
    /// that only suggests when asked is an editor you forget can.
    pub suggest_while_typing: bool,
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
    /// What a resolution just did. A resolution starts a re-scan, and the
    /// re-scan has its own thing to say when it lands — so the sentence
    /// the reader actually needs is held here and put back on the line
    /// once nothing else is in flight.
    resolved_note: Option<String>,

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
            context: None,
            local: false,
            local_branch: None,
            merge_op: None,
            tracking: None,
            conflict: None,
            prs: Vec::new(),
            pr_cursor: 0,
            pr_scroll: 0,
            pr: None,
            checked_out: false,
            pending: Vec::new(),
            pending_commit: None,
            review: ReviewBox::new(),
            discard_armed: false,
            merge_base: String::new(),
            stack_base: String::new(),
            layers: Layers::default(),
            stack: None,
            files: Vec::new(),
            viewed: HashSet::new(),
            stage: HashMap::new(),
            file_cursor: 0,
            file_scroll: 0,
            tree_view: true,
            collapsed_dirs: HashSet::new(),
            collapsed_sections: HashSet::new(),
            stashes: Vec::new(),
            commits: Vec::new(),
            commits_base: None,
            commit_files: HashMap::new(),
            open_commits: HashSet::new(),
            commits_note: None,
            commits_read: None,
            commits_job: None,
            open_commit: None,
            entries: Vec::new(),
            tree: TreeNodes::default(),
            repo_paths: Vec::new(),
            changed_by_path: HashMap::new(),
            repo_ignored: Vec::new(),
            repo_ignored_dirs: HashSet::new(),
            file_tree: TreeNodes::default(),
            panel: PanelMode::Changes,
            collapsed_files: HashSet::new(),
            repo_job: None,
            dir_job: None,
            repo_listed: None,
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
            quit_armed: false,
            parked: Vec::new(),
            appearance_auto: false,
            themes: Themes::default(),
            appearance_asked: Instant::now(),
            theme_gen: 0,
            parked_diffs: Vec::new(),
            keep_tab: false,
            tab_order: Vec::new(),
            tab_hole: None,
            tab_drag: None,
            peek: None,
            preview: None,
            preview_only: false,
            pins: Pins::default(),
            pin_wants_preview: false,
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
            completion_due: None,
            lint_enabled: true,
            lint_touched: None,
            linted: None,
            lint_job: None,
            lint_gen: 0,
            lint_complained: false,
            editor_gen: 0,
            editor_touched: None,
            diag_seen: 0,
            format_on_save: false,
            suggest_while_typing: true,
            format_then_save: false,
            search_job: None,
            search_gen: 0,
            find: Find::default(),
            pending_jump: None,
            post_load_err: None,
            auto_open_note: None,
            resolved_note: None,
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

    /// True while a debounce is armed — a suggestion to fetch, or a
    /// buffer to push to the language server.
    ///
    /// The main loop sits on a 250 ms poll when nothing is happening,
    /// which would be added to every debounce below it. A typist waiting
    /// on a popup feels that. This shortens the tick until the pending
    /// work has fired.
    pub fn settling(&self) -> bool {
        self.completion_due.is_some() || self.editor_touched.is_some()
    }

    /// True while a silent background refresh is running (keeps the main
    /// loop ticking so its spinner animates and its result lands promptly).
    pub fn refreshing(&self) -> bool {
        self.quiet.is_some()
    }

    /// True while the reader is waiting on a file they clicked. The main
    /// loop picks these up on the next frame rather than on the
    /// spinner's tick — see [`QuietJob::eager`].
    pub fn opening(&self) -> bool {
        self.quiet.as_ref().is_some_and(|q| q.eager)
    }

    /// The buffer changed. Both clocks that watch it start again: the
    /// one that pushes the text to the language server, and the one that
    /// runs the linter over it.
    ///
    /// One helper rather than two assignments at every edit site,
    /// because the two arming again together is the invariant — a
    /// keystroke that reached one and not the other is a keystroke that
    /// leaves the underlines and the message disagreeing.
    fn buffer_changed(&mut self) {
        let now = Instant::now();
        self.editor_touched = Some(now);
        self.lint_touched = Some(now);
    }

    /// Decide whether the character just typed is worth suggestions, and
    /// arm the debounce when it is.
    ///
    /// Two ways a suggestion is worth asking for. A **trigger character**
    /// is one the server itself named — `.` in TypeScript, `.` and `:` in
    /// Rust — and means "the thing before me has members"; it suggests
    /// with no prefix at all. Anything else has to be a word character
    /// with at least [`SUGGEST_AFTER`] characters of a name behind it, or
    /// the popup would open on every space.
    ///
    /// An open popup that still has matches is left alone: it already
    /// holds the server's answer for this word, and narrowing it locally
    /// is instant where a round trip is not. An open popup that has just
    /// been filtered down to nothing asks again — the longer word may
    /// have suggestions the shorter one did not.
    fn maybe_suggest(&mut self, typed: char) {
        if !self.suggest_while_typing {
            return;
        }
        let Some(editor) = &self.editor else { return };
        if editor.read_only || editor.find.typing.is_some() {
            return;
        }
        // Still filtering the list we have. Nothing to ask.
        if editor.completion.is_some() {
            return;
        }
        let triggers = self.lsp.trigger_characters(&editor.path);
        // Before the server has started there is nothing to ask it about
        // yet, so fall back to the punctuation that means "members" in
        // every language loupe drives.
        let is_trigger = if triggers.is_empty() {
            matches!(typed, '.' | ':' | '>')
        } else {
            triggers.contains(&typed)
        };
        let armed = if is_trigger {
            Some(typed)
        } else if crate::editor::is_word(typed)
            && self
                .editor
                .as_ref()
                .is_some_and(|e| e.word_prefix().chars().count() >= SUGGEST_AFTER)
        {
            None
        } else {
            // A space, a bracket, a newline: whatever was being typed is
            // finished, and a pending request about it is stale.
            self.completion_due = None;
            return;
        };
        self.completion_due = Some((Instant::now(), armed));
    }

    /// Fire an armed suggestion request once typing pauses. Returns true
    /// when it did, so the caller knows the frame changed.
    fn maybe_spawn_suggestion(&mut self) -> bool {
        let Some((armed, trigger)) = self.completion_due else {
            return false;
        };
        if armed.elapsed() < SUGGEST_DEBOUNCE {
            return false;
        }
        self.completion_due = None;
        // Typed on past it, or left the buffer: the answer would be
        // about a word that is no longer being written.
        if self.editor.is_none() {
            return false;
        }
        self.spawn_editor_request(EditorRequest::Complete(trigger));
        true
    }

    /// Push the editor buffer to its language server once typing pauses.
    ///
    /// Debounced, and deliberately best-effort: `sync_open` never blocks
    /// and never starts a server, so a slow or missing one costs a
    /// skipped tick rather than a stutter between keystrokes.
    ///
    /// "Never blocks" covers the locks — it takes them with `try_lock` —
    /// and, since the writer thread landed, the pipe as well. Before that
    /// it did not: a `didChange` larger than the pipe waited here on a
    /// server that was busy indexing. See `lsp::Client::send`.
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
        match &mut self.editor {
            Some(editor) => editor.set_server_diagnostics(list),
            None => false,
        }
    }

    /// Run the linter over the buffer once typing settles.
    ///
    /// A subprocess per run, so the debounce is longer than the language
    /// server's: a server takes a `didChange` down a pipe it is already
    /// holding open, where this pays for a process every time. One run
    /// at a time, and a run whose text is already what was last linted
    /// is skipped entirely.
    fn maybe_spawn_lint(&mut self) -> bool {
        if !self.lint_enabled || self.lint_job.is_some() {
            return false;
        }
        let Some(touched) = self.lint_touched else {
            return false;
        };
        if touched.elapsed() < LINT_DEBOUNCE {
            return false;
        }
        self.lint_touched = None;
        let Some(editor) = &self.editor else {
            return false;
        };
        if linter::spec_for(&editor.path).is_none() {
            return false;
        }
        let text = editor.content();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&text, &mut hasher);
        let hash = std::hash::Hasher::finish(&hasher);
        if self.linted == Some(hash) {
            return false;
        }
        self.linted = Some(hash);
        let path = editor.path.clone();
        let root = self.repo_root.clone();
        self.lint_gen += 1;
        let gen = self.lint_gen;
        let (tx, rx) = mpsc::channel();
        let for_thread = path.clone();
        thread::spawn(move || {
            let _ = tx.send(linter::lint(&root, &for_thread, &text));
        });
        self.lint_job = Some(LintJob { rx, gen, path });
        false
    }

    /// Pick up a finished lint run.
    ///
    /// A linter that could not run says so once and is then left alone:
    /// a broken `.eslintrc` must not put an error in the status bar every
    /// time typing pauses.
    fn poll_lint(&mut self) -> bool {
        let Some(job) = &self.lint_job else {
            return false;
        };
        let result = match job.rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => {
                self.lint_job = None;
                return false;
            }
        };
        let (gen, path) = (job.gen, job.path.clone());
        self.lint_job = None;
        if gen != self.lint_gen {
            return false;
        }
        // The buffer moved on, or a different file is on screen.
        if self.editor.as_ref().map(|e| e.path.as_str()) != Some(path.as_str()) {
            return false;
        }
        match result {
            Ok(Some(list)) => match &mut self.editor {
                Some(editor) => editor.set_lint_diagnostics(list),
                None => false,
            },
            // No linter for this file, or it is not installed. Silence is
            // the right answer: a reviewer without eslint on their
            // machine still wants to read the diff.
            Ok(None) => false,
            Err(e) => {
                if !self.lint_complained {
                    self.lint_complained = true;
                    self.err(format!("{e:#}"));
                }
                false
            }
        }
    }

    /// True while a finder query is running or waiting out its debounce.
    /// The main loop treats this like `busy()` for timing purposes only —
    /// input stays live throughout.
    pub fn searching(&self) -> bool {
        self.search_job.is_some()
            || matches!(&self.overlay, Overlay::Finder(f) if f.pending.is_some())
            // An editor lookup animates a spinner of its own, so the main
            // loop has to keep ticking while one is in flight.
            || self.editor_job.as_ref().is_some_and(|j| j.label.is_some())
    }

    /// The language-server request the editor is waiting on, if the
    /// reader asked for one: a spinner frame and what it is waiting for.
    ///
    /// Deliberately not a [`ForegroundJob`]: those are modal, and you
    /// must be able to keep typing while a lookup is in flight. This is
    /// the visible half of that promise — the work shows, the keyboard
    /// stays yours.
    pub fn editor_waiting(&self) -> Option<(char, &str)> {
        let job = self.editor_job.as_ref()?;
        let label = job.label.as_deref()?;
        Some((spinner_frame(job.started), label))
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
                            // The sections are drawn from this map, so a
                            // file that turned out to be partly staged
                            // has to move to the half it really belongs
                            // in before the next frame.
                            self.rebuild_entries();
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
                        (Err(e), BgKind::StageAll { before, add }) => {
                            self.stage = before;
                            self.rebuild_entries();
                            let what = if add { "Staging" } else { "Unstaging" };
                            self.err(format!("{what} everything failed: {e:#}"));
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
        if self.maybe_spawn_suggestion() {
            changed = true;
        }
        self.maybe_spawn_lint();
        if self.poll_lint() {
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
        if let Some(rx) = &self.dir_job {
            match rx.try_recv() {
                Ok(Ok(read)) => {
                    self.dir_job = None;
                    self.apply_dir_read(read);
                    changed = true;
                }
                Ok(Err(e)) => {
                    self.dir_job = None;
                    self.err(format!("Couldn't read that directory: {e:#}"));
                    changed = true;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.dir_job = None;
                    changed = true;
                }
            }
        }
        if let Some(rx) = &self.commits_job {
            match rx.try_recv() {
                Ok(Ok(data)) => {
                    self.commits_job = None;
                    self.apply_commits(data);
                    changed = true;
                }
                Ok(Err(e)) => {
                    self.commits_job = None;
                    self.commits_note = Some(format!("Couldn't read the commit list: {e:#}"));
                    self.rebuild_entries();
                    changed = true;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.commits_job = None;
                    changed = true;
                }
            }
        }
        if let Some(rx) = &self.repo_job {
            match rx.try_recv() {
                Ok(Ok(listing)) => {
                    self.repo_job = None;
                    self.apply_repo_listing(listing);
                    changed = true;
                }
                Ok(Err(e)) => {
                    self.repo_job = None;
                    self.err(format!("Couldn't list the repository: {e:#}"));
                    changed = true;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.repo_job = None;
                    changed = true;
                }
            }
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
            Outcome::Stashed { note, local } => {
                // The working tree is a different shape now, so this is
                // an ordinary local open — with the stash's own word on
                // the status line instead of the file count.
                self.apply(Outcome::LocalOpened(local));
                self.ok(note);
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
                // Held comments belong to the pull request, and travel
                // with it into the stash — but the box must not keep the
                // keyboard on a side that has no review box at all.
                self.review.focused = false;
                self.review.picking = false;
                self.merge_op = d.merge_op;
                self.tracking = d.tracking;
                self.conflict = None;
                self.pr = None;
                // The working tree IS the review target, so editing works.
                self.checked_out = true;
                // Old side of every diff: HEAD (empty in a commitless repo —
                // show_file then yields None and files render as added).
                self.merge_base = d.head.unwrap_or_default();
                // …and the bottom of the stack: what the whole branch is
                // written against, rather than what its last commit was.
                self.stack_base = d.stack_base.unwrap_or_default();
                // A GitHub stack belongs to a pull request, and this side
                // has none.
                self.set_stack(None);
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
                self.rebuild_files();
                if self.files.is_empty() {
                    self.diff = None;
                    self.display.clear();
                    self.ok(
                        "Working tree clean — no uncommitted changes. Press b for pull requests.",
                    );
                } else {
                    let n = self.files.len();
                    let what = if n == 1 { "file" } else { "files" };
                    // A conflict outranks the file count: it is the reason
                    // the reader opened loupe, and it blocks the commit.
                    self.auto_open_note = Some(match self.conflict_note() {
                        Some(note) => note,
                        None => format!(
                            "Reviewing uncommitted changes vs HEAD ({n} {what}; b for the PR list)"
                        ),
                    });
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
                // All three describe the working tree, and a PR review is
                // about a commit range instead.
                self.merge_op = None;
                self.tracking = None;
                self.conflict = None;
                self.repo = Some(d.repo);
                if let Some(root) = d.repo_root {
                    self.repo_root = root;
                }
                self.checked_out = d.checked_out;
                self.merge_base = d.merge_base;
                // Under a pull request the two are the same commit: the
                // change is already written against the base branch. In a
                // stack that base is the pull request below this one, so
                // the diff is this link's own work and nothing under it.
                self.stack_base = self.merge_base.clone();
                self.set_stack(d.stack);
                self.reset_blame_review();
                self.files = d.files;
                self.viewed = d.viewed;
                // Whatever was held for this pull request last time. Read
                // after `pr` is set, since the file is keyed by number.
                self.load_pending();
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
                self.rebuild_files();
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
                // flight — trust the path, not the captured index. A file
                // out of a commit is not in `files` at all, and its own
                // row keeps the cursor.
                if self.open_commit.is_none() {
                    self.file_cursor = if self.files.get(d.idx).map(|f| f.path.as_str())
                        == Some(d.path.as_str())
                    {
                        d.idx
                    } else {
                        self.files
                            .iter()
                            .position(|f| f.path == d.path)
                            .unwrap_or(d.idx.min(self.files.len().saturating_sub(1)))
                    };
                }
                self.old_content = d.old;
                self.new_content = d.new;
                self.old_hl = d.old_hl;
                self.new_hl = d.new_hl;
                self.differs_from_head = d.differs;
                self.diff = Some(d.diff);
                self.conflict = d.conflict;
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
                // A pinned tab asked for the document rather than the
                // diff, so it forces the same door open.
                // The tab this file has. It takes over the peek tab —
                // the reviewer walks the whole file list, and a tab per
                // step would be a row nobody could read — unless a
                // double click asked to keep it. Dropped after the open,
                // not before, so the pane is never empty in between and
                // the new tab lands where the old one was.
                //
                // The peek tab goes either way. "Keep" means keep *this*
                // tab, not keep both — a double click that left the file
                // it replaced behind would grow the row exactly where a
                // single click does not.
                let keep = std::mem::take(&mut self.keep_tab);
                self.drop_peek(&d.path);
                self.peek = (!keep).then(|| d.path.clone());
                let wants = std::mem::take(&mut self.pin_wants_preview);
                let keep_preview =
                    (self.preview.take().is_some() || wants) && markdown::is_markdown(&d.path);
                self.recompute_matches();
                self.reveal_current_file();
                // The blame of the file that just landed, on its own job.
                // A conflict view is not the working-tree file — the marker
                // lines are gone — so its line numbers would blame the
                // wrong lines. The pane stands down until it is resolved.
                if self.conflict.is_some() {
                    self.clear_blame();
                } else {
                    self.spawn_blame(self.file_cursor);
                }
                // Start this language's server in the background, so the
                // first gd / gr / K doesn't pay for the handshake.
                // Not for a commit's file: it cannot be edited, and the
                // text is not what is on disk for the server to agree
                // with.
                if self.lsp_enabled && self.open_commit.is_none() {
                    if let (Some(file), Some(text)) = (
                        self.files.get(self.file_cursor),
                        self.new_content.as_deref(),
                    ) {
                        self.lsp.warm(&self.repo_root, &file.path, text);
                    }
                }
                let msg = match &self.conflict {
                    Some(c) => {
                        let n = c.file.len();
                        let (ours, theirs) = c.file.labels();
                        format!(
                            "⚠ {} — {n} conflict{} · left is {ours}, right is {theirs} · o resolves the one at the cursor",
                            d.path,
                            if n == 1 { "" } else { "s" }
                        )
                    }
                    // A file out of the Commits panel names the commit
                    // it came from. It is read-only for a different
                    // reason than an unchecked-out branch is, and the
                    // reason is the useful half of the sentence.
                    None => match &self.open_commit {
                        Some(c) => format!(
                            "{} at {} {} — read-only; a commit already happened.",
                            d.path, c.short, c.subject
                        ),
                        None => {
                            let mode = if self.checked_out {
                                "editable"
                            } else {
                                "read-only (branch not checked out)"
                            };
                            format!("{} — {}", d.path, mode)
                        }
                    },
                };
                match self.auto_open_note.take() {
                    Some(note) => self.ok(format!("{note} · {msg}")),
                    None => self.ok(msg),
                }
                if keep_preview {
                    self.preview_current_file();
                }
            }
            Outcome::ReviewSubmitted { verdict, count } => {
                let n = self.pr.as_ref().map(|p| p.number).unwrap_or(0);
                // It is on GitHub now, so nothing is held any more.
                self.pending.clear();
                self.pending_commit = None;
                self.review.clear();
                self.save_pending();
                self.blur_review();
                let with = if count == 0 {
                    String::new()
                } else {
                    format!(
                        " with {count} inline comment{}",
                        if count == 1 { "" } else { "s" }
                    )
                };
                self.ok(format!("✔ You {} PR #{n}{with}.", verdict.past()));
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
                // A hand edit of a conflicted file rewrites the very text
                // the conflict view was built from, so the parse behind it
                // is now stale — and resolving against a stale parse would
                // write the file back the way it was before the edit. Drop
                // it and re-read the file, which parses whatever is left.
                if self.conflict.take().is_some() {
                    self.ok(format!("✔ Saved {} — re-reading the conflict…", d.path));
                    let idx = self.file_cursor;
                    self.spawn_quiet_file(idx, true);
                    return;
                }
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
        // The file is in hand, so this is the frame the pane changes on.
        // A document holds nothing unsaved, so it simply gives way.
        self.preview = None;
        self.open_buffer(editor);
        if d.peek {
            // The peek tab this file takes over. Closed now rather than
            // when the click happened: `peek` still names the old tab
            // here, and closing it before the read is what left the pane
            // empty — and so showing the diff — for every frame in
            // between. The order matters too. The new file has the
            // screen already, so the close shuts the old tab's gap and
            // the new one lands in its place in the row.
            self.drop_peek(&d.path);
            self.peek = Some(d.path.clone());
        }
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
        } else if self.peek_path() == Some(d.path.as_str()) {
            // Say what the next click does. A reader who does not know
            // the rule loses the file they were reading and never learns
            // why.
            self.ok(format!(
                "{}{where_} — peeking, so the next click replaces it. Double-click to keep it.",
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
        // Last session's tabs. Read once the root is known, since the
        // file lives under this clone's git directory.
        self.load_pins();
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
                    merge_op: gitops::merge_op(&root),
                    tracking: gitops::tracking(&root),
                    stack_base: gitops::default_base(&root),
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
            // …and so is the stack: most pull requests are not in one,
            // and a server that has never heard of them must still open
            // this one. `pr_stack` answers None to every failure.
            let stack = github::pr_stack(&repo, number);
            Ok(Outcome::PrOpened(Box::new(PrOpenedData {
                repo,
                repo_root,
                detail,
                checked_out: checkout,
                merge_base,
                files,
                viewed,
                stack,
                auto,
            })))
        });
    }

    /// What a file load needs from the app, captured before the worker
    /// thread starts (both the modal and the quiet load use this).
    fn file_load_ctx(&self) -> LoadCtx {
        match &self.open_commit {
            // A commit is read against its own first parent, both sides
            // from git. The copy on disk belongs to HEAD, which for every
            // commit but the newest is a later version of the same file.
            Some(c) => LoadCtx {
                old_ref: c.parent.clone(),
                new_ref: c.oid.clone(),
                from_disk: false,
                anchor: false,
                // A commit is one step of the change, already; there is
                // no second step under it to layer.
                layers: Layers::Off,
                stack_base: String::new(),
                pushed_ref: String::new(),
                root: self.repo_root.clone(),
            },
            None => LoadCtx {
                old_ref: self.merge_base.clone(),
                new_ref: self
                    .pr
                    .as_ref()
                    .map(|p| p.head_ref_oid.clone())
                    .unwrap_or_default(),
                from_disk: self.checked_out,
                anchor: !self.local,
                layers: self.layers_now(),
                stack_base: self.stack_base.clone(),
                pushed_ref: self.pushed_ref(),
                root: self.repo_root.clone(),
            },
        }
    }

    /// The middle version of the stack: the branch as it already stood,
    /// before whatever the reader is doing to it now.
    ///
    /// A pull request names its own head, which is exactly the copy
    /// GitHub is showing reviewers. Local review has no pull request to
    /// ask, so it takes the branch's upstream — the same commit whenever
    /// the pull request is up to date, and the honest answer when it is
    /// not. A branch with nothing on the remote falls back to HEAD: the
    /// middle is then what is committed and the top is what is not, which
    /// is the same question one step earlier and the one that matters
    /// before the first push.
    ///
    /// Empty only where there is no such version at all — a repository
    /// with no commits.
    fn pushed_ref(&self) -> String {
        if let Some(pr) = &self.pr {
            return pr.head_ref_oid.clone();
        }
        if let Some(up) = self.tracking.as_ref().map(|t| t.upstream.clone()) {
            if !up.is_empty() {
                return up;
            }
        }
        // Local review's old side is HEAD already, so this is it.
        if self.local {
            return self.merge_base.clone();
        }
        String::new()
    }

    /// True where all three versions of a file exist to be read: a
    /// working tree on top, something on the remote (or committed) under
    /// it, and a base branch under that.
    pub fn layers_possible(&self) -> bool {
        self.checked_out
            && self.open_commit.is_none()
            && !self.stack_base.is_empty()
            && !self.pushed_ref().is_empty()
    }

    /// The layers actually in force.
    ///
    /// The setting itself can arrive from the config file, which is read
    /// before any review is open and so before there is anything to check
    /// it against — and a review can stop being able to draw them
    /// afterwards, by opening a commit. Everything that reads the setting
    /// reads it through here, so the diff, the change bar and the title
    /// never disagree about what is on screen.
    pub fn layers_now(&self) -> Layers {
        if self.layers_possible() {
            self.layers
        } else {
            Layers::Off
        }
    }

    /// What the middle version of the stack is, in words: the message
    /// that offers the setting has to say what the purple stands for.
    fn pushed_label(&self) -> &'static str {
        if self.pr.is_some() || self.tracking.is_some() {
            "already pushed"
        } else {
            "already committed"
        }
    }

    /// Open a file of the change as a diff.
    ///
    /// A file already open in a tab is switched to rather than read
    /// again, so its scroll, its cursor, its folds and its selection all
    /// come back — that is what makes a tab a place rather than a
    /// bookmark. Anything else is read, and takes over the peek tab: a
    /// reviewer walks the whole file list, and a tab per step would be a
    /// row nobody could read. A double click keeps it.
    fn spawn_load_file(&mut self, idx: usize) {
        let Some(file) = self.files.get(idx).cloned() else {
            return;
        };
        // This file belongs to the change, not to a commit, so the diff
        // pane stops being a commit's before opening it.
        self.leave_open_commit();
        if self.switch_diff(&file.path) {
            self.file_cursor = idx;
            self.ok(format!("{} — already open.", file.path));
            return;
        }
        self.park_diff();
        self.note_tab(&file.path);
        let ctx = self.file_load_ctx();
        self.spawn(format!("Loading {}", file.path), true, false, move || {
            load_file_data(idx, file, ctx).map(|d| Outcome::FileLoaded(Box::new(d)))
        });
    }

    /// Read the file on screen again, whatever the pane already holds
    /// for it.
    ///
    /// [`Self::spawn_load_file`] swaps to a tab it already has, which is
    /// what makes a tab a place. This is for the one case where every
    /// tab in hand is out of date: a change of [`Layers`] means each was
    /// read between the wrong two versions of its file.
    fn reload_open_file(&mut self) {
        // The parked tabs were read under the old setting too. Dropping
        // them costs their scroll and their folds, and is what lets the
        // row promise that every tab in it agrees with the status line.
        // The row itself is `tab_order`, so no tab is closed by this;
        // opening one reads it again.
        self.parked_diffs.clear();
        let Some(idx) = self
            .diff_path()
            .and_then(|p| self.files.iter().position(|f| f.path == p))
        else {
            return;
        };
        let Some(file) = self.files.get(idx).cloned() else {
            return;
        };
        let ctx = self.file_load_ctx();
        self.spawn(format!("Loading {}", file.path), true, false, move || {
            load_file_data(idx, file, ctx).map(|d| Outcome::FileLoaded(Box::new(d)))
        });
    }

    /// `s`: walk between one layer of the change and three (see
    /// [`Layers`]).
    ///
    /// It refuses rather than turning on a setting that would draw the
    /// same thing twice: with nothing pushed there is no middle version
    /// to layer against, and with no `origin/HEAD` there is no bottom.
    /// Both messages say which half is missing.
    pub fn cycle_layers(&mut self) {
        if self.open_commit.is_some() {
            self.err("A commit is one step of the change already — s layers a branch.");
            return;
        }
        let next = self.layers.next();
        let under = self.pushed_label();
        if next.on() {
            // The top of the stack is the working tree. A read-only
            // review has none: its new side is the pushed branch itself,
            // so every row would come out purple and the setting would
            // look broken rather than empty.
            if !self.checked_out {
                self.err(
                    "This review is read-only — there is no working tree over the pushed branch to lay anything on. Check the branch out first.",
                );
                return;
            }
            if self.pushed_ref().is_empty() {
                self.err(
                    "This branch has no commits yet — there is nothing under your changes to lay them over.",
                );
                return;
            }
            if self.stack_base.is_empty() {
                self.err(
                    "No default branch on the remote — loupe cannot tell where this branch's own change starts.",
                );
                return;
            }
        }
        self.layers = next;
        self.ok(match next {
            Layers::Off => "Layers off — the change as a whole.".to_string(),
            Layers::Full => format!(
                "Layers: the whole stack — purple is {under}, green and red are new, amber is written twice."
            ),
            Layers::Mine => format!(
                "Layers: only what is new — amber where it lands on lines {under}."
            ),
        });
        self.reload_open_file();
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

    /// Take the open inline draft and hold it for the review.
    ///
    /// No network at all: the comment goes into the batch and to disk, and
    /// reaches GitHub only when the review is submitted.
    fn hold_comment(&mut self) {
        let Overlay::Comment(draft) = &self.overlay else {
            return;
        };
        let body = draft.textarea.lines().join("\n");
        if body.trim().is_empty() {
            self.err("Comment is empty — write something or press Esc to cancel.");
            return;
        }
        let comment = ReviewComment {
            path: draft.path.clone(),
            side: match draft.side {
                Side::Left => CommentSide::Left,
                Side::Right => CommentSide::Right,
            },
            line: draft.hi,
            start_line: (draft.lo != draft.hi).then_some(draft.lo),
            body,
        };
        self.overlay = Overlay::None;
        self.selection = None;
        self.add_to_review(comment);
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
        let root = self.repo_root.clone();
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
                let head = gitops::show_file(&root, &head_oid, &path);
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
    /// Whether this review can put changes back at all.
    ///
    /// Separate from [`Self::can_revert`], which also asks whether the
    /// change bar is on screen to click. The editor covers the diff, so
    /// there is no bar to draw while it is open — but `u` and `U` and the
    /// ☰ lines still mean something, and refusing them because a file was
    /// open was a lockout with no reason behind it.
    pub fn revert_possible(&self) -> bool {
        self.checked_out
            // A commit already happened. Nothing in its diff is a change
            // anybody can put back from here.
            && self.open_commit.is_none()
            && self.screen == Screen::Review
    }

    pub fn can_revert(&self) -> bool {
        self.revert_possible() && self.editor.is_none()
    }

    /// Columns the change bar takes off the left of the diff pane — zero
    /// when there is nothing to revert, so nothing but the feature pays for
    /// it. Every piece of diff geometry measures from
    /// [`Self::diff_body`], which subtracts this.
    pub fn revert_gutter(&self) -> u16 {
        // The same two columns carry the ⚑ conflict markers, which are
        // offered even where a revert is not (see `ui::draw_diff`).
        if self.diff.is_some() && (self.can_revert() || self.conflict.is_some()) {
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

    /// True when the section on this display row holds a line the change
    /// has now written twice — the ‡ the change bar marks while the
    /// layers are on. See [`crate::diff::Layer::Rework`].
    pub fn rework_on_row(&self, display_row: usize) -> bool {
        let Some((s, e)) = self.section_on_row(display_row) else {
            return false;
        };
        let Some(d) = self.diff.as_ref() else {
            return false;
        };
        d.rows[s..e.min(d.rows.len())]
            .iter()
            .any(|r| r.layer == Layer::Rework)
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

    // ------------------------------------------------------ pending review

    /// Where the held comments for one pull request live between runs.
    ///
    /// Under the git directory rather than in the working tree: it is
    /// per-clone state, it must never be committed by accident, and it
    /// travels with the checkout the comments were written against.
    fn pending_path(&self, number: u64) -> Option<PathBuf> {
        let dir = gitops::git_dir(&self.repo_root)?.join("loupe");
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir.join(format!("pending-review-{number}.json")))
    }

    /// Read back whatever was held for this pull request last time.
    fn load_pending(&mut self) {
        #[derive(serde::Deserialize)]
        struct Saved {
            commit: Option<String>,
            comments: Vec<ReviewComment>,
            #[serde(default)]
            body: String,
            #[serde(default)]
            verdict: Option<String>,
        }
        self.pending.clear();
        self.pending_commit = None;
        self.review.clear();
        let Some(number) = self.pr.as_ref().map(|p| p.number) else {
            return;
        };
        let Some(saved) = self
            .pending_path(number)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<Saved>(&t).ok())
        else {
            return;
        };
        self.pending = saved.comments;
        self.pending_commit = saved.commit;
        if !saved.body.is_empty() {
            self.review.textarea = TextArea::from(saved.body.split('\n').collect::<Vec<_>>());
        }
        self.review.verdict = Verdict::all()
            .into_iter()
            .find(|v| Some(v.api()) == saved.verdict.as_deref())
            .unwrap_or_default();
        if !self.pending.is_empty() {
            let n = self.pending.len();
            self.auto_open_note = Some(format!(
                "{n} held comment{} from last time — R opens the review box",
                if n == 1 { "" } else { "s" }
            ));
        }
    }

    /// Write the held comments out. Called after every change to them, so
    /// a crash or a `q` never costs the reader a review they had written.
    fn save_pending(&mut self) {
        let Some(number) = self.pr.as_ref().map(|p| p.number) else {
            return;
        };
        let Some(path) = self.pending_path(number) else {
            return;
        };
        // Nothing held and nothing written: leave no file behind.
        if self.pending.is_empty() && self.review.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let payload = serde_json::json!({
            "commit": self.pending_commit,
            "comments": self.pending,
            "body": self.review.body(),
            "verdict": self.review.verdict.api(),
        });
        if let Err(e) = std::fs::write(&path, payload.to_string()) {
            self.err(format!("Couldn't save the held comments: {e}"));
        }
    }

    /// Hold one inline comment back for the review instead of posting it.
    fn add_to_review(&mut self, draft: ReviewComment) {
        let first = self.pending.is_empty();
        let where_at = draft.where_at();
        self.pending.push(draft);
        self.pending_commit = self.pr.as_ref().map(|p| p.head_ref_oid.clone());
        self.save_pending();
        let n = self.pending.len();
        if first {
            self.ok(format!(
                "✎ Review started — {where_at} held. Nothing is on GitHub yet; R opens the review box."
            ));
        } else {
            self.ok(format!(
                "✎ {where_at} held — {n} comments in this review. R opens the review box."
            ));
        }
    }

    /// How many held comments there are for a file, for the panel badge.
    pub fn pending_in(&self, path: &str) -> usize {
        self.pending.iter().filter(|c| c.path == path).count()
    }

    /// True when a held comment covers the display row — the 💬 in the
    /// change bar, so a note already written is visible where it was left.
    pub fn pending_on_row(&self, display_row: usize) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        let Some(path) = self.files.get(self.file_cursor).map(|f| f.path.as_str()) else {
            return false;
        };
        let Some(DisplayEntry::Line(i)) = self.display.get(display_row).copied() else {
            return false;
        };
        let Some(diff) = &self.diff else { return false };
        let (row, only) = match self.view {
            ViewMode::SideBySide => (diff.rows.get(i), None),
            ViewMode::Inline => {
                let Some(e) = diff.inline.get(i) else {
                    return false;
                };
                (diff.rows.get(e.row), Some(e.side))
            }
        };
        let Some(row) = row else { return false };
        let covers = |c: &ReviewComment, ln: usize| {
            let lo = c.start_line.unwrap_or(c.line).min(c.line);
            ln >= lo && ln <= c.line
        };
        self.pending.iter().any(|c| {
            if c.path != path {
                return false;
            }
            let side = match c.side {
                CommentSide::Left => Side::Left,
                CommentSide::Right => Side::Right,
            };
            // An inline row shows one side, so only that side can match.
            if only.is_some_and(|s| s != side) {
                return false;
            }
            match side {
                Side::Left => row.old_ln.is_some_and(|n| covers(c, n)),
                Side::Right => row.new_ln.is_some_and(|n| covers(c, n)),
            }
        })
    }

    /// True when the review composer should be drawn at all: a pull
    /// request review, with the pane showing the diff rather than a
    /// document or an editor.
    pub fn review_box_on(&self) -> bool {
        self.screen == Screen::Review
            && !self.local
            && self.pr.is_some()
            && self.editor.is_none()
            && self.preview.is_none()
    }

    /// `R`, and a click in the box: give it the keyboard.
    pub fn focus_review(&mut self) {
        if !self.review_box_on() {
            self.err("No pull request open — a review needs one (b for the PR list).");
            return;
        }
        self.review.focused = true;
        self.selection = None;
        self.ok("Review summary — Ctrl+S submits · Tab changes the verdict · Esc leaves it.");
    }

    fn blur_review(&mut self) {
        self.review.focused = false;
        self.review.picking = false;
        self.save_pending();
    }

    /// Set the verdict the button will send.
    pub fn set_verdict(&mut self, v: Verdict) {
        self.review.verdict = v;
        self.review.picking = false;
        self.save_pending();
        self.ok(format!("{} {} — Ctrl+S submits it.", v.icon(), v.label()));
    }

    fn cycle_verdict(&mut self) {
        let all = Verdict::all();
        let at = all
            .iter()
            .position(|v| *v == self.review.verdict)
            .unwrap_or(0);
        self.set_verdict(all[(at + 1) % all.len()]);
    }

    /// Ask before the review goes up. Submitting notifies everyone
    /// watching the pull request and cannot be taken back, so it is the
    /// second thing loupe confirms — reverting is the first.
    pub fn ask_submit_review(&mut self) {
        if !self.review_box_on() {
            self.err("No pull request open — a review needs one (b for the PR list).");
            return;
        }
        let verdict = self.review.verdict;
        let body = self.review.body();
        if body.trim().is_empty() && self.pending.is_empty() {
            self.err("Nothing to send — write a summary, or hold a comment with c first.");
            return;
        }
        // GitHub refuses "request changes" with nothing said. An approval
        // is a complete statement on its own, and a plain comment is
        // carried by its inline notes.
        if verdict == Verdict::RequestChanges && body.trim().is_empty() {
            self.err("Request changes needs a summary — say what has to change.");
            return;
        }
        let Some(pr) = &self.pr else { return };
        // Comments anchored to a head that has since moved point at lines
        // that may no longer exist, and GitHub refuses the whole review
        // over one of them — so the prompt says so before it is sent.
        let stale = !self.pending.is_empty()
            && self
                .pending_commit
                .as_deref()
                .is_some_and(|c| c != pr.head_ref_oid);
        self.review.picking = false;
        self.overlay = Overlay::ReviewConfirm(Box::new(ReviewPrompt {
            number: pr.number,
            verdict,
            body,
            comments: self.pending.clone(),
            stale,
        }));
    }

    /// Send it.
    fn spawn_submit_review(&mut self) {
        let Overlay::ReviewConfirm(prompt) = &self.overlay else {
            return;
        };
        let (Some(repo), Some(pr)) = (self.repo.clone(), self.pr.as_ref()) else {
            return;
        };
        let number = pr.number;
        let commit = self
            .pending_commit
            .clone()
            .unwrap_or_else(|| pr.head_ref_oid.clone());
        let (verdict, body) = (prompt.verdict, prompt.body.clone());
        let comments = prompt.comments.clone();
        let n = comments.len();
        self.overlay = Overlay::None;
        self.spawn(
            format!("Submitting your review of PR #{number}"),
            false,
            false,
            move || {
                github::submit_review(&repo, number, &commit, &body, verdict, &comments)?;
                Ok(Outcome::ReviewSubmitted { verdict, count: n })
            },
        );
    }

    /// Throw the held comments away.
    ///
    /// Asks once, the way the editor asks before dropping unsaved text:
    /// these are written words with no copy anywhere else, and one stray
    /// click on ✕ should not cost a review.
    pub fn discard_pending(&mut self) {
        if self.pending.is_empty() {
            self.err("No held comments to discard.");
            self.discard_armed = false;
            return;
        }
        let n = self.pending.len();
        let s = if n == 1 { "" } else { "s" };
        if !self.discard_armed {
            self.discard_armed = true;
            self.err(format!(
                "Discard {n} held comment{s}? Press ✕ again (or the menu line) to throw them away."
            ));
            return;
        }
        self.discard_armed = false;
        self.pending.clear();
        self.pending_commit = None;
        self.save_pending();
        self.ok(format!(
            "Discarded {n} held comment{s} — nothing was ever sent to GitHub."
        ));
    }
    // -------------------------------------------------------- conflicts

    /// How many files of the open review are conflicted.
    pub fn conflict_count(&self) -> usize {
        self.files.iter().filter(|f| f.conflicted).count()
    }

    /// True when the file at `idx` is conflicted.
    pub fn file_conflicted(&self, idx: usize) -> bool {
        self.files.get(idx).is_some_and(|f| f.conflicted)
    }

    /// The one-line summary of the merge, for the status bar after a scan.
    /// `None` when nothing is conflicted.
    fn conflict_note(&self) -> Option<String> {
        let n = self.conflict_count();
        if n == 0 {
            return None;
        }
        let what = if n == 1 { "file has" } else { "files have" };
        let finish = self.finish_hint();
        Some(format!(
            "⚠ {n} {what} merge conflicts — press o to resolve one.{finish}"
        ))
    }

    /// The conflict a display row belongs to, when the open file is a
    /// conflict view. Fold banners and agreed lines belong to none.
    pub fn conflict_on_row(&self, display_row: usize) -> Option<usize> {
        let view = self.conflict.as_ref()?;
        let entry = *self.display.get(display_row)?;
        let DisplayEntry::Line(i) = entry else {
            return None;
        };
        let diff = self.diff.as_ref()?;
        let row = match self.view {
            ViewMode::SideBySide => diff.rows.get(i)?,
            ViewMode::Inline => diff.rows.get(diff.inline.get(i)?.row)?,
        };
        view.owner(row.old_ln, row.new_ln)
    }

    /// What the change bar draws on a conflicted file: `Some(true)` on the
    /// first row of a conflict (the ⚑ marker), `Some(false)` further down
    /// it, None on the lines both branches agree on.
    pub fn conflict_bar(&self, display_row: usize) -> Option<bool> {
        let here = self.conflict_on_row(display_row)?;
        let above = display_row
            .checked_sub(1)
            .and_then(|i| self.conflict_on_row(i));
        Some(above != Some(here))
    }

    /// Open the resolve menu for the conflict on `display_row`, anchored
    /// at (`x`, `y`). With no conflict on that row the menu covers the
    /// whole file, which is the only thing left to offer.
    pub fn open_conflict_menu(&mut self, display_row: usize, x: u16, y: u16) {
        let Some(file) = self.files.get(self.file_cursor) else {
            self.err("No file open — pick one on the left.");
            return;
        };
        if !file.conflicted {
            self.err("No merge conflict in this file — ⚠ marks the ones that have one.");
            return;
        }
        let path = file.path.clone();
        let hunk = self.conflict_on_row(display_row);
        let view = self.conflict.as_ref();
        let mut items = Vec::new();
        let title = match (hunk, view) {
            (Some(i), Some(v)) => {
                let h = &v.file.hunks[i];
                let (ours_n, theirs_n) = h.counts();
                let (ours, theirs) = v.file.labels();
                items.push(ConflictItem {
                    key: 'o',
                    label: format!("Take ours — {ours}"),
                    note: format!("{ours_n} line{}", if ours_n == 1 { "" } else { "s" }),
                    act: ConflictAction::Take(Resolution::Ours),
                });
                items.push(ConflictItem {
                    key: 't',
                    label: format!("Take theirs — {theirs}"),
                    note: format!("{theirs_n} line{}", if theirs_n == 1 { "" } else { "s" }),
                    act: ConflictAction::Take(Resolution::Theirs),
                });
                items.push(ConflictItem {
                    key: 'b',
                    label: "Take both".into(),
                    note: "ours first, then theirs".into(),
                    act: ConflictAction::Take(Resolution::Both),
                });
                if h.base.is_some() {
                    items.push(ConflictItem {
                        key: 'a',
                        label: "Take the common ancestor".into(),
                        note: "what both branches started from".into(),
                        act: ConflictAction::Take(Resolution::Base),
                    });
                }
                format!("Conflict {} of {}", i + 1, v.file.len())
            }
            _ => format!("{path} — whole file"),
        };
        // The whole-file lines, always. They are the only answer for a
        // conflict with no markers to read.
        if let Some(v) = view {
            let n = v.file.len();
            if n > 1 || hunk.is_none() {
                items.push(ConflictItem {
                    key: 'O',
                    label: "Take ours everywhere".into(),
                    note: format!("all {n} conflicts in this file"),
                    act: ConflictAction::TakeAll(Resolution::Ours),
                });
                items.push(ConflictItem {
                    key: 'T',
                    label: "Take theirs everywhere".into(),
                    note: format!("all {n} conflicts in this file"),
                    act: ConflictAction::TakeAll(Resolution::Theirs),
                });
            }
        } else {
            items.push(ConflictItem {
                key: 'o',
                label: "Take our whole file".into(),
                note: "the version on this branch".into(),
                act: ConflictAction::TakeSide { ours: true },
            });
            items.push(ConflictItem {
                key: 't',
                label: "Take their whole file".into(),
                note: "the version being merged in".into(),
                act: ConflictAction::TakeSide { ours: false },
            });
        }
        items.push(ConflictItem {
            key: 'e',
            label: "Edit it by hand".into(),
            note: "opens the file with its markers".into(),
            act: ConflictAction::EditByHand,
        });
        items.push(ConflictItem {
            key: 'x',
            label: "Mark it resolved".into(),
            note: "git add — do this once it reads right".into(),
            act: ConflictAction::MarkResolved,
        });
        self.overlay = Overlay::ConflictMenu(Box::new(ConflictMenu {
            path,
            hunk,
            title,
            items,
            sel: 0,
            anchor: (x, y),
        }));
    }

    /// `o`: the resolve menu for the conflict under the keyboard cursor,
    /// drawn where the diff cursor is.
    pub fn open_conflict_menu_from_key(&mut self) {
        let r = self.layout.diff;
        let y = r.y + (self.diff_cursor.saturating_sub(self.diff_scroll)) as u16;
        self.open_conflict_menu(self.diff_cursor, r.x + 2, y.min(r.y + r.height));
    }

    /// Run the conflict menu line at `i` and close the menu.
    fn conflict_menu_run(&mut self, i: usize) {
        let Overlay::ConflictMenu(menu) = &self.overlay else {
            return;
        };
        let Some(item) = menu.items.get(i) else {
            return;
        };
        let (act, path, hunk) = (item.act, menu.path.clone(), menu.hunk);
        self.overlay = Overlay::None;
        match act {
            ConflictAction::Take(how) => self.resolve_hunk(&path, hunk, how),
            ConflictAction::TakeAll(how) => self.resolve_all(&path, how),
            ConflictAction::TakeSide { ours } => self.take_whole_side(&path, ours),
            ConflictAction::EditByHand => self.open_editor(None),
            ConflictAction::MarkResolved => self.mark_resolved(&path),
        }
    }

    /// Write one conflict's chosen side back to the file, then reload.
    fn resolve_hunk(&mut self, path: &str, hunk: Option<usize>, how: Resolution) {
        let Some(idx) = hunk else {
            self.err("No conflict on that line — ⚑ marks each one.");
            return;
        };
        let Some(view) = &self.conflict else { return };
        if idx >= view.file.len() {
            return;
        }
        let text = view.file.resolve_one(idx, how);
        let left = view.file.len() - 1;
        self.write_resolution(path, text, format!("kept {}", how.label()), left);
    }

    /// The same, for every conflict in the file at once.
    fn resolve_all(&mut self, path: &str, how: Resolution) {
        let Some(view) = &self.conflict else {
            self.err("Nothing to resolve — this file has no conflict markers.");
            return;
        };
        let n = view.file.len();
        let text = view.file.resolve_all(how);
        self.write_resolution(
            path,
            text,
            format!(
                "kept {} in all {n} conflict{}",
                how.label(),
                if n == 1 { "" } else { "s" }
            ),
            0,
        );
    }

    /// Write resolved text, stage the file when nothing is left to
    /// resolve, and reload the pane in place.
    ///
    /// Staging is part of resolving: git treats a path as conflicted until
    /// it is added, so a file left unstaged would keep showing the warning
    /// after the last conflict was settled. `x` puts it back.
    fn write_resolution(&mut self, path: &str, text: String, what: String, left: usize) {
        if let Err(e) = gitops::write_repo_file(&self.repo_root, path, &text) {
            self.err(format!("Couldn't write {path}: {e:#}"));
            return;
        }
        if left == 0 {
            match gitops::stage_file(&self.repo_root, path, None) {
                Ok(()) => self.resolved(format!(
                    "✔ Resolved {path} — {what}, and staged it.{}",
                    self.finish_hint()
                )),
                // The file is written either way; only the index missed out.
                Err(e) => self.err(format!("Wrote {path}, but `git add` failed: {e:#}")),
            }
        } else {
            self.resolved(format!(
                "✔ Resolved 1 conflict — {what}. {left} left in this file."
            ));
        }
        self.after_resolution();
    }

    /// " Finish with `git commit`." — empty outside a merge.
    fn finish_hint(&self) -> String {
        match self.merge_op {
            Some(op) => format!(" Finish with `{}`.", op.finish()),
            None => String::new(),
        }
    }

    /// Say what a resolution did, and say it again after the re-scan that
    /// follows has had its turn.
    fn resolved(&mut self, msg: String) {
        self.ok(msg.clone());
        self.resolved_note = Some(msg);
    }

    /// Take a whole path from the index rather than from marker text.
    fn take_whole_side(&mut self, path: &str, ours: bool) {
        let which = if ours { "ours" } else { "theirs" };
        let finish = self.finish_hint();
        match gitops::take_side(&self.repo_root, path, ours) {
            Ok(true) => self.resolved(format!(
                "✔ Resolved {path} — took {which}, and staged it.{finish}"
            )),
            Ok(false) => self.resolved(format!(
                "✔ Resolved {path} — {which} deleted it, so it is gone and staged.{finish}"
            )),
            Err(e) => {
                self.err(format!("Couldn't take {which} for {path}: {e:#}"));
                return;
            }
        }
        self.after_resolution();
    }

    /// `git add` the file exactly as it stands, conflict markers included
    /// if any are left. This is what a hand resolution ends with.
    fn mark_resolved(&mut self, path: &str) {
        let still = gitops::safe_repo_path(&self.repo_root, path)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .is_some_and(|t| crate::conflict::has_markers(&t));
        if still {
            self.err(format!(
                "{path} still holds conflict markers — resolve them first, or edit it by hand."
            ));
            return;
        }
        let finish = self.finish_hint();
        match gitops::stage_file(&self.repo_root, path, None) {
            Ok(()) => {
                self.resolved(format!("✔ Marked {path} resolved.{finish}"));
                self.after_resolution();
            }
            Err(e) => self.err(format!("Couldn't stage {path}: {e:#}")),
        }
    }

    /// Re-read the tree after a resolution. The file list changes shape —
    /// a resolved file stops being a conflict and leaves the top group —
    /// so this is the full scan rather than the cheap index re-read.
    fn after_resolution(&mut self) {
        self.last_auto_rescan = Instant::now();
        if self.quiet.is_none() {
            // Quiet on purpose: the reader is told what the resolution did,
            // not what the re-scan behind it found.
            self.spawn_quiet_local(true);
        } else {
            self.refresh_stage_states();
        }
    }

    /// Ask before reverting one section of the open diff, named by the
    /// display row the click landed on. Deliberately not tied to the cursor:
    /// the marker you click is the change that goes.
    pub fn ask_revert_section(&mut self, display_row: usize) {
        // On a conflict view the sections are conflicts, not changes, and
        // the two sides are two branches rather than before and after.
        // Reverting one would write the wrong text, so `o` takes over —
        // asked before the guard, so `u` offers the menu instead of an
        // error the reader can do nothing with.
        if self.conflict.is_some() {
            self.open_conflict_menu(display_row, self.layout.diff.x + 2, self.layout.diff.y + 1);
            return;
        }
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
        if !self.revert_allowed_for(idx) {
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

    /// Shared guard for the open file: says why not, rather than doing
    /// nothing.
    fn revert_allowed(&mut self) -> bool {
        self.revert_allowed_for(self.file_cursor)
    }

    /// The same guard, for a file named by the row that was clicked.
    fn revert_allowed_for(&mut self, idx: usize) -> bool {
        // The layered diff is a reading of three versions, not a
        // before-and-after of two: its left column is the base branch,
        // and putting a section back would write the pull request's own
        // work out of the file along with the edit the reader meant. `s`
        // is one key away, and the flat diff reverts as it always has.
        if self.layers_now().on() {
            self.err(
                "Layers are on — the left column is the base branch, not the file as it was. Press s to turn them off before reverting.",
            );
            return false;
        }
        if self.file_conflicted(idx) {
            self.err(
                "This file has a merge conflict — press o to resolve it. Reverting would undo the merge.",
            );
            return false;
        }
        // A revert rewrites the file on disk. An open buffer holding
        // unsaved edits to that very file would then be a stale copy,
        // and saving it would quietly undo the revert — so that one case
        // is refused. Every other buffer is none of this file's business,
        // and refusing over one is how "close the editor first" came to
        // block a revert of a file nobody had open.
        let target = self.files.get(idx).map(|f| f.path.clone());
        if let Some(path) = target {
            if self
                .editor
                .as_ref()
                .is_some_and(|e| e.dirty && e.path == path)
            {
                self.err(format!(
                    "{path} has unsaved edits open — save it (Ctrl+S) or discard them (Esc twice) before reverting."
                ));
                return false;
            }
        }
        if !self.revert_possible() {
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

    /// How many files have something in the index (for the panel title).
    ///
    /// A partly staged file counts, because the STAGED heading is where
    /// it is listed. Two numbers on one screen that disagree about the
    /// same file would be worse than either number alone.
    pub fn staged_count(&self) -> usize {
        self.section_count(StageSection::Staged)
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
        if let Some(c) = &self.open_commit {
            self.err(format!(
                "{} is a commit — press F for the change to stage anything.",
                c.short
            ));
            return;
        }
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
        // The row moves between the sections on this frame; the re-read
        // only corrects it if git disagrees.
        self.rebuild_entries();
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

    /// `X`: stage the whole change, or take the whole change back out
    /// when there is nothing left to stage. One key for the question the
    /// two heading buttons ask separately.
    pub fn toggle_stage_all(&mut self) {
        if !self.local {
            self.err("Staging is for local changes — a pull request has no index here.");
            return;
        }
        if self.section_count(StageSection::Unstaged) > 0 {
            self.stage_all();
        } else {
            self.unstage_all();
        }
    }

    /// `git add -A`: everything the panel lists goes into the index, and
    /// the whole UNSTAGED section moves up into STAGED.
    ///
    /// A conflicted file is left out of the count but not out of the add.
    /// git treats a path as conflicted until it is added, so staging
    /// everything while a conflict is open would mark it resolved without
    /// anybody resolving it — the command is refused instead.
    pub fn stage_all(&mut self) {
        if !self.local {
            return;
        }
        if self.conflict_count() > 0 {
            self.err(
                "Resolve the merge conflicts first — staging them all would mark them resolved.",
            );
            return;
        }
        let n = self.section_count(StageSection::Unstaged);
        if n == 0 {
            self.ok("Everything is staged already.");
            return;
        }
        self.optimistic_stage_all(true);
        self.ok(format!(
            "✚ Staged {n} file{} — the working tree is untouched.",
            if n == 1 { "" } else { "s" }
        ));
        self.spawn_stage_all(true);
    }

    /// `git reset`: the index goes back to HEAD and the whole STAGED
    /// section drops back down. Nothing on disk moves.
    pub fn unstage_all(&mut self) {
        if !self.local {
            return;
        }
        let n = self.section_count(StageSection::Staged);
        if n == 0 {
            self.ok("Nothing is staged.");
            return;
        }
        self.optimistic_stage_all(false);
        self.ok(format!(
            "↩ Unstaged {n} file{} — the working tree is untouched.",
            if n == 1 { "" } else { "s" }
        ));
        self.spawn_stage_all(false);
    }

    /// Move every non-conflicted file across at once, so the panel redraws
    /// before git has answered. The real index arrives with the re-read.
    fn optimistic_stage_all(&mut self, add: bool) {
        let want = if add {
            StageState::Staged
        } else {
            StageState::Unstaged
        };
        let paths: Vec<String> = self
            .files
            .iter()
            .filter(|f| !f.conflicted)
            .map(|f| f.path.clone())
            .collect();
        for path in paths {
            self.stage.insert(path, want);
        }
        self.rebuild_entries();
    }

    /// Run the whole-index command on a worker thread and adopt the index
    /// it reads back, the same way one file's toggle does.
    fn spawn_stage_all(&mut self, add: bool) {
        let root = self.repo_root.clone();
        let before = self.stage.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let res = if add {
                gitops::stage_all(&root)
            } else {
                gitops::unstage_all(&root)
            };
            let _ = tx.send(res.and_then(|()| gitops::stage_states(&root).map(Some)));
        });
        self.bg_jobs.push(BgJob {
            rx,
            kind: BgKind::StageAll { before, add },
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
        self.overlay = Overlay::ThemePicker(ThemePicker::new(self.appearance_auto));
        self.ok("Pick a theme — j/k or click to preview, Enter to keep it, Esc to cancel.");
    }

    /// Keep the selected theme: save it to the global config and re-highlight
    /// whatever is open so the whole UI reflects it.
    /// Hand the appearance back to the terminal, and show what it says.
    ///
    /// This is the way out of a pinned appearance. Pinning one is a
    /// button press; before this, un-pinning it meant knowing the config
    /// file had an `appearance` key in it and going to delete it.
    fn pick_auto_appearance(&mut self) {
        let found = crate::theme::detect().unwrap_or_else(crate::theme::appearance);
        if let Overlay::ThemePicker(tp) = &mut self.overlay {
            tp.follow_terminal(found);
        }
    }

    fn apply_theme_pick(&mut self) {
        let Overlay::ThemePicker(tp) = &self.overlay else {
            return;
        };
        let name = highlight::theme_key(highlight::THEMES[tp.sel].1);
        let appearance = crate::theme::appearance();
        // The two appearances have their own theme slot, so saving a light
        // theme never clobbers the dark one (or the other way round).
        let mut pairs = vec![(crate::config::theme_key_for(appearance), name)];
        // Auto is written out too, not just left off: the reader may be
        // undoing a pin that is already in the file, and leaving the key
        // alone would leave the pin in place.
        let auto = tp.auto;
        let pinned = !auto && tp.appearance_changed();
        // The live check follows the setting, so choosing Auto starts the
        // terminal being asked again without a restart.
        self.appearance_auto = auto;
        if auto {
            pairs.push(("appearance", "auto"));
        } else if pinned {
            pairs.push(("appearance", appearance.key()));
        }
        self.overlay = Overlay::None;
        // Re-highlighting the open file replaces the status message when it
        // lands, so route the confirmation through the prepended note.
        let reload = self.screen == Screen::Review && self.diff.is_some();
        match crate::config::save_global(&pairs) {
            Ok(path) => {
                let note = if auto {
                    " (following the terminal)".to_string()
                } else if pinned {
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

    /// The `ChangedFile` behind a row, when the row has one.
    ///
    /// A row from the repository tree does not: it names a file the change
    /// never touched, so there is no diff to open, nothing to stage, and
    /// nothing to revert. Every caller has to answer for that case, which
    /// is why this returns an `Option` rather than panicking on a bad
    /// index the way `files[idx]` did.
    pub fn changed_of(&self, src: RowSrc) -> Option<&ChangedFile> {
        self.changed_idx(src).and_then(|i| self.files.get(i))
    }

    /// The index into `files` behind a row, for the callers that need to
    /// act on the changeset rather than read from it.
    /// A row in the repository list that names a file the change *does*
    /// touch resolves to it. So its stage mark, its viewed mark and its
    /// conflict mark all show in either panel, and clicking it opens the
    /// diff rather than the plain file — the two lists disagreeing about
    /// the same file would be the worse answer.
    pub fn changed_idx(&self, src: RowSrc) -> Option<usize> {
        match src {
            RowSrc::Changed(i) if i < self.files.len() => Some(i),
            RowSrc::Changed(_) => None,
            RowSrc::Path(i) => {
                let path = self.repo_paths.get(i)?;
                self.changed_by_path.get(path).copied()
            }
        }
    }

    /// The change behind a row, when the row is one that shows a change.
    ///
    /// The Files panel lists the repository, not the change under review.
    /// A row there is a file and nothing else, so it carries no staging
    /// box, no +/- counts and no revert mark, and clicking it opens the
    /// file rather than its diff. Every one of those belongs to the
    /// change, and the Changes panel one key away is where they live.
    ///
    /// The two panels share their rows, so the gate has to be here
    /// rather than at each of the six places that read one.
    pub fn diff_row(&self, src: RowSrc) -> Option<&ChangedFile> {
        if self.panel == PanelMode::Files {
            return None;
        }
        self.changed_of(src)
    }

    /// The same gate, for the callers that want the index.
    pub fn diff_row_idx(&self, src: RowSrc) -> Option<usize> {
        if self.panel == PanelMode::Files {
            return None;
        }
        self.changed_idx(src)
    }

    /// The collapse set for whichever list the panel is showing.
    pub(crate) fn collapsed(&mut self) -> &mut HashSet<String> {
        match self.panel {
            // Neither the Commits panel nor the Stack panel draws a
            // directory, so nothing asks either of them this. They answer
            // with the change's set rather than a third nobody reads.
            PanelMode::Changes | PanelMode::Commits | PanelMode::Stack => &mut self.collapsed_dirs,
            PanelMode::Files => &mut self.collapsed_files,
        }
    }

    /// The same set, for the renderer, which only reads it.
    pub fn collapsed_set(&self) -> &HashSet<String> {
        match self.panel {
            PanelMode::Changes | PanelMode::Commits | PanelMode::Stack => &self.collapsed_dirs,
            PanelMode::Files => &self.collapsed_files,
        }
    }

    /// The file the diff pane is showing.
    ///
    /// Usually the file at the cursor in the change; a file out of the
    /// Commits panel while one of those is open. Every part of the
    /// window that describes the diff asks this rather than `files`,
    /// which a commit's file is not in.
    pub fn open_file(&self) -> Option<&ChangedFile> {
        match &self.open_commit {
            Some(c) => self.commit_files.get(&c.oid)?.get(c.file),
            None => self.files.get(self.file_cursor),
        }
    }

    /// The path a row names, whichever list it came from.
    pub fn row_path(&self, src: RowSrc) -> Option<&str> {
        match src {
            RowSrc::Changed(i) => self.files.get(i).map(|f| f.path.as_str()),
            RowSrc::Path(i) => self.repo_paths.get(i).map(String::as_str),
        }
    }

    /// Switch the panel between the change and the whole repository.
    ///
    /// The listing is not waited for. Flipping to Files with nothing
    /// loaded shows an empty panel and a note for the moment the job takes;
    /// flipping back is instant, because both trees stay built.
    pub fn set_panel(&mut self, panel: PanelMode) {
        if self.panel == panel {
            return;
        }
        self.panel = panel;
        self.file_scroll = 0;
        match panel {
            PanelMode::Files => {
                self.refresh_repo_listing_if_stale();
                self.ok("Files — every file in the repository. F goes back to the change.");
            }
            PanelMode::Commits => {
                self.refresh_commits_if_stale();
                self.ok("Commits — what this branch has that the upstream does not.");
            }
            PanelMode::Stack => {
                self.ok(match &self.stack {
                    // What the reader most needs is not the size of the
                    // chain but what this link rests on: that is the
                    // branch its diff is read against, and the pull
                    // request that has to land before this one can.
                    Some(st) => match st.position.checked_sub(1).and_then(|p| st.at(p)) {
                        Some(under) => format!(
                            "Stack #{} — {} of {}, onto {}. This one sits on #{} {}.",
                            st.number,
                            st.position,
                            st.size,
                            st.base_ref_name,
                            under.number,
                            under.title
                        ),
                        None => format!(
                            "Stack #{} — {} of {}, straight onto {}.",
                            st.number, st.position, st.size, st.base_ref_name
                        ),
                    },
                    None => "This pull request is not in a stack.".to_string(),
                });
                self.reveal_stack_cursor();
            }
            PanelMode::Changes => {
                self.ok("Back to the files this change touches.");
                self.ensure_file_visible();
            }
        }
        self.rebuild_entries();
    }

    /// `F` walks the three panels in the order the toggle row draws them.
    pub fn toggle_panel(&mut self) {
        // The stack is only a stop on the walk when there is one. A
        // reader with no stacked pull request open should not pay for the
        // feature with an empty panel between two useful ones.
        let next = match self.panel {
            PanelMode::Changes => PanelMode::Files,
            PanelMode::Files => PanelMode::Commits,
            PanelMode::Commits if self.stack.is_some() => PanelMode::Stack,
            PanelMode::Commits | PanelMode::Stack => PanelMode::Changes,
        };
        self.set_panel(next);
    }

    // ----------------------------------------------------- the commits

    /// Read the commit list again if it has gone stale.
    ///
    /// A commit lands when somebody runs `git commit`, which loupe never
    /// does, so the list is re-read when the reader comes back to the
    /// panel rather than on a timer.
    fn refresh_commits_if_stale(&mut self) {
        if self.commits_job.is_some() {
            return;
        }
        let stale = match self.commits_read {
            None => true,
            Some(at) => at.elapsed() >= COMMIT_LIST_MAX_AGE,
        };
        if stale {
            self.spawn_commits();
        }
    }

    /// Read the unpushed commits on a worker thread.
    pub fn spawn_commits(&mut self) {
        let root = self.repo_root.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send((|| {
                let base = gitops::unpushed_base(&root);
                let commits = match &base {
                    Some(b) => gitops::unpushed_commits(&root, b)?,
                    None => Vec::new(),
                };
                Ok(CommitsData { base, commits })
            })());
        });
        self.commits_job = Some(rx);
    }

    /// The list arrived. Commits already opened stay open, so a refresh
    /// does not fold the panel up under the reader.
    pub(crate) fn apply_commits(&mut self, data: CommitsData) {
        self.commits_base = data.base;
        self.commits = data.commits;
        self.commits_read = Some(Instant::now());
        self.commits_note = if self.commits_base.is_none() {
            Some("No upstream to compare against — push the branch once, or set one with `git branch -u`.".into())
        } else if self.commits.is_empty() {
            Some(format!(
                "Nothing to push — level with {}.",
                self.commits_base.as_deref().unwrap_or("the upstream")
            ))
        } else {
            None
        };
        // A commit that is no longer in the list cannot stay open.
        let live: HashSet<String> = self.commits.iter().map(|c| c.oid.clone()).collect();
        self.open_commits.retain(|oid| live.contains(oid));
        if self.panel == PanelMode::Commits {
            self.rebuild_entries();
        }
    }

    /// Open or close one commit in the panel.
    ///
    /// Its file list is read here, once, and kept: a commit's files never
    /// change, so nothing cached about one goes stale. `git show
    /// --name-status` on a single commit is one process and a few
    /// milliseconds, which is cheap enough to pay for on the click that
    /// asks for it.
    pub fn toggle_commit(&mut self, idx: usize) {
        let Some(commit) = self.commits.get(idx).cloned() else {
            return;
        };
        if self.open_commits.remove(&commit.oid) {
            self.rebuild_entries();
            return;
        }
        if !self.commit_files.contains_key(&commit.oid) {
            match gitops::commit_files(&self.repo_root, &commit.oid) {
                Ok(files) => {
                    self.commit_files.insert(commit.oid.clone(), files);
                }
                Err(e) => {
                    self.err(format!("Couldn't read {}: {e:#}", commit.short));
                    return;
                }
            }
        }
        let n = self.commit_files.get(&commit.oid).map_or(0, Vec::len);
        self.open_commits.insert(commit.oid.clone());
        self.ok(format!(
            "{} {} — {n} file{}.",
            commit.short,
            commit.subject,
            if n == 1 { "" } else { "s" }
        ));
        self.rebuild_entries();
    }

    /// Read one file of one commit into the diff pane.
    ///
    /// The diff is the commit against its own first parent, both sides
    /// read from git. The copy on disk belongs to HEAD, which for every
    /// commit but the newest is a later version of the same file.
    pub fn open_commit_file(&mut self, idx: usize, file: usize) {
        let Some(commit) = self.commits.get(idx).cloned() else {
            return;
        };
        let Some(cf) = self
            .commit_files
            .get(&commit.oid)
            .and_then(|fs| fs.get(file))
            .cloned()
        else {
            return;
        };
        self.open_commit = Some(OpenCommit {
            oid: commit.oid.clone(),
            short: commit.short.clone(),
            subject: commit.subject.clone(),
            parent: gitops::first_parent(&self.repo_root, &commit.oid).unwrap_or_default(),
            file,
        });
        // The blame pane numbers lines in the working-tree file, which for
        // every commit but the newest is a later version of this one.
        self.clear_blame();
        let ctx = self.file_load_ctx();
        let label = format!("Loading {} at {}", cf.path, commit.short);
        self.spawn(label, true, false, move || {
            load_file_data(file, cf, ctx).map(|d| Outcome::FileLoaded(Box::new(d)))
        });
    }

    /// Leave the commit the diff pane is showing.
    ///
    /// The commit carries its own two refs, so nothing about the review
    /// had to move aside for it and nothing has to be put back.
    fn leave_open_commit(&mut self) {
        self.open_commit = None;
    }

    /// Re-read the repository if the listing has gone stale.
    ///
    /// Deliberately not a tick. `git ls-files` costs about 93 ms on a
    /// 16,761-file repository, and paying that on a timer while somebody
    /// reads would spend the whole budget this panel was built around. It
    /// runs when the reader comes back to the panel, and when they ask.
    fn refresh_repo_listing_if_stale(&mut self) {
        if self.repo_job.is_some() {
            return;
        }
        let stale = match self.repo_listed {
            None => true,
            Some(at) => at.elapsed() >= REPO_LISTING_MAX_AGE,
        };
        if stale {
            self.spawn_repo_listing();
        }
    }

    /// Read the repository's file list on a worker thread.
    ///
    /// Two `git ls-files` calls — about 93 ms for 16,761 tracked files, and
    /// 14 ms for the ignored half — so this can never be on the drawing
    /// thread. It is not modal either: the change under review is already
    /// on screen and stays readable while this lands.
    fn spawn_repo_listing(&mut self) {
        let root = self.repo_root.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(search::list_repo(&root));
        });
        self.repo_job = Some(rx);
    }

    /// The listing arrived: build the tree, and collapse all of it.
    ///
    /// Collapsed is not a nicety. Emitting a fully expanded 400,000-path
    /// tree costs 2.1 ms and a 400,000-entry `Vec` on every toggle;
    /// collapsed it costs about 2 µs and seven entries.
    pub(crate) fn apply_repo_listing(&mut self, listing: search::RepoListing) {
        // What the reader had opened, before the tree is replaced. A
        // refresh that collapsed the folder somebody was reading would be
        // worse than not refreshing at all.
        let was_open: HashSet<String> = self
            .file_tree
            .all_dirs()
            .into_iter()
            .filter(|d| !self.collapsed_files.contains(d))
            .collect();
        self.file_tree = TreeNodes::from_listing(&listing);
        self.collapsed_files = self
            .file_tree
            .all_dirs()
            .into_iter()
            .filter(|d| !was_open.contains(d))
            .collect();
        self.repo_listed = Some(Instant::now());
        self.repo_ignored = (0..listing.paths.len())
            .map(|i| i >= listing.ignored_from)
            .collect();
        self.repo_ignored_dirs = listing
            .stubs
            .iter()
            .filter(|(_, ignored)| *ignored)
            .map(|(dir, _)| dir.clone())
            .collect();
        self.repo_paths = listing.paths;
        if self.panel == PanelMode::Files {
            self.rebuild_entries();
        }
    }

    /// Apply a rename or a code action, and show every file it touched.
    ///
    /// The buffer on screen is edited in place, so one undo puts it back.
    /// Every other file the server named is **opened as an unsaved
    /// buffer** rather than written: a tool that silently rewrites twelve
    /// files is a tool nobody can review, and `Ctrl+S` per file is a small
    /// price for seeing what changed. The tab row shows a dot on each one
    /// until it is saved.
    ///
    /// A file the reader already has open is edited where it stands, so
    /// unsaved work in it is never read over.
    fn apply_workspace_edit(&mut self, what: &str, edits: Vec<(String, Vec<lsp::TextEdit>)>) {
        if edits.is_empty() {
            self.err(format!("Nothing to change for {what}."));
            return;
        }
        let here = self.editor.as_ref().map(|e| e.path.clone());
        let mut touched = 0usize;
        let mut opened = 0usize;
        for (path, file_edits) in edits {
            if Some(&path) == here.as_ref() {
                if let Some(ed) = &mut self.editor {
                    ed.apply_edits(&file_edits);
                }
                touched += 1;
                continue;
            }
            if self.switch_buffer(&path) {
                if let Some(ed) = &mut self.editor {
                    ed.apply_edits(&file_edits);
                }
                touched += 1;
                continue;
            }
            let Some(abs) = gitops::safe_repo_path(&self.repo_root, &path) else {
                self.err(format!("Refusing to change “{path}” — unsafe path."));
                continue;
            };
            if is_symlink(&abs) {
                self.err(format!("Skipped {path} — it is a symlink."));
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&abs) else {
                self.err(format!("Skipped {path} — cannot read it."));
                continue;
            };
            let mut ed = Editor::new(&path, abs, &content);
            ed.standalone = true;
            ed.apply_edits(&file_edits);
            ed.dirty = true;
            self.open_buffer(ed);
            touched += 1;
            opened += 1;
        }
        // Back to where the reader was.
        if let Some(path) = here {
            self.switch_buffer(&path);
        }
        self.ok(format!(
            "{what}: {touched} file{} changed{} — nothing is saved yet, Ctrl+S each.",
            if touched == 1 { "" } else { "s" },
            if opened > 0 {
                format!(", {opened} opened for you to read")
            } else {
                String::new()
            }
        ));
    }

    /// Read one directory the repository listing stopped at.
    ///
    /// One level, never a walk: the whole reason `git ls-files --directory`
    /// hands back `node_modules/` as a single row is that nobody wants its
    /// 17,000 descendants, and reading them here would give them back.
    /// A subdirectory found inside becomes a stub of its own, so going
    /// deeper costs another read and only if somebody goes deeper.
    fn spawn_read_dir(&mut self, dir: String) {
        if self.dir_job.is_some() {
            return;
        }
        let Some(abs) = gitops::safe_repo_path(&self.repo_root, &dir) else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(read_one_dir(&abs).map(|(files, dirs)| DirRead { dir, files, dirs }));
        });
        self.dir_job = Some(rx);
    }

    /// Splice a directory's contents into the cached tree.
    ///
    /// New paths are appended to `repo_paths`, never inserted, so every
    /// `RowSrc::Path` already on screen keeps pointing at the same file.
    /// They inherit the directory's own ignored answer, which is why that
    /// is stored per path rather than as a split point.
    fn apply_dir_read(&mut self, read: DirRead) {
        // Inside an ignored directory everything is ignored. Inside a
        // nested repository or a worktree nothing is — they are ordinary
        // files that git simply declined to walk into from here.
        let ignored = self
            .repo_ignored_dirs
            .iter()
            .any(|d| read.dir == *d || read.dir.starts_with(&format!("{d}/")));
        for name in &read.files {
            let path = format!("{}/{}", read.dir, name);
            let src = RowSrc::Path(self.repo_paths.len());
            self.repo_paths.push(path.clone());
            self.repo_ignored.push(ignored);
            self.file_tree.insert(&path, src);
        }
        for name in &read.dirs {
            let path = format!("{}/{}", read.dir, name);
            self.file_tree.dir_at(&path);
            // Collapsed, like every other directory in this panel.
            self.collapsed_files.insert(path);
        }
        self.file_tree.node_at(&read.dir).stub = false;
        self.file_tree.resort_at(&read.dir);
        self.rebuild_entries();
    }

    /// Whether a row names a file git is ignoring, which the panel draws
    /// dim — `.env` is worth opening, and worth knowing is not committed.
    pub fn row_ignored(&self, src: RowSrc) -> bool {
        match src {
            RowSrc::Path(i) => self.repo_ignored.get(i).copied().unwrap_or(false),
            RowSrc::Changed(_) => false,
        }
    }

    /// The file list changed: rebuild the directory tree, then the rows.
    ///
    /// Every path that assigns `self.files` ends here. Use
    /// [`Self::rebuild_entries`] when only the view changed — a collapse,
    /// or the tree/flat toggle — because that keeps the tree.
    pub fn rebuild_files(&mut self) {
        self.tree = TreeNodes::build(&self.files);
        self.changed_by_path = self
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| (f.path.clone(), i))
            .collect();
        self.rebuild_entries();
    }

    /// The view changed: emit the rows from the cached tree.
    ///
    /// This is on the click path for every collapse and expand, so it must
    /// stay free of the tree build. See [`TreeNodes`].
    pub fn rebuild_entries(&mut self) {
        let mut out: Vec<FileEntry> = Vec::new();
        match self.panel {
            PanelMode::Changes => self.emit_changes(&mut out),
            PanelMode::Files => self.emit_files(&mut out),
            PanelMode::Commits => self.emit_commits(&mut out),
            PanelMode::Stack => self.emit_stack(&mut out),
        }
        self.entries = out;
        let max = self.entries.len().saturating_sub(1);
        if self.file_scroll > max {
            self.file_scroll = max;
        }
    }

    /// The change under review: conflicts first under their heading, then
    /// the tree or the flat list.
    fn emit_changes(&self, out: &mut Vec<FileEntry>) {
        debug_assert_eq!(
            self.tree.built_from,
            self.files.len(),
            "the cached tree is stale — call rebuild_files() after changing self.files"
        );
        out.reserve(self.files.len() + 1);
        // `local_changes` already sorts conflicted files to the front of
        // `files`, so they are contiguous here.
        let conflicts: Vec<usize> = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| f.conflicted)
            .map(|(i, _)| i)
            .collect();
        if !conflicts.is_empty() {
            out.push(FileEntry::ConflictHeading {
                count: conflicts.len(),
            });
            out.extend(conflicts.iter().map(|idx| FileEntry::File {
                src: RowSrc::Changed(*idx),
                depth: 0,
            }));
        }
        // A pull request has no index to divide, so it keeps the one
        // list loupe has always shown.
        if !self.local {
            self.emit_section(None, out);
            return;
        }
        // Local review splits what is left in two: what `git commit`
        // would take, then what it would leave behind. Both headings are
        // drawn even when a section is empty — the heading is where the
        // "move the whole section" action lives, and a section that
        // vanished would take its own button with it.
        // A clean tree has no sections to head. Two empty headings over
        // nothing would say less than the empty panel does.
        if !self.files.iter().any(|f| !f.conflicted) {
            return;
        }
        for section in [StageSection::Staged, StageSection::Unstaged] {
            let count = self
                .files
                .iter()
                .filter(|f| !f.conflicted && self.section_of(&f.path) == section)
                .count();
            let collapsed = self.collapsed_sections.contains(&section);
            out.push(FileEntry::StageHeading {
                section,
                count,
                collapsed,
            });
            if !collapsed {
                self.emit_section(Some(section), out);
            }
        }
    }

    /// The rows of one staging section, or of the whole change when
    /// `section` is None (a pull request, which has no index).
    fn emit_section(&self, section: Option<StageSection>, out: &mut Vec<FileEntry>) {
        let keep = |src: RowSrc| {
            let RowSrc::Changed(i) = src else {
                return false;
            };
            let Some(file) = self.files.get(i) else {
                return false;
            };
            if file.conflicted {
                return false;
            }
            match section {
                None => true,
                Some(want) => self.section_of(&file.path) == want,
            }
        };
        if self.tree_view {
            // The tree skips conflicted files; they are already above it.
            self.tree.emit_where(&self.collapsed_dirs, &keep, out);
        } else {
            out.extend(
                (0..self.files.len())
                    .filter(|i| keep(RowSrc::Changed(*i)))
                    .map(|idx| FileEntry::File {
                        src: RowSrc::Changed(idx),
                        depth: 0,
                    }),
            );
        }
    }

    /// Which half of the panel a file belongs in.
    ///
    /// A partly staged file is in the index and in the working tree at
    /// once. It is listed once, under STAGED, with the `[±]` icon that
    /// says the rest of it is not — listing it twice would make the two
    /// counts add up to more than the change has files.
    pub fn section_of(&self, path: &str) -> StageSection {
        match self.stage_state(path) {
            StageState::Staged | StageState::Partial => StageSection::Staged,
            StageState::Unstaged | StageState::Conflicted => StageSection::Unstaged,
        }
    }

    /// How many files sit in one section, for the heading and for the
    /// commands that act on a whole section.
    pub fn section_count(&self, section: StageSection) -> usize {
        self.files
            .iter()
            .filter(|f| !f.conflicted && self.section_of(&f.path) == section)
            .count()
    }

    /// The commits the upstream does not have, each openable to its files.
    fn emit_commits(&self, out: &mut Vec<FileEntry>) {
        if self.commits.is_empty() {
            let note = self
                .commits_note
                .clone()
                .unwrap_or_else(|| "Reading the commit list…".to_string());
            out.push(FileEntry::CommitNote(note));
            return;
        }
        for (idx, c) in self.commits.iter().enumerate() {
            let open = self.open_commits.contains(&c.oid);
            out.push(FileEntry::CommitRow { idx, open });
            if !open {
                continue;
            }
            let Some(files) = self.commit_files.get(&c.oid) else {
                continue;
            };
            out.extend((0..files.len()).map(|file| FileEntry::CommitFileRow { commit: idx, file }));
        }
    }

    /// The files one commit changed, once it has been opened.
    pub fn files_of_commit(&self, idx: usize) -> Option<&[ChangedFile]> {
        let oid = &self.commits.get(idx)?.oid;
        self.commit_files.get(oid).map(Vec::as_slice)
    }

    // ------------------------------------------------------- the stack

    /// The stack as a ladder, top first.
    ///
    /// The entries are held bottom-first, the order they land in and the
    /// order GitHub numbers them. They are drawn the other way up because
    /// that is how a stack is talked about and how everything else draws
    /// one: the trunk at the bottom, each pull request resting on the one
    /// under it. The trunk gets a row of its own at the foot, so the
    /// ladder says what the chain is aimed at without the reader having
    /// to read the panel title.
    fn emit_stack(&self, out: &mut Vec<FileEntry>) {
        let Some(st) = &self.stack else {
            out.push(FileEntry::StackNote(
                "This pull request is not in a stack.".into(),
            ));
            return;
        };
        for idx in (0..st.entries.len()).rev() {
            out.push(FileEntry::StackRow { idx });
        }
        out.push(FileEntry::StackTrunk);
    }

    /// Take a fresh reading of the stack.
    ///
    /// The panel has to give way when a stack goes: a refresh where the
    /// last pull request in the chain was merged, or a swap to local
    /// review, would otherwise leave the reader looking at a panel with
    /// nothing behind it and no obvious way out.
    fn set_stack(&mut self, stack: Option<github::Stack>) {
        self.stack = stack;
        if self.stack.is_none() && self.panel == PanelMode::Stack {
            self.panel = PanelMode::Changes;
            self.file_scroll = 0;
        }
        if self.panel == PanelMode::Stack {
            self.rebuild_entries();
            self.reveal_stack_cursor();
        }
    }

    /// Scroll the panel so the pull request being reviewed is on screen.
    /// A long stack opened from the middle would otherwise start at the
    /// top with no sign of where the reader actually is.
    fn reveal_stack_cursor(&mut self) {
        let Some(here) = self.stack.as_ref().and_then(|st| st.cursor()) else {
            return;
        };
        let Some(pos) = self
            .entries
            .iter()
            .position(|e| matches!(e, FileEntry::StackRow { idx } if *idx == here))
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

    /// Move one rung up or down the stack — `Alt+↑` and `Alt+↓`.
    ///
    /// The same two directions `gh stack up` and `gh stack down` name, so
    /// the habit carries over from the extension. Up is away from the
    /// trunk. It lands on the same prompt a click on the rung does.
    pub fn step_stack(&mut self, delta: i32) {
        let Some(st) = &self.stack else {
            self.err("This pull request is not in a stack.");
            return;
        };
        let Some(here) = st.cursor() else {
            return;
        };
        let next = here as i32 + delta;
        if next < 0 || next as usize >= st.entries.len() {
            self.err(if delta > 0 {
                "Top of the stack — nothing rests on this one."
            } else {
                "Bottom of the stack — this one sits straight on the base branch."
            });
            return;
        }
        self.open_stack_pr(next as usize);
    }

    /// Open another pull request out of the stack.
    ///
    /// Through the same prompt the pull request list uses, because it is
    /// the same question and it has the same two answers: check the
    /// branch out, or read it where you stand. Moving up a stack usually
    /// means checking out, and a click that rewrote the working tree
    /// without asking would be a click nobody could take back.
    pub fn open_stack_pr(&mut self, idx: usize) {
        let Some(entry) = self.stack.as_ref().and_then(|st| st.entries.get(idx)) else {
            return;
        };
        let number = entry.number;
        if self.pr.as_ref().is_some_and(|p| p.number == number) {
            self.ok(format!("PR #{number} — already open."));
            return;
        }
        self.overlay = Overlay::CheckoutPrompt(number);
    }

    /// The whole repository. No conflict heading — a merge conflict is a
    /// fact about the change, and this list is not about the change.
    fn emit_files(&self, out: &mut Vec<FileEntry>) {
        if self.tree_view {
            self.file_tree.emit(&self.collapsed_files, out);
        } else {
            out.extend((0..self.repo_paths.len()).map(|i| FileEntry::File {
                src: RowSrc::Path(i),
                depth: 0,
            }));
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
        // Arming is for the very next key, not for later. Anything but a
        // second `q` puts the guard back.
        if self.quit_armed && key.code != KeyCode::Char('q') {
            self.quit_armed = false;
        }
        // A foreground job is modal: only cancel/quit get through.
        if self.job.is_some() {
            match key.code {
                KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Esc => self.cancel_job(),
                KeyCode::Char('q') => self.request_quit(),
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
            // Read, then dismissed with any key, the way the help card
            // is. There is nothing to choose in it.
            Overlay::Problem(_) => {
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
                    self.run_path_menu(i);
                }
                return;
            }
            Overlay::CodeActions(menu) => {
                let last = menu.actions.len().saturating_sub(1);
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => menu.sel = menu.sel.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => menu.sel = (menu.sel + 1).min(last),
                    KeyCode::Enter => {
                        let chosen = menu.actions.get(menu.sel).cloned();
                        self.overlay = Overlay::None;
                        if let Some(a) = chosen {
                            self.apply_workspace_edit(&a.title, a.edits);
                        }
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.overlay = Overlay::None;
                        self.ok("Nothing applied.");
                    }
                    _ => {}
                }
                return;
            }
            Overlay::ReviewConfirm(_) => {
                match key.code {
                    KeyCode::Enter | KeyCode::Char('y') => self.spawn_submit_review(),
                    KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                        self.overlay = Overlay::None;
                        self.ok("Not sent — your comments are still held here.");
                    }
                    _ => {}
                }
                return;
            }
            Overlay::VerdictMenu => {
                let all = Verdict::all();
                let last = all.len() - 1;
                let mut close = false;
                let hit = match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.review.pick = self.review.pick.saturating_sub(1);
                        None
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.review.pick = (self.review.pick + 1).min(last);
                        None
                    }
                    KeyCode::Enter => Some(self.review.pick),
                    KeyCode::Esc | KeyCode::Char('q') => {
                        close = true;
                        None
                    }
                    // The first letter of each verdict: c, a, r.
                    KeyCode::Char(c) => all.iter().position(|v| {
                        v.label().chars().next().map(|f| f.to_ascii_lowercase()) == Some(c)
                    }),
                    _ => None,
                };
                if close {
                    self.overlay = Overlay::None;
                    self.review.picking = false;
                } else if let Some(i) = hit {
                    self.overlay = Overlay::None;
                    self.set_verdict(all[i.min(last)]);
                }
                return;
            }
            // The path box owns every printable key while it is open —
            // it is a text field, and a path is made of letters the
            // review screen would otherwise treat as commands.
            Overlay::OpenPath(box_) => {
                match key.code {
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        box_.insert(&c.to_string());
                    }
                    KeyCode::Backspace => box_.backspace(),
                    KeyCode::Delete => box_.delete(),
                    KeyCode::Left => box_.move_caret(-1),
                    KeyCode::Right => box_.move_caret(1),
                    KeyCode::Home => box_.caret = 0,
                    KeyCode::End => box_.caret = box_.input.chars().count(),
                    // Ctrl+U clears the line, as it does in a shell.
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        box_.input.clear();
                        box_.caret = 0;
                    }
                    KeyCode::Enter => {
                        self.confirm_open_path();
                        return;
                    }
                    KeyCode::Esc => {
                        self.overlay = Overlay::None;
                        self.ok("Closed — no file opened.");
                    }
                    _ => {}
                }
                return;
            }
            Overlay::ConflictMenu(menu) => {
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
                    // Every line has its own letter, so resolving a
                    // conflict is one key press.
                    KeyCode::Char(c) => menu.items.iter().position(|it| it.key == c),
                    _ => None,
                };
                if close {
                    self.overlay = Overlay::None;
                    self.ok("Left alone — nothing was resolved.");
                } else if let Some(i) = hit {
                    self.conflict_menu_run(i);
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
                    // The two ways a comment can leave this box. Holding
                    // it is the primary one: a review reads as one piece,
                    // and notifies its watchers once instead of per note.
                    (KeyCode::Char('s'), KeyModifiers::CONTROL) => self.hold_comment(),
                    (KeyCode::Enter, KeyModifiers::CONTROL) => self.spawn_post_comment(),
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
                    // `a` pins one appearance; `A` hands it back to the
                    // terminal, which is the setting that follows a
                    // system that turns dark in the evening.
                    KeyCode::Char('a') => {
                        tp.toggle_appearance();
                        tp.pin_appearance();
                    }
                    KeyCode::Char('A') => self.pick_auto_appearance(),
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

        // The tab row, from anywhere — a text box included, which is why
        // these keys all hold a modifier. The bare `1`-`9`, `,`, `.`, `=`
        // and `-` do the same thing on the screens where a bare key is
        // not a letter (see the review and preview blocks below).
        if self.pin_key(key) {
            return;
        }

        // The `/` prompt owns every keystroke while it is open.
        if self.find.typing {
            self.find_key(key);
            return;
        }

        // The review box, while it has the keyboard. It is a panel rather
        // than an overlay — the diff stays readable beside it — so it
        // takes keys here instead of in the overlay match above.
        if self.review.focused {
            match (key.code, key.modifiers) {
                (KeyCode::Esc, _) => {
                    self.blur_review();
                    self.ok("Left the review box — R comes back to it.");
                }
                (KeyCode::Char('s'), KeyModifiers::CONTROL) => self.ask_submit_review(),
                // Tab picks the verdict from the keyboard; the ▾ does the
                // same with the mouse.
                (KeyCode::Tab, _) => self.cycle_verdict(),
                (KeyCode::BackTab, _) => {
                    let all = Verdict::all();
                    let at = all
                        .iter()
                        .position(|v| *v == self.review.verdict)
                        .unwrap_or(0);
                    self.set_verdict(all[(at + all.len() - 1) % all.len()]);
                }
                _ => {
                    self.review.textarea.input(key);
                    // Cheap enough to write on every keystroke, and it is
                    // what makes a summary survive a crash.
                    self.save_pending();
                }
            }
            return;
        }

        // Preview mode. Reading keys only: everything that changes the
        // document happens in the source view, one `P` away.
        if self.preview.is_some() {
            let page = self.preview.as_ref().map(|p| p.page()).unwrap_or(10);
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            // The prompt is a mode: while it is open every key is part of
            // the query, so it is asked first and nothing below it runs.
            if self.preview.as_ref().is_some_and(|pv| pv.find.typing) {
                self.preview_find_key(key);
                return;
            }
            if self.pending_g {
                self.pending_g = false;
                if let (KeyCode::Char('g'), Some(pv)) = (key.code, &mut self.preview) {
                    pv.scroll_to_top();
                    return;
                }
            }
            let searching = self.preview_find_active();
            match key.code {
                // Find in this file, on the two keys the rest of loupe
                // puts it on. Ctrl+F was vim's forward-page here;
                // PageDown, Space and Ctrl+D all still are.
                _ if is_find_key(key) => self.start_preview_find(),
                _ if is_find_all_key(key) => self.open_finder(FinderMode::Grep),
                KeyCode::Char('/') => self.start_preview_find(),
                KeyCode::Char('n') if searching => self.step_preview_match(true),
                KeyCode::Char('N') if searching => self.step_preview_match(false),
                // Esc clears the search before it closes the pane: the
                // first one puts the highlight away, the second puts the
                // document away.
                KeyCode::Esc if searching => {
                    if let Some(pv) = &mut self.preview {
                        pv.clear_find();
                    }
                    self.ok("Search cleared.");
                }
                KeyCode::Char('P') | KeyCode::Char('e') | KeyCode::Char('i') => {
                    self.toggle_preview();
                }
                KeyCode::Esc => self.close_preview(),
                // `r` re-reads the document rather than the review, which
                // is the one thing this pane means by "refresh".
                KeyCode::Char('r') => self.reload_preview(),
                KeyCode::Char('g') => self.pending_g = true,
                // `loupe md <file>` has no review behind the document, so
                // the keys that would go there are left off.
                _ if self.preview_only
                    && matches!(
                        key.code,
                        KeyCode::Char('b')
                            | KeyCode::Char('`')
                            | KeyCode::Char(']')
                            | KeyCode::Char('[')
                            | KeyCode::Char('x')
                            | KeyCode::Char('X')
                            | KeyCode::Char('S')
                            | KeyCode::Char('F')
                    ) => {}
                // Reading a document is not a reason to be locked out of
                // the app: everything the review answers, this answers.
                _ if self.global_key(key) => {}
                _ => {
                    let Some(pv) = &mut self.preview else { return };
                    match key.code {
                        KeyCode::Down | KeyCode::Char('j') => pv.scroll_rows(1),
                        KeyCode::Up | KeyCode::Char('k') => pv.scroll_rows(-1),
                        KeyCode::Char('d') if ctrl => pv.scroll_rows(page / 2),
                        KeyCode::Char('u') if ctrl => pv.scroll_rows(-page / 2),
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
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.copy_from_editor(),
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
                    self.spawn_editor_request(EditorRequest::Format);
                }
                (KeyCode::Char(' '), KeyModifiers::CONTROL) => {
                    self.completion_due = None;
                    self.spawn_editor_request(EditorRequest::Complete(None));
                }
                // The function keys, where an editor puts them. They cost
                // no modifier and collide with nothing the buffer types,
                // so they are the shortest way to the two questions a
                // reader asks most.
                (KeyCode::F(12), m) if m.contains(KeyModifiers::SHIFT) => {
                    self.spawn_editor_request(EditorRequest::References);
                }
                (KeyCode::F(12), _) => {
                    self.spawn_editor_request(EditorRequest::Definition);
                }
                (KeyCode::F(10), _) => {
                    self.spawn_editor_request(EditorRequest::References);
                }
                (KeyCode::F(2), _) => {
                    self.activate(ButtonId::EditorRename);
                }
                (KeyCode::F(8), m) if m.contains(KeyModifiers::SHIFT) => self.step_problem(-1),
                (KeyCode::F(8), _) => self.step_problem(1),
                (KeyCode::F(3), m) if m.contains(KeyModifiers::SHIFT) => editor.step_match(-1),
                (KeyCode::F(3), _) => editor.step_match(1),
                (KeyCode::F(1), _) => self.overlay = Overlay::Help,
                // Back to the rendered document. Alt rather than Ctrl:
                // Ctrl+P is tui-textarea's cursor-up, and taking it away
                // would break moving around the buffer.
                (KeyCode::Char('p'), KeyModifiers::ALT)
                | (KeyCode::Char('P'), KeyModifiers::ALT) => {
                    self.toggle_preview();
                }
                // The find prompt, when it has the keyboard.
                _ if editor.find.typing.is_some() && editor.find_key(key) => {}
                // Every problem in the file, as a list. After the find
                // prompt, like the other Alt keys: while the prompt has
                // the keyboard, it gets first refusal.
                (KeyCode::Char('e'), KeyModifiers::ALT)
                | (KeyCode::Char('E'), KeyModifiers::ALT) => self.open_problems(),
                // And the one under the cursor, laid out to be read.
                (KeyCode::Char('x'), KeyModifiers::ALT)
                | (KeyCode::Char('X'), KeyModifiers::ALT) => self.explain_problem(),
                // Comment or uncomment the line, or the selection.
                (KeyCode::Char('c'), KeyModifiers::ALT)
                | (KeyCode::Char('C'), KeyModifiers::ALT) => {
                    if !editor.toggle_comment() {
                        self.err("Loupe does not know how this language writes a comment.");
                    }
                }
                // A newline that keeps the indent.
                (KeyCode::Enter, m) if m.is_empty() && editor.completion.is_none() => {
                    editor.newline_with_indent();
                    editor.dirty = true;
                    editor.discard_armed = false;
                    editor.touched();
                    self.buffer_changed();
                }
                // Rename the symbol under the cursor, everywhere.
                (KeyCode::Char('m'), KeyModifiers::ALT)
                | (KeyCode::Char('M'), KeyModifiers::ALT) => {
                    let word = editor.word_at_cursor();
                    if word.is_empty() {
                        self.err("Put the cursor on a name first.");
                    } else {
                        self.open_path_box(PathBoxKind::RenameSymbol { word });
                    }
                }
                // What the server offers here: quick fixes, refactors.
                (KeyCode::Char('.'), KeyModifiers::ALT) => {
                    self.spawn_editor_request(EditorRequest::CodeActions);
                }
                // The signature of the call the cursor is inside.
                (KeyCode::Char('s'), KeyModifiers::ALT)
                | (KeyCode::Char('S'), KeyModifiers::ALT) => {
                    self.spawn_editor_request(EditorRequest::SignatureHelp);
                }
                // Find and replace inside the buffer. Ctrl+F is the key
                // every other editor puts it on; Alt+F stays because it
                // is the one the rest of loupe's editor commands use.
                (KeyCode::Char('f'), KeyModifiers::ALT)
                | (KeyCode::Char('F'), KeyModifiers::ALT) => {
                    editor.open_find();
                    self.ok("Find in this file — Tab adds a replacement, Esc cancels.");
                }
                _ if is_find_key(key) => {
                    editor.open_find();
                    self.ok("Find in this file — Tab adds a replacement, Esc cancels.");
                }
                _ if is_find_all_key(key) => self.open_finder(FinderMode::Grep),
                // The editor takes bare letters as text, so `m` cannot
                // open the ☰ menu here — and the menu is where a reader
                // in a buffer reaches everything the app answers
                // everywhere else. These two open it: the context-menu
                // key a PC keyboard has, and the gesture for it that
                // every keyboard has.
                (KeyCode::Menu, _) | (KeyCode::Enter, KeyModifiers::ALT) => {
                    self.open_menu_from_key();
                }
                (KeyCode::Char('n'), KeyModifiers::ALT)
                | (KeyCode::Char('N'), KeyModifiers::ALT) => editor.step_match(1),
                (KeyCode::Char('b'), KeyModifiers::ALT)
                | (KeyCode::Char('B'), KeyModifiers::ALT) => editor.step_match(-1),
                // `gr` in the diff view. Alt rather than Ctrl for the same
                // reason as the preview above: the editor takes plain
                // characters as text, and the free Ctrl keys are spoken
                // for.
                (KeyCode::Char('r'), KeyModifiers::ALT)
                | (KeyCode::Char('R'), KeyModifiers::ALT) => {
                    self.spawn_editor_request(EditorRequest::References);
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
                    self.buffer_changed();
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
                (KeyCode::Esc, _) if editor.signature.is_some() => {
                    editor.signature = None;
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
                        self.buffer_changed();
                    }
                    // Narrow an open popup to what has been typed since,
                    // then decide whether this keystroke is worth asking
                    // the server about.
                    let open = self.editor.as_ref().is_some_and(|e| e.completion.is_some());
                    if open {
                        if let Some(e) = &mut self.editor {
                            e.update_completion();
                        }
                    }
                    if let Some(c) = typed {
                        self.maybe_suggest(c);
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
                KeyCode::Char('?') | KeyCode::F(1) => self.overlay = Overlay::Help,
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
                // The tab row is above this screen too, so a pinned plan
                // file is one key away from the list as well as from a
                // review.
                _ if self.pin_key_bare(key) => {}
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
                let alt = key.modifiers.contains(KeyModifiers::ALT);
                match key.code {
                    // --- the stack, one rung at a time. The two
                    // directions `gh stack up` and `gh stack down` name,
                    // under the arrows, so the habit carries over. Read
                    // before the plain arrows, which take any modifier.
                    KeyCode::Up if alt => self.step_stack(1),
                    KeyCode::Down if alt => self.step_stack(-1),
                    // --- motions (they move the cursor; the view follows)
                    KeyCode::Up | KeyCode::Char('k') => self.cursor_by(-1),
                    KeyCode::Down | KeyCode::Char('j') => self.cursor_by(1),
                    KeyCode::Char('d') if ctrl => self.cursor_by(page / 2),
                    KeyCode::Char('u') if ctrl => self.cursor_by(-page / 2),
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
                    //
                    // Ctrl+F is what everyone else's find is on, so it is
                    // what loupe's is on. vim's forward-page keeps
                    // PageDown and Ctrl+D; the search had only `/`.
                    KeyCode::Char('/') => self.start_find(),
                    _ if is_find_key(key) => self.start_find(),
                    _ if is_find_all_key(key) => self.open_finder(FinderMode::Grep),
                    // vim's "what is this?" key.
                    KeyCode::Char('K') => self.lsp_action(LspAction::Hover),
                    // The same function keys the editor uses, so one set
                    // of habits works on both sides of `e`.
                    KeyCode::F(12) if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        self.lsp_action(LspAction::References)
                    }
                    KeyCode::F(12) => self.lsp_action(LspAction::Definition),
                    KeyCode::F(10) => self.lsp_action(LspAction::References),
                    KeyCode::F(3) if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        self.goto_match(false)
                    }
                    KeyCode::F(3) => self.goto_match(true),
                    KeyCode::F(1) => self.overlay = Overlay::Help,
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
                    KeyCode::Char('Y') => self.yank_context(),
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
                    KeyCode::Esc => self.back_to_pr_list(),
                    KeyCode::Char('v') => self.toggle_view(),
                    KeyCode::Char('z') => self.toggle_fold(),
                    // The three layers of a branch's change: one, three,
                    // and the step in between. `S` is the stash, and the
                    // two are as unrelated as `b` and `B` already are.
                    KeyCode::Char('s') => self.cycle_layers(),
                    // Put changes back: the section at the cursor, or (with
                    // shift) the whole file. Both ask first.
                    KeyCode::Char('u') => self.ask_revert_section(self.diff_cursor),
                    KeyCode::Char('U') => self.ask_revert_file(self.file_cursor),
                    // Resolve the merge conflict at the cursor. `o` is
                    // free here, and it is the letter every merge tool
                    // already uses for "ours".
                    KeyCode::Char('o') => self.open_conflict_menu_from_key(),
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
                    // The review box at the foot of the file panel: the
                    // pull request as a whole, rather than one line of it.
                    KeyCode::Char('R') => self.focus_review(),
                    KeyCode::Char('r') => self.refresh_review(),
                    // Everything the app answers everywhere — quit, go,
                    // find, stage, stash, settings, and the tab row —
                    // lives in one place. See [`Self::global_key`].
                    _ => {
                        self.global_key(key);
                    }
                }
            }
        }
    }

    /// The keys that are about the app rather than about the pane in
    /// front of the reader: quit, go somewhere, find something, stage,
    /// stash, and the settings.
    ///
    /// Every mode answers these. They were spelled out once per mode
    /// before, and each mode answered a different subset — a reader in a
    /// document could not swap to the pull request, open the grep, or
    /// reach the Commits panel, for no reason anybody chose. Modes keep
    /// their own keys above this and fall through to it.
    ///
    /// True when the key was answered.
    fn global_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if is_find_all_key(key) {
            self.open_finder(FinderMode::Grep);
            return true;
        }
        if ctrl && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P')) {
            self.open_finder(FinderMode::Files);
            return true;
        }
        // Everything below is a bare key. A modifier on it means it
        // belongs to whoever bound it — Ctrl+B pages back in both the
        // diff and the document, and must not read as "back to the list".
        if ctrl || alt {
            return false;
        }
        match key.code {
            // --- leaving
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('b') => self.back_to_pr_list(),
            KeyCode::Char('`') => self.toggle_workspace(),
            // --- finding
            KeyCode::Char('#') => self.open_finder(FinderMode::Grep),
            KeyCode::Char('@') => self.open_finder(FinderMode::Symbols),
            // --- the file panel
            KeyCode::Char('F') => self.toggle_panel(),
            KeyCode::Char(']') => self.step_file(1),
            KeyCode::Char('[') => self.step_file(-1),
            KeyCode::Char('<') => self.resize_file_panel(-2),
            KeyCode::Char('>') => self.resize_file_panel(2),
            // --- the index
            KeyCode::Char('x') => self.toggle_file_mark(self.file_cursor),
            KeyCode::Char('X') => self.toggle_stage_all(),
            KeyCode::Char('S') => self.open_stash_menu_from_key(),
            // --- everything else
            KeyCode::Char('B') => self.toggle_blame(),
            KeyCode::Char('Y') => self.yank_context(),
            KeyCode::Char('m') => self.open_menu_from_key(),
            KeyCode::Char('t') => self.open_theme_picker(),
            KeyCode::Char('?') | KeyCode::F(1) => self.overlay = Overlay::Help,
            _ => return self.pin_key_bare(key),
        }
        true
    }

    fn back_to_pr_list(&mut self) {
        self.screen = Screen::PrList;
        self.diff = None;
        self.conflict = None;
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
        let (x, y) = (m.column, m.row);

        // Overlays.
        match &mut self.overlay {
            Overlay::Help | Overlay::CodeActions(_) | Overlay::Problem(_) => {
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
                        Some(ButtonId::CommentHold) => self.hold_comment(),
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
                        Some(ButtonId::AppearanceAuto) => self.pick_auto_appearance(),
                        Some(ButtonId::AppearanceToggle) => {
                            if let Overlay::ThemePicker(tp) = &mut self.overlay {
                                tp.toggle_appearance();
                                tp.pin_appearance();
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
                        Some(ButtonId::PathMenuRow(i)) => self.run_path_menu(i),
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
            Overlay::ReviewConfirm(_) => {
                if matches!(m.kind, MouseEventKind::Down(_)) {
                    match self.layout.button_at(x, y) {
                        Some(ButtonId::ReviewYes) => self.spawn_submit_review(),
                        Some(ButtonId::ReviewCancel) => {
                            self.overlay = Overlay::None;
                            self.ok("Not sent — your comments are still held here.");
                        }
                        _ => {}
                    }
                }
                return;
            }
            Overlay::OpenPath(_) => {
                if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
                    match self.layout.button_at(x, y) {
                        Some(ButtonId::OpenPathGo) => self.confirm_open_path(),
                        Some(ButtonId::OpenPathCancel) => {
                            self.overlay = Overlay::None;
                            self.ok("Closed — no file opened.");
                        }
                        _ => {}
                    }
                }
                return;
            }
            Overlay::VerdictMenu => {
                if matches!(m.kind, MouseEventKind::Down(_)) {
                    match self.layout.button_at(x, y) {
                        Some(ButtonId::VerdictRow(i)) => {
                            self.overlay = Overlay::None;
                            let all = Verdict::all();
                            self.set_verdict(all[i.min(all.len() - 1)]);
                        }
                        _ => {
                            self.overlay = Overlay::None;
                            self.review.picking = false;
                        }
                    }
                }
                return;
            }
            Overlay::ConflictMenu(_) => {
                if matches!(m.kind, MouseEventKind::Down(_)) {
                    match self.layout.button_at(x, y) {
                        Some(ButtonId::ConflictMenuRow(i)) => self.conflict_menu_run(i),
                        _ => {
                            self.overlay = Overlay::None;
                            self.ok("Left alone — nothing was resolved.");
                        }
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

        // The tab row sits above every screen and every mode, so its
        // clicks are answered before any of them get a look.
        if contains(self.layout.pin_row, x, y) {
            match (m.kind, self.layout.button_at(x, y)) {
                (MouseEventKind::Down(MouseButton::Left), Some(id)) => {
                    // A double click on the peek tab keeps it, the way
                    // it does in the file tree. Asked before
                    // the tab is activated, because activating it is
                    // what makes it the buffer on screen.
                    let twice = self.double_click(x, y);
                    // A press on a tab may turn out to be a drag. The
                    // path is what is remembered, not the index, because
                    // dragging is the thing that changes the indices.
                    if let ButtonId::BufferTab(i) = id {
                        let held = self.buffer_tabs().get(i).map(|t| t.path.clone());
                        self.tab_drag = held;
                    }
                    self.activate(id);
                    if twice && matches!(id, ButtonId::BufferTab(_)) {
                        self.keep_peek();
                    }
                }
                // Drag a tab along the row to put it where you want it.
                // The row is the reader's own order, and this is the only
                // thing that changes it.
                (MouseEventKind::Drag(MouseButton::Left), Some(ButtonId::BufferTab(to))) => {
                    if let Some(path) = self.tab_drag.clone() {
                        let from = self.buffer_tabs().iter().position(|t| t.path == path);
                        if let Some(from) = from {
                            self.move_buffer(from, to);
                        }
                    }
                }
                (MouseEventKind::Up(MouseButton::Left), _) => self.tab_drag = None,
                // The right button asks about the file the tab holds,
                // the same question the file panel answers.
                (MouseEventKind::Down(MouseButton::Right), Some(id)) => {
                    self.open_tab_menu(id, x, y)
                }
                // Middle-click closes a tab, as it does in a browser.
                (MouseEventKind::Down(MouseButton::Middle), Some(ButtonId::PinTab(i)))
                | (MouseEventKind::Down(MouseButton::Middle), Some(ButtonId::PinClose(i))) => {
                    self.close_pin(i);
                }
                (MouseEventKind::Down(MouseButton::Middle), Some(ButtonId::BufferTab(i)))
                | (MouseEventKind::Down(MouseButton::Middle), Some(ButtonId::BufferClose(i))) => {
                    self.close_buffer_at(i);
                }
                // The wheel walks the row, which is the only way to reach
                // a tab that has scrolled off a narrow window.
                (MouseEventKind::ScrollDown, _) | (MouseEventKind::ScrollRight, _) => {
                    self.step_pin(1)
                }
                (MouseEventKind::ScrollUp, _) | (MouseEventKind::ScrollLeft, _) => {
                    self.step_pin(-1)
                }
                _ => {}
            }
            return;
        }

        // Everything that belongs to the window rather than to whatever
        // is in the middle of it. Answered once, above the mode split, so
        // no mode can forget one of them.
        if self.chrome_mouse(m, x, y) {
            return;
        }

        // A file loading is modal for the *pane* — there is nothing there
        // to click yet — but not for the window. It used to be modal for
        // both, which is why the second click of a double click went
        // nowhere: the first click started the read, and the read
        // swallowed the second. A load takes tens of milliseconds and a
        // double click takes four hundred, so every one of them landed
        // mid-read.
        if self.job.is_some() {
            return;
        }

        // Preview mode: the document, and the wheel over it.
        if self.preview.is_some() {
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(pv) = &mut self.preview {
                        pv.on_click(x, y);
                    }
                }
                MouseEventKind::ScrollUp => {
                    if let Some(pv) = &mut self.preview {
                        pv.scroll_rows(-preview::WHEEL_ROWS);
                    }
                }
                MouseEventKind::ScrollDown => {
                    if let Some(pv) = &mut self.preview {
                        pv.scroll_rows(preview::WHEEL_ROWS);
                    }
                }
                _ => {}
            }
            return;
        }

        // Editor mode: the text surface itself.
        if self.editor.is_some() {
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    // One click places the cursor, two take the word,
                    // three take the line — what every other editor does,
                    // and the reason a double click is how you pick a
                    // name to ask about.
                    let run = self.click_run(x, y);
                    let word = match (run, &mut self.editor) {
                        (2, Some(ed)) => ed.on_double_click(x, y),
                        (3, Some(ed)) => {
                            ed.on_triple_click(x, y);
                            None
                        }
                        (_, Some(ed)) => {
                            ed.on_click(x, y);
                            None
                        }
                        (_, None) => None,
                    };
                    if let Some(word) = word {
                        self.ok(format!(
                            "{word} — F12 definition · F10 every use · right-click for more"
                        ));
                    }
                }
                // Over the text itself, the right button is the code
                // menu. Over the file panel it is the path menu, and the
                // chrome above has already answered that.
                MouseEventKind::Down(MouseButton::Right) => {
                    if self
                        .editor
                        .as_mut()
                        .is_some_and(|ed| ed.on_right_click(x, y))
                    {
                        self.open_editor_menu(x, y);
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
                    if let Some(ed) = &mut self.editor {
                        ed.scroll_lines(-3);
                    }
                }
                MouseEventKind::ScrollDown => {
                    if let Some(ed) = &mut self.editor {
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
            // The toolbar and the ☰ menu are answered by `chrome_mouse`
            // before this runs, on this screen as on every other.
            MouseEventKind::Down(MouseButton::Left) => {
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

    /// The parts of the window that belong to the window rather than to
    /// whatever is in the middle of it: the panel divider, the toolbar,
    /// the ☰ menu, the review badge, and the file panel.
    ///
    /// Every mode used to answer these itself, and every mode answered a
    /// different subset — right-clicking the badge to copy the PR link
    /// did nothing at all while a document or the editor was open. They
    /// are answered once here instead, above the mode split, so a mode
    /// cannot forget one and a new mode gets them for free.
    ///
    /// True when the click was answered and the mode should not see it.
    fn chrome_mouse(&mut self, m: MouseEvent, x: u16, y: u16) -> bool {
        if self.divider_mouse(m, x, y) {
            return true;
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                match self.layout.button_at(x, y) {
                    Some(ButtonId::Menu) => {
                        self.open_menu(x, y);
                        return true;
                    }
                    Some(ButtonId::PanelMenu) => {
                        self.open_section_menu(x, y);
                        return true;
                    }
                    Some(id) if self.activate(id) => return true,
                    _ => {}
                }
                // `PR #44 · 3/5` says there is a stack; clicking it is
                // the shortest way to ask what the other four are.
                if self.stack.is_some() && contains(self.layout.badge, x, y) {
                    self.set_panel(PanelMode::Stack);
                    return true;
                }
                // The file panel keeps working with the editor or a
                // document open. Switching files parks the buffer rather
                // than closing it, so there is nothing to save first and
                // nothing to lose by clicking away.
                if contains(self.layout.file_list, x, y) {
                    self.file_list_click(x, y);
                    return true;
                }
                false
            }
            MouseEventKind::Down(MouseButton::Right) => {
                // The badge carries the PR link wherever the reader is.
                if contains(self.layout.badge, x, y) {
                    self.copy_pr_link();
                    return true;
                }
                if contains(self.layout.file_list, x, y) {
                    self.open_path_menu(x, y);
                    return true;
                }
                false
            }
            MouseEventKind::ScrollUp if contains(self.layout.file_list, x, y) => {
                self.file_scroll = self.file_scroll.saturating_sub(3);
                true
            }
            MouseEventKind::ScrollDown if contains(self.layout.file_list, x, y) => {
                let h = self.layout.file_list.height as usize;
                let max = self.entries.len().saturating_sub(h.max(1));
                self.file_scroll = (self.file_scroll + 3).min(max);
                true
            }
            _ => false,
        }
    }

    fn mouse_review(&mut self, m: MouseEvent, x: u16, y: u16) {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if contains(self.layout.review_box, x, y) {
                    self.focus_review();
                    return;
                }
                // A click anywhere else takes the keyboard back from the
                // review box, so typing goes where the eye is.
                if self.review.focused {
                    self.blur_review();
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
            // The badge and the file panel are answered above. Over the
            // blame pane the right button asks about the commit; anywhere
            // else it drops the diff selection.
            MouseEventKind::Down(MouseButton::Right) => {
                if contains(self.layout.blame, x, y) {
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
                self.scroll_diff(-3);
                self.clamp_cursor_to_view();
            }
            MouseEventKind::ScrollDown => {
                self.scroll_diff(3);
                self.clamp_cursor_to_view();
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

    /// How many presses in a row this one makes on the same spot: 1 for a
    /// fresh click, 2 for a double, 3 for a triple. A fourth press starts
    /// the count again, so clicking on and on cycles rather than sticking
    /// on "triple" forever.
    ///
    /// Call it once per press. It records this press as the one the next
    /// press is measured against, so asking twice makes every second
    /// click a first one again.
    fn click_run(&mut self, x: u16, y: u16) -> u8 {
        let now = Instant::now();
        let n = match self.last_click {
            Some((t, lx, ly, n))
                if now.duration_since(t).as_millis() < 400 && lx.abs_diff(x) <= 1 && ly == y =>
            {
                if n >= 3 {
                    1
                } else {
                    n + 1
                }
            }
            _ => 1,
        };
        self.last_click = Some((now, x, y, n));
        n
    }

    /// True when this press lands on the same spot as the last one, quickly.
    fn double_click(&mut self, x: u16, y: u16) -> bool {
        self.click_run(x, y) >= 2
    }

    /// A click inside the file panel: checkbox toggles viewed, a directory
    /// row collapses/expands, a file row opens the file.
    fn file_list_click(&mut self, x: u16, y: u16) {
        let fl = self.layout.file_list;
        let vis = self.file_scroll + (y - fl.y) as usize;
        let Some(entry) = self.entries.get(vis).cloned() else {
            return;
        };
        // Asked before anything is opened, and asked once: the helper
        // remembers this click as the one the next click is measured
        // against, so calling it twice would make every second click a
        // single one again.
        let double = self.double_click(x, y);
        match entry {
            FileEntry::Dir { path, .. } => {
                let set = self.collapsed();
                let opened = set.remove(&path);
                if !opened {
                    set.insert(path.clone());
                }
                // A directory git would not walk into has nothing under it
                // until somebody asks, and this is the asking.
                if opened && self.panel == PanelMode::Files && self.file_tree.is_stub(&path) {
                    self.spawn_read_dir(path);
                }
                self.rebuild_entries();
            }
            FileEntry::File { src, depth } => {
                // A row with no diff behind it — one the change does not
                // touch, or any row at all in the Files panel — has
                // nothing to stage and nothing to put back, so none of
                // the targets below mean anything. It still opens.
                let Some(idx) = self.diff_row_idx(src) else {
                    if let Some(path) = self.row_path(src).map(str::to_string) {
                        self.open_from_tree(path, double);
                    }
                    return;
                };
                // The icon plus its trailing space: a 4-column target.
                let cb_start = fl.x + depth;
                // ↺ at the far right of the row, where the counts end.
                let revert_from = fl.x + fl.width.saturating_sub(REVERT_W);
                let conflicted = self.file_conflicted(idx);
                if x >= cb_start && x < cb_start + 4 {
                    // A conflicted file cannot be staged as it stands, so
                    // its icon offers the resolve menu instead.
                    if conflicted {
                        self.open_conflict_for_file(idx, x, y);
                    } else {
                        self.toggle_file_mark(idx);
                    }
                } else if self.can_revert() && !conflicted && x >= revert_from {
                    self.ask_revert_file(idx);
                } else if idx != self.file_cursor || self.diff.is_none() {
                    // A double click keeps the tab, the way it does in
                    // the tree: one click walks the change, two say "I am
                    // coming back to this one".
                    self.keep_tab = double;
                    self.spawn_load_file(idx);
                } else if double {
                    self.keep_peek();
                }
            }
            // The heading is a label, not a target.
            FileEntry::ConflictHeading { .. } => {}
            FileEntry::CommitRow { idx, .. } => self.toggle_commit(idx),
            FileEntry::CommitFileRow { commit, file } => self.open_commit_file(commit, file),
            FileEntry::StackRow { idx } => self.open_stack_pr(idx),
            // A note is a sentence, and the trunk is not a pull request.
            FileEntry::CommitNote(_) | FileEntry::StackNote(_) | FileEntry::StackTrunk => {}
            FileEntry::StageHeading { section, count, .. } => {
                // The right-hand end moves the whole section across; the
                // rest of the row folds it away.
                let fl = self.layout.file_list;
                let action_from = fl.x + fl.width.saturating_sub(SECTION_ACTION_W);
                if count > 0 && x >= action_from {
                    match section {
                        StageSection::Staged => self.unstage_all(),
                        StageSection::Unstaged => self.stage_all(),
                    }
                } else {
                    self.toggle_section(section);
                }
            }
        }
    }

    /// Fold one staging section away, or bring it back.
    pub fn toggle_section(&mut self, section: StageSection) {
        if !self.collapsed_sections.remove(&section) {
            self.collapsed_sections.insert(section);
        }
        self.rebuild_entries();
    }

    /// A click on a file row that has no diff behind it: open the file.
    ///
    /// One click opens it as the tab the next click replaces, so walking
    /// a tree of files leaves one tab behind rather than forty. A double
    /// click keeps it. Neither asks the reader to save first, and neither
    /// closes what is already open: an unsaved buffer is parked, keeps
    /// its tab, and keeps the dot that says it is unsaved.
    fn open_from_tree(&mut self, path: String, double: bool) {
        // Already in front of the reader, as source or as a document. A
        // second click on it is the double click that keeps it; a first
        // click has nothing to do.
        if self.buffer_showing(&path) {
            if double {
                self.keep_peek();
            }
            return;
        }
        // Pinned already: the reader chose that tab, and it is the tab
        // this file has. A peek beside it would name one file twice, and
        // a double click would leave two tabs on the same file with no
        // way to tell which one an edit went into. The pin's own door
        // decides what opening it means.
        if let Some(i) = self.pinned_at(&path) {
            self.open_pin(i);
            return;
        }
        // Open already: no read, so the swap happens inside this one
        // event and the reader never sees a frame without it. The cursor,
        // the scroll and any unsaved edits all come back with it.
        if self.switch_buffer(&path) {
            self.preview = None;
            self.drop_peek(&path);
            if double {
                self.keep_peek();
            } else {
                self.ok(format!("{path} — already open."));
            }
            self.spawn_blame_external(path, false);
            return;
        }
        // Not open: it has to be read. Nothing on screen changes until it
        // lands — see `spawn_open_tree_file`.
        self.spawn_open_tree_file(path, double);
    }

    /// The resolve menu for a file in the panel. When it is not the open
    /// one, load it first — the menu describes the conflict on screen, and
    /// resolving from the wrong file would be a trap.
    fn open_conflict_for_file(&mut self, idx: usize, x: u16, y: u16) {
        if idx != self.file_cursor {
            self.spawn_load_file(idx);
            self.ok("Opening it — press o to resolve a conflict once it is on screen.");
            return;
        }
        self.open_conflict_menu(self.diff_cursor, x, y);
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
            FileEntry::File { src, .. } => match self.row_path(*src) {
                Some(path) => (path.to_string(), false),
                None => return,
            },
            // A heading names no path, so there is nothing to copy.
            FileEntry::ConflictHeading { .. }
            | FileEntry::StageHeading { .. }
            | FileEntry::CommitRow { .. }
            | FileEntry::CommitNote(_)
            | FileEntry::StackRow { .. }
            | FileEntry::StackTrunk
            | FileEntry::StackNote(_) => return,
            FileEntry::CommitFileRow { commit, file } => {
                match self.files_of_commit(*commit).and_then(|fs| fs.get(*file)) {
                    Some(f) => (f.path.clone(), false),
                    None => return,
                }
            }
        };
        let mut items = vec![PathMenuItem {
            key: 'r',
            label: "Copy relative path",
            action: PathAction::Copy(path.clone()),
        }];
        // A path git cannot express as a plain relative path has no
        // absolute form worth handing out, so that line is left off.
        if let Some(abs) = self.abs_path(&path) {
            items.push(PathMenuItem {
                key: 'f',
                label: "Copy full path",
                action: PathAction::Copy(abs.to_string_lossy().into_owned()),
            });
        }
        // Making and unmaking files belongs to the panel that shows the
        // repository. In the change under review the same row means "part
        // of this diff", and offering to delete it there reads as an offer
        // to drop it from the change.
        if self.panel == PanelMode::Files {
            items.push(PathMenuItem {
                key: 'n',
                label: "New file here…",
                action: PathAction::NewFile,
            });
            if !is_dir {
                items.push(PathMenuItem {
                    key: 'm',
                    label: "Rename…",
                    action: PathAction::Rename,
                });
                items.push(PathMenuItem {
                    key: 'd',
                    label: "Delete…",
                    action: PathAction::Delete,
                });
            }
        }
        self.overlay = Overlay::PathMenu(Box::new(PathMenu {
            path,
            is_dir,
            items,
            sel: 0,
            anchor: (x, y),
        }));
    }

    /// The name a tab shows, and the absolute path behind it.
    ///
    /// A pin carries both already, because a pinned file can live
    /// anywhere on the machine. A buffer knows where it was read from. A
    /// tab holding only a diff is a file of the change, and the change is
    /// in the repository, so its absolute form is built from the root.
    fn tab_paths(&self, id: ButtonId) -> Option<(String, Option<PathBuf>)> {
        match id {
            ButtonId::PinTab(i) | ButtonId::PinClose(i) => {
                let pin = self.pins.items.get(i)?;
                Some((pin.path.clone(), Some(pin.abs_path.clone())))
            }
            ButtonId::BufferTab(i) | ButtonId::BufferClose(i) => {
                let path = self.buffer_tabs().get(i)?.path.clone();
                let abs = self
                    .editor_for(&path)
                    .map(|e| e.abs_path.clone())
                    .or_else(|| self.abs_path(&path));
                Some((path, abs))
            }
            _ => None,
        }
    }

    /// The right-click menu on a tab: the ways to name the file it holds.
    ///
    /// The tab is a name, and a name is not something you can hand to a
    /// coding agent. For a file pinned from outside the repository this
    /// is the only place its path comes from at all — the alternative was
    /// going and finding it in the filesystem again.
    fn open_tab_menu(&mut self, id: ButtonId, x: u16, y: u16) {
        let Some((shown, abs)) = self.tab_paths(id) else {
            return;
        };
        // A file outside the repository is already shown by its absolute
        // path, and "relative" would have nothing to be relative to.
        let outside = Path::new(&shown).is_absolute();
        let mut items = Vec::new();
        if !outside {
            items.push(PathMenuItem {
                key: 'r',
                label: "Copy relative path",
                action: PathAction::Copy(shown.clone()),
            });
        }
        if let Some(abs) = abs {
            items.push(PathMenuItem {
                key: 'f',
                label: "Copy full path",
                action: PathAction::Copy(abs.to_string_lossy().into_owned()),
            });
        }
        if items.is_empty() {
            return;
        }
        self.overlay = Overlay::PathMenu(Box::new(PathMenu {
            path: shown,
            is_dir: false,
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
    fn run_path_menu(&mut self, i: usize) {
        let Overlay::PathMenu(menu) = &self.overlay else {
            return;
        };
        let Some(item) = menu.items.get(i) else {
            return;
        };
        let action = item.action.clone();
        let path = menu.path.clone();
        let is_dir = menu.is_dir;
        match action {
            PathAction::Copy(text) => {
                self.overlay = Overlay::None;
                match clipboard::copy(&text) {
                    Ok(via) => self.ok(format!("⧉ Copied {text} to the clipboard via {via}.")),
                    Err(e) => self.err(format!("Couldn't copy: {e:#}")),
                }
            }
            PathAction::NewFile => {
                self.overlay = Overlay::None;
                // Beside the file that was clicked, or inside the folder.
                let dir = if is_dir {
                    path
                } else {
                    path.rsplit_once('/')
                        .map(|(d, _)| d.to_string())
                        .unwrap_or_default()
                };
                self.open_path_box(PathBoxKind::NewFile { dir });
            }
            PathAction::Rename => {
                self.overlay = Overlay::None;
                self.open_path_box(PathBoxKind::Rename { from: path });
            }
            PathAction::Delete => {
                // Only asks. Deleting a file is the one thing this menu
                // does that cannot be undone from inside loupe, so it gets
                // the treatment the revert already has.
                if let Overlay::PathMenu(menu) = &mut self.overlay {
                    menu.items = vec![PathMenuItem {
                        key: 'y',
                        label: "Yes, delete it",
                        action: PathAction::DeleteConfirmed,
                    }];
                    menu.sel = 0;
                }
                self.err(format!("Delete {path}? This cannot be undone from here."));
            }
            PathAction::DeleteConfirmed => {
                self.overlay = Overlay::None;
                self.delete_file(&path);
            }
        }
    }

    // ------------------------------------------------------------- ☰ menu

    /// Build the ☰ menu for whatever is on screen right now.
    ///
    /// The toolbar shows the two or three things that fit; this is the rest
    /// of the tool. It is rebuilt on every open rather than cached, so a
    /// line's enabled state and its on/off mark always describe the state
    /// the reader is actually looking at.
    /// The ☰ lines for the tab row. The same set on every screen, because
    /// the row itself sits above every screen.
    fn pin_menu_rows(&self) -> Vec<MenuRow> {
        // `hint` is a static string, so the tab numbers come from a table
        // rather than from `format!`.
        const DIGITS: [&str; 9] = ["1", "2", "3", "4", "5", "6", "7", "8", "9"];
        let mut rows = vec![MenuRow::Heading("PINNED FILES")];
        rows.push(MenuRow::Item(MenuItem {
            label: if self.current_is_pinned() {
                "📌 Unpin this file".into()
            } else {
                "📌 Pin this file".into()
            },
            hint: "=",
            id: ButtonId::PinToggle,
            enabled: self.current_target().is_some(),
            checked: None,
        }));
        rows.push(MenuRow::Item(MenuItem {
            label: "📂 Open a file by path…".into(),
            hint: "Ctrl+O",
            id: ButtonId::PinOpenPath,
            enabled: true,
            checked: None,
        }));
        let open = self.active_pin();
        for (i, label) in self.pins.labels().into_iter().enumerate() {
            rows.push(MenuRow::Item(MenuItem {
                label: format!(
                    "{} {label}",
                    if self.pins.items[i].outside {
                        "↗"
                    } else {
                        " "
                    }
                ),
                hint: DIGITS.get(i).copied().unwrap_or(""),
                id: ButtonId::PinTab(i),
                enabled: true,
                checked: Some(open == Some(i)),
            }));
        }
        rows
    }

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
            rows.extend(self.pin_menu_rows());
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
            rows.extend(self.pin_menu_rows());
            rows.push(MenuRow::Heading("SETTINGS"));
            rows.push(item("🎨 Theme".into(), "t", ButtonId::Theme));
            rows.push(item("?  Help".into(), "?", ButtonId::Help));
            rows.push(item("✕  Quit".into(), "q", ButtonId::Quit));
            return rows;
        }

        if self.editor.is_some() {
            let read_only = self.editor.as_ref().is_some_and(|e| e.read_only);
            let path = self
                .editor
                .as_ref()
                .map(|e| e.path.clone())
                .unwrap_or_default();
            let has_server = lsp::Lsp::supports(&path).is_some();

            rows.push(MenuRow::Heading("EDITOR"));
            rows.push(item("💾 Save".into(), "Ctrl+S", ButtonId::EditorSave));
            rows.push(item("⇥  Format".into(), "Ctrl+T", ButtonId::EditorFormat));
            rows.push(item(
                "🔍 Find and replace in this file".into(),
                "Ctrl+F",
                ButtonId::EditorFind,
            ));
            if !read_only {
                rows.push(item(
                    "💬 Comment or uncomment".into(),
                    "Alt+C",
                    ButtonId::EditorComment,
                ));
            }
            if markdown::is_markdown(self.editor.as_ref().map(|e| e.path.as_str()).unwrap_or("")) {
                rows.push(item(
                    "📖 Preview the markdown".into(),
                    "Alt+P",
                    ButtonId::PreviewToggle,
                ));
            }
            // The language-server lines are shown for a language loupe can
            // drive, and greyed rather than hidden when the server itself
            // is missing — "install gopls" is a more useful answer than a
            // menu that quietly has fewer lines on Go files.
            if has_server {
                rows.push(MenuRow::Heading("CODE"));
                rows.push(item(
                    "⇢  Go to the definition".into(),
                    "F12",
                    ButtonId::EditorDefinition,
                ));
                rows.push(item(
                    "≡  Find every use".into(),
                    "F10",
                    ButtonId::EditorReferences,
                ));
                rows.push(item(
                    "ℹ  What is this?".into(),
                    "Ctrl+G",
                    ButtonId::EditorHover,
                ));
                rows.push(item(
                    "ƒ  Signature of this call".into(),
                    "Alt+S",
                    ButtonId::EditorSignature,
                ));
                if !read_only {
                    rows.push(item(
                        "✎  Rename it everywhere".into(),
                        "F2",
                        ButtonId::EditorRename,
                    ));
                    rows.push(item(
                        "🔧 Fixes and refactors".into(),
                        "Alt+.",
                        ButtonId::EditorActions,
                    ));
                }
            }

            // Only when there is something wrong: three lines about
            // problems on a clean file is three lines that do nothing.
            let problems = self
                .editor
                .as_ref()
                .map(|e| e.problems().len())
                .unwrap_or(0);
            if problems > 0 {
                rows.push(MenuRow::Heading("PROBLEMS"));
                rows.push(item(
                    format!(
                        "✗  {problems} {} in this file",
                        if problems == 1 { "problem" } else { "problems" }
                    ),
                    "Alt+E",
                    ButtonId::EditorProblems,
                ));
                rows.push(item(
                    "✎  Explain the one here".into(),
                    "Alt+X",
                    ButtonId::EditorExplain,
                ));
                rows.push(item(
                    "▾  Next problem".into(),
                    "F8",
                    ButtonId::EditorNextProblem,
                ));
                rows.push(item(
                    "▴  Previous problem".into(),
                    "Shift+F8",
                    ButtonId::EditorPrevProblem,
                ));
            }

            // Only worth a line when there is somewhere else to go.
            if self.buffers().count() > 1 {
                rows.push(MenuRow::Heading("OPEN FILES"));
                rows.push(item("→  Next file".into(), "Alt+]", ButtonId::BufferNext));
                rows.push(item(
                    "←  Previous file".into(),
                    "Alt+[",
                    ButtonId::BufferPrev,
                ));
            }
            rows.push(item(
                "✕  Close the editor".into(),
                "Esc",
                ButtonId::EditorClose,
            ));
            rows.extend(self.pin_menu_rows());
            // The editor takes bare letters as text, so the keys that go
            // somewhere cannot be pressed while it is open. The menu is
            // how a reader in a buffer reaches them.
            rows.push(MenuRow::Heading("FIND"));
            rows.push(item("🔍 Find in every file".into(), "", ButtonId::FindGrep));
            rows.push(item("Go to a file".into(), "Ctrl+P", ButtonId::Find));
            if self.local {
                rows.push(MenuRow::Heading("STAGE"));
                rows.push(item("✚  Stage this file".into(), "", ButtonId::StageFile));
                rows.push(item("📦 Stash…".into(), "", ButtonId::StashMenu));
            }
            rows.push(MenuRow::Heading("GO"));
            rows.push(item(
                if self.local {
                    "⇄  Swap to the pull request".into()
                } else {
                    "⇄  Swap to local changes".into()
                },
                "",
                ButtonId::SwapView,
            ));
            rows.push(item("←  Pull request list".into(), "", ButtonId::BackToPrs));
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
        // Greyed rather than hidden when the branch has nothing pushed:
        // "there is nothing under your changes yet" is a more useful
        // answer than a menu that quietly has one fewer line.
        rows.push(MenuRow::Item(MenuItem {
            label: format!("▤  Layers — {}", self.layers.label()),
            hint: "s",
            id: ButtonId::LayersCycle,
            enabled: self.layers_possible(),
            checked: Some(self.layers.on()),
        }));
        rows.push(switch(
            "🌲 Tree file panel".into(),
            "",
            ButtonId::TreeToggle,
            self.tree_view,
        ));
        // The same list the panel's own ▾ opens, so there is one place
        // that decides what the sections are — and so this menu can get
        // back to the change, which it could not before.
        rows.extend(self.section_items());
        rows.push(switch(
            "👤 Blame column".into(),
            "B",
            ButtonId::BlameToggle,
            self.blame_on,
        ));

        rows.push(MenuRow::Heading("FIND"));
        rows.push(item(
            "🔍 Find in this file".into(),
            "Ctrl+F",
            ButtonId::FindInDiff,
        ));
        rows.push(item(
            "🔍 Find in every file".into(),
            "Ctrl+Shift+F",
            ButtonId::FindGrep,
        ));
        rows.push(item("Go to a file".into(), "Ctrl+P", ButtonId::Find));
        rows.push(item(
            "Symbols in this file".into(),
            "@",
            ButtonId::FindSymbols,
        ));

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
        // Always offered, unlike the copy above: an agent is told where the
        // reader is even when the reader is pointing at nothing in
        // particular, and that is often the whole answer it needed.
        rows.push(item(
            "🤖  Copy the context for your agent".into(),
            "Y",
            ButtonId::CopyContext,
        ));
        rows.extend(self.pin_menu_rows());
        let revert = self.can_revert();
        // Conflicts first: they block the commit, so they outrank
        // everything else the menu offers.
        if self.file_conflicted(self.file_cursor) {
            rows.push(MenuRow::Heading("MERGE CONFLICT"));
            rows.push(item(
                "⚑  Resolve the one at the cursor".into(),
                "o",
                ButtonId::ResolveConflict,
            ));
            rows.push(item(
                "⚑  Resolve this whole file".into(),
                "",
                ButtonId::ResolveFile,
            ));
        }

        // Staging is the local reviewer's whole job, so it gets its own
        // group rather than a line lost among the diff commands.
        if self.local {
            let unstaged = self.section_count(StageSection::Unstaged);
            let staged = self.section_count(StageSection::Staged);
            rows.push(MenuRow::Heading("STAGE"));
            rows.push(maybe(
                "✚  Stage this file".into(),
                "x",
                ButtonId::StageFile,
                !self.files.is_empty(),
            ));
            rows.push(maybe(
                format!("✚  Stage everything ({unstaged})"),
                "X",
                ButtonId::StageAll,
                unstaged > 0,
            ));
            rows.push(maybe(
                format!("↩  Unstage everything ({staged})"),
                "",
                ButtonId::UnstageAll,
                staged > 0,
            ));
            rows.push(item("📦 Stash…".into(), "S", ButtonId::StashMenu));
        }

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

        // The pull request as a whole, rather than one line of it. Local
        // review has no pull request to say anything about.
        if self.review_box_on() {
            let held = self.pending.len();
            rows.push(MenuRow::Heading("REVIEW"));
            rows.push(item(
                match held {
                    0 => "✎  Write a review".into(),
                    1 => "✎  Write a review (1 comment held)".into(),
                    n => format!("✎  Write a review ({n} comments held)"),
                },
                "R",
                ButtonId::ReviewBody,
            ));
            for v in Verdict::all() {
                rows.push(switch(
                    format!("{}  Send as: {}", v.icon(), v.label()),
                    "",
                    ButtonId::SetVerdict(v),
                    v == self.review.verdict,
                ));
            }
            rows.push(maybe(
                "▲  Submit it now".into(),
                "",
                ButtonId::ReviewSubmit,
                held > 0 || !self.review.is_empty(),
            ));
            rows.push(maybe(
                "✕  Discard the held comments".into(),
                "",
                ButtonId::ReviewDiscard,
                held > 0,
            ));
        }

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
    /// The lines of the editor's right-click menu, for the word the click
    /// landed on.
    ///
    /// Every line is a [`ButtonId`] that already exists, so the menu can
    /// never do something the key for it does not. A line that needs a
    /// symbol and has none is drawn dim rather than dropped: a menu that
    /// changes length depending on where you clicked is a menu you have
    /// to read every time.
    fn editor_menu_rows(&self, word: &str) -> Vec<MenuRow> {
        let item = |label: String, hint: &'static str, id: ButtonId, enabled: bool| {
            MenuRow::Item(MenuItem {
                label,
                hint,
                id,
                enabled,
                checked: None,
            })
        };
        let read_only = self.editor.as_ref().is_some_and(|e| e.read_only);
        let path = self
            .editor
            .as_ref()
            .map(|e| e.path.clone())
            .unwrap_or_default();
        let has_server = lsp::Lsp::supports(&path).is_some();
        let on_word = !word.is_empty();
        let problems = self
            .editor
            .as_ref()
            .map(|e| e.problems().len())
            .unwrap_or(0);

        let mut rows = Vec::new();
        if has_server {
            rows.push(MenuRow::Heading("CODE"));
            rows.push(item(
                "⇢  Go to the definition".into(),
                "F12",
                ButtonId::EditorDefinition,
                on_word,
            ));
            rows.push(item(
                "≡  Find every use".into(),
                "F10",
                ButtonId::EditorReferences,
                on_word,
            ));
            rows.push(item(
                "ℹ  What is this?".into(),
                "Ctrl+G",
                ButtonId::EditorHover,
                on_word,
            ));
            rows.push(item(
                "ƒ  Signature of this call".into(),
                "Alt+S",
                ButtonId::EditorSignature,
                true,
            ));
            if !read_only {
                rows.push(item(
                    "✎  Rename it everywhere".into(),
                    "F2",
                    ButtonId::EditorRename,
                    on_word,
                ));
                rows.push(item(
                    "🔧 Fixes and refactors".into(),
                    "Alt+.",
                    ButtonId::EditorActions,
                    true,
                ));
            }
        }
        if problems > 0 {
            rows.push(MenuRow::Heading("PROBLEMS"));
            rows.push(item(
                format!(
                    "✗  {problems} {} in this file",
                    if problems == 1 { "problem" } else { "problems" }
                ),
                "Alt+E",
                ButtonId::EditorProblems,
                true,
            ));
            rows.push(item(
                "✎  Explain the one here".into(),
                "Alt+X",
                ButtonId::EditorExplain,
                true,
            ));
            rows.push(item(
                "▾  Next problem".into(),
                "F8",
                ButtonId::EditorNextProblem,
                true,
            ));
            rows.push(item(
                "▴  Previous problem".into(),
                "Shift+F8",
                ButtonId::EditorPrevProblem,
                true,
            ));
        }
        rows.push(MenuRow::Heading("EDIT"));
        rows.push(item("⧉  Copy".into(), "Ctrl+C", ButtonId::EditorCopy, true));
        if !read_only {
            rows.push(item(
                "💬 Comment or uncomment".into(),
                "Alt+C",
                ButtonId::EditorComment,
                true,
            ));
            rows.push(item(
                "⇥  Format the file".into(),
                "Ctrl+T",
                ButtonId::EditorFormat,
                true,
            ));
        }
        rows.push(item(
            "🔍 Find and replace in this file".into(),
            "Ctrl+F",
            ButtonId::EditorFind,
            true,
        ));
        rows
    }

    /// Open the editor's right-click menu at the pointer.
    ///
    /// The click has already taken the word under it (see
    /// [`Editor::on_right_click`]), so the title names what the menu is
    /// about and every question below it asks about the same symbol.
    pub fn open_editor_menu(&mut self, x: u16, y: u16) {
        let word = self
            .editor
            .as_ref()
            .map(|e| e.word_at_cursor())
            .unwrap_or_default();
        let rows = self.editor_menu_rows(&word);
        let mut menu = Menu {
            rows,
            sel: 0,
            scroll: 0,
            anchor: (x, y),
            title: if word.is_empty() {
                " ⌁ Editor ".to_string()
            } else {
                format!(" ⌁ {word} ")
            },
            flip: true,
        };
        menu.sel = menu.first_selectable();
        self.overlay = Overlay::Menu(Box::new(menu));
    }

    // ------------------------------------------------------------- stash

    /// The stash menu: three ways to make one, then every stash there is.
    ///
    /// The list is read here rather than kept fresh in the background.
    /// `git stash list` is a reflog read of a local ref — cheap enough to
    /// pay for on the click that asks for it, and a stash nobody is
    /// looking at costs nothing to not know about.
    pub fn open_stash_menu(&mut self, x: u16, y: u16) {
        if !self.local {
            self.err("Stashing is for local changes — press ` to swap to them.");
            return;
        }
        self.stashes = gitops::stash_list(&self.repo_root).unwrap_or_default();
        let staged = self.section_count(StageSection::Staged);
        let dirty = !self.files.is_empty();
        let item = |label: String, hint: &'static str, id: ButtonId, enabled: bool| {
            MenuRow::Item(MenuItem {
                label,
                hint,
                id,
                enabled,
                checked: None,
            })
        };

        let mut rows = vec![MenuRow::Heading("PUT WORK ASIDE")];
        rows.push(item(
            "📦 Stash the tracked changes".into(),
            "",
            ButtonId::StashPush(StashScope::Tracked),
            dirty,
        ));
        rows.push(item(
            "📦 Stash everything, untracked files too".into(),
            "",
            ButtonId::StashPush(StashScope::WithUntracked),
            dirty,
        ));
        rows.push(item(
            format!("📦 Stash the staged files only ({staged})"),
            "",
            ButtonId::StashPush(StashScope::StagedOnly),
            staged > 0,
        ));

        if self.stashes.is_empty() {
            rows.push(MenuRow::Heading("NO STASHES YET"));
        } else {
            rows.push(MenuRow::Heading("STASHES"));
            for st in &self.stashes {
                rows.push(item(
                    format!("{}  ·  {}", st.subject, st.when),
                    "",
                    ButtonId::StashPick(st.index),
                    true,
                ));
            }
        }
        self.open_menu_at(rows, " 📦 Stash ".into(), x, y);
    }

    /// What can be done with one stash. A second menu rather than three
    /// more lines per stash in the first: the list is as long as the
    /// reader's history, and every row would carry the same three.
    pub fn open_stash_item_menu(&mut self, index: usize, x: u16, y: u16) {
        let Some(st) = self.stashes.iter().find(|s| s.index == index) else {
            self.err("That stash is gone — reopen the menu for the current list.");
            return;
        };
        let title = format!(" 📦 {} ", st.subject);
        let item = |label: &str, id: ButtonId| {
            MenuRow::Item(MenuItem {
                label: label.to_string(),
                hint: "",
                id,
                enabled: true,
                checked: None,
            })
        };
        let rows = vec![
            MenuRow::Heading("PUT IT BACK"),
            item(
                "⤓  Apply it and keep the stash",
                ButtonId::StashApply(index),
            ),
            item("⤓  Apply it and drop the stash", ButtonId::StashPop(index)),
            MenuRow::Heading("OR"),
            item(
                "✕  Drop it — this cannot be undone",
                ButtonId::StashDrop(index),
            ),
        ];
        self.open_menu_at(rows, title, x, y);
    }

    /// A menu opened at the pointer, with rows somebody else built.
    fn open_menu_at(&mut self, rows: Vec<MenuRow>, title: String, x: u16, y: u16) {
        let mut menu = Menu {
            rows,
            sel: 0,
            scroll: 0,
            anchor: (x, y),
            title,
            flip: true,
        };
        menu.sel = menu.first_selectable();
        self.overlay = Overlay::Menu(Box::new(menu));
    }

    /// Ask what to call the stash, then make it.
    ///
    /// Every scope goes through the box, so "stash with a name" is not a
    /// fourth way to stash — it is the same three, named. Enter on an
    /// empty box is a real answer: git writes `WIP on <branch>` itself.
    fn ask_stash_name(&mut self, scope: StashScope) {
        self.open_path_box(PathBoxKind::StashName { scope });
    }

    /// `git stash push`, then a fresh scan of what is left.
    fn spawn_stash_push(&mut self, scope: StashScope, name: Option<String>) {
        let root = self.repo_root.clone();
        let called = name.clone().unwrap_or_default();
        self.spawn("Stashing your changes", false, false, move || {
            gitops::stash_push(&root, name.as_deref(), scope)?;
            let note = if called.is_empty() {
                format!(
                    "📦 Stashed {} — `git stash pop` puts them back.",
                    scope.label()
                )
            } else {
                format!("📦 Stashed {} as “{called}”.", scope.label())
            };
            Ok(Outcome::Stashed {
                note,
                local: Box::new(scan_local(&root)?),
            })
        });
    }

    /// `git stash apply` or `git stash pop`, then a fresh scan.
    fn spawn_stash_apply(&mut self, index: usize, pop: bool) {
        let root = self.repo_root.clone();
        let subject = self
            .stashes
            .iter()
            .find(|s| s.index == index)
            .map(|s| s.subject.clone())
            .unwrap_or_else(|| format!("stash@{{{index}}}"));
        let label = if pop { "Popping" } else { "Applying" };
        self.spawn(format!("{label} the stash"), false, false, move || {
            gitops::stash_apply(&root, index, pop)?;
            let tail = if pop {
                "and dropped the stash"
            } else {
                "and kept the stash"
            };
            Ok(Outcome::Stashed {
                note: format!("⤓ Applied “{subject}” {tail}."),
                local: Box::new(scan_local(&root)?),
            })
        });
    }

    /// `git stash drop`. Nothing is put back, so the menu line says so.
    fn spawn_stash_drop(&mut self, index: usize) {
        let root = self.repo_root.clone();
        let subject = self
            .stashes
            .iter()
            .find(|s| s.index == index)
            .map(|s| s.subject.clone())
            .unwrap_or_else(|| format!("stash@{{{index}}}"));
        self.spawn("Dropping the stash", false, false, move || {
            gitops::stash_drop(&root, index)?;
            Ok(Outcome::Stashed {
                note: format!("✕ Dropped “{subject}”."),
                local: Box::new(scan_local(&root)?),
            })
        });
    }

    pub fn open_menu(&mut self, x: u16, y: u16) {
        let rows = self.build_menu();
        let mut menu = Menu {
            rows,
            sel: 0,
            scroll: 0,
            anchor: (x, y),
            title: " ☰ Menu ".into(),
            flip: false,
        };
        menu.sel = menu.first_selectable();
        self.overlay = Overlay::Menu(Box::new(menu));
    }

    /// The stash menu, anchored on the ☰ button. Reached from `S`, and
    /// from the ☰ line, which closes the menu it was clicked in — so it
    /// has no pointer of its own to open under.
    pub fn open_stash_menu_from_key(&mut self) {
        let (x, y) = self.menu_anchor();
        self.open_stash_menu(x, y);
    }

    /// The same, for one stash picked out of that menu.
    pub fn open_stash_item_menu_from_key(&mut self, index: usize) {
        let (x, y) = self.menu_anchor();
        self.open_stash_item_menu(index, x, y);
    }

    /// The cell the ☰ button occupies, which is where a menu with no
    /// pointer behind it hangs from.
    fn menu_anchor(&self) -> (u16, u16) {
        self.layout
            .buttons
            .iter()
            .find(|(_, id)| *id == ButtonId::Menu)
            .map(|(r, _)| (r.x, r.y))
            .unwrap_or((0, 0))
    }

    /// What the file panel can show, as a menu: one line per section,
    /// with a mark on the one in front of the reader.
    ///
    /// The panel's own button row drops buttons it has no room for, and
    /// it drops from the left — so the change itself, which is the
    /// leftmost and the one everybody comes back to, was the first to go.
    /// A narrow panel could reach every other section and not that one.
    /// This list never runs out of room.
    pub fn section_items(&self) -> Vec<MenuRow> {
        let item = |label: String, id: ButtonId, on: bool| {
            MenuRow::Item(MenuItem {
                label,
                hint: "F",
                id,
                enabled: true,
                checked: Some(on),
            })
        };
        let mut rows = vec![item(
            "◈  The change".into(),
            ButtonId::PanelChanges,
            self.panel == PanelMode::Changes,
        )];
        rows.push(item(
            "📁 Every file in the repository".into(),
            ButtonId::PanelFiles,
            self.panel == PanelMode::Files,
        ));
        rows.push(item(
            "◷  Commits not pushed yet".into(),
            ButtonId::PanelCommits,
            self.panel == PanelMode::Commits,
        ));
        // Only where there is a stack. A line that never does anything is
        // a line every reader has to read past.
        if let Some(st) = &self.stack {
            rows.push(item(
                format!("⑃  Stack #{} — {} pull requests", st.number, st.size),
                ButtonId::PanelStack,
                self.panel == PanelMode::Stack,
            ));
        }
        rows
    }

    /// The sections menu, hung under the `▾` that opens it.
    pub fn open_section_menu(&mut self, x: u16, y: u16) {
        let rows = self.section_items();
        self.open_menu_at(rows, " Show in this panel ".into(), x, y);
    }

    /// The same, from wherever the `▾` is — so the keyboard and the
    /// mouse land it in the same place, the way the ☰ menu does.
    pub fn open_section_menu_from_button(&mut self) {
        let (x, y) = self
            .layout
            .buttons
            .iter()
            .find(|(_, id)| *id == ButtonId::PanelMenu)
            .map(|(r, _)| (r.x, r.y))
            .unwrap_or_else(|| {
                let fl = self.layout.file_list;
                (fl.x, fl.y + fl.height)
            });
        self.open_section_menu(x, y);
    }

    /// What the panel is showing, in one word — the label the collapsed
    /// toggle button wears.
    pub fn section_label(&self) -> &'static str {
        match self.panel {
            PanelMode::Changes => "Change",
            PanelMode::Files => "Files",
            PanelMode::Commits => "Commits",
            PanelMode::Stack => "Stack",
        }
    }

    /// `m`: open the ☰ menu where the ☰ button is, so it lands in the same
    /// place whether the mouse or the keyboard asked for it.
    pub fn open_menu_from_key(&mut self) {
        let (x, y) = self.menu_anchor();
        self.open_menu(x, y);
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
        // Only a second Discard confirms a discard; anything else in
        // between means the reader moved on.
        if id != ButtonId::ReviewDiscard {
            self.discard_armed = false;
        }
        match id {
            ButtonId::ViewToggle => self.toggle_view(),
            ButtonId::LayersCycle => self.cycle_layers(),
            ButtonId::EditorFind => {
                if let Some(ed) = &mut self.editor {
                    ed.open_find();
                    self.ok("Find in this file — Tab adds a replacement, Esc cancels.");
                }
            }
            ButtonId::EditorComment => {
                let known = self.editor.as_mut().is_some_and(|e| e.toggle_comment());
                if !known {
                    self.err("Loupe does not know how this language writes a comment.");
                }
            }
            ButtonId::EditorDefinition => self.spawn_editor_request(EditorRequest::Definition),
            ButtonId::EditorHover => self.spawn_editor_request(EditorRequest::Hover),
            ButtonId::EditorReferences => self.spawn_editor_request(EditorRequest::References),
            ButtonId::EditorRename => {
                let word = self
                    .editor
                    .as_ref()
                    .map(|e| e.word_at_cursor())
                    .unwrap_or_default();
                if word.is_empty() {
                    self.err("Put the cursor on a name first.");
                } else {
                    self.open_path_box(PathBoxKind::RenameSymbol { word });
                }
            }
            ButtonId::EditorActions => self.spawn_editor_request(EditorRequest::CodeActions),
            ButtonId::EditorSignature => self.spawn_editor_request(EditorRequest::SignatureHelp),
            ButtonId::EditorCopy => self.copy_from_editor(),
            ButtonId::EditorNextProblem => self.step_problem(1),
            ButtonId::EditorPrevProblem => self.step_problem(-1),
            ButtonId::EditorProblems => self.open_problems(),
            ButtonId::EditorExplain => self.explain_problem(),
            ButtonId::BufferNext => self.step_buffer(1),
            ButtonId::BufferPrev => self.step_buffer(-1),
            ButtonId::PanelChanges => self.set_panel(PanelMode::Changes),
            ButtonId::PanelFiles => self.set_panel(PanelMode::Files),
            ButtonId::PanelCommits => self.set_panel(PanelMode::Commits),
            ButtonId::PanelStack => self.set_panel(PanelMode::Stack),
            ButtonId::PanelMenu => self.open_section_menu_from_button(),
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
            ButtonId::PinTab(i) => self.open_pin(i),
            ButtonId::BufferTab(i) => self.open_buffer_at(i),
            ButtonId::BufferClose(i) => self.close_buffer_at(i),
            ButtonId::PinClose(i) => self.close_pin(i),
            ButtonId::PinToggle => self.toggle_pin_current(),
            ButtonId::PinOpenPath => self.open_path_box(PathBoxKind::Open),
            ButtonId::OpenPathGo => self.confirm_open_path(),
            ButtonId::OpenPathCancel => {
                self.overlay = Overlay::None;
                self.ok("Closed — no file opened.");
            }
            ButtonId::Comment => self.open_comment(),
            ButtonId::Copy => self.yank(),
            ButtonId::CopyContext => self.yank_context(),
            ButtonId::RevertSection => self.ask_revert_section(self.diff_cursor),
            ButtonId::RevertFile => self.ask_revert_file(self.file_cursor),
            ButtonId::StageFile => self.toggle_file_mark(self.file_cursor),
            ButtonId::StageAll => self.stage_all(),
            ButtonId::UnstageAll => self.unstage_all(),
            ButtonId::StashMenu => self.open_stash_menu_from_key(),
            ButtonId::StashPush(scope) => self.ask_stash_name(scope),
            ButtonId::StashPick(i) => self.open_stash_item_menu_from_key(i),
            ButtonId::StashApply(i) => self.spawn_stash_apply(i, false),
            ButtonId::StashPop(i) => self.spawn_stash_apply(i, true),
            ButtonId::StashDrop(i) => self.spawn_stash_drop(i),
            ButtonId::ReviewBody => self.focus_review(),
            ButtonId::ReviewSubmit => self.ask_submit_review(),
            ButtonId::ReviewVerdict => {
                self.review.picking = true;
                self.review.pick = Verdict::all()
                    .iter()
                    .position(|v| *v == self.review.verdict)
                    .unwrap_or(0);
                self.overlay = Overlay::VerdictMenu;
            }
            ButtonId::ReviewDiscard => self.discard_pending(),
            ButtonId::SetVerdict(v) => self.set_verdict(v),
            ButtonId::ResolveConflict => self.open_conflict_menu_from_key(),
            // Away from any one conflict: the menu then offers only the
            // lines that settle the file as a whole.
            ButtonId::ResolveFile => {
                let r = self.layout.diff;
                self.open_conflict_menu(usize::MAX, r.x + 2, r.y + 1);
            }
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
            ButtonId::Quit => self.request_quit(),
            ButtonId::EditorSave => self.spawn_save_editor(),
            ButtonId::EditorClose => self.request_close_editor(),
            ButtonId::EditorFormat => self.spawn_editor_request(EditorRequest::Format),
            _ => return false,
        }
        true
    }

    fn diff_click(&mut self, x: u16, y: u16) {
        // Clicking a fold row expands it.
        let r = self.layout.diff;
        let vis = self.diff_scroll + (y - r.y) as usize;
        // The change bar down the left edge: anywhere on a section's bar
        // offers to put that section back — or, on a conflicted file, to
        // resolve the conflict that bar belongs to.
        if x < r.x + self.revert_gutter() {
            if self.conflict.is_some() {
                self.open_conflict_menu(vis, x, y);
            } else {
                self.ask_revert_section(vis);
            }
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
            .position(|e| {
                matches!(e, FileEntry::File { src: RowSrc::Changed(i), .. } if *i == self.file_cursor)
            })
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

    /// Move the file cursor one row, following what is on screen.
    ///
    /// The panel groups files by directory and by staging section, so the
    /// order of `self.files` and the order of the rows are not the same
    /// thing. Stepping through the list would send the cursor jumping
    /// between the two sections, so the rows are what `j` and `k` walk.
    fn step_file(&mut self, delta: i32) {
        if self.files.is_empty() {
            return;
        }
        let rows: Vec<usize> = self
            .entries
            .iter()
            .filter_map(|e| match e {
                FileEntry::File {
                    src: RowSrc::Changed(i),
                    ..
                } => Some(*i),
                _ => None,
            })
            .collect();
        let idx = match rows.iter().position(|i| *i == self.file_cursor) {
            Some(at) => rows[(at as i32 + delta).clamp(0, rows.len() as i32 - 1) as usize],
            // The open file has no row — a folded section holds it, or
            // the panel is showing the repository instead of the change.
            // Fall back to the list, which is always there.
            None => {
                (self.file_cursor as i32 + delta).clamp(0, self.files.len() as i32 - 1) as usize
            }
        };
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
        // Opens over the editor. It used to refuse, which meant a reader
        // in a buffer could not search the repository at all — and
        // whatever they pick parks the buffer rather than closing it, so
        // there was never anything to lose by opening it.
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
            FinderMode::Refs | FinderMode::Pick | FinderMode::Problems => {
                "Type to filter · Enter chooses"
            }
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
            let _ = tx.send(
                // Only the files: a stub is a directory git would not walk
                // into, and the finder has nothing to open there.
                search::list_files(&root, rev.as_deref())
                    .map(|found| SearchOutcome::Files(found.files)),
            );
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
        // Landing somewhere from the editor has to leave it first, and
        // must not throw away unsaved work to do it. Inside the open
        // buffer there is nothing to leave.
        if let Some(ed) = &self.editor {
            if ed.path == path {
                if let (Some(l), Some(ed)) = (line, &mut self.editor) {
                    ed.jump_to_line(l);
                }
                return;
            }
            if ed.dirty {
                self.err(format!(
                    "{path} is another file — save (Ctrl+S) or discard first."
                ));
                return;
            }
            self.editor = None;
        }
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
    /// Read a file the file tree named, on a worker, and open it.
    ///
    /// **Nothing on screen changes until the file is in hand.** The pane
    /// keeps the file it is showing, the tab row keeps its tabs, and the
    /// blame column keeps its column, right up to the frame the new file
    /// lands on. Tearing any of it down first is what made clicking
    /// through a tree flash back to the diff between every pair of
    /// files: the read takes a frame or two, and every frame in between
    /// drew a pane with nothing in it.
    ///
    /// A *quiet* job rather than a foreground one, for the same reason.
    /// A foreground job swallows every mouse event until it lands
    /// (`handle_mouse` returns on `self.job.is_some()`), which ate the
    /// second half of every double click on the tree. Quiet keeps input
    /// live, and the one quiet slot gives the behaviour a reader walking
    /// a list wants for free: a click that lands while the last one is
    /// still reading replaces it, so the newest file wins.
    ///
    /// Always the editor, never the rendered document. The document has
    /// no buffer behind it and so no tab, and a click on the tree is
    /// promised a tab. `P` renders it from there.
    fn spawn_open_tree_file(&mut self, path: String, double: bool) {
        let root = self.repo_root.clone();
        let rev = self.search_rev();
        let tree_is_review = self.local || self.checked_out;
        let label = format!("Opening {path}");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let read = read_external(&root, &path, rev.as_deref(), tree_is_review).map(|mut d| {
                d.peek = !double;
                QuietOutcome::External(Box::new(d))
            });
            let _ = tx.send(read);
        });
        self.quiet = Some(QuietJob {
            rx,
            label,
            started: Instant::now(),
            auto: false,
            eager: true,
        });
    }

    fn spawn_open_external(&mut self, path: String, line: Option<usize>) {
        // Already open: come back to it, unsaved edits and all, rather
        // than reading the file again over the top of them.
        if self.switch_buffer(&path) {
            if let (Some(l), Some(ed)) = (line, &mut self.editor) {
                ed.jump_to_line(l);
            }
            self.ok(format!("{path} — already open."));
            return;
        }
        let root = self.repo_root.clone();
        let rev = self.search_rev();
        // In local review the working tree *is* what is under review.
        let tree_is_review = self.local || self.checked_out;
        let label = format!("Opening {path}");
        self.spawn(label, true, false, move || {
            let mut d = read_external(&root, &path, rev.as_deref(), tree_is_review)?;
            // A search result or a jump lands on a document when the file
            // is one; the file tree always wants the source, and asks for
            // it through its own door.
            d.preview = markdown::is_markdown(&d.path);
            d.line = line;
            Ok(Outcome::ExternalOpened(Box::new(d)))
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
        // A cold language server can take half a minute to answer the
        // first question: the handshake, then the request, then the wait
        // for indexing to settle. Silence for that long reads as a key
        // that did nothing, so every request the reader asked for says
        // what it is waiting on until the answer lands.
        let label = what.waiting_for(&word);
        let diagnostics = editor.problems().to_vec();
        let lsp = self.lsp.clone();
        let root = self.repo_root.clone();
        self.editor_gen += 1;
        let gen = self.editor_gen;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = match what {
                EditorRequest::Rename(to) => lsp
                    .rename(&root, &path, &text, at, &to)
                    .map(|edits| EditorOutcome::Renamed { to, edits }),
                EditorRequest::CodeActions => lsp
                    .code_actions(&root, &path, &text, at, &diagnostics)
                    .map(EditorOutcome::CodeActions),
                EditorRequest::SignatureHelp => lsp
                    .signature_help(&root, &path, &text, at)
                    .map(EditorOutcome::Signature),
                EditorRequest::References => lsp.references(&root, &path, &text, at).map(|locs| {
                    EditorOutcome::References(Box::new(LocationsData {
                        action: LspAction::References,
                        places: build_places(&root, None, locs),
                        word,
                    }))
                }),
                EditorRequest::Complete(trigger) => lsp
                    .complete(&root, &path, &text, at, trigger)
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
        self.editor_job = Some(EditorJob {
            rx,
            gen,
            started: Instant::now(),
            label,
        });
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
            EditorOutcome::References(d) => self.apply_locations(*d),
            EditorOutcome::Renamed { to, edits } => self.apply_workspace_edit(&to, edits),
            EditorOutcome::CodeActions(actions) => {
                if actions.is_empty() {
                    self.err("Nothing on offer here.");
                    return;
                }
                self.ok(format!(
                    "{} on offer — Enter applies, Esc closes.",
                    actions.len()
                ));
                self.overlay = Overlay::CodeActions(Box::new(CodeActionMenu { actions, sel: 0 }));
            }
            EditorOutcome::Signature(sig) => match sig {
                Some(sig) => {
                    if let Some(ed) = &mut self.editor {
                        ed.signature = Some(sig);
                    }
                }
                None => {
                    if let Some(ed) = &mut self.editor {
                        ed.signature = None;
                    }
                    self.err("The cursor is not inside a call the server knows.");
                }
            },
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
    /// Publish what is on screen for the context provider.
    ///
    /// The socket thread cannot borrow `App`, so the UI thread leaves it a
    /// snapshot of owned strings instead. This runs only when something
    /// changed, and it costs one short clone of the selected text.
    pub fn publish_context(&self) {
        let Some(shared) = &self.context else {
            return;
        };
        let snapshot = self.context_snapshot();
        if let Ok(mut slot) = shared.lock() {
            *slot = snapshot;
        }
    }

    /// `Y`: copy the context block, for an agent that has no hook — or for
    /// a loupe on the far end of an SSH connection, where the socket cannot
    /// reach the machine the agent runs on.
    pub fn yank_context(&mut self) {
        let text = self.context_snapshot().render();
        match clipboard::copy(&text) {
            Ok(via) => self.ok(format!(
                "⧉ Copied the context for your agent via {via} — paste it before your instruction."
            )),
            Err(e) => self.err(format!("Couldn't copy: {e:#}")),
        }
    }

    /// What is on screen, as owned data the socket thread can hold.
    fn context_snapshot(&self) -> ctx::Snapshot {
        let (selection, hunk) = match self.selection {
            Some(sel) => {
                let (lo, hi) = sel.range();
                let side = if sel.side == Side::Left { "old" } else { "new" };
                let text = self
                    .side_content(sel.side)
                    .map(|content| sel.text(content))
                    .filter(|t| !t.is_empty());
                (Some((side, lo, hi)), text)
            }
            None => (None, None),
        };
        // In local review the file panel stages instead of marking viewed,
        // so "not yet read" is a question only a pull request can answer.
        let unviewed = if self.local {
            Vec::new()
        } else {
            self.files
                .iter()
                .filter(|f| !self.viewed.contains(&f.path))
                .map(|f| f.path.clone())
                .collect()
        };
        ctx::Snapshot {
            // A pull request knows its `owner/name`; a local review has no
            // reason to have asked GitHub anything, so fall back to what
            // the clone is called on disk.
            repo: self.repo.clone().or_else(|| {
                self.repo_root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            }),
            branch: match &self.pr {
                Some(pr) => Some(pr.head_ref_name.clone()),
                None => self.local_branch.clone(),
            },
            pr: self.pr.as_ref().map(|pr| (pr.number, pr.title.clone())),
            local: self.local,
            // Whatever the diff pane is showing, which while a commit is
            // open is one of its files rather than one of the change's.
            file: self.open_file().map(|f| f.path.clone()),
            selection,
            hunk,
            unviewed,
            held: self
                .pending
                .iter()
                .map(|c| (c.path.clone(), c.line))
                .collect(),
        }
    }

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
                let places = build_places(&root, rev.as_deref(), locs);
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

    /// Copy what the editor's `Ctrl+C` would copy: the selection, or the
    /// cursor line when there is none. One helper behind the key, the
    /// right-click menu and the ☰ line, so all three copy the same thing.
    fn copy_from_editor(&mut self) {
        let target = self.editor.as_mut().and_then(|e| e.copy_target());
        match target {
            Some((text, what)) => match clipboard::copy(&text) {
                Ok(via) => self.ok(format!("⧉ Copied {what} via {via}.")),
                Err(e) => self.err(format!("Couldn't copy: {e:#}")),
            },
            None => self.err("Nothing to copy."),
        }
    }

    /// Put the cursor on the next problem in the open file, wrapping at
    /// the ends, and say what it is. The status bar shows the message on
    /// its own once the cursor is on the line, so this only has to say
    /// where the reader has landed.
    fn step_problem(&mut self, delta: i32) {
        let Some(ed) = &mut self.editor else { return };
        let total = ed.problems().len();
        match ed.step_problem(delta) {
            Some(d) => {
                let at = ed
                    .problems()
                    .iter()
                    .position(|p| p.line == d.line && p.col == d.col)
                    .map(|i| i + 1)
                    .unwrap_or(1);
                self.ok(format!(
                    "{} {at} of {total} — line {}: {}",
                    if d.is_error() { "✗" } else { "▲" },
                    d.line,
                    d.message
                ));
            }
            None => {
                let name = self
                    .editor
                    .as_ref()
                    .map(|e| e.path.clone())
                    .unwrap_or_default();
                self.err(format!("Nothing wrong in {name}."));
            }
        }
    }

    /// Lay out the problem the cursor is on, so a message too long for
    /// one line can actually be read.
    ///
    /// The worst one on the line, when there is more than one — the same
    /// rule the gutter mark and the margin already follow, so the panel
    /// is about the problem the reader can see.
    fn explain_problem(&mut self) {
        let Some(ed) = &self.editor else { return };
        let here = ed.diagnostics_here();
        let of = here.len();
        let Some(d) = here.first().copied().cloned() else {
            let line = ed.cursor_pos().0;
            self.err(format!("Nothing wrong on line {line}."));
            return;
        };
        let panel = ProblemPanel {
            rows: crate::explain::rows(&d.message),
            code: d.code_label(),
            severity: d.severity,
            line: d.line,
            of,
        };
        self.overlay = Overlay::Problem(Box::new(panel));
    }

    /// Every problem in the open file, as a list to pick from.
    ///
    /// The same overlay references use, for the same reason: a list of
    /// places in a file is a list of places in a file, and typing filters
    /// it. Enter goes to the line.
    fn open_problems(&mut self) {
        let Some(ed) = &self.editor else { return };
        let path = ed.path.clone();
        let problems: Vec<lsp::Diagnostic> = ed.problems().to_vec();
        if problems.is_empty() {
            self.err(format!("Nothing wrong in {path}."));
            return;
        }
        let n = problems.len();
        let errors = problems.iter().filter(|d| d.is_error()).count();
        let warnings = problems.iter().filter(|d| d.is_warning()).count();
        let changeset = self.changeset_paths();
        let in_changeset = changeset.contains(&path);
        let (symbols, symbol_path) = self.current_symbols();
        let mut finder = Finder::new(FinderMode::Problems, changeset, symbols, symbol_path);
        finder.preset = problems
            .iter()
            .map(|d| FinderRow {
                path: path.clone(),
                line: Some(d.line),
                text: match &d.code {
                    Some(c) => format!(
                        "{} {}  {c}",
                        if d.is_error() { "✗" } else { "▲" },
                        d.message
                    ),
                    None => format!("{} {}", if d.is_error() { "✗" } else { "▲" }, d.message),
                },
                matched: Vec::new(),
                range: None,
                tag: d.label(),
                in_changeset,
                pick: None,
            })
            .collect();
        self.overlay = Overlay::Finder(Box::new(finder));
        self.refresh_finder();
        // rebuild() only rewrites the note once a filter is typed.
        if let Overlay::Finder(f) = &mut self.overlay {
            f.note = format!(
                "{path} · {errors} {} · {warnings} {} · Enter goes to the line · type to filter",
                if errors == 1 { "error" } else { "errors" },
                if warnings == 1 { "warning" } else { "warnings" }
            );
        }
        self.ok(format!("{n} problems in {path}."));
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

    /// Whether the preview is holding a search, which changes what `n`,
    /// `N` and Esc mean while it is.
    fn preview_find_active(&self) -> bool {
        self.preview.as_ref().is_some_and(|pv| pv.find.active())
    }

    /// `n` and `N` in the preview.
    fn step_preview_match(&mut self, forward: bool) {
        let stepped = self
            .preview
            .as_mut()
            .is_some_and(|pv| pv.step_match(forward));
        if !stepped {
            self.err("No matches.");
        }
    }

    /// `/` and Ctrl+F in the preview: open the prompt over the document.
    pub fn start_preview_find(&mut self) {
        let Some(pv) = &mut self.preview else { return };
        pv.open_find();
        self.ok("/");
    }

    /// Keystrokes while the preview's prompt is open.
    ///
    /// The same shape as the diff's: the query re-runs on every character,
    /// Enter keeps it and the highlight, Esc puts the reader back where
    /// they started reading.
    fn preview_find_key(&mut self, key: KeyEvent) {
        let Some(pv) = &mut self.preview else { return };
        match key.code {
            KeyCode::Esc => {
                pv.cancel_find();
                self.ok("Search cancelled.");
            }
            KeyCode::Enter => {
                pv.find.typing = false;
                let (empty, none) = (pv.find.query.is_empty(), pv.find.rows.is_empty());
                let query = pv.find.query.clone();
                if empty {
                    self.ok("Search cleared.");
                } else if none {
                    self.err(format!(
                        "No match for “{query}” in this document — # searches every file."
                    ));
                } else {
                    self.ok("n and N walk the matches · Esc clears them.");
                }
            }
            KeyCode::Backspace => {
                pv.find.query.pop();
                pv.refresh_find();
            }
            KeyCode::Char(c) => {
                pv.find.query.push(c);
                pv.refresh_find();
            }
            _ => {}
        }
    }

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
        // Parked, not dropped. The tab row is the reader's list of open
        // files, and a document is a rendering of an open file, not a
        // different thing — a tab that vanished on P and came back on the
        // next P would be lying about what is open.
        self.park_active();
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
        // The buffer P parked. Coming back to it keeps its undo history
        // and its unsaved edits; a fresh one built from the rendered text
        // would keep neither.
        if self.switch_buffer(&pv.path) {
            if let Some(ed) = &mut self.editor {
                ed.jump_to_line(line);
            }
        } else {
            let mut ed = Editor::new(&pv.path, pv.abs_path.clone(), &pv.src);
            ed.standalone = true;
            ed.dirty = pv.from_buffer;
            ed.jump_to_line(line);
            self.open_buffer(ed);
        }
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
        let Some(was) = self.preview.take() else {
            return;
        };
        // A document P opened has a buffer parked behind it. Leaving the
        // document leaves the file, so the buffer goes with it and the
        // tab row stops listing a file nobody has open.
        if was.from_editor && self.switch_buffer(&was.path) {
            self.close_editor();
            return;
        }
        // The blame pane goes back to the file under review.
        self.spawn_blame(self.file_cursor);
        self.ok("Back to the diff.");
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
                .open_file()
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
        // `loupe md` is often run inside a repository even though it
        // shows no review. When it is, the tab row is the same row the
        // review would have had.
        if let Some(root) = gitops::repo_root() {
            self.repo_root = root;
            self.load_pins();
        }
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

    // ---------------------------------------------------------- pinned files

    /// Where this clone's pins live between runs. `None` outside a git
    /// repository, which is `loupe md <path>` with nothing around it —
    /// there the tabs last for the session and no file is written.
    fn pins_path(&self) -> Option<PathBuf> {
        let dir = gitops::git_dir(&self.repo_root)?;
        pins::state_path(&dir)
    }

    /// Read last session's tabs back. Called once the repository root is
    /// known, which is after the first load, not at construction.
    pub fn load_pins(&mut self) {
        let Some(path) = self.pins_path() else { return };
        let items = pins::load(&path);
        if items.is_empty() {
            return;
        }
        let n = items.len();
        self.pins.items = items;
        self.auto_open_note = Some(format!(
            "{n} pinned file{} — Alt+1 opens the first",
            if n == 1 { "" } else { "s" }
        ));
    }

    /// Write the tabs out. Called after every change to them, so quitting
    /// never costs the reader their row.
    fn save_pins(&mut self) {
        let Some(path) = self.pins_path() else { return };
        if let Err(e) = pins::save(&path, &self.pins.items) {
            self.err(format!("Couldn't save the pinned files: {e}"));
        }
    }

    /// The file in front of the reader, as an absolute path: what the
    /// preview is rendering, what the editor holds, or the file under the
    /// file-panel cursor.
    fn current_target(&self) -> Option<PathBuf> {
        let raw = if let Some(pv) = &self.preview {
            pv.abs_path.clone()
        } else if let Some(ed) = &self.editor {
            ed.abs_path.clone()
        } else {
            // The file panel is not on screen on the PR list, so there is
            // nothing in front of the reader to pin there.
            if self.screen != Screen::Review {
                return None;
            }
            // Whatever the diff pane is showing, which while a commit
            // is open is one of its files rather than the change's.
            let file = self.open_file()?;
            gitops::safe_repo_path(&self.repo_root, &file.path)?
        };
        // Resolved, so it compares equal to a pin made from a dropped
        // path — which the terminal always hands over already resolved.
        Some(std::fs::canonicalize(&raw).unwrap_or(raw))
    }

    /// The tab whose file is on screen, if any.
    ///
    /// Derived rather than remembered: the reader leaves a document by a
    /// dozen different doors, and every one of them would otherwise have
    /// to clear a stored index. This cannot go stale.
    pub fn active_pin(&self) -> Option<usize> {
        // Nothing pinned is the common case, and it costs nothing: the
        // answer is asked for on every draw.
        if self.pins.is_empty() {
            return None;
        }
        let abs = self.current_target()?;
        self.pins.find(&abs)
    }

    /// True when the file in front of the reader already has a tab. The
    /// 📌 button draws as pressed while it is.
    pub fn current_is_pinned(&self) -> bool {
        self.active_pin().is_some()
    }

    /// `=` and the 📌 button: pin the file in front of the reader, or
    /// unpin it when it is already pinned.
    pub fn toggle_pin_current(&mut self) {
        let Some(abs) = self.current_target() else {
            self.err("Nothing to pin — open a file first.");
            return;
        };
        if let Some(i) = self.pins.find(&abs) {
            self.close_pin(i);
            return;
        }
        if !abs.is_file() {
            self.err("That file is not on disk — there is nothing to pin.");
            return;
        }
        let pin = Pin::new(&self.repo_root, abs);
        let name = pin.path.clone();
        match self.pins.add(pin) {
            Ok(i) => {
                self.save_pins();
                self.ok(format!(
                    "📌 {name} pinned — “{}” opens it again, “-” unpins it.",
                    i + 1
                ));
            }
            Err(max) => self.err(format!(
                "The tab row holds {max} files — unpin one first (“-”, or the ✕ on a tab)."
            )),
        }
    }

    /// Unpin one tab. The file itself is untouched — a pin is a bookmark,
    /// not a copy.
    pub fn close_pin(&mut self, idx: usize) {
        let was_open = self.active_pin() == Some(idx) && self.preview.is_some();
        let Some(gone) = self.pins.remove(idx) else {
            return;
        };
        self.save_pins();
        // Leaving the tab that is on screen would leave the reader inside
        // a document with no tab, so put them back in the review.
        if was_open && !self.preview_only {
            self.close_preview();
        }
        self.ok(format!("{} unpinned — the file is untouched.", gone.path));
    }

    /// `Alt+]` / `Alt+[`: the next or previous tab, wrapping at each end.
    pub fn step_pin(&mut self, delta: i32) {
        match self.pins.step(delta, self.active_pin()) {
            Some(i) => self.open_pin(i),
            None => self.err("Nothing pinned yet — “=” pins the file you are looking at."),
        }
    }

    /// Open the file one tab holds.
    ///
    /// A markdown file renders as a document, wherever it lives — that is
    /// what a tab is for. A file the change touches opens as its diff,
    /// because during a review that is what coming back to it means.
    /// Anything else opens in the editor.
    pub fn open_pin(&mut self, idx: usize) {
        let Some(pin) = self.pins.items.get(idx).cloned() else {
            return;
        };
        if self.job.is_some() {
            return;
        }
        if !pin.abs_path.is_file() {
            self.err(format!(
                "{} is not there any more — “-” unpins it.",
                pin.path
            ));
            return;
        }
        // Whether there is a review to open the file *inside*. Asked
        // before the screen changes: the file list is left standing when
        // the reader goes back to the pull-request list, so on that
        // screen it names a review that is not on screen.
        let in_review = self.screen == Screen::Review;
        self.overlay = Overlay::None;
        self.screen = Screen::Review;

        // In the changeset: go through the ordinary file-open door, so the
        // diff, the blame pane and the staging state all follow.
        let in_change = (in_review && !pin.outside)
            .then(|| self.files.iter().position(|f| f.path == pin.path))
            .flatten();
        if let Some(i) = in_change {
            let md = markdown::is_markdown(&pin.path);
            // Already the file under the cursor with its diff in hand, so
            // it does not have to be read again. The *pane* still has to
            // change hands, though: what is on screen is usually another
            // pinned file's document, and leaving it there is what made a
            // tab for a changed file look like it did nothing at all.
            if i == self.file_cursor && self.diff.is_some() {
                // Parked, not closed. Unsaved editor text is the
                // reader's own work; a tab does not get to throw it
                // away, and parking means it does not have to — the
                // buffer keeps its text, its place in the row and the
                // dot that says it is unsaved.
                self.park_active();
                let showing_it = self
                    .preview
                    .as_ref()
                    .is_some_and(|pv| pv.abs_path == pin.abs_path);
                if md && !showing_it {
                    // Rebuilt rather than kept: an open preview belongs
                    // to some other file.
                    self.preview = None;
                    self.preview_current_file();
                } else if !md && self.preview.take().is_some() {
                    // A document was covering the diff this tab is for.
                    // The blame pane comes back with it.
                    self.spawn_blame(self.file_cursor);
                    self.ok(format!("📌 {} — back to its diff.", pin.path));
                } else {
                    self.ok(format!("📌 {} — already open.", pin.path));
                }
            } else {
                // The document, not the diff — that is what the tab is
                // for. The flag is read when the file lands.
                self.pin_wants_preview = md;
                // Parked before the load, which closes the editor when
                // it lands. Same reason as above.
                self.park_active();
                self.spawn_load_file(i);
            }
            return;
        }

        // Outside the changeset — and possibly outside the repository.
        // Read it here rather than on a worker: the file is the reader's
        // own choice, and the size guard keeps the wait bounded.
        let content = match self.read_pinned(&pin) {
            Ok(text) => text,
            Err(e) => {
                self.err(e);
                return;
            }
        };
        if markdown::is_markdown(&pin.path) {
            self.park_active();
            let mut pv = Preview::new(&pin.path, pin.abs_path.clone(), &content);
            pv.standalone = true;
            pv.mtime = preview::mtime_of(&pin.abs_path);
            self.preview = Some(pv);
            self.clear_blame();
            self.ok(format!(
                "📖 {} — P shows the source, Esc goes back.",
                pin.path
            ));
            return;
        }
        self.preview = None;
        let mut ed = Editor::new(&pin.path, pin.abs_path.clone(), &content);
        ed.standalone = true;
        self.open_buffer(ed);
        // git blame only knows files in this repository; an outside file
        // has no history here to draw.
        if pin.outside {
            self.clear_blame();
        } else {
            self.spawn_blame_external(pin.path.clone(), false);
        }
        self.ok(format!(
            "✎ {} — Ctrl+S saves, Esc goes back to the review.",
            pin.path
        ));
    }

    /// Read a pinned file, refusing the two cases that are never what the
    /// reader meant: a symlink, and something far too big to be a
    /// document.
    fn read_pinned(&self, pin: &Pin) -> Result<String, String> {
        if is_symlink(&pin.abs_path) {
            return Err(format!(
                "{} is a symlink — refusing to open through it.",
                pin.path
            ));
        }
        let size = std::fs::metadata(&pin.abs_path)
            .map(|m| m.len())
            .unwrap_or(0);
        if size > pins::MAX_BYTES {
            return Err(format!(
                "{} is {} MB — too big to read as a document.",
                pin.path,
                size / (1024 * 1024)
            ));
        }
        std::fs::read_to_string(&pin.abs_path).map_err(|e| format!("Cannot read {}: {e}", pin.path))
    }

    // ------------------------------------------------- pastes and drops

    /// One drain of the terminal's event queue.
    ///
    /// The events are taken as a *batch* rather than one at a time, and
    /// that is the whole point of this function. Not every terminal wraps
    /// a dropped file in bracketed paste: several write the path in as if
    /// it had been typed, one key event per character, all of them
    /// arriving in the same read. Dispatched as ordinary keys, the
    /// leading `/` of the path opens the search prompt and the rest of
    /// the path lands in the query box at the foot of the window — the
    /// file never opens, and what the reader sees is their own path
    /// spelled out along the bottom of the screen.
    ///
    /// So the batch is read for a path *before* any of it is dispatched.
    pub fn handle_events(&mut self, events: Vec<Event>) {
        if let Some(text) = typed_path_burst(&events) {
            self.handle_paste(text);
            return;
        }
        for ev in events {
            match ev {
                Event::Key(key) if key.kind != KeyEventKind::Release => self.handle_key(key),
                Event::Mouse(m) => self.handle_mouse(m),
                // A paste, or a drop from a terminal that does bracket
                // it. `handle_paste` tells those two apart.
                Event::Paste(text) => self.handle_paste(text),
                _ => {}
            }
        }
    }

    /// A paste, or a file dropped on the terminal window. The two arrive
    /// the same way, so the text decides which it is: a paste in which
    /// every token is an absolute path to a file that exists is a drop,
    /// and anything else is text (see [`crate::pins::dropped_paths`]).
    pub fn handle_paste(&mut self, text: String) {
        self.last_input = Instant::now();
        // A drop is answered even while a job is running. Pinning costs
        // nothing, and a file dropped on the window must not vanish
        // because loupe happened to be loading something at the time —
        // the reader has no way of knowing that, and nothing on screen
        // would say the drop was lost.
        if let Some(paths) = pins::dropped_paths(&text) {
            self.open_dropped(paths);
            return;
        }
        // Text, then. While a job is modal there is no box to put it in.
        if self.job.is_some() {
            return;
        }
        // The one place text has to go is wherever the keyboard already
        // is.
        match &mut self.overlay {
            Overlay::OpenPath(box_) => box_.insert(text.trim()),
            Overlay::Comment(draft) => {
                draft.textarea.insert_str(&text);
            }
            Overlay::Finder(finder) => {
                // The finder holds one line, so a multi-line paste is
                // flattened rather than half-swallowed.
                let one_line: String = text.split(['\n', '\r']).collect::<Vec<_>>().join(" ");
                finder.input.push_str(&one_line);
                finder.cursor = finder.input.chars().count();
                self.refresh_finder();
            }
            _ => {
                if self.find.typing {
                    // The `/` prompt is one line, like the finder.
                    let one_line: String = text.split(['\n', '\r']).collect::<Vec<_>>().join(" ");
                    self.find.query.push_str(&one_line);
                    self.recompute_matches();
                } else if self.review.focused {
                    self.review.textarea.insert_str(&text);
                } else if let Some(ed) = &mut self.editor {
                    if ed.read_only {
                        self.err("This file is read-only — nothing was pasted.");
                        return;
                    }
                    ed.textarea.insert_str(&text);
                    ed.dirty = true;
                    self.buffer_changed();
                } else {
                    self.err(
                        "Pasted text has nowhere to go here — drop a file to pin it, or Ctrl+O opens one by path.",
                    );
                }
            }
        }
    }

    /// Pin every file that was dropped, and open the first of them. A
    /// drop is the fast way to read a document that lives outside the
    /// repository, so it never asks and never copies the file anywhere.
    fn open_dropped(&mut self, paths: Vec<PathBuf>) {
        let mut opened: Option<usize> = None;
        let mut full = false;
        let mut names: Vec<String> = Vec::new();
        for path in paths {
            let abs = std::fs::canonicalize(&path).unwrap_or(path);
            let pin = Pin::new(&self.repo_root, abs);
            let name = pin.path.clone();
            match self.pins.add(pin) {
                Ok(i) => {
                    names.push(name);
                    opened.get_or_insert(i);
                }
                Err(_) => {
                    full = true;
                    break;
                }
            }
        }
        if names.is_empty() {
            if full {
                self.err(format!(
                    "The tab row already holds {} files — unpin one first (“-”).",
                    pins::MAX_PINS
                ));
            }
            return;
        }
        self.save_pins();
        let dropped = names.len();
        // A load is in flight, so the pane is not the reader's to take.
        // The tabs are made all the same; opening one waits for a key.
        if self.job.is_some() {
            self.ok(format!(
                "📌 {} pinned — “1” opens {} once this finishes.",
                names.join(", "),
                if dropped == 1 { "it" } else { "the first" }
            ));
            return;
        }
        if let Some(i) = opened {
            self.open_pin(i);
        }
        // `open_pin` has already said what it opened; add what else came
        // in with it, and the warning when the row filled up.
        let mut note = String::new();
        if dropped > 1 {
            note.push_str(&format!(" · {} more pinned", dropped - 1));
        }
        if full {
            note.push_str(" · the rest did not fit in the tab row");
        }
        if !note.is_empty() && !self.status_err {
            let now = self.status.clone();
            self.ok(format!("{now}{note}"));
        }
    }

    /// `-`: unpin the tab the reader is in.
    pub fn close_open_pin(&mut self) {
        match self.active_pin() {
            Some(i) => self.close_pin(i),
            None => self.err("No pinned file open — “=” pins the one you are looking at."),
        }
    }

    /// The tab-row keys that hold a modifier, so they reach the row from
    /// inside the editor and every text box too. True when the key was
    /// one of them.
    ///
    /// `Ctrl+O` rather than `Alt+O` for the path box: a macOS terminal
    /// sends the Option key as an accented character by default, not as
    /// Meta, so an Alt-only binding would not arrive at all there. The
    /// Alt spellings stay for the terminals that do send Meta.
    fn pin_key(&mut self, key: KeyEvent) -> bool {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('o') | KeyCode::Char('O') if ctrl => {
                self.open_path_box(PathBoxKind::Open)
            }
            KeyCode::Char(c @ '1'..='9') if alt => self.open_pin_number(c),
            KeyCode::Char('=') | KeyCode::Char('+') if alt => self.toggle_pin_current(),
            KeyCode::Char('-') if alt => self.close_open_pin(),
            KeyCode::Char('.') if alt => self.step_pin(1),
            KeyCode::Char(',') if alt => self.step_pin(-1),
            // `,` and `.` step the pins, so the buffers take the brackets.
            // Alt, not Ctrl: the editor takes plain characters as text and
            // `Ctrl+]` is already the jump to a definition.
            KeyCode::Char(']') if alt => self.step_buffer(1),
            KeyCode::Char('[') if alt => self.step_buffer(-1),
            _ => return false,
        }
        true
    }

    /// The bare `1`-`9`, `,`, `.`, `=` and `-`, for the screens where a
    /// bare key is a command rather than a letter. True when the key was
    /// one of them.
    fn pin_key_bare(&mut self, key: KeyEvent) -> bool {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        match key.code {
            KeyCode::Char(c @ '1'..='9') => self.open_pin_number(c),
            KeyCode::Char('=') | KeyCode::Char('+') => self.toggle_pin_current(),
            KeyCode::Char('-') => self.close_open_pin(),
            KeyCode::Char('.') => self.step_pin(1),
            KeyCode::Char(',') => self.step_pin(-1),
            _ => return false,
        }
        true
    }

    /// Open the tab a digit names, counting from 1 the way the row is
    /// labelled.
    fn open_pin_number(&mut self, c: char) {
        let n = c.to_digit(10).unwrap_or(0) as usize;
        if n == 0 {
            return;
        }
        if n > self.pins.len() {
            if self.pins.is_empty() {
                self.err("Nothing pinned yet — “=” pins the file you are looking at, or drop one on the window.");
            } else {
                self.err(format!(
                    "There is no tab {n} — {} pinned right now.",
                    self.pins.len()
                ));
            }
            return;
        }
        self.open_pin(n - 1);
    }

    /// `Ctrl+O`: the path box. It is how a reader on a terminal that
    /// cannot report a drop still opens a file from anywhere, and how a
    /// path an agent just printed gets pasted straight in.
    pub fn open_path_box(&mut self, kind: PathBoxKind) {
        self.ok(match &kind {
            PathBoxKind::Open => {
                "Type or paste a path — Enter opens and pins it, Esc cancels.".to_string()
            }
            PathBoxKind::NewFile { dir } if dir.is_empty() => {
                "Name for the new file — Enter creates and opens it, Esc cancels.".to_string()
            }
            PathBoxKind::NewFile { dir } => {
                format!("Name for the new file in {dir} — Enter creates and opens it.")
            }
            PathBoxKind::Rename { from } => format!("New name for {from} — Esc cancels."),
            PathBoxKind::RenameSymbol { word } => {
                format!("Rename {word} to — every file it touches opens unsaved for you to read.")
            }
            PathBoxKind::StashName { scope } => format!(
                "Name this stash of {} — Enter with nothing typed lets git name it.",
                scope.label()
            ),
        });
        self.overlay = Overlay::OpenPath(Box::new(OpenPathBox {
            kind,
            ..Default::default()
        }));
    }

    /// Create an empty file and open it.
    ///
    /// Every guard the editor uses applies: a path that would escape the
    /// repository is refused, and so is one that already exists — silently
    /// opening a file somebody meant to create is how work gets
    /// overwritten.
    fn create_file(&mut self, path: &str) {
        let Some(abs) = gitops::safe_repo_path(&self.repo_root, path) else {
            self.err(format!("Refusing to create “{path}” — unsafe path."));
            return;
        };
        if abs.exists() {
            self.err(format!("{path} already exists."));
            return;
        }
        if let Some(parent) = abs.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                self.err(format!("Couldn't make {}: {e}", parent.display()));
                return;
            }
        }
        if let Err(e) = std::fs::write(&abs, "") {
            self.err(format!("Couldn't create {path}: {e}"));
            return;
        }
        self.add_repo_path(path);
        self.ok(format!("Created {path}."));
        self.spawn_open_external(path.to_string(), None);
        self.rescan_after_file_change();
    }

    fn rename_file(&mut self, from: &str, to: &str) {
        if from == to {
            self.ok("Same name — nothing done.");
            return;
        }
        let (Some(a), Some(b)) = (
            gitops::safe_repo_path(&self.repo_root, from),
            gitops::safe_repo_path(&self.repo_root, to),
        ) else {
            self.err("Refusing to rename — unsafe path.");
            return;
        };
        if b.exists() {
            self.err(format!("{to} already exists."));
            return;
        }
        if let Some(parent) = b.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::rename(&a, &b) {
            self.err(format!("Couldn't rename {from}: {e}"));
            return;
        }
        self.file_tree.remove(from);
        self.add_repo_path(to);
        self.ok(format!("{from} → {to}"));
        self.rebuild_entries();
        self.rescan_after_file_change();
    }

    fn delete_file(&mut self, path: &str) {
        let Some(abs) = gitops::safe_repo_path(&self.repo_root, path) else {
            self.err(format!("Refusing to delete “{path}” — unsafe path."));
            return;
        };
        // Never through a symlink: the link is what should go, not
        // whatever it happens to point at.
        if is_symlink(&abs) {
            self.err(format!(
                "{path} is a symlink — refusing to delete through it."
            ));
            return;
        }
        if let Err(e) = std::fs::remove_file(&abs) {
            self.err(format!("Couldn't delete {path}: {e}"));
            return;
        }
        self.file_tree.remove(path);
        self.ok(format!("Deleted {path}."));
        self.rebuild_entries();
        self.rescan_after_file_change();
    }

    /// Add one path to the repository list and its tree.
    ///
    /// Appended, never inserted, for the same reason a directory read is:
    /// every `RowSrc::Path` on screen holds an index into this list.
    fn add_repo_path(&mut self, path: &str) {
        if self.repo_paths.iter().any(|p| p == path) {
            return;
        }
        let src = RowSrc::Path(self.repo_paths.len());
        self.repo_paths.push(path.to_string());
        // A file somebody just made is one they mean to commit, so it is
        // not drawn as ignored unless git says otherwise on the next read.
        self.repo_ignored.push(false);
        self.file_tree.insert(path, src);
    }

    /// A file appeared or vanished, so the change under review moved too.
    fn rescan_after_file_change(&mut self) {
        if self.local && self.quiet.is_none() {
            self.spawn_quiet_local(false);
        }
    }

    /// Enter in the path box.
    fn confirm_open_path(&mut self) {
        let Overlay::OpenPath(box_) = &self.overlay else {
            return;
        };
        let typed = box_.input.trim().to_string();
        let kind = box_.kind.clone();
        // Every other box needs something typed. A stash name does not:
        // git writes `WIP on <branch>` when you give it none, so an empty
        // box means "you name it" rather than "never mind".
        if let PathBoxKind::StashName { scope } = kind {
            self.overlay = Overlay::None;
            let name = Some(typed).filter(|t| !t.is_empty());
            self.spawn_stash_push(scope, name);
            return;
        }
        if typed.is_empty() {
            self.overlay = Overlay::None;
            self.ok("Nothing typed — nothing done.");
            return;
        }
        match kind {
            PathBoxKind::Open => {}
            // Handled above: it is the one box an empty answer belongs in.
            PathBoxKind::StashName { .. } => return,
            PathBoxKind::NewFile { dir } => {
                self.overlay = Overlay::None;
                let path = if dir.is_empty() {
                    typed
                } else {
                    format!("{dir}/{typed}")
                };
                self.create_file(&path);
                return;
            }
            PathBoxKind::RenameSymbol { .. } => {
                self.overlay = Overlay::None;
                self.spawn_editor_request(EditorRequest::Rename(typed));
                return;
            }
            PathBoxKind::Rename { from } => {
                self.overlay = Overlay::None;
                let to = match from.rsplit_once('/') {
                    Some((dir, _)) if !typed.contains('/') => format!("{dir}/{typed}"),
                    _ => typed,
                };
                self.rename_file(&from, &to);
                return;
            }
        }
        // A path relative to the repository is the other half of what a
        // reader types here; absolute and `~` paths reach the rest of the
        // machine.
        let expanded = pins::expand_home(&typed);
        let candidate = if expanded.is_absolute() {
            expanded
        } else {
            self.repo_root.join(expanded)
        };
        let abs = match std::fs::canonicalize(&candidate) {
            Ok(p) => p,
            Err(e) => {
                self.err(format!("Cannot open {typed}: {e}"));
                return;
            }
        };
        if !abs.is_file() {
            self.err(format!("{typed} is not a file."));
            return;
        }
        self.overlay = Overlay::None;
        let pin = Pin::new(&self.repo_root, abs);
        match self.pins.add(pin) {
            Ok(i) => {
                self.save_pins();
                self.open_pin(i);
            }
            Err(max) => self.err(format!(
                "The tab row holds {max} files — unpin one first (“-”)."
            )),
        }
    }

    pub fn open_editor(&mut self, jump_line: Option<usize>) {
        if let Some(c) = &self.open_commit {
            self.err(format!(
                "{} is a commit — the file on disk is a later version of it.",
                c.short
            ));
            return;
        }
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
        self.open_buffer(editor);
        self.ok("Editing new side — Ctrl+S saves to your working tree and refreshes the diff.");
    }

    /// `q` / the ✕ button: leave, unless there is unsaved work.
    ///
    /// One buffer guarded itself — Esc armed, Esc again discarded. With
    /// several open, `q` can reach past every one of those, so the count
    /// is what answers here rather than the buffer on screen.
    pub fn request_quit(&mut self) {
        let dirty = self.dirty_buffers();
        if dirty == 0 || self.quit_armed {
            self.should_quit = true;
            return;
        }
        self.quit_armed = true;
        self.err(format!(
            "{dirty} unsaved {} — q again to quit and lose {}.",
            if dirty == 1 { "buffer" } else { "buffers" },
            if dirty == 1 { "it" } else { "them" }
        ));
    }

    /// What the tab row says about the open buffers.
    ///
    /// The label rule is the pins' rule (`pins::Pins::labels`): the file
    /// name alone, unless two buffers share one, in which case both get
    /// their parent directory. Two tabs both reading `mod.rs` name
    /// nothing.
    pub fn buffer_tabs(&self) -> Vec<BufferTab> {
        // A pinned file already has a tab, so it gets no second one. The
        // row would otherwise name one file twice — once under the pin
        // the reader chose and once under the tab that happens to hold
        // it — and closing either would leave the other looking wrong.
        let mine: Vec<&String> = self
            .tab_order
            .iter()
            .filter(|p| !self.is_pinned(p))
            .collect();
        let name = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
        // Whatever the pane is showing, whichever of the three it is
        // showing it as. The document `P` opens is a rendering of a
        // parked buffer, so the tab it belongs to is still the one on
        // screen; so is the diff underneath the editor.
        let active = match &self.editor {
            Some(e) => Some(e.path.as_str()),
            None => match &self.preview {
                Some(pv) => Some(pv.path.as_str()),
                None => self.diff_path(),
            },
        };
        let peek = self.peek_path();
        mine.iter()
            .enumerate()
            .map(|(i, path)| {
                let own = name(path);
                let shared = mine
                    .iter()
                    .enumerate()
                    .any(|(j, other)| j != i && name(other) == own);
                let label = if shared {
                    let mut parts = path.rsplitn(3, '/');
                    let base = parts.next().unwrap_or("");
                    match parts.next() {
                        Some(dir) => format!("{dir}/{base}"),
                        None => base.to_string(),
                    }
                } else {
                    own
                };
                BufferTab {
                    path: (*path).clone(),
                    label,
                    // A diff has nothing unsaved in it; only a buffer can
                    // carry the dot.
                    dirty: self.editor_for(path).is_some_and(|e| e.dirty),
                    active: active == Some(path.as_str()),
                    peek: peek == Some(path.as_str()),
                }
            })
            .collect()
    }

    /// The tab holding `path`, when the reader has pinned it.
    ///
    /// Resolved the way `Pin::new` resolves, so a pin made from a dropped
    /// path and a path built from the file tree compare equal. On macOS
    /// `/tmp` alone is a symlink, and one symlink between them is enough
    /// to make one file look like two.
    pub fn pinned_at(&self, path: &str) -> Option<usize> {
        if self.pins.is_empty() {
            return None;
        }
        let abs = self.abs_path(path)?;
        let abs = std::fs::canonicalize(&abs).unwrap_or(abs);
        self.pins.find(&abs)
    }

    fn is_pinned(&self, path: &str) -> bool {
        self.pinned_at(path).is_some()
    }

    /// Unsaved work in the buffer behind pin `idx`, if one is open.
    ///
    /// A pinned file's tab is its *pin* tab, so that is where the mark
    /// saying "this is not on disk yet" has to go. Without this the mark
    /// simply disappeared the moment a file was pinned.
    pub fn pin_dirty(&self, idx: usize) -> bool {
        let Some(pin) = self.pins.items.get(idx) else {
            return false;
        };
        self.buffers().any(|e| e.dirty && e.path == pin.path)
    }

    /// The labels the tab row is drawing, for the tests that check it.
    #[cfg(test)]
    pub fn tab_labels(&self) -> Vec<String> {
        self.buffer_tabs().into_iter().map(|t| t.label).collect()
    }

    /// Bring the nth tab forward, counting the way the row reads.
    ///
    /// A tab is a file, and a file can be open as a buffer, as a diff, or
    /// as both. The buffer wins: it is the one that can hold unsaved
    /// work, so it is the one the reader would be surprised to lose.
    pub fn open_buffer_at(&mut self, i: usize) {
        let Some(path) = self.buffer_tabs().get(i).map(|t| t.path.clone()) else {
            return;
        };
        if self.switch_buffer(&path) {
            self.preview = None;
            self.ok(format!("{path} — {} open.", self.tab_order.len()));
            return;
        }
        if self.switch_diff(&path) {
            // The diff is what this tab holds, so the editor and the
            // document over it stand down.
            self.editor = None;
            self.preview = None;
            self.ok(format!("{path} — {} open.", self.tab_order.len()));
            return;
        }
        // The tab is in the row but its diff is not in hand: a change of
        // [`Layers`] dropped every parked one, because each was read
        // between the wrong two versions. Read this one again.
        if let Some(idx) = self.files.iter().position(|f| f.path == path) {
            self.editor = None;
            self.preview = None;
            self.spawn_load_file(idx);
        }
    }

    /// The ✕ on a tab, and a middle click on one: close that file.
    ///
    /// Unsaved work is asked about, once, per tab. The buffer on screen
    /// goes through the same door `Esc` uses, so one rule covers both.
    /// A parked buffer arms its own `discard_armed`, so arming one tab
    /// never arms another.
    pub fn close_buffer_at(&mut self, i: usize) {
        let Some(path) = self.buffer_tabs().get(i).map(|t| t.path.clone()) else {
            return;
        };
        if self.editor.as_ref().is_some_and(|e| e.path == path) {
            self.request_close_editor();
            return;
        }
        // A tab holding only a diff has nothing to ask about: there is no
        // unsaved work in one, and the file list is still right there.
        if !self.parked.iter().any(|e| e.path == path) {
            if self.close_diff_tab(&path) {
                self.ok(format!(
                    "{path} closed — {} still open.",
                    self.tab_order.len()
                ));
            }
            return;
        }
        let Some(at) = self.parked.iter().position(|e| e.path == path) else {
            return;
        };
        if self.parked[at].dirty && !self.parked[at].discard_armed {
            self.parked[at].discard_armed = true;
            self.err(format!(
                "{path} has unsaved changes — click its ✕ again to throw them away."
            ));
            return;
        }
        self.parked.remove(at);
        // The tab is the file, so its ✕ closes both views of it.
        self.close_diff_tab(&path);
        self.forget_tab(&path);
        // Fire and forget, on a worker: the registry blocks, and the
        // reader is already looking at something else.
        let lsp = self.lsp.clone();
        let root = self.repo_root.clone();
        let closing = path.clone();
        thread::spawn(move || lsp.close(&root, &closing));
        self.ok(format!(
            "{path} closed — {} still open.",
            self.buffers().count()
        ));
    }

    /// Step to the next or previous buffer, wrapping at each end.
    pub fn step_buffer(&mut self, delta: i32) {
        let n = self.buffers().count();
        if n < 2 {
            self.err("Only one file is open.");
            return;
        }
        let at = self
            .buffer_tabs()
            .iter()
            .position(|t| t.active)
            .unwrap_or(0) as i32;
        let next = (at + delta).rem_euclid(n as i32) as usize;
        self.open_buffer_at(next);
    }

    // ----------------------------------------------------- appearance

    /// Whether now is a moment to ask the terminal what its background is.
    ///
    /// The query writes to the terminal and reads the reply straight off
    /// the tty, so a key pressed inside that window would be eaten by it.
    /// The window is a millisecond or two — this makes sure it is a
    /// millisecond or two in which nobody was typing, nothing was
    /// loading, and no overlay was open to type into.
    pub fn wants_appearance_check(&self) -> bool {
        self.appearance_auto
            && self.job.is_none()
            && self.editor.is_none()
            && matches!(self.overlay, Overlay::None)
            && self.last_input.elapsed() >= APPEARANCE_IDLE
            && self.appearance_asked.elapsed() >= APPEARANCE_POLL
    }

    /// The terminal answered. Follow it if it has moved since last time.
    ///
    /// True when something changed and the window needs redrawing.
    pub fn appearance_seen(&mut self, found: Option<Appearance>) -> bool {
        self.appearance_asked = Instant::now();
        let Some(found) = found else { return false };
        if found == crate::theme::appearance() {
            return false;
        }
        self.follow_appearance(found);
        self.ok(format!(
            "☀🌙 The terminal turned {} — {} to match.",
            found.key(),
            highlight::theme_key(highlight::current_theme())
        ));
        true
    }

    /// Move the whole window to `appearance`: the palette, the syntax
    /// theme that goes with it, and everything already drawn under the
    /// old one.
    pub fn follow_appearance(&mut self, appearance: Appearance) {
        crate::theme::set_appearance(appearance);
        highlight::set_theme(self.themes.for_appearance(appearance));
        self.theme_gen = self.theme_gen.wrapping_add(1);
        // The preview caches its rendered lines with the colours they
        // were built from.
        if let Some(pv) = &mut self.preview {
            pv.restyle();
        }
        // The diff on screen was highlighted under the old theme. Quietly,
        // because nobody asked for this — the terminal did.
        if self.diff.is_some() && self.open_commit.is_none() {
            self.spawn_quiet_file(self.file_cursor, true);
        }
    }

    /// Re-highlight a parked diff that was put aside under another theme.
    ///
    /// Done when it comes forward rather than when the theme changed:
    /// re-highlighting every open tab at once would cost the length of a
    /// read per tab, in the middle of an idle moment nobody asked
    /// anything of.
    fn restyle_if_stale(&mut self, tab: &mut DiffTab) {
        if tab.theme_gen == self.theme_gen {
            return;
        }
        tab.theme_gen = self.theme_gen;
        let old_path = tab.path.clone();
        if let Some(text) = tab.old_content.as_deref() {
            tab.old_hl = highlight::highlight(&old_path, text);
        }
        if let Some(text) = tab.new_content.as_deref() {
            tab.new_hl = highlight::highlight(&tab.path, text);
        }
    }

    // ------------------------------------------------------ diff tabs

    /// The path the diff pane is showing, when it is showing one.
    pub fn diff_path(&self) -> Option<&str> {
        self.diff.as_ref()?;
        self.open_file().map(|f| f.path.as_str())
    }

    /// Take the diff off the screen and keep it, so coming back to this
    /// file is a swap rather than a re-read.
    ///
    /// Nothing is parked for a file with no diff loaded, and nothing is
    /// parked twice: a file already in the list is replaced, because the
    /// copy on screen is the newer one.
    fn park_diff(&mut self) {
        let Some(path) = self.diff_path().map(str::to_string) else {
            return;
        };
        let tab = DiffTab {
            path: path.clone(),
            theme_gen: self.theme_gen,
            open_commit: self.open_commit.take(),
            conflict: self.conflict.take(),
            diff: self.diff.take(),
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
        };
        self.parked_diffs.retain(|t| t.path != path);
        self.parked_diffs.push(tab);
        self.display.clear();
    }

    /// Put a parked diff back on screen, exactly as it was left.
    fn restore_diff(&mut self, mut tab: DiffTab) {
        self.restyle_if_stale(&mut tab);
        self.open_commit = tab.open_commit;
        self.conflict = tab.conflict;
        self.diff = tab.diff;
        self.expanded_folds = tab.expanded_folds;
        self.old_content = tab.old_content;
        self.new_content = tab.new_content;
        self.old_hl = tab.old_hl;
        self.new_hl = tab.new_hl;
        self.differs_from_head = tab.differs_from_head;
        self.selection = tab.selection;
        // The file cursor follows the tab, so the panel highlights the
        // row for what is on screen. A re-scan can have dropped the file
        // since; the diff stays, and the cursor simply does not move.
        if let Some(i) = self.files.iter().position(|f| f.path == tab.path) {
            self.file_cursor = i;
        }
        self.rebuild_display();
        // The view mode is shared between tabs, so a stored position can
        // be out of range if it changed while parked — clamp.
        let last = self.display.len().saturating_sub(1);
        self.diff_cursor = tab.diff_cursor.min(last);
        self.diff_scroll = tab.diff_scroll.min(last);
        self.diff_hscroll = tab.diff_hscroll.min(self.max_hscroll());
        self.recompute_matches();
        self.reveal_current_file();
        self.spawn_blame(self.file_cursor);
    }

    /// Close the tab a diff has, and show the neighbour it leaves.
    ///
    /// A diff holds nothing unsaved, so nothing is asked. Closing the one
    /// on screen leaves the pane needing something to show: the tab
    /// beside it, or the file list when that was the last one.
    fn close_diff_tab(&mut self, path: &str) -> bool {
        let was = self.tab_order.iter().position(|p| p == path);
        let on_screen = self.diff_path() == Some(path);
        self.parked_diffs.retain(|t| t.path != path);
        if !on_screen && was.is_none() {
            return false;
        }
        self.forget_tab(path);
        if !on_screen {
            return true;
        }
        // The neighbour in the row, which is the tab that took its place.
        let next = was
            .map(|i| i.min(self.tab_order.len().saturating_sub(1)))
            .and_then(|i| self.tab_order.get(i))
            .cloned();
        self.diff = None;
        self.conflict = None;
        self.display.clear();
        self.selection = None;
        match next {
            Some(p) if self.switch_diff(&p) => {}
            Some(p) if self.switch_buffer(&p) => {}
            _ => self.ok("Nothing open — pick a file on the left."),
        }
        true
    }

    /// Bring a parked diff forward by path. False when this file has none.
    fn switch_diff(&mut self, path: &str) -> bool {
        if self.diff_path() == Some(path) {
            return true;
        }
        let Some(i) = self.parked_diffs.iter().position(|t| t.path == path) else {
            return false;
        };
        self.park_diff();
        let Some(i) = self
            .parked_diffs
            .iter()
            .position(|t| t.path == path)
            .or(Some(i).filter(|i| *i < self.parked_diffs.len()))
        else {
            return false;
        };
        let tab = self.parked_diffs.remove(i);
        self.restore_diff(tab);
        true
    }

    /// Put a path in the tab row, if it is not there already.
    ///
    /// It lands where a closed peek tab left a hole, and at the end
    /// otherwise — which is where a reader looks for the file they just
    /// opened.
    fn note_tab(&mut self, path: &str) {
        if self.tab_order.iter().any(|p| p == path) {
            return;
        }
        match self.tab_hole.take() {
            Some(at) if at <= self.tab_order.len() => self.tab_order.insert(at, path.to_string()),
            _ => self.tab_order.push(path.to_string()),
        }
    }

    /// Take a path out of the tab row.
    fn forget_tab(&mut self, path: &str) {
        self.tab_order.retain(|p| p != path);
        self.tab_hole = None;
    }

    /// The buffer holding `path`, whether it is on screen or parked.
    fn editor_for(&self, path: &str) -> Option<&Editor> {
        self.editor
            .iter()
            .chain(self.parked.iter())
            .find(|e| e.path == path)
    }

    /// Every open buffer, in the order the tab row draws them.
    ///
    /// The row is [`Self::tab_order`] and nothing else, so the buffer on
    /// screen sits where it was opened rather than at the end. It used to
    /// be last, because it lived in its own field and the field came last
    /// — which meant clicking a tab moved that tab to the right, and a
    /// reader clicking along the row watched it reshuffle under the
    /// pointer.
    pub fn buffers(&self) -> impl Iterator<Item = &Editor> {
        self.tab_order.iter().filter_map(|p| self.editor_for(p))
    }

    /// Put the buffer on screen back among the others. Its place in the
    /// row is [`Self::tab_order`]'s business, so this is only storage.
    fn park_active(&mut self) {
        if let Some(cur) = self.editor.take() {
            self.parked.push(cur);
        }
    }

    /// Drag a tab to a new place in the row.
    ///
    /// Both indices count tabs as the row draws them. The buffer on
    /// screen goes back among the others first, so the permutation is one
    /// `remove` and one `insert` on a single list — doing it with the
    /// active buffer held out to the side is where the off-by-ones live.
    pub fn move_buffer(&mut self, from: usize, to: usize) {
        let row = self.buffer_tabs();
        if from == to || from >= row.len() || to >= row.len() {
            return;
        }
        // Both ends by path, not by index. The row is a subsequence of
        // the open files — a pinned file has a tab elsewhere and none
        // here — so a position in the row is not a position in the list.
        let moved = row[from].path.clone();
        let onto = row[to].path.clone();
        let Some(i) = self.tab_order.iter().position(|p| *p == moved) else {
            return;
        };
        let held = self.tab_order.remove(i);
        // Dropped on a tab to the right means after it, and to the left
        // means before it. Anything else moves the tab past the one the
        // reader aimed at.
        let at = self
            .tab_order
            .iter()
            .position(|p| *p == onto)
            .map(|at| at + usize::from(from < to))
            .unwrap_or(self.tab_order.len());
        self.tab_order.insert(at.min(self.tab_order.len()), held);
    }

    /// True when `path` is the file in front of the reader — the buffer
    /// the editor holds, or the document the preview is rendering. The
    /// file tree marks that row, because with no diff behind these rows
    /// there is no file cursor for the panel to follow.
    pub fn buffer_showing(&self, path: &str) -> bool {
        if let Some(ed) = &self.editor {
            return ed.path == path;
        }
        self.preview.as_ref().is_some_and(|pv| pv.path == path)
    }

    /// The peek tab, or `None` when there is not one.
    ///
    /// Derived rather than stored, for the reason `active_pin` is: a
    /// buffer stops being this one through a dozen different doors, and
    /// a stored flag would have to be cleared in every one of them. Two
    /// rules do all of it here.
    ///
    /// The first: a buffer with unsaved work in it is never this tab.
    /// The reader typed into it, so it is theirs, and the next click on
    /// the file tree must not throw it away. `dirty` is set at fourteen
    /// sites in the editor; asking here means none of them had to know.
    ///
    /// The second: a buffer that is closed is not open, so it cannot be
    /// this tab either. `find` answers that without a second field.
    pub fn peek_path(&self) -> Option<&str> {
        let path = self.peek.as_deref()?;
        // A buffer somebody has typed into is theirs, not a peek.
        if let Some(held) = self.editor_for(path) {
            return (!held.dirty).then_some(path);
        }
        // A diff holds nothing unsaved, so it is always replaceable —
        // and it is only a peek while it is still in the row.
        self.tab_order.iter().any(|p| p == path).then_some(path)
    }

    /// Park a language-server request in flight, for the render test,
    /// which must not start a real server to see what the wait looks
    /// like. The channel is never written to, so the job stays pending.
    #[cfg(test)]
    pub fn set_editor_waiting_for_test(&mut self, label: &str) {
        let (tx, rx) = mpsc::channel();
        std::mem::forget(tx);
        self.editor_job = Some(EditorJob {
            rx,
            gen: self.editor_gen,
            started: Instant::now(),
            label: Some(label.to_string()),
        });
    }

    /// Set the peek tab directly, for the render tests, which build
    /// their buffers rather than clicking a tree to get them.
    #[cfg(test)]
    pub fn set_peek_for_test(&mut self, path: &str) {
        self.peek = Some(path.to_string());
    }

    /// Make the peek tab a tab like any other. The double click that
    /// says so lands on either the tab or the file row.
    pub fn keep_peek(&mut self) {
        let Some(path) = self.peek_path().map(str::to_string) else {
            return;
        };
        self.peek = None;
        self.ok(format!("{path} — kept. Esc closes it."));
    }

    /// Close the peek tab, to make room for the next one.
    ///
    /// Called once the file that replaces it is in hand, so the row
    /// gains no tab for a file the reader only looked at — and so the
    /// pane never stands empty waiting for a read. It answers the same
    /// two rules as `peek_path`, so a buffer the reader typed into
    /// survives this untouched.
    fn drop_peek(&mut self, keep: &str) {
        let Some(path) = self.peek_path().map(str::to_string) else {
            return;
        };
        if path == keep {
            return;
        }
        self.peek = None;
        // The language server is holding this file open. Telling it on a
        // worker: the registry blocks, and the reader is already looking
        // at the next file.
        let lsp = self.lsp.clone();
        let root = self.repo_root.clone();
        let closing = path.clone();
        thread::spawn(move || lsp.close(&root, &closing));
        // The place this tab had belongs to the file that replaces it,
        // so walking a tree does not grow the row a tab per click.
        let hole = self.tab_order.iter().position(|p| *p == path);
        if let Some(i) = self.parked.iter().position(|e| e.path == path) {
            self.parked.remove(i);
        } else if self.editor.as_ref().is_some_and(|e| e.path == path) {
            self.editor = None;
        }
        // The same file may also have been open as a diff. The tab is
        // the file, so closing it closes both.
        self.parked_diffs.retain(|t| t.path != path);
        self.forget_tab(&path);
        // The replacement usually has the screen already — see
        // `apply_external`, which opens it first so the pane is never
        // empty — in which case it is at the end of the row and has to be
        // moved back into the tab it took over. When it has not arrived
        // yet, the place is held for it instead.
        match hole {
            Some(at) => self.move_tab_to(keep, at),
            None => self.tab_hole = None,
        }
    }

    /// Put an open tab at `at` in the row, or hold that place for it when
    /// it is not in the row yet.
    fn move_tab_to(&mut self, path: &str, at: usize) {
        let Some(i) = self.tab_order.iter().position(|p| p == path) else {
            self.tab_hole = Some(at);
            return;
        };
        let held = self.tab_order.remove(i);
        self.tab_order.insert(at.min(self.tab_order.len()), held);
    }

    /// How many open buffers hold unsaved work. Every "are you sure"
    /// counts through this rather than looking at the active buffer, which
    /// would have quietly stopped being the whole answer.
    pub fn dirty_buffers(&self) -> usize {
        self.buffers().filter(|e| e.dirty).count()
    }

    /// Park the buffer on screen and put `next` in front of the reader.
    ///
    /// A file already open is switched to rather than read again, so its
    /// cursor, its scroll and its unsaved edits all survive — and it
    /// keeps its place in the row rather than jumping to the end.
    pub fn open_buffer(&mut self, next: Editor) {
        // Re-opening what is already in front: keep the buffer, unsaved
        // edits and all.
        if self.switch_buffer(&next.path) {
            return;
        }
        self.park_active();
        // `note_tab` puts it where a closed peek tab left a hole, and at
        // the end otherwise.
        self.note_tab(&next.path);
        self.editor = Some(next);
        // A file that has just opened has never been linted. Arm the
        // clock so the first pause runs it, rather than waiting for an
        // edit that may never come — a reviewer reads far more files
        // than they type into.
        self.lint_touched = Some(Instant::now());
        self.linted = None;
    }

    /// Bring an open buffer forward by path, leaving the row in the order
    /// the reader put it.
    pub fn switch_buffer(&mut self, path: &str) -> bool {
        if self.editor.as_ref().is_some_and(|e| e.path == path) {
            return true;
        }
        if !self.parked.iter().any(|e| e.path == path) {
            return false;
        }
        self.park_active();
        // Found again after the park: the buffer that was on screen went
        // back into the list, and it may have gone in ahead of this one.
        let Some(i) = self.parked.iter().position(|e| e.path == path) else {
            return false;
        };
        self.editor = Some(self.parked.remove(i));
        true
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
        let closing = self.editor.as_ref().map(|e| e.path.clone());
        if let Some(path) = closing.clone() {
            // Fire and forget, on a worker: the registry blocks, and the
            // reader is already looking at something else.
            let lsp = self.lsp.clone();
            let root = self.repo_root.clone();
            thread::spawn(move || lsp.close(&root, &path));
        }
        self.editor = None;
        // Where the closed tab was, so the tab that takes its place is
        // the one that was beside it.
        let was = closing
            .as_deref()
            .and_then(|p| self.tab_order.iter().position(|q| q == p));
        if let Some(path) = &closing {
            // The same file may still be open as a diff — the editor
            // opened over it — and the tab is the file, so it stays.
            let has_diff = self.diff_path() == Some(path.as_str())
                || self.parked_diffs.iter().any(|t| t.path == *path);
            if !has_diff {
                self.forget_tab(path);
            }
        }
        // Another buffer waiting is what the reader goes back to, not the
        // diff. Closing one file of several should not close them all.
        //
        // The neighbour in the row, not the last file opened: the row is
        // what the reader is looking at, so the tab that takes the closed
        // one's place should be the tab that was beside it.
        if !self.parked.is_empty() {
            let at = was.unwrap_or(0).min(self.tab_order.len().saturating_sub(1));
            let next = self.tab_order.get(at).cloned();
            let take = next
                .and_then(|p| self.parked.iter().position(|e| e.path == p))
                .unwrap_or(0);
            let prev = self.parked.remove(take);
            let path = prev.path.clone();
            let read_only = prev.read_only;
            self.editor = Some(prev);
            self.spawn_blame_external(path.clone(), read_only);
            self.ok(format!("{path} — {} still open.", self.parked.len() + 1));
            return;
        }
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
        // The buffer goes to the row rather than in the way. A file is a
        // file whichever side of the swap opened it, and parking keeps
        // its text, its cursor and the dot that says it is unsaved —
        // there is nothing here to refuse over.
        self.park_active();
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
            merge_op: self.merge_op.take(),
            tracking: self.tracking.take(),
            conflict: self.conflict.take(),
            pr: self.pr.take(),
            checked_out: self.checked_out,
            stack: self.stack.take(),
            merge_base: std::mem::take(&mut self.merge_base),
            stack_base: std::mem::take(&mut self.stack_base),
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
        self.merge_op = ws.merge_op;
        self.tracking = ws.tracking;
        self.conflict = ws.conflict;
        self.pr = ws.pr;
        self.checked_out = ws.checked_out;
        self.set_stack(ws.stack);
        self.merge_base = ws.merge_base;
        self.stack_base = ws.stack_base;
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
        self.rebuild_files();
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
        // Refreshing re-reads the change and the diff behind the editor.
        // The buffer keeps its own text either way, so there is nothing
        // to close first — and a reader waiting on an agent's edits is
        // exactly who needs this while a file is open.
        if self.quiet.is_some() {
            self.ok("Already refreshing…");
            return;
        }
        // A manual refresh restarts the idle clock: the tree was just read.
        self.last_auto_rescan = Instant::now();
        // Asking for a refresh means the whole panel, whichever list is in
        // front of the reader.
        if self.panel == PanelMode::Files && self.repo_job.is_none() {
            self.spawn_repo_listing();
        }
        if self.panel == PanelMode::Commits && self.commits_job.is_none() {
            self.spawn_commits();
        }
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
                let stack = github::pr_stack(&repo, number);
                Ok(QuietOutcome::Pr(Box::new(PrRefreshData {
                    detail,
                    merge_base,
                    files,
                    viewed,
                    stack,
                })))
            })());
        });
        self.quiet = Some(QuietJob {
            rx,
            label: format!("Refreshing PR #{number}"),
            started: Instant::now(),
            auto,
            eager: false,
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
                    merge_op: gitops::merge_op(&root),
                    tracking: gitops::tracking(&root),
                    stack_base: gitops::default_base(&root),
                })))
            })());
        });
        self.quiet = Some(QuietJob {
            rx,
            label: "Rescanning local changes".into(),
            started: Instant::now(),
            auto,
            eager: false,
        });
    }

    /// Reload one file in place — same work as [`Self::spawn_load_file`],
    /// but non-modal, and applied without touching the scroll position.
    fn spawn_quiet_file(&mut self, idx: usize, auto: bool) {
        // The pane is showing a commit, not the change. A commit's diff
        // cannot go stale, and replacing it with a working-tree one would
        // take the reader somewhere they did not ask to go.
        if self.open_commit.is_some() {
            return;
        }
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
            eager: false,
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
            // No staleness check: there is one quiet slot, so a newer
            // click has already replaced any result that went stale.
            QuietOutcome::External(data) => self.apply_external(*data),
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
                // Under a pull request the two are the same commit: the
                // change is already written against the base branch.
                self.stack_base = self.merge_base.clone();
                self.set_stack(d.stack);
                // A refresh that found nothing must leave the pane alone;
                // resetting here would blank it with nothing on the way
                // to fill it back in.
                if changed {
                    self.reset_blame_review();
                }
                self.files = d.files;
                self.viewed = d.viewed;
                self.retarget_file_cursor(cur);
                self.rebuild_files();
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
                self.merge_op = d.merge_op;
                self.tracking = d.tracking;

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
                self.rebuild_files();
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
                self.conflict = d.conflict;
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
                if self.conflict.is_some() {
                    self.clear_blame();
                } else if !same || (self.blame_on && !self.blame_done && self.blame_job.is_none()) {
                    self.spawn_blame(self.file_cursor);
                }
                if !same {
                    self.ok(format!("⟳ {} updated with the latest changes.", d.path));
                } else if !auto {
                    self.ok(format!("✔ {} — up to date.", d.path));
                }
            }
        }
        // The last word belongs to the resolution that started this, not
        // to the re-scan it triggered. Held until the chain is finished:
        // a local re-scan starts a file reload, and only that one lands
        // with nothing left in flight.
        if self.quiet.is_none() && self.job.is_none() {
            if let Some(note) = self.resolved_note.take() {
                self.ok(note);
            }
        }
    }
}

/// Read one file for the editor, off the UI thread.
///
/// The working tree first when it is what is under review, and the
/// commit under review when it is not — in which case the text must
/// never be written back, and `read_only` says so.
fn read_external(
    root: &Path,
    path: &str,
    rev: Option<&str>,
    tree_is_review: bool,
) -> Result<ExternalFile> {
    let Some(abs_path) = gitops::safe_repo_path(root, path) else {
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
            let from_commit = match rev {
                Some(rev) => gitops::show_file(root, rev, path),
                None => gitops::head_oid().and_then(|oid| gitops::show_file(root, &oid, path)),
            };
            match from_commit {
                Some(text) => (text, true),
                None => anyhow::bail!("Cannot read {path}."),
            }
        }
    };
    Ok(ExternalFile {
        preview: false,
        peek: false,
        path: path.to_string(),
        abs_path,
        content,
        line: None,
        read_only,
    })
}

/// Load everything the diff view needs for one file: both sides' content,
/// the computed diff, and syntax highlighting. Runs on a worker thread for
/// both the modal load and the silent refresh.
/// One read of everything local review is built from.
///
/// The three places that change the working tree wholesale — the stash
/// commands — all end with this, so a stash and an ordinary rescan land
/// the app in exactly the same state.
fn scan_local(root: &Path) -> Result<LocalOpenedData> {
    Ok(LocalOpenedData {
        branch: gitops::current_branch(),
        head: gitops::head_oid(),
        files: gitops::local_changes(root)?,
        // Cosmetic: a status read that fails shows everything as
        // unstaged rather than sinking the whole scan.
        stage: gitops::stage_states(root).unwrap_or_default(),
        merge_op: gitops::merge_op(root),
        tracking: gitops::tracking(root),
        stack_base: gitops::default_base(root),
    })
}

/// The two refs a file's diff is read between, and where the new side
/// comes from.
///
/// Three reviews use it: a pull request (base…head, the working tree when
/// it is checked out), local changes (HEAD…the working tree), and one
/// commit out of the Commits panel (its parent…itself).
#[derive(Clone)]
pub struct LoadCtx {
    /// Old side of the diff. Empty means there is none, and every file
    /// reads as added.
    old_ref: String,
    /// New side, when it is a commit rather than the working tree.
    new_ref: String,
    /// Read the new side from disk instead of from `new_ref`.
    from_disk: bool,
    /// Also read the copy at `new_ref`, which is what an inline comment
    /// anchors its line numbers to. Only a pull request has comments.
    anchor: bool,
    /// How many layers of the change to draw. See [`Layers`].
    layers: Layers,
    /// Bottom of the stack: what the branch's whole change is written
    /// against. Empty means there is none to read, and the layers fall
    /// back to the flat diff.
    stack_base: String,
    /// Middle of the stack: the branch as the remote has it. Same.
    pushed_ref: String,
    root: PathBuf,
}

fn load_file_data(idx: usize, file: ChangedFile, ctx: LoadCtx) -> Result<FileLoadedData> {
    let LoadCtx {
        old_ref: merge_base,
        new_ref,
        from_disk,
        anchor,
        layers,
        stack_base,
        pushed_ref,
        root,
    } = ctx;
    // A conflicted file is a different question, so it gets a different
    // answer: our version against their version, with the marker lines
    // taken out. Diffing the working tree against HEAD would instead show
    // the markers themselves as added lines, which says nothing about
    // what the two branches disagree on.
    if file.conflicted {
        if let Some(d) = load_conflict_data(idx, &file, &root) {
            return Ok(d);
        }
    }
    let old = if file.status == "added" || merge_base.is_empty() {
        None
    } else {
        gitops::show_file(&root, &merge_base, file.old_path())
    };
    // The copy at `new_ref` only matters for comment anchoring — local
    // review has no PR to comment on, and a commit's diff is already the
    // copy at that ref, so both skip the lookup.
    let head_content = if !anchor || file.status == "removed" {
        None
    } else {
        gitops::show_file(&root, &new_ref, &file.path)
    };
    let new = if file.status == "removed" {
        None
    } else if from_disk {
        // safe_repo_path rejects API paths that could resolve outside
        // the repository (absolute, `..`, …).
        gitops::safe_repo_path(&root, &file.path)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .or_else(|| head_content.clone())
    } else if new_ref.is_empty() {
        None
    } else {
        // Not on disk and not anchored: a commit, whose new side is the
        // file as that commit left it.
        head_content
            .clone()
            .or_else(|| gitops::show_file(&root, &new_ref, &file.path))
    };
    let differs = if !anchor {
        false
    } else {
        match (&new, &head_content) {
            (Some(a), Some(b)) => a != b,
            (None, None) => false,
            _ => true,
        }
    };
    // With the layers on, the diff reads a third version of the file —
    // the branch as the remote has it — and says of every row which step
    // of the change wrote it. The left column moves with it: the whole
    // stack is drawn against the base branch, "since the push" against
    // the push itself. Anything the repository cannot answer (no
    // upstream, no `origin/HEAD`, a conflicted file) falls back to the
    // flat diff rather than showing a stack with a layer missing.
    let layered =
        (layers.on() && !file.conflicted && !stack_base.is_empty() && !pushed_ref.is_empty()).then(
            || {
                let base = gitops::show_file(&root, &stack_base, file.old_path());
                // A rename the push already carries sits at the new path there;
                // one made since the push is still at the old one.
                let pushed = gitops::show_file(&root, &pushed_ref, &file.path)
                    .or_else(|| gitops::show_file(&root, &pushed_ref, file.old_path()));
                match layers {
                    Layers::Mine => (
                        pushed.clone(),
                        FileDiff::since_push(base.as_deref(), pushed.as_deref(), new.as_deref()),
                    ),
                    _ => (
                        base.clone(),
                        FileDiff::stack(base.as_deref(), pushed.as_deref(), new.as_deref()),
                    ),
                }
            },
        );
    let (old, diff) = match layered {
        Some(pair) => pair,
        None => {
            let d = FileDiff::compute(old.as_deref(), new.as_deref());
            (old, d)
        }
    };
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
        conflict: None,
    })
}

/// Load a conflicted file as the two versions that disagree.
///
/// The old side is our version and the new side is theirs, both with the
/// marker lines removed. Every line the two branches agree on is in both,
/// so it diffs to a context row and folds away; every conflict diffs to a
/// changed section. That makes each conflict a section the reader can
/// already jump between, fold, search, and act on.
///
/// `None` when the file has no markers to read — a delete/modify conflict
/// has none, and neither does a binary file. The caller then falls back to
/// the ordinary diff, and the whole-file resolve still works from the
/// index (see [`gitops::take_side`]).
fn load_conflict_data(idx: usize, file: &ChangedFile, root: &Path) -> Option<FileLoadedData> {
    let text =
        gitops::safe_repo_path(root, &file.path).and_then(|p| std::fs::read_to_string(p).ok())?;
    let parsed = Conflicted::parse(&text)?;
    let (view, ours, theirs) = ConflictView::build(parsed);
    let diff = FileDiff::compute(Some(&ours), Some(&theirs));
    let (old_hl, new_hl) = thread::scope(|s| {
        let left = s.spawn(|| highlight::highlight(&file.path, &ours));
        let right = highlight::highlight(&file.path, &theirs);
        (left.join().expect("highlight thread panicked"), right)
    });
    Some(FileLoadedData {
        idx,
        path: file.path.clone(),
        old: Some(ours),
        new: Some(theirs),
        old_hl,
        new_hl,
        // Nothing to anchor a PR comment against: there is no PR here.
        differs: false,
        diff,
        conflict: Some(view),
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;

    fn cf(path: &str) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            status: "modified".into(),
            additions: 1,
            deletions: 0,
            previous: None,
            conflicted: false,
        }
    }

    /// A conflicted entry for the file panel.
    fn cf_conflicted(path: &str) -> ChangedFile {
        ChangedFile {
            conflicted: true,
            ..cf(path)
        }
    }

    // ---------------------------------------------------- pending review

    /// A PR review of one file, in a real repository, with the diff loaded
    /// — the state `c` and `R` act on.
    fn review_app(dir: &std::path::Path) -> App {
        init_repo(dir);
        let mut app = App::new(LaunchMode::Pr, None);
        app.screen = Screen::Review;
        app.local = false;
        app.checked_out = true;
        app.repo_root = dir.to_path_buf();
        app.repo = Some("acme/tool".into());
        app.pr = Some(pr_detail(7));
        app.files = vec![cf("src/a.rs")];
        app.rebuild_files();
        let old = "one\ntwo\nthree\nfour\n";
        let new = "one\nTWO\nthree\nFOUR\n";
        app.old_content = Some(old.into());
        app.new_content = Some(new.into());
        app.diff = Some(FileDiff::compute(Some(old), Some(new)));
        app.collapse_unchanged = false;
        app.rebuild_display();
        app
    }

    /// Ctrl+S holds a comment instead of posting it, and nothing at all
    /// goes to the network.
    #[test]
    fn ctrl_s_holds_a_comment_for_the_review() {
        let dir = TempDir::new("review-hold");
        let mut app = review_app(&dir.0);
        app.selection = Some(Selection::lines(Side::Right, 2, 2));
        app.handle_key(key(KeyCode::Char('c')));
        assert!(matches!(app.overlay, Overlay::Comment(_)));

        type_into_comment(&mut app, "this needs a name");
        app.handle_key(ctrl(KeyCode::Char('s')));

        assert!(
            matches!(app.overlay, Overlay::None),
            "the draft is put away"
        );
        assert!(app.job.is_none(), "nothing was posted");
        assert_eq!(app.pending.len(), 1);
        let c = &app.pending[0];
        assert_eq!(c.path, "src/a.rs");
        assert_eq!(c.line, 2);
        assert_eq!(c.start_line, None, "one line carries no range");
        assert_eq!(c.side, CommentSide::Right);
        assert_eq!(c.body, "this needs a name");
        assert!(app.status.contains("Review started"), "{}", app.status);
        assert!(
            app.status.contains("Nothing is on GitHub"),
            "{}",
            app.status
        );
    }

    /// A multi-line selection keeps both ends.
    #[test]
    fn a_range_comment_keeps_its_first_line() {
        let dir = TempDir::new("review-range");
        let mut app = review_app(&dir.0);
        app.selection = Some(Selection::lines(Side::Right, 2, 4));
        app.handle_key(key(KeyCode::Char('c')));
        type_into_comment(&mut app, "all of this");
        app.handle_key(ctrl(KeyCode::Char('s')));
        let c = &app.pending[0];
        assert_eq!((c.start_line, c.line), (Some(2), 4));
        assert_eq!(c.where_at(), "src/a.rs:2–4");
    }

    /// Held comments outlive the process: they are written under the git
    /// directory as they are made, and read back when the PR reopens.
    #[test]
    fn held_comments_survive_a_restart() {
        let dir = TempDir::new("review-persist");
        let mut app = review_app(&dir.0);
        app.selection = Some(Selection::lines(Side::Right, 2, 2));
        app.handle_key(key(KeyCode::Char('c')));
        type_into_comment(&mut app, "held");
        app.handle_key(ctrl(KeyCode::Char('s')));
        app.focus_review();
        app.handle_key(key(KeyCode::Char('L')));
        app.handle_key(key(KeyCode::Char('G')));
        app.handle_key(key(KeyCode::Char('T')));
        app.set_verdict(Verdict::RequestChanges);

        // A second run of loupe, opening the same pull request.
        let mut fresh = review_app(&dir.0);
        assert!(fresh.pending.is_empty(), "nothing until it is read");
        fresh.load_pending();
        assert_eq!(fresh.pending.len(), 1);
        assert_eq!(fresh.pending[0].body, "held");
        assert_eq!(fresh.review.body(), "LGT");
        assert_eq!(fresh.review.verdict, Verdict::RequestChanges);
        assert!(
            fresh
                .auto_open_note
                .as_deref()
                .is_some_and(|n| n.contains("1 held comment")),
            "{:?}",
            fresh.auto_open_note
        );

        // Discarding removes the file rather than leaving an empty one.
        // It asks once first, so this is two presses.
        fresh.discard_pending();
        fresh.discard_pending();
        assert!(fresh.pending.is_empty());
        fresh.review.clear();
        fresh.save_pending();
        let path = fresh.pending_path(7).expect("a path under .git");
        assert!(!path.exists(), "nothing held, nothing left behind");
    }

    /// The 💬 in the change bar sits on the lines a held comment covers,
    /// and nowhere else.
    #[test]
    fn held_comments_mark_their_own_lines() {
        let dir = TempDir::new("review-marks");
        let mut app = review_app(&dir.0);
        app.selection = Some(Selection::lines(Side::Right, 2, 2));
        app.handle_key(key(KeyCode::Char('c')));
        type_into_comment(&mut app, "here");
        app.handle_key(ctrl(KeyCode::Char('s')));

        let marked: Vec<usize> = (0..app.display.len())
            .filter(|i| app.pending_on_row(*i))
            .collect();
        assert_eq!(marked.len(), 1, "one row, not the whole file");
        // …and it is the row showing new-side line 2.
        let row = &app.diff.as_ref().unwrap().rows[marked[0]];
        assert_eq!(row.new_ln, Some(2));

        // A comment on the old side does not mark the new side's rows.
        app.pending[0].side = CommentSide::Left;
        app.pending[0].line = 4;
        app.view = ViewMode::Inline;
        app.rebuild_display();
        let marked: Vec<usize> = (0..app.display.len())
            .filter(|i| app.pending_on_row(*i))
            .collect();
        assert_eq!(marked.len(), 1);
        let e = app.diff.as_ref().unwrap().inline[match app.display[marked[0]] {
            DisplayEntry::Line(i) => i,
            _ => panic!("a line"),
        }];
        assert_eq!(e.side, Side::Left, "the old side's row, not the new one's");

        // A comment on another file marks nothing here.
        app.pending[0].path = "src/other.rs".into();
        assert!(!(0..app.display.len()).any(|i| app.pending_on_row(i)));
    }

    /// The submit prompt lists exactly what is about to be sent, and the
    /// two cases GitHub would refuse are caught before it is.
    #[test]
    fn submitting_asks_first_and_says_what_goes() {
        let dir = TempDir::new("review-submit");
        let mut app = review_app(&dir.0);

        // Nothing written and nothing held: there is no review to send.
        app.ask_submit_review();
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.status_err);
        assert!(app.status.contains("Nothing to send"), "{}", app.status);

        // "Request changes" with no summary is refused before GitHub does.
        app.set_verdict(Verdict::RequestChanges);
        app.pending.push(ReviewComment {
            path: "src/a.rs".into(),
            side: CommentSide::Right,
            line: 2,
            start_line: None,
            body: "rename this".into(),
        });
        app.ask_submit_review();
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.status.contains("needs a summary"), "{}", app.status);

        // With a summary it asks, and the prompt carries the whole review.
        app.focus_review();
        for c in "please fix".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(ctrl(KeyCode::Char('s')));
        let Overlay::ReviewConfirm(prompt) = &app.overlay else {
            panic!("the confirm prompt is open, got {}", app.status);
        };
        assert_eq!(prompt.number, 7);
        assert_eq!(prompt.verdict, Verdict::RequestChanges);
        assert_eq!(prompt.body, "please fix");
        assert_eq!(prompt.comments.len(), 1);
        assert!(!prompt.stale);
        assert!(app.job.is_none(), "nothing is sent until it is confirmed");

        // Esc keeps everything held rather than throwing it away.
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(app.pending.len(), 1);
        assert!(app.status.contains("still held"), "{}", app.status);
    }

    /// A head that moved under the held comments is called out, because
    /// GitHub refuses the whole review over one bad anchor.
    #[test]
    fn a_moved_head_is_flagged_before_sending() {
        let dir = TempDir::new("review-stale");
        let mut app = review_app(&dir.0);
        app.selection = Some(Selection::lines(Side::Right, 2, 2));
        app.handle_key(key(KeyCode::Char('c')));
        type_into_comment(&mut app, "note");
        app.handle_key(ctrl(KeyCode::Char('s')));

        // The PR head moves — a push landing under an open review.
        if let Some(pr) = &mut app.pr {
            pr.head_ref_oid = "f".repeat(40);
        }
        app.ask_submit_review();
        let Overlay::ReviewConfirm(prompt) = &app.overlay else {
            panic!("the confirm prompt is open");
        };
        assert!(prompt.stale, "the prompt warns rather than letting it fail");
    }

    /// A submitted review leaves nothing behind.
    #[test]
    fn a_sent_review_clears_what_was_held() {
        let dir = TempDir::new("review-clear");
        let mut app = review_app(&dir.0);
        app.pending.push(ReviewComment {
            path: "src/a.rs".into(),
            side: CommentSide::Right,
            line: 2,
            start_line: None,
            body: "x".into(),
        });
        app.save_pending();
        assert!(app.pending_path(7).expect("path").exists());

        app.apply(Outcome::ReviewSubmitted {
            verdict: Verdict::Approve,
            count: 1,
        });
        assert!(app.pending.is_empty());
        assert!(app.review.is_empty());
        assert!(!app.review.focused);
        assert!(!app.pending_path(7).expect("path").exists());
        assert!(app.status.contains("approved PR #7"), "{}", app.status);
        assert!(app.status.contains("1 inline comment"), "{}", app.status);
    }

    /// Discarding written work asks once first.
    #[test]
    fn discarding_held_comments_asks_first() {
        let dir = TempDir::new("review-discard");
        let mut app = review_app(&dir.0);
        app.pending.push(ReviewComment {
            path: "src/a.rs".into(),
            side: CommentSide::Right,
            line: 2,
            start_line: None,
            body: "keep me".into(),
        });
        app.save_pending();

        app.activate(ButtonId::ReviewDiscard);
        assert_eq!(app.pending.len(), 1, "the first press only asks");
        assert!(
            app.status.contains("Discard 1 held comment?"),
            "{}",
            app.status
        );

        // Doing something else in between means the reader moved on.
        app.activate(ButtonId::ViewToggle);
        app.activate(ButtonId::ReviewDiscard);
        assert_eq!(app.pending.len(), 1, "it asks again rather than acting");

        app.activate(ButtonId::ReviewDiscard);
        assert!(app.pending.is_empty());
        assert!(
            app.status.contains("nothing was ever sent to GitHub"),
            "{}",
            app.status
        );
    }

    /// Tab walks the three verdicts and comes back round.
    #[test]
    fn tab_cycles_the_verdict() {
        let dir = TempDir::new("review-verdict");
        let mut app = review_app(&dir.0);
        app.focus_review();
        assert_eq!(app.review.verdict, Verdict::Comment);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.review.verdict, Verdict::Approve);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.review.verdict, Verdict::RequestChanges);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.review.verdict, Verdict::Comment);
        // Esc gives the keyboard back to the diff.
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.review.focused);
        // …and `j` moves the diff cursor again rather than typing a "j".
        let before = app.diff_cursor;
        app.handle_key(key(KeyCode::Char('j')));
        assert_ne!(app.diff_cursor, before);
        assert!(app.review.is_empty(), "nothing was typed into the box");
    }

    /// The review box belongs to a pull request, and says so elsewhere.
    #[test]
    fn the_review_box_is_pr_only() {
        let dir = TempDir::new("review-local");
        let mut app = review_app(&dir.0);
        assert!(app.review_box_on());
        app.local = true;
        app.pr = None;
        assert!(!app.review_box_on());
        app.focus_review();
        assert!(app.status_err);
        assert!(
            app.status.contains("No pull request open"),
            "{}",
            app.status
        );
    }
    // -------------------------------------------------------- conflicts

    const CONFLICT_TEXT: &str = "\
keep-one
<<<<<<< HEAD
ours-line
=======
theirs-line
>>>>>>> feature
keep-two
keep-three
<<<<<<< HEAD
ours-two
=======
theirs-two
>>>>>>> feature
keep-four
";

    /// A repository in `dir`, so the staging half of a resolution has an
    /// index to write to.
    fn init_repo(dir: &std::path::Path) {
        let d = dir.to_string_lossy().into_owned();
        for args in [
            vec!["-C", d.as_str(), "init", "-q", "-b", "main", "."],
            vec!["-C", d.as_str(), "config", "user.email", "loupe@test"],
            vec!["-C", d.as_str(), "config", "user.name", "loupe"],
        ] {
            gitops::run_git(&args).unwrap();
        }
    }

    /// A local review of one conflicted file, loaded the way the real load
    /// job loads it.
    fn conflict_app(dir: &std::path::Path) -> App {
        init_repo(dir);
        std::fs::write(dir.join("merge.txt"), CONFLICT_TEXT).unwrap();
        let mut app = App::new(LaunchMode::Local, None);
        app.local = true;
        app.checked_out = true;
        app.screen = Screen::Review;
        app.repo_root = dir.to_path_buf();
        app.files = vec![cf_conflicted("merge.txt")];
        app.rebuild_files();
        let d = load_conflict_data(0, &app.files[0], dir).expect("the markers parse");
        app.apply(Outcome::FileLoaded(Box::new(d)));
        app
    }

    #[test]
    fn conflicts_are_listed_first_under_their_own_heading() {
        let mut app = App::new(LaunchMode::Local, None);
        app.local = true;
        // As `local_changes` returns them: conflicts first, then by path.
        app.files = vec![cf_conflicted("src/z.rs"), cf("a.rs"), cf("src/deep/b.rs")];
        for tree in [true, false] {
            app.tree_view = tree;
            app.rebuild_files();
            assert_eq!(
                app.entries[0],
                FileEntry::ConflictHeading { count: 1 },
                "tree_view = {tree}"
            );
            assert_eq!(
                app.entries[1],
                FileEntry::File {
                    src: RowSrc::Changed(0),
                    depth: 0
                },
                "the conflict sits under the heading, tree_view = {tree}"
            );
            // …and it is not repeated further down.
            let repeats = app.entries[2..]
                .iter()
                .filter(|e| {
                    matches!(
                        e,
                        FileEntry::File {
                            src: RowSrc::Changed(0),
                            ..
                        }
                    )
                })
                .count();
            assert_eq!(repeats, 0, "no second copy, tree_view = {tree}");
            // The other two files are still all there.
            let files = app
                .entries
                .iter()
                .filter(|e| matches!(e, FileEntry::File { .. }))
                .count();
            assert_eq!(files, 3, "tree_view = {tree}");
        }
        // With nothing conflicted the heading is gone entirely.
        app.files = vec![cf("a.rs")];
        app.rebuild_files();
        assert!(!app
            .entries
            .iter()
            .any(|e| matches!(e, FileEntry::ConflictHeading { .. })));
    }

    /// The diff of a conflicted file is our version against theirs, with
    /// one changed section per conflict and the markers nowhere on screen.
    #[test]
    fn a_conflict_shows_as_our_version_against_theirs() {
        let dir = TempDir::new("conflict-view");
        let app = conflict_app(&dir.0);

        let view = app.conflict.as_ref().expect("the file is a conflict view");
        assert_eq!(view.file.len(), 2);
        assert_eq!(view.file.labels(), ("HEAD", "feature"));

        let old = app.old_content.as_deref().expect("our side");
        let new = app.new_content.as_deref().expect("their side");
        assert_eq!(
            old,
            "keep-one\nours-line\nkeep-two\nkeep-three\nours-two\nkeep-four\n"
        );
        assert_eq!(
            new,
            "keep-one\ntheirs-line\nkeep-two\nkeep-three\ntheirs-two\nkeep-four\n"
        );
        for text in [old, new] {
            assert!(!text.contains("<<<<<<<"), "markers never reach the pane");
            assert!(!text.contains("======="));
        }

        // Two conflicts, so two changed sections in the row model.
        let diff = app.diff.as_ref().expect("a diff");
        assert_eq!(diff.sections().len(), 2);
        // …and the blame pane stands down: its line numbers would be wrong.
        assert!(app.blame_new.is_none() && app.blame_old.is_none());
    }

    /// The change bar marks each conflict, and only the rows inside one.
    #[test]
    fn the_change_bar_marks_each_conflict() {
        let dir = TempDir::new("conflict-bar");
        let mut app = conflict_app(&dir.0);
        app.collapse_unchanged = false;
        app.rebuild_display();

        let bars: Vec<Option<bool>> = (0..app.display.len())
            .map(|i| app.conflict_bar(i))
            .collect();
        let firsts: Vec<usize> = bars
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == Some(true))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(firsts.len(), 2, "one ⚑ per conflict: {bars:?}");
        // The agreed lines carry nothing at all.
        assert_eq!(app.conflict_bar(0), None, "keep-one is agreed");
        // And each ⚑ row names the conflict it opens.
        assert_eq!(app.conflict_on_row(firsts[0]), Some(0));
        assert_eq!(app.conflict_on_row(firsts[1]), Some(1));
    }

    /// The whole flow: put the cursor on a conflict, take one side, and
    /// see it written to disk with the other conflict left alone.
    #[test]
    fn taking_a_side_rewrites_only_that_conflict() {
        let dir = TempDir::new("conflict-resolve");
        let mut app = conflict_app(&dir.0);
        app.collapse_unchanged = false;
        app.rebuild_display();
        let first = (0..app.display.len())
            .find(|i| app.conflict_bar(*i) == Some(true))
            .expect("a ⚑ row");
        app.diff_cursor = first;

        // `o` opens the menu for the conflict under the cursor.
        app.handle_key(key(KeyCode::Char('o')));
        let Overlay::ConflictMenu(menu) = &app.overlay else {
            panic!("the resolve menu is open");
        };
        assert_eq!(menu.hunk, Some(0));
        assert_eq!(menu.title, "Conflict 1 of 2");
        // Ours, theirs, both, take-all ×2, edit, mark resolved. No
        // ancestor line: this file was written in the plain marker style.
        let keys: Vec<char> = menu.items.iter().map(|it| it.key).collect();
        assert_eq!(keys, vec!['o', 't', 'b', 'O', 'T', 'e', 'x']);

        // `t` keeps their side of this one conflict.
        app.handle_key(key(KeyCode::Char('t')));
        let on_disk = std::fs::read_to_string(dir.0.join("merge.txt")).unwrap();
        assert_eq!(
            on_disk,
            "keep-one\ntheirs-line\nkeep-two\nkeep-three\n\
             <<<<<<< HEAD\nours-two\n=======\ntheirs-two\n>>>>>>> feature\nkeep-four\n",
            "the second conflict keeps its markers"
        );
        assert!(app.status.contains("1 left"), "{}", app.status);
        assert!(!app.status_err, "{}", app.status);
    }

    /// Taking one side everywhere clears the file in one go.
    #[test]
    fn taking_one_side_everywhere_clears_the_file() {
        let dir = TempDir::new("conflict-all");
        let mut app = conflict_app(&dir.0);
        app.resolve_all("merge.txt", Resolution::Ours);
        assert_eq!(
            std::fs::read_to_string(dir.0.join("merge.txt")).unwrap(),
            "keep-one\nours-line\nkeep-two\nkeep-three\nours-two\nkeep-four\n"
        );
        // Nothing is left to resolve, so the file is staged with it —
        // git treats a path as conflicted until it is added.
        assert!(!app.status_err, "{}", app.status);
        assert!(app.status.contains("staged it"), "{}", app.status);
        assert_eq!(
            gitops::stage_states(&dir.0).unwrap().get("merge.txt"),
            Some(&StageState::Staged)
        );
    }

    /// A conflicted file must not be reverted: `git checkout` against HEAD
    /// mid-merge throws the merge away for that path.
    #[test]
    fn a_conflicted_file_refuses_to_be_reverted() {
        let dir = TempDir::new("conflict-revert");
        let mut app = conflict_app(&dir.0);
        app.handle_key(key(KeyCode::Char('U')));
        assert!(matches!(app.overlay, Overlay::None), "no revert prompt");
        assert!(app.status_err);
        assert!(app.status.contains("merge conflict"), "{}", app.status);

        // `u` on a conflict view offers the resolve menu instead.
        app.status.clear();
        app.handle_key(key(KeyCode::Char('u')));
        assert!(matches!(app.overlay, Overlay::ConflictMenu(_)));
    }

    /// Away from any one conflict, the menu offers only the lines that
    /// settle the whole file.
    #[test]
    fn the_whole_file_menu_leaves_out_the_single_conflict_lines() {
        let dir = TempDir::new("conflict-whole");
        let mut app = conflict_app(&dir.0);
        app.open_conflict_menu(usize::MAX, 0, 0);
        let Overlay::ConflictMenu(menu) = &app.overlay else {
            panic!("the menu is open");
        };
        assert_eq!(menu.hunk, None);
        assert!(menu.title.contains("whole file"), "{}", menu.title);
        let keys: Vec<char> = menu.items.iter().map(|it| it.key).collect();
        assert_eq!(keys, vec!['O', 'T', 'e', 'x']);
    }

    /// A conflict git could not write markers for — one side deleted the
    /// file — still opens, still warns, and still offers the whole-file
    /// lines that read the index instead.
    #[test]
    fn a_conflict_with_no_markers_still_offers_a_way_out() {
        let dir = TempDir::new("conflict-nomarkers");
        init_repo(&dir.0);
        std::fs::write(dir.0.join("plain.txt"), "no markers here\n").unwrap();
        let file = cf_conflicted("plain.txt");
        assert!(
            load_conflict_data(0, &file, &dir.0).is_none(),
            "nothing to parse, so the ordinary diff is used"
        );

        let mut app = App::new(LaunchMode::Local, None);
        app.local = true;
        app.checked_out = true;
        app.screen = Screen::Review;
        app.repo_root = dir.0.clone();
        app.files = vec![file];
        app.rebuild_files();
        app.open_conflict_menu(0, 0, 0);
        let Overlay::ConflictMenu(menu) = &app.overlay else {
            panic!("the menu is open");
        };
        let keys: Vec<char> = menu.items.iter().map(|it| it.key).collect();
        assert_eq!(keys, vec!['o', 't', 'e', 'x']);
        assert!(menu
            .items
            .iter()
            .any(|it| it.act == ConflictAction::TakeSide { ours: true }));
    }

    /// Two conflicts with no agreed line between them collapse into one
    /// diff section. The row-to-conflict map does not come from the
    /// sections, so both are still marked and both are still resolvable.
    #[test]
    fn back_to_back_conflicts_stay_separate() {
        let dir = TempDir::new("conflict-adjacent");
        init_repo(&dir.0);
        let text = "\
<<<<<<< HEAD
a1
=======
b1
>>>>>>> feature
<<<<<<< HEAD
a2
=======
b2
>>>>>>> feature
";
        std::fs::write(dir.0.join("merge.txt"), text).unwrap();
        let mut app = App::new(LaunchMode::Local, None);
        app.local = true;
        app.checked_out = true;
        app.screen = Screen::Review;
        app.repo_root = dir.0.clone();
        app.files = vec![cf_conflicted("merge.txt")];
        app.rebuild_files();
        let d = load_conflict_data(0, &app.files[0], &dir.0).expect("markers");
        app.apply(Outcome::FileLoaded(Box::new(d)));
        app.collapse_unchanged = false;
        app.rebuild_display();

        // One section — there is no context row to split them.
        assert_eq!(app.diff.as_ref().unwrap().sections().len(), 1);
        // …but two conflicts, and every row belongs to one of them.
        let owners: Vec<Option<usize>> = (0..app.display.len())
            .map(|i| app.conflict_on_row(i))
            .collect();
        assert!(
            owners.contains(&Some(0)) && owners.contains(&Some(1)),
            "{owners:?}"
        );

        // Resolving the second leaves the first exactly as it was.
        app.resolve_hunk("merge.txt", Some(1), Resolution::Theirs);
        assert_eq!(
            std::fs::read_to_string(dir.0.join("merge.txt")).unwrap(),
            "<<<<<<< HEAD\na1\n=======\nb1\n>>>>>>> feature\nb2\n"
        );
    }

    /// A hand edit rewrites the very text the conflict view was parsed
    /// from. Keeping the old parse would let the next resolution write the
    /// file back the way it was before the edit, silently losing it.
    #[test]
    fn saving_a_hand_edit_throws_the_stale_parse_away() {
        let dir = TempDir::new("conflict-save");
        let mut app = conflict_app(&dir.0);
        assert!(app.conflict.is_some());

        // What `spawn_save_editor` lands with once the buffer is written.
        let edited = CONFLICT_TEXT.replace("keep-one", "keep-one-edited");
        std::fs::write(dir.0.join("merge.txt"), &edited).unwrap();
        app.apply(Outcome::EditorSaved(Box::new(EditorSavedData {
            path: "merge.txt".into(),
            content: edited.clone(),
            differs: false,
            diff: FileDiff::compute(app.old_content.as_deref(), Some(&edited)),
            new_hl: Vec::new(),
        })));
        assert!(
            app.conflict.is_none(),
            "the parse is dropped until the file is read again"
        );
        assert!(app.status.contains("re-reading"), "{}", app.status);

        // Reading it again picks the edit up, still as a conflict view.
        let d = load_conflict_data(0, &app.files[0], &dir.0).expect("markers survive the edit");
        app.apply(Outcome::FileLoaded(Box::new(d)));
        let view = app.conflict.as_ref().expect("a fresh parse");
        assert_eq!(view.file.len(), 2);
        assert!(app
            .old_content
            .as_deref()
            .unwrap()
            .starts_with("keep-one-edited"));
    }

    /// A conflict settled by hand: no markers left, so `x` stages it.
    #[test]
    fn marking_resolved_needs_the_markers_gone() {
        let dir = TempDir::new("conflict-mark");
        let mut app = conflict_app(&dir.0);

        // Markers still there: it refuses rather than staging a broken file.
        app.mark_resolved("merge.txt");
        assert!(app.status_err);
        assert!(
            app.status.contains("still holds conflict markers"),
            "{}",
            app.status
        );
        assert_eq!(
            gitops::stage_states(&dir.0).unwrap().get("merge.txt"),
            Some(&StageState::Unstaged),
            "nothing reached the index"
        );

        // Resolved by hand, then marked.
        std::fs::write(dir.0.join("merge.txt"), "settled by hand\n").unwrap();
        app.mark_resolved("merge.txt");
        assert!(!app.status_err, "{}", app.status);
        assert!(
            app.status.contains("Marked merge.txt resolved"),
            "{}",
            app.status
        );
        assert_eq!(
            gitops::stage_states(&dir.0).unwrap().get("merge.txt"),
            Some(&StageState::Staged)
        );
    }

    /// The ancestor line only appears when git wrote an ancestor.
    #[test]
    fn the_ancestor_line_appears_only_in_the_diff3_style() {
        let dir = TempDir::new("conflict-diff3");
        init_repo(&dir.0);
        std::fs::write(
            dir.0.join("merge.txt"),
            "<<<<<<< HEAD\nours\n||||||| base\nwas\n=======\ntheirs\n>>>>>>> feature\n",
        )
        .unwrap();
        let mut app = App::new(LaunchMode::Local, None);
        app.local = true;
        app.checked_out = true;
        app.screen = Screen::Review;
        app.repo_root = dir.0.clone();
        app.files = vec![cf_conflicted("merge.txt")];
        app.rebuild_files();
        let d = load_conflict_data(0, &app.files[0], &dir.0).expect("markers");
        app.apply(Outcome::FileLoaded(Box::new(d)));
        app.collapse_unchanged = false;
        app.rebuild_display();
        let first = (0..app.display.len())
            .find(|i| app.conflict_bar(*i) == Some(true))
            .expect("a ⚑ row");
        app.diff_cursor = first;
        app.handle_key(key(KeyCode::Char('o')));
        let Overlay::ConflictMenu(menu) = &app.overlay else {
            panic!("the menu is open");
        };
        assert!(menu
            .items
            .iter()
            .any(|it| it.act == ConflictAction::Take(Resolution::Base)));
        // Taking it writes the ancestor's line.
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(
            std::fs::read_to_string(dir.0.join("merge.txt")).unwrap(),
            "was\n"
        );
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
            conflicted: false,
        }];
        app.rebuild_files();
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

    // ---------------------------------------------------- pinned files

    /// The whole point of the feature: a markdown file from somewhere
    /// else on the machine, dropped on the window, read without ever
    /// being copied into the repository.
    #[test]
    fn a_dropped_file_from_outside_the_repo_pins_and_renders() {
        let dir = TempDir::new("drop-outside");
        let mut app = md_review_app(&dir.0, "# Plan\n");
        // Somewhere the repository root cannot reach.
        let away = std::env::temp_dir().join("loupe-pins-elsewhere");
        std::fs::create_dir_all(&away).unwrap();
        let doc = away.join("agent-notes.md");
        std::fs::write(&doc, "# Notes\n\nfrom the agent\n").unwrap();

        app.handle_paste(doc.to_string_lossy().into_owned());

        assert_eq!(app.pins.len(), 1, "the drop made a tab");
        assert!(app.pins.items[0].outside, "and marked it as outside");
        let pv = app.preview.as_ref().expect("it renders the document");
        assert!(pv.src.contains("from the agent"));
        assert!(pv.standalone, "there is no diff behind it");
        assert_eq!(app.active_pin(), Some(0), "the tab is the one on screen");
        std::fs::remove_dir_all(&away).ok();
    }

    /// A local review of one source file with its diff already in hand —
    /// the state a tab for a changed file has to come back to.
    fn diff_review_app(dir: &std::path::Path) -> App {
        const SRC: &str = "fn main() {\n    println!(\"hi\");\n}\n";
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.checked_out = true;
        app.repo_root = dir.to_path_buf();
        app.files = vec![ChangedFile {
            path: "src/main.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 0,
            previous: None,
            conflicted: false,
        }];
        app.rebuild_files();
        std::fs::create_dir_all(dir.join("src")).expect("scratch src");
        std::fs::write(dir.join("src/main.rs"), SRC).expect("fixture written");
        app.new_content = Some(SRC.to_string());
        app.diff = Some(FileDiff::compute(Some(SRC), Some(SRC)));
        app.rebuild_display();
        app
    }

    /// One markdown file somewhere else on the machine, to read and come
    /// back from.
    fn outside_doc(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("loupe-pins-away-{name}"));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let doc = dir.join(format!("{name}.md"));
        std::fs::write(&doc, body).expect("fixture written");
        doc
    }

    /// Pin a changed file, go and read a document, then come back to the
    /// diff by its tab. The tab used to do nothing at all: the file was
    /// already under the cursor with its diff loaded, so loupe decided
    /// there was nothing to do — and left the document covering it.
    #[test]
    fn a_tab_for_a_changed_file_comes_back_to_its_diff() {
        let dir = TempDir::new("pin-diff-back");
        let mut app = diff_review_app(&dir.0);
        app.handle_key(key(KeyCode::Char('=')));
        assert_eq!(app.pins.len(), 1, "the source file is pinned");

        let doc = outside_doc("notes", "# Notes\n\nsomething else\n");
        app.handle_paste(doc.to_string_lossy().into_owned());
        assert!(app.preview.is_some(), "the document is on screen");
        assert_eq!(app.active_pin(), Some(1), "and its tab is the live one");

        app.handle_key(key(KeyCode::Char('1')));

        assert!(app.preview.is_none(), "the document made way for the diff");
        assert!(app.diff.is_some(), "which is still loaded");
        assert_eq!(app.active_pin(), Some(0), "and tab 1 is the live one");
        std::fs::remove_dir_all(doc.parent().unwrap()).ok();
    }

    /// The same round trip with the mouse, since that is how the row is
    /// meant to be used.
    #[test]
    fn clicking_a_tab_for_a_changed_file_works_too() {
        let dir = TempDir::new("pin-diff-click");
        let mut app = diff_review_app(&dir.0);
        app.handle_key(key(KeyCode::Char('=')));
        let doc = outside_doc("clicked", "# Clicked\n");
        app.handle_paste(doc.to_string_lossy().into_owned());
        assert!(app.preview.is_some());

        // The row records a click target per tab; use the one for tab 1.
        assert!(app.activate(ButtonId::PinTab(0)));

        assert!(app.preview.is_none(), "back to the diff");
        assert_eq!(app.active_pin(), Some(0));
        std::fs::remove_dir_all(doc.parent().unwrap()).ok();
    }

    /// A markdown file *in the changeset* has the same problem in
    /// reverse: its tab has to show its own document, not whichever one
    /// happens to be open.
    #[test]
    fn a_tab_for_a_changed_markdown_file_shows_that_file() {
        let dir = TempDir::new("pin-md-swap");
        let mut app = md_review_app(&dir.0, "# The plan\n\nin the changeset\n");
        app.diff = Some(FileDiff::compute(
            Some("# The plan\n"),
            Some("# The plan\n"),
        ));
        app.rebuild_display();
        app.handle_key(key(KeyCode::Char('=')));
        assert_eq!(app.pins.len(), 1);

        let doc = outside_doc("other", "# Other\n\nnot the plan\n");
        app.handle_paste(doc.to_string_lossy().into_owned());
        let pv = app.preview.as_ref().expect("the other document");
        assert!(pv.src.contains("not the plan"));

        app.handle_key(key(KeyCode::Char('1')));

        let pv = app.preview.as_ref().expect("still a document");
        assert!(
            pv.src.contains("in the changeset"),
            "but the one this tab is for: {}",
            pv.src
        );
        assert_eq!(app.active_pin(), Some(0));
        std::fs::remove_dir_all(doc.parent().unwrap()).ok();
    }

    /// Every character of `text` as its own key event — what a terminal
    /// that does not use bracketed paste sends when a file is dropped on
    /// it. Warp does this; Ghostty sends one paste instead.
    fn typed(text: &str) -> Vec<Event> {
        text.chars()
            .map(|c| Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)))
            .collect()
    }

    /// The bug this fixes: dropped on a terminal that types the path in,
    /// the leading `/` opened the search prompt and the rest of the path
    /// filled the query box along the bottom of the window. The file
    /// never opened.
    #[test]
    fn a_drop_typed_one_character_at_a_time_still_opens() {
        let dir = TempDir::new("typed-drop");
        let mut app = md_review_app(&dir.0, "# Plan\n");
        app.diff = Some(FileDiff::compute(Some("# Plan\n"), Some("# Plan\n")));
        app.rebuild_display();
        let away = std::env::temp_dir().join("loupe-pins-typed");
        std::fs::create_dir_all(&away).unwrap();
        let doc = away.join("typed-note.md");
        std::fs::write(&doc, "# Typed note\n\nnot a search term\n").unwrap();

        app.handle_events(typed(&doc.to_string_lossy()));

        assert!(!app.find.typing, "the search prompt did not open");
        assert!(app.find.query.is_empty(), "and swallowed no path");
        assert_eq!(app.pins.len(), 1, "the drop made a tab");
        let pv = app.preview.as_ref().expect("and opened the document");
        assert!(pv.src.contains("not a search term"));
        std::fs::remove_dir_all(&away).ok();
    }

    /// The same, with the spellings terminals use for a path that has a
    /// space in it, and with the trailing space several of them add.
    #[test]
    fn a_typed_drop_survives_quotes_and_escapes() {
        let dir = TempDir::new("typed-quoted");
        let away = std::env::temp_dir().join("loupe-pins-typed-space");
        std::fs::create_dir_all(&away).unwrap();
        let doc = away.join("my notes.md");
        std::fs::write(&doc, "# Spaced\n").unwrap();
        let raw = doc.to_string_lossy().into_owned();
        // A path with a space arrives quoted or escaped — never bare,
        // because bare is genuinely two paths.
        let plain = away.join("plain.md");
        std::fs::write(&plain, "# Plain\n").unwrap();
        let mut spellings = vec![
            format!("'{raw}'"),
            format!("\"{raw}\""),
            // The trailing space several terminals add after the path.
            format!("{} ", plain.to_string_lossy()),
        ];
        // Backslash-escaping is a POSIX spelling: on Windows the
        // backslash is the path separator, so quoting is the only way a
        // terminal there spells a space.
        if !cfg!(windows) {
            spellings.push(raw.replace(' ', "\\ "));
        }
        for text in spellings {
            let mut app = md_review_app(&dir.0, "# Plan\n");
            app.handle_events(typed(&text));
            assert_eq!(app.pins.len(), 1, "{text}");
            assert!(app.preview.is_some(), "{text}");
        }
        std::fs::remove_dir_all(&away).ok();
    }

    /// The guard that makes the whole thing safe: a burst of keys that is
    /// not a real path is still just keys. `/` opens the search prompt,
    /// as it always did.
    #[test]
    fn a_typed_burst_that_is_not_a_path_is_still_typed() {
        let dir = TempDir::new("typed-search");
        let mut app = md_review_app(&dir.0, "# Plan\n");
        app.diff = Some(FileDiff::compute(Some("# Plan\n"), Some("# Plan\n")));
        app.rebuild_display();

        // Someone searching for a word, fast enough to land in one batch.
        app.handle_events(typed("/needle"));

        assert!(app.find.typing, "the search prompt opened");
        assert_eq!(app.find.query, "needle");
        assert!(app.pins.is_empty(), "and nothing was pinned");
    }

    /// A path that does not exist is not a drop either — it is someone
    /// searching for something that looks like a path.
    #[test]
    fn a_typed_path_that_is_not_there_is_not_a_drop() {
        let dir = TempDir::new("typed-missing");
        let mut app = md_review_app(&dir.0, "# Plan\n");
        app.diff = Some(FileDiff::compute(Some("# Plan\n"), Some("# Plan\n")));
        app.rebuild_display();
        app.handle_events(typed("/definitely/not/here.md"));
        assert!(app.find.typing, "it is a search, not a drop");
        assert_eq!(app.find.query, "definitely/not/here.md");
        assert!(app.pins.is_empty());
    }

    /// Ordinary single key presses still go through the batch path
    /// untouched — that is most of what the loop ever carries.
    #[test]
    fn single_keys_are_unaffected_by_the_batch() {
        let dir = TempDir::new("typed-single");
        let mut app = md_review_app(&dir.0, "# Plan\n");
        app.handle_events(vec![Event::Key(KeyEvent::new(
            KeyCode::Char('P'),
            KeyModifiers::NONE,
        ))]);
        let pv = app.preview.as_ref().expect("P still opens the preview");
        assert_eq!(pv.path, "PLAN.md");
    }

    /// The event loop waits a moment for the rest of a path only when the
    /// batch really looks like one starting. One key press never waits.
    #[test]
    fn only_a_path_shaped_burst_waits_for_more() {
        assert!(!partial_typed_path(&typed("/")), "one key is not a path");
        assert!(!partial_typed_path(&typed("jk")), "nor two motions");
        assert!(
            partial_typed_path(&typed("/U")),
            "an absolute path might be"
        );
        assert!(partial_typed_path(&typed("~/")), "so might a home path");
        assert!(partial_typed_path(&typed("'/")), "and a quoted one");
        assert!(partial_typed_path(&typed("fil")), "and a file:// URL");
        // A modifier means a command, so the batch is never a path.
        assert!(!partial_typed_path(&[Event::Key(KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::CONTROL
        ))]));
    }

    /// The rule that lets paste stay paste: text that is not a path goes
    /// where the keyboard is, and pins nothing.
    #[test]
    fn pasted_text_is_not_a_drop() {
        let dir = TempDir::new("drop-text");
        let mut app = md_review_app(&dir.0, "# Plan\n");
        app.handle_key(key(KeyCode::Char('P')));
        app.handle_key(key(KeyCode::Char('P'))); // preview → source
        assert!(app.editor.is_some(), "the editor is open");

        app.handle_paste("let x = 1;".into());

        assert!(app.pins.is_empty(), "nothing was pinned");
        let ed = app.editor.as_ref().expect("still in the editor");
        assert!(ed.content().contains("let x = 1;"), "it was pasted");
        assert!(ed.dirty, "and counts as an edit");
    }

    /// `=` pins what is in front of the reader, and `-` takes it back.
    #[test]
    fn the_pin_key_adds_and_removes_a_tab() {
        let dir = TempDir::new("pin-key");
        let mut app = md_review_app(&dir.0, "# Plan\n");
        app.handle_key(key(KeyCode::Char('=')));
        assert_eq!(app.pins.len(), 1);
        assert_eq!(app.pins.items[0].path, "PLAN.md", "named against the repo");
        assert!(!app.pins.items[0].outside);
        assert!(app.current_is_pinned(), "the 📌 button reads as pressed");

        app.handle_key(key(KeyCode::Char('-')));
        assert!(app.pins.is_empty(), "and unpinned again");
        assert!(!app.current_is_pinned());
    }

    /// A digit opens that tab; a digit past the end says so rather than
    /// doing nothing.
    #[test]
    fn a_digit_opens_the_tab_it_names() {
        let dir = TempDir::new("pin-digit");
        let mut app = md_review_app(&dir.0, "# Plan\n");
        let away = std::env::temp_dir().join("loupe-pins-digit");
        std::fs::create_dir_all(&away).unwrap();
        let doc = away.join("design.md");
        std::fs::write(&doc, "# Design\n\nthe shape of it\n").unwrap();
        app.handle_paste(doc.to_string_lossy().into_owned());
        app.close_preview();
        assert!(app.preview.is_none(), "back in the diff");

        app.handle_key(key(KeyCode::Char('1')));
        let pv = app.preview.as_ref().expect("tab 1 opened");
        assert!(pv.src.contains("the shape of it"));

        app.handle_key(key(KeyCode::Char('4')));
        assert!(app.status_err, "it says so: {}", app.status);
        assert!(app.status.contains("no tab 4"));
        std::fs::remove_dir_all(&away).ok();
    }

    /// Closing the tab the reader is inside puts them back in the review
    /// rather than leaving them in a document with no tab.
    #[test]
    fn closing_the_open_tab_returns_to_the_diff() {
        let dir = TempDir::new("pin-close");
        let mut app = md_review_app(&dir.0, "# Plan\n");
        let away = std::env::temp_dir().join("loupe-pins-close");
        std::fs::create_dir_all(&away).unwrap();
        let doc = away.join("read-me.md");
        std::fs::write(&doc, "# Read me\n").unwrap();
        app.handle_paste(doc.to_string_lossy().into_owned());
        assert!(app.preview.is_some());

        app.handle_key(key(KeyCode::Char('-')));

        assert!(app.pins.is_empty(), "the tab is gone");
        assert!(app.preview.is_none(), "and so is the document");
        assert!(doc.is_file(), "the file itself is untouched");
        std::fs::remove_dir_all(&away).ok();
    }

    /// A dropped file that is not markdown opens in the editor, and it is
    /// a real editor: outside the repository or not, Ctrl+S writes it.
    #[test]
    fn a_dropped_source_file_opens_in_the_editor() {
        let dir = TempDir::new("drop-source");
        let mut app = md_review_app(&dir.0, "# Plan\n");
        let away = std::env::temp_dir().join("loupe-pins-source");
        std::fs::create_dir_all(&away).unwrap();
        let src = away.join("snippet.rs");
        std::fs::write(&src, "fn main() {}\n").unwrap();

        app.handle_paste(src.to_string_lossy().into_owned());

        assert!(app.preview.is_none(), "not a document");
        let ed = app.editor.as_ref().expect("the editor holds it");
        assert!(ed.standalone, "it is not part of the changeset");
        assert!(!ed.read_only, "and it can be saved");
        assert_eq!(ed.abs_path, std::fs::canonicalize(&src).unwrap());
        std::fs::remove_dir_all(&away).ok();
    }

    /// Two files dropped together both get a tab, and the first opens.
    #[test]
    fn dropping_two_files_pins_both() {
        let dir = TempDir::new("drop-two");
        let mut app = md_review_app(&dir.0, "# Plan\n");
        let away = std::env::temp_dir().join("loupe-pins-two");
        std::fs::create_dir_all(&away).unwrap();
        let a = away.join("one.md");
        let b = away.join("two.md");
        std::fs::write(&a, "# One\n").unwrap();
        std::fs::write(&b, "# Two\n").unwrap();

        app.handle_paste(format!("{} {}", a.to_string_lossy(), b.to_string_lossy()));

        assert_eq!(app.pins.len(), 2);
        assert_eq!(app.active_pin(), Some(0), "the first one opened");
        assert!(app.status.contains("1 more pinned"), "{}", app.status);
        std::fs::remove_dir_all(&away).ok();
    }

    /// A tab for a file the change touches opens its diff, not a copy of
    /// the file: during a review that is what coming back to it means.
    #[test]
    fn a_tab_for_a_changed_file_goes_to_its_diff() {
        let dir = TempDir::new("pin-changed");
        let mut app = md_review_app(&dir.0, "# Plan\n");
        app.handle_key(key(KeyCode::Char('=')));
        assert_eq!(app.pins.len(), 1);
        assert!(!app.pins.items[0].outside, "it is a repository file");
        // Already the file under the cursor with its diff loaded, so the
        // tab renders it rather than paying for a second load.
        app.diff = Some(FileDiff::compute(Some("# Plan\n"), Some("# Plan\n")));
        app.handle_key(key(KeyCode::Char('1')));
        let pv = app.preview.as_ref().expect("markdown opens as a document");
        assert!(!pv.standalone, "the diff is still behind it");
    }

    /// The path box is the way in when a terminal cannot report a drop.
    #[test]
    fn the_path_box_opens_a_file_from_anywhere() {
        let dir = TempDir::new("path-box");
        let mut app = md_review_app(&dir.0, "# Plan\n");
        let away = std::env::temp_dir().join("loupe-pins-box");
        std::fs::create_dir_all(&away).unwrap();
        let doc = away.join("typed.md");
        std::fs::write(&doc, "# Typed\n\nby hand\n").unwrap();

        app.handle_key(ctrl(KeyCode::Char('o')));
        assert!(matches!(app.overlay, Overlay::OpenPath(_)));
        for ch in doc.to_string_lossy().chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Enter));

        assert!(matches!(app.overlay, Overlay::None), "the box closed");
        assert_eq!(app.pins.len(), 1);
        let pv = app.preview.as_ref().expect("it opened");
        assert!(pv.src.contains("by hand"));
        std::fs::remove_dir_all(&away).ok();
    }

    /// A path the box cannot resolve leaves the box open with the reason,
    /// rather than closing on a typo.
    #[test]
    fn the_path_box_keeps_a_bad_path_on_screen() {
        let dir = TempDir::new("path-bad");
        let mut app = md_review_app(&dir.0, "# Plan\n");
        app.handle_key(ctrl(KeyCode::Char('o')));
        for ch in "/definitely/not/here.md".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.overlay, Overlay::OpenPath(_)), "still open");
        assert!(app.status_err, "it says why: {}", app.status);
        assert!(app.pins.is_empty());
    }

    /// Pinning the same file twice is one tab, not two.
    #[test]
    fn pinning_the_same_file_twice_is_one_tab() {
        let dir = TempDir::new("pin-twice");
        let mut app = md_review_app(&dir.0, "# Plan\n");
        let away = std::env::temp_dir().join("loupe-pins-twice");
        std::fs::create_dir_all(&away).unwrap();
        let doc = away.join("same.md");
        std::fs::write(&doc, "# Same\n").unwrap();
        let text = doc.to_string_lossy().into_owned();
        app.handle_paste(text.clone());
        app.handle_paste(text);
        assert_eq!(app.pins.len(), 1);
        std::fs::remove_dir_all(&away).ok();
    }

    /// A tab does not get to throw away work the reader has not saved —
    /// and it does not have to refuse the switch to manage it. It used
    /// to: every tab said "save or discard first", which made moving
    /// between files a chore. The buffer is parked instead, so it keeps
    /// its text, its place in the row and the dot that says it is
    /// unsaved, and the reader goes where they were going.
    #[test]
    fn a_tab_switch_parks_unsaved_work_rather_than_refusing() {
        let dir = TempDir::new("pin-dirty");
        let mut app = md_review_app(&dir.0, "# Plan\n");
        let away = std::env::temp_dir().join("loupe-pins-dirty");
        std::fs::create_dir_all(&away).unwrap();
        let doc = away.join("held.md");
        std::fs::write(&doc, "# Held\n").unwrap();
        app.handle_paste(doc.to_string_lossy().into_owned());
        app.close_preview();
        // Open the changeset file in the editor and change it.
        app.handle_key(key(KeyCode::Char('P')));
        app.handle_key(key(KeyCode::Char('P')));
        app.editor.as_mut().expect("the editor").dirty = true;
        let held = app.editor.as_ref().unwrap().path.clone();

        app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::ALT));

        assert!(!app.status_err, "it went through: {}", app.status);
        assert!(
            app.buffers().any(|e| e.path == held && e.dirty),
            "and the unsaved text is still open, in its own tab"
        );
        std::fs::remove_dir_all(&away).ok();
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
            conflicted: false,
        });
        app.files.push(cf("src/main.rs"));
        app.rebuild_files();
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
            conflict: None,
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
            conflict: None,
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
            peek: false,
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

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    /// Type into the inline comment draft that is open.
    fn type_into_comment(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    /// An app in local review with something pushed under it, so the
    /// layers have all three versions to read.
    fn layered_app() -> App {
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.stack_base = "b".repeat(40);
        // The top of the stack is the working tree, so the branch has to
        // be checked out for any of this to mean anything.
        app.checked_out = true;
        app.tracking = Some(crate::gitops::Tracking {
            upstream: "origin/feature".into(),
            ahead: 1,
            behind: 0,
        });
        app
    }

    /// `s` walks the three settings and comes back to where it started.
    #[test]
    fn s_walks_the_layers_and_back() {
        let mut app = layered_app();
        assert_eq!(app.layers, Layers::Off);
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.layers, Layers::Full);
        assert!(app.status.contains("whole stack"), "{}", app.status);
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.layers, Layers::Mine);
        assert!(app.status.contains("only what is new"), "{}", app.status);
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.layers, Layers::Off);
    }

    /// With no middle version at all the setting would draw exactly what
    /// is already on screen. Refuse it and say why, rather than turning
    /// on something that looks broken.
    #[test]
    fn the_layers_refuse_a_branch_with_no_commits() {
        let mut app = layered_app();
        app.tracking = None;
        app.local = false;
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.layers, Layers::Off);
        assert!(app.status_err);
        assert!(app.status.contains("no commits yet"), "{}", app.status);
    }

    /// …and one whose remote has no default branch has no bottom.
    #[test]
    fn the_layers_refuse_a_repository_with_no_default_branch() {
        let mut app = layered_app();
        app.stack_base = String::new();
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.layers, Layers::Off);
        assert!(app.status_err);
        assert!(app.status.contains("No default branch"), "{}", app.status);
    }

    /// The layered diff reads the base branch on the left, so putting a
    /// section back would write the pull request's own work out of the
    /// file along with the edit the reader meant. Refused, with the key
    /// that undoes the refusal in the message.
    #[test]
    fn a_revert_is_refused_while_the_layers_are_on() {
        let mut app = layered_app();
        app.files = vec![ChangedFile {
            path: "code.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            previous: None,
            conflicted: false,
        }];
        app.rebuild_files();
        app.diff = Some(FileDiff::stack(
            Some("one\ntwo\n"),
            Some("one\nTWO\n"),
            Some("one\nTHREE\n"),
        ));
        app.rebuild_display();
        app.layers = Layers::Full;
        app.handle_key(key(KeyCode::Char('u')));
        assert!(app.status_err);
        assert!(app.status.contains("Press s"), "{}", app.status);
        assert!(
            matches!(app.overlay, Overlay::None),
            "no confirm prompt was opened"
        );
    }

    /// The refs a load carries. The layers need a bottom and a middle,
    /// and either one wrong would draw a stack of the wrong versions.
    #[test]
    fn a_load_carries_the_refs_the_layers_need() {
        let mut app = layered_app();
        app.layers = Layers::Full;
        let ctx = app.file_load_ctx();
        assert_eq!(ctx.layers, Layers::Full);
        assert_eq!(ctx.stack_base, "b".repeat(40));
        assert_eq!(ctx.pushed_ref, "origin/feature");
    }

    /// A branch with nothing on the remote still has a middle version:
    /// what is committed. The layers then say which of the edits in front
    /// of the reader land on lines this branch's own commits already
    /// wrote — the same question, one step before the first push.
    #[test]
    fn an_unpushed_branch_layers_against_its_own_commits() {
        let mut app = layered_app();
        app.tracking = None;
        // Local review's old side is HEAD, and so is the middle version.
        app.merge_base = "a".repeat(40);
        assert_eq!(app.pushed_ref(), "a".repeat(40));
        assert_eq!(app.pushed_label(), "already committed");
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.layers, Layers::Full);
        assert!(
            app.status.contains("already committed"),
            "the message says what the purple stands for: {}",
            app.status
        );
    }

    /// A read-only review has no working tree over the pushed branch, so
    /// there is no top layer and every row would come out purple.
    #[test]
    fn the_layers_refuse_a_read_only_review() {
        let mut app = layered_app();
        app.checked_out = false;
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.layers, Layers::Off);
        assert!(app.status_err);
        assert!(app.status.contains("read-only"), "{}", app.status);
    }

    /// A commit out of the Commits panel is one step of the change
    /// already; there is no second step under it to lay anything over.
    #[test]
    fn the_layers_stand_down_on_a_commits_panel_file() {
        let mut app = layered_app();
        app.open_commit = Some(OpenCommit {
            oid: "a".repeat(40),
            short: "aaaaaaa".into(),
            subject: "a commit".into(),
            parent: "b".repeat(40),
            file: 0,
        });
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.layers, Layers::Off);
        assert!(
            app.status.contains("A commit is one step"),
            "{}",
            app.status
        );
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
            // Read the appearance the picker actually opened on rather
            // than assuming it is `start`: what this test is about is
            // that a toggle flips it and a second toggle brings the
            // theme back, not what `theme_is_light` says about a theme
            // whose colors come from the terminal.
            let opened = crate::theme::appearance();
            app.handle_key(key(KeyCode::Char('a')));
            assert_eq!(crate::theme::appearance(), opened.other(), "{name}");
            app.handle_key(key(KeyCode::Char('a')));
            assert_eq!(crate::theme::appearance(), opened, "{name}");
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

    /// A repository on disk with three files in it, opened in the Files
    /// panel with every folder expanded and the file list where a click
    /// can reach it.
    pub fn tree_app(dir: &std::path::Path) -> App {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join("src/one.rs"), "fn one() {}\n").unwrap();
        std::fs::write(dir.join("src/two.rs"), "fn two() {}\n").unwrap();
        std::fs::write(dir.join("docs/plan.md"), "# Plan\n").unwrap();

        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        app.repo_root = dir.to_path_buf();
        app.apply_repo_listing(search::RepoListing {
            paths: vec![
                "src/one.rs".into(),
                "src/two.rs".into(),
                "docs/plan.md".into(),
            ],
            ignored_from: 3,
            stubs: Vec::new(),
        });
        app.set_panel(PanelMode::Files);
        app.collapsed().remove("src");
        app.collapsed().remove("docs");
        app.rebuild_entries();
        app.layout.file_list = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 20,
        };
        app
    }

    /// The row `path` is drawn on, so a click can be aimed at it.
    fn row_of(app: &App, path: &str) -> u16 {
        app.entries
            .iter()
            .position(|e| match e {
                FileEntry::File { src, .. } => app.row_path(*src) == Some(path),
                _ => false,
            })
            .unwrap_or_else(|| panic!("no row for {path}")) as u16
    }

    /// Click the file tree row for `path` and wait for the read to land.
    pub fn click_file(app: &mut App, path: &str) {
        let y = row_of(app, path);
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 6, y));
        for _ in 0..300 {
            if app.quiet.is_none() && app.job.is_none() {
                break;
            }
            app.poll_jobs();
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// How long a click on the tree actually costs, so the budget in the
    /// plan has a number to check against rather than a feeling.
    #[test]
    #[ignore = "a measurement, not an assertion"]
    fn measure_the_cost_of_a_tree_click() {
        let dir = std::env::temp_dir().join(format!("loupe-cost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut app = tree_app(&dir);
        for (n, path) in ["src/one.rs", "src/two.rs", "src/one.rs", "src/two.rs"]
            .iter()
            .enumerate()
        {
            let t = Instant::now();
            let y = row_of(&app, path);
            app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 6, y));
            let click = t.elapsed();
            let mut frames = 0;
            while app.quiet.is_some() {
                app.poll_jobs();
                frames += 1;
            }
            println!(
                "{n} {path}: click {click:?}, landed after {frames} polls, {:?} total",
                t.elapsed()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pinned file already has a tab. Clicking it in the tree opened a
    /// second one beside it — a peek on a single click, a kept tab on a
    /// double — so one file sat in the row twice and an edit went into
    /// whichever of them the reader happened to be looking at.
    #[test]
    fn a_pinned_file_opens_its_pin_and_never_a_second_tab() {
        let dir = std::env::temp_dir().join(format!("loupe-pinned-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut app = tree_app(&dir);
        let abs = std::fs::canonicalize(dir.join("src/one.rs")).unwrap();
        app.pins
            .add(crate::pins::Pin::new(&app.repo_root, abs))
            .expect("room for one pin");
        assert_eq!(app.pins.len(), 1);

        // One click: the pin opens, and the row gains nothing.
        click_file(&mut app, "src/one.rs");
        assert_eq!(
            app.editor.as_ref().map(|e| e.path.as_str()),
            Some("src/one.rs"),
            "the file is on screen"
        );
        assert_eq!(app.active_pin(), Some(0), "under its own pin tab");
        assert!(
            app.buffer_tabs().is_empty(),
            "and no second tab beside it: {:?}",
            labels_of(&app.buffer_tabs())
        );
        assert_eq!(app.peek_path(), None, "a pinned file is never a peek");

        // A double click has nothing to keep — it is already permanent.
        let y = row_of(&app, "src/one.rs");
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 6, y));
        assert!(
            app.buffer_tabs().is_empty(),
            "still one tab for one file: {:?}",
            labels_of(&app.buffer_tabs())
        );
        assert_eq!(app.pins.len(), 1);

        // Unpinning gives the file back its buffer tab rather than
        // leaving it open with no tab at all.
        app.pins.remove(0);
        assert_eq!(labels_of(&app.buffer_tabs()), vec!["one.rs"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The unsaved mark follows the tab the file actually has. Pinning a
    /// file moves its tab from the buffer row to the pin row, and the
    /// mark has to move with it or it simply disappears.
    #[test]
    fn a_pinned_file_carries_its_unsaved_mark_on_its_pin() {
        let dir = std::env::temp_dir().join(format!("loupe-pinned-dot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut app = tree_app(&dir);

        click_file(&mut app, "src/one.rs");
        app.editor.as_mut().unwrap().dirty = true;
        assert!(app.buffer_tabs()[0].dirty, "unpinned: the mark is here");
        assert!(!app.pin_dirty(0), "and there is no pin to carry it");

        let abs = std::fs::canonicalize(dir.join("src/one.rs")).unwrap();
        app.pins
            .add(crate::pins::Pin::new(&app.repo_root, abs))
            .expect("room for one pin");
        assert!(
            app.buffer_tabs().is_empty(),
            "pinned: the buffer tab gives way to the pin"
        );
        assert!(app.pin_dirty(0), "and the mark went with it");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The ask this whole thing exists for. Walking a tree means opening a
    /// great many files to look at one of them, so one click opens a tab
    /// the next click replaces, and a double click keeps it. The reader
    /// never saves, never closes, and never leaves the editor to do it.
    #[test]
    fn one_click_reuses_a_tab_and_a_double_click_keeps_it() {
        let dir = std::env::temp_dir().join(format!("loupe-tree-tab-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut app = tree_app(&dir);

        click_file(&mut app, "src/one.rs");
        assert_eq!(
            app.editor.as_ref().map(|e| e.path.as_str()),
            Some("src/one.rs"),
            "one click opened the file"
        );
        assert_eq!(app.buffer_tabs().len(), 1, "and made one tab for it");
        assert_eq!(app.peek_path(), Some("src/one.rs"), "the peek tab");

        // A second file takes that tab over rather than adding to the row.
        click_file(&mut app, "src/two.rs");
        assert_eq!(
            app.editor.as_ref().map(|e| e.path.as_str()),
            Some("src/two.rs")
        );
        let tabs = app.buffer_tabs();
        assert_eq!(tabs.len(), 1, "still one tab: {:?}", labels_of(&tabs));
        assert_eq!(tabs[0].label, "two.rs");
        assert!(tabs[0].peek, "and it is still the peek tab");

        // Double-click keeps it, so the next file gets a tab of its own.
        let y = row_of(&app, "src/two.rs");
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 6, y));
        assert_eq!(app.peek_path(), None, "the double click kept it");

        click_file(&mut app, "docs/plan.md");
        let tabs = app.buffer_tabs();
        assert_eq!(
            labels_of(&tabs),
            vec!["two.rs", "plan.md"],
            "the kept tab stayed and the new file got its own"
        );
        assert!(!tabs[0].peek, "two.rs is the reader's now");
        assert!(tabs[1].peek, "plan.md is the one a click replaces");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn labels_of(tabs: &[BufferTab]) -> Vec<String> {
        tabs.iter().map(|t| t.label.clone()).collect()
    }

    /// A buffer the reader typed into is theirs. The next click on the
    /// tree parks it and leaves its tab alone, so unsaved work cannot be
    /// lost by clicking somewhere else — and the reader never has to save
    /// to move on.
    #[test]
    fn an_edited_tab_is_never_replaced() {
        let dir = std::env::temp_dir().join(format!("loupe-tree-dirty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut app = tree_app(&dir);

        click_file(&mut app, "src/one.rs");
        app.editor.as_mut().unwrap().dirty = true;
        assert_eq!(
            app.peek_path(),
            None,
            "typing into it kept it, with no second flag to clear"
        );

        click_file(&mut app, "src/two.rs");
        let tabs = app.buffer_tabs();
        assert_eq!(
            labels_of(&tabs),
            vec!["one.rs", "two.rs"],
            "the unsaved file kept its tab"
        );
        assert!(tabs[0].dirty, "and the dot that says it is unsaved");
        assert!(
            app.buffers().any(|e| e.path == "src/one.rs" && e.dirty),
            "with the unsaved text still in it"
        );
        assert!(
            !app.status_err,
            "and nothing asked the reader to save first: {}",
            app.status
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file the change touches is still only a file in the Files panel.
    /// Clicking it opens the file, not its diff — the diff has a panel of
    /// its own one key away, and a row that means two things depending on
    /// which panel drew it is a row nobody can predict.
    #[test]
    fn a_changed_file_opens_as_a_file_from_the_tree() {
        let dir = std::env::temp_dir().join(format!("loupe-tree-changed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut app = tree_app(&dir);
        app.files = vec![ChangedFile {
            path: "src/one.rs".into(),
            status: "modified".into(),
            additions: 10,
            deletions: 2,
            previous: None,
            conflicted: false,
        }];

        click_file(&mut app, "src/one.rs");
        assert_eq!(
            app.editor.as_ref().map(|e| e.path.as_str()),
            Some("src/one.rs"),
            "the file opened"
        );
        assert!(app.diff.is_none(), "and no diff was loaded for it");
        assert_eq!(
            app.editor.as_ref().unwrap().content(),
            "fn one() {}\n",
            "with what is on disk in it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The editor does not lock the file panel. Clicking another file
    /// parks the buffer on screen and opens the next one, so a reader can
    /// walk a tree of files without saving, closing, or pressing Esc.
    #[test]
    fn the_tree_still_opens_files_with_the_editor_up() {
        let dir = std::env::temp_dir().join(format!("loupe-tree-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut app = tree_app(&dir);

        click_file(&mut app, "src/one.rs");
        app.keep_peek();
        assert!(app.editor.is_some(), "the editor is up");

        click_file(&mut app, "src/two.rs");
        assert_eq!(
            app.editor.as_ref().map(|e| e.path.as_str()),
            Some("src/two.rs"),
            "and the panel still switched files under it"
        );
        assert!(
            !app.status_err,
            "with no “close the editor first”: {}",
            app.status
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Markdown opens as source from the tree, so it gets a tab like any
    /// other file. `P` renders it and `P` puts the source back, and the
    /// tab has to survive the round trip: a document is a rendering of an
    /// open file, not a different thing, and a tab that came and went
    /// would be lying about what is open.
    #[test]
    fn the_preview_toggle_keeps_the_tab_and_the_buffer() {
        let dir = std::env::temp_dir().join(format!("loupe-tree-md-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut app = tree_app(&dir);

        click_file(&mut app, "docs/plan.md");
        assert_eq!(
            app.editor.as_ref().map(|e| e.path.as_str()),
            Some("docs/plan.md"),
            "the source opened, not the document"
        );
        app.editor.as_mut().unwrap().dirty = true;

        app.toggle_preview();
        assert!(app.previewing(), "P rendered it");
        let tabs = app.buffer_tabs();
        assert_eq!(labels_of(&tabs), vec!["plan.md"], "the tab stayed");
        assert!(tabs[0].active, "and is still the one on screen");
        assert!(tabs[0].dirty, "still carrying its unsaved mark");

        app.toggle_preview();
        assert!(!app.previewing(), "P put the source back");
        assert_eq!(
            app.editor.as_ref().map(|e| e.path.as_str()),
            Some("docs/plan.md")
        );
        assert!(
            app.editor.as_ref().unwrap().dirty,
            "with the unsaved text still in it"
        );
        assert_eq!(app.buffer_tabs().len(), 1, "and no second tab for it");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn buf(path: &str) -> Editor {
        Editor::new(path, std::path::PathBuf::from(path), "one\ntwo\n")
    }

    /// Opening a second file parks the first rather than closing it, and
    /// closing the second comes back to it — with whatever was unsaved
    /// still there. Re-opening a file already open switches to it instead
    /// of reading over the top of the edits.
    #[test]
    fn a_second_file_parks_the_first_instead_of_replacing_it() {
        let mut app = App::new(LaunchMode::Local, None);
        app.open_buffer(buf("a.rs"));
        app.editor.as_mut().unwrap().dirty = true;
        app.open_buffer(buf("b.rs"));

        assert_eq!(app.editor.as_ref().unwrap().path, "b.rs");
        assert_eq!(app.parked.len(), 1, "the first is parked, not gone");
        assert_eq!(app.buffers().count(), 2);
        assert_eq!(app.dirty_buffers(), 1);

        // Back to the first: the same buffer, still dirty.
        assert!(app.switch_buffer("a.rs"));
        assert_eq!(app.editor.as_ref().unwrap().path, "a.rs");
        assert!(
            app.editor.as_ref().unwrap().dirty,
            "switching kept the buffer, so the unsaved edit is still there"
        );

        // Opening it again is a switch, never a re-read.
        app.open_buffer(buf("a.rs"));
        assert_eq!(app.buffers().count(), 2, "no third copy of a.rs");
        assert!(app.editor.as_ref().unwrap().dirty);
    }

    /// The promise a rename makes: it changes the buffer you are looking
    /// at, opens every other file it touched so you can read it, saves
    /// none of them, and puts you back where you started.
    #[test]
    fn a_rename_opens_every_file_it_touched_and_saves_none() {
        let dir = std::env::temp_dir().join(format!("loupe-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "let alpha = 1;\n").unwrap();
        std::fs::write(dir.join("src/b.rs"), "use alpha;\n").unwrap();

        let mut app = App::new(LaunchMode::Local, None);
        app.repo_root = dir.clone();
        let mut open = Editor::new("src/a.rs", dir.join("src/a.rs"), "let alpha = 1;\n");
        open.standalone = true;
        app.open_buffer(open);

        let swap = |line: usize| {
            vec![lsp::TextEdit {
                start: (line, 4),
                end: (line, 9),
                text: "beta".into(),
            }]
        };
        app.apply_workspace_edit(
            "alpha → beta",
            vec![
                ("src/a.rs".to_string(), swap(0)),
                (
                    "src/b.rs".to_string(),
                    vec![lsp::TextEdit {
                        start: (0, 4),
                        end: (0, 9),
                        text: "beta".into(),
                    }],
                ),
            ],
        );

        assert_eq!(app.buffers().count(), 2, "the other file opened");
        assert_eq!(
            app.editor.as_ref().unwrap().path,
            "src/a.rs",
            "and the reader is back where they were"
        );
        assert_eq!(app.dirty_buffers(), 2, "neither is saved");
        assert_eq!(
            std::fs::read_to_string(dir.join("src/b.rs")).unwrap(),
            "use alpha;\n",
            "nothing was written behind the reader's back"
        );

        let changed: Vec<String> = app.buffers().map(|e| e.content()).collect();
        assert!(changed.iter().any(|c| c.contains("let beta")));
        assert!(changed.iter().any(|c| c.contains("use beta")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Above the threshold the tree holds the top level only    /// Above the threshold the tree holds the top level only, and every
    /// directory arrives as a stub. What matters is that the panel is
    /// usable at once rather than holding half a million `String`s nobody
    /// is going to look at.
    #[test]
    fn a_very_large_repository_is_read_one_directory_at_a_time() {
        let paths: Vec<String> = (0..MAX_EAGER_PATHS + 1)
            .map(|i| format!("pkg{}/file{i}.rs", i % 50))
            .collect();
        let listing = search::RepoListing {
            ignored_from: paths.len(),
            paths,
            stubs: Vec::new(),
        };
        let tree = TreeNodes::from_listing(&listing);

        assert!(tree.is_stub("pkg0"), "the top level is there, unread");
        let mut rows = Vec::new();
        tree.emit(&HashSet::new(), &mut rows);
        assert_eq!(
            rows.len(),
            50,
            "fifty directories and nothing under them: {}",
            rows.len()
        );
    }

    /// A file inside a nested worktree is an ordinary file. Only a
    /// directory git is *ignoring* passes ignored-ness down — the earlier
    /// rule treated every stub the same and would have drawn a whole
    /// worktree as if it were `node_modules`.
    #[test]
    fn only_an_ignored_directory_passes_that_down_to_what_is_inside() {
        let mut app = App::new(LaunchMode::Local, None);
        app.apply_repo_listing(search::RepoListing {
            paths: vec!["src/app.rs".into()],
            ignored_from: 1,
            stubs: vec![
                ("node_modules".to_string(), true),
                ("wt/feat".to_string(), false),
            ],
        });

        app.apply_dir_read(DirRead {
            dir: "node_modules".into(),
            files: vec!["index.js".into()],
            dirs: Vec::new(),
        });
        app.apply_dir_read(DirRead {
            dir: "wt/feat".into(),
            files: vec!["main.rs".into()],
            dirs: Vec::new(),
        });

        let ignored = |app: &App, path: &str| {
            let i = app.repo_paths.iter().position(|p| p == path).unwrap();
            app.row_ignored(RowSrc::Path(i))
        };
        assert!(ignored(&app, "node_modules/index.js"));
        assert!(
            !ignored(&app, "wt/feat/main.rs"),
            "a worktree holds ordinary files git only declined to walk into"
        );
    }

    /// The three file operations, and the guards    /// The three file operations, and the guards that stop each one
    /// doing damage. All of them are refusals rather than clever recovery:
    /// a path that escapes the repository, a name already taken, and a
    /// symlink standing in for the file somebody meant to remove.
    #[test]
    fn making_and_unmaking_files_refuses_the_dangerous_shapes() {
        let dir = std::env::temp_dir().join(format!("loupe-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/app.rs"), "fn main() {}\n").unwrap();

        let mut app = App::new(LaunchMode::Local, None);
        app.repo_root = dir.clone();
        app.panel = PanelMode::Files;
        app.apply_repo_listing(search::RepoListing {
            paths: vec!["src/app.rs".into()],
            ignored_from: 1,
            stubs: Vec::new(),
        });

        // Escaping the repository is refused outright.
        app.create_file("../escaped.rs");
        assert!(!dir.parent().unwrap().join("escaped.rs").exists());

        // So is writing over something that is already there.
        app.create_file("src/app.rs");
        assert_eq!(
            std::fs::read_to_string(dir.join("src/app.rs")).unwrap(),
            "fn main() {}\n",
            "the file that was already there is untouched"
        );

        app.create_file("src/new.rs");
        assert!(dir.join("src/new.rs").exists());
        assert!(
            app.repo_paths.iter().any(|p| p == "src/new.rs"),
            "and it is in the list without a re-read"
        );

        app.rename_file("src/new.rs", "src/renamed.rs");
        assert!(!dir.join("src/new.rs").exists());
        assert!(dir.join("src/renamed.rs").exists());

        // A rename onto a name that exists is refused, both files intact.
        app.rename_file("src/renamed.rs", "src/app.rs");
        assert!(dir.join("src/renamed.rs").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("src/app.rs")).unwrap(),
            "fn main() {}\n"
        );

        app.delete_file("src/renamed.rs");
        assert!(!dir.join("src/renamed.rs").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Deleting asks. It is the only thing this menu does that loupe
    /// cannot undo, so choosing it replaces the menu with the question
    /// rather than running.
    #[test]
    fn deleting_a_file_asks_before_it_does_it() {
        let mut app = App::new(LaunchMode::Local, None);
        app.panel = PanelMode::Files;
        app.overlay = Overlay::PathMenu(Box::new(PathMenu {
            path: "src/app.rs".into(),
            is_dir: false,
            items: vec![PathMenuItem {
                key: 'd',
                label: "Delete…",
                action: PathAction::Delete,
            }],
            sel: 0,
            anchor: (0, 0),
        }));

        app.run_path_menu(0);

        let Overlay::PathMenu(menu) = &app.overlay else {
            panic!("the menu is still open, asking");
        };
        assert_eq!(menu.items.len(), 1);
        assert_eq!(menu.items[0].action, PathAction::DeleteConfirmed);
        assert_eq!(menu.items[0].key, 'y', "and it takes a deliberate key");
    }

    /// Every key the editor answers to has a line in ☰. The menu is how a
    /// mouse-first reader finds a command at all, and seven of these are
    /// Alt keys some terminals never deliver — a line that is only a
    /// keyboard shortcut is a line those readers cannot reach.
    #[test]
    fn the_menu_offers_every_editor_command() {
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.open_buffer(buf("src/a.rs"));
        app.open_buffer(buf("src/b.rs"));

        let hints: Vec<&str> = app
            .build_menu()
            .iter()
            .filter_map(|r| match r {
                MenuRow::Item(i) => Some(i.hint),
                _ => None,
            })
            .collect();

        for want in [
            "Ctrl+S", "Ctrl+T", "Ctrl+F", "Alt+C", "F12", "F10", "Ctrl+G", "Alt+S", "F2", "Alt+.",
            "Alt+]", "Alt+[",
        ] {
            assert!(hints.contains(&want), "☰ has no line for {want}: {hints:?}");
        }
    }

    /// Typing a name has to offer suggestions on its own. This is the
    /// whole of "tab completion" from the reader's side: they type, the
    /// list appears, `Tab` takes one.
    #[test]
    fn typing_a_name_asks_for_suggestions_and_a_space_does_not() {
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.open_buffer(Editor::new(
            "src/a.rs",
            "src/a.rs".into(),
            "let total = cou",
        ));
        app.editor.as_mut().unwrap().jump_to_line(1);
        for _ in 0..15 {
            app.editor
                .as_mut()
                .unwrap()
                .textarea
                .move_cursor(tui_textarea::CursorMove::Forward);
        }

        app.maybe_suggest('u');
        assert!(
            app.completion_due.is_some(),
            "a word character with a name behind it asks"
        );
        assert_eq!(
            app.completion_due.unwrap().1,
            None,
            "and it is not a trigger character, so the server is told it was invoked"
        );

        // A space finishes the word: whatever was pending is stale.
        app.maybe_suggest(' ');
        assert!(app.completion_due.is_none(), "a space asks for nothing");

        // A trigger character asks even with no prefix at all — that is
        // what `object.` is for.
        app.maybe_suggest('.');
        assert_eq!(
            app.completion_due.map(|(_, c)| c),
            Some(Some('.')),
            "the dot travels with the request"
        );

        // Turned off, nothing is ever armed.
        app.completion_due = None;
        app.suggest_while_typing = false;
        app.maybe_suggest('u');
        assert!(app.completion_due.is_none());
    }

    /// A popup that is already open is filtered, not re-fetched: the
    /// list in hand is the server's answer for this word, and narrowing
    /// it is instant where a round trip is not.
    #[test]
    fn an_open_popup_is_not_asked_for_again() {
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.open_buffer(Editor::new("src/a.rs", "src/a.rs".into(), "cou"));
        let item = lsp::Completion {
            label: "count".into(),
            insert: "count".into(),
            detail: None,
            kind: "fn",
            sort: "count".into(),
            replace: None,
        };
        assert!(app.editor.as_mut().unwrap().open_completion(vec![item]));

        app.maybe_suggest('u');
        assert!(
            app.completion_due.is_none(),
            "the list is already here — filter it"
        );
    }

    /// A request the reader asked for says what it is waiting on; one
    /// that fires on its own while typing says nothing.
    #[test]
    fn only_the_requests_you_asked_for_announce_themselves() {
        assert_eq!(
            EditorRequest::Definition.waiting_for("parse"),
            Some("Finding the definition of parse…".into())
        );
        assert_eq!(
            EditorRequest::References.waiting_for("parse"),
            Some("Finding every use of parse…".into())
        );
        assert_eq!(
            EditorRequest::Complete(None).waiting_for("par"),
            None,
            "completion fires while typing — a spinner per character is noise"
        );
        assert_eq!(
            EditorRequest::Definition.waiting_for(""),
            Some("Finding the definition of…".into()),
            "no word under the cursor still names the question"
        );
    }

    /// The function keys have to reach the language-server calls, in the
    /// editor and in the diff view. `lsp_enabled = false` makes the call
    /// answer instantly and say so, which is proof the key arrived
    /// without starting a real server.
    #[test]
    fn the_function_keys_reach_the_language_server_calls() {
        for code in [KeyCode::F(12), KeyCode::F(10)] {
            let mut app = App::new(LaunchMode::Local, None);
            app.screen = Screen::Review;
            app.lsp_enabled = false;
            app.open_buffer(buf("src/a.rs"));
            app.handle_key(key(code));
            assert!(
                app.status.contains("Language servers are off"),
                "{code:?} in the editor went nowhere: {:?}",
                app.status
            );
        }
        // Shift+F12 is the other way to say F10.
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.lsp_enabled = false;
        app.open_buffer(buf("src/a.rs"));
        app.handle_key(KeyEvent::new(KeyCode::F(12), KeyModifiers::SHIFT));
        assert!(app.status.contains("Language servers are off"));
    }

    /// A click, then a second on the same cell, then a third: one press
    /// places the cursor, two take the word, three take the line. A
    /// fourth starts the count again rather than sticking on three.
    #[test]
    fn clicks_on_one_spot_count_up_and_start_again() {
        let mut app = App::new(LaunchMode::Local, None);
        assert_eq!(app.click_run(5, 5), 1);
        assert_eq!(app.click_run(5, 5), 2);
        assert_eq!(app.click_run(5, 5), 3);
        assert_eq!(app.click_run(5, 5), 1, "a fourth press starts again");
        assert_eq!(app.click_run(40, 9), 1, "and so does one somewhere else");
    }

    /// The right-click menu is about the word under the pointer: the
    /// lines that need a symbol are live when there is one, and dim
    /// rather than missing when there is not.
    #[test]
    fn the_code_menu_needs_a_word_under_the_pointer() {
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.open_buffer(buf("src/a.rs"));

        let live = |rows: &[MenuRow], id: ButtonId| {
            rows.iter()
                .any(|r| matches!(r, MenuRow::Item(i) if i.id == id && i.enabled))
        };
        let on_word = app.editor_menu_rows("count");
        assert!(live(&on_word, ButtonId::EditorDefinition));
        assert!(live(&on_word, ButtonId::EditorReferences));
        assert!(live(&on_word, ButtonId::EditorCopy));

        let on_nothing = app.editor_menu_rows("");
        assert!(
            !live(&on_nothing, ButtonId::EditorDefinition),
            "no symbol, nothing to look up"
        );
        assert!(
            on_nothing
                .iter()
                .any(|r| matches!(r, MenuRow::Item(i) if i.id == ButtonId::EditorDefinition)),
            "the line is still there, so the menu keeps its shape"
        );
        assert!(
            live(&on_nothing, ButtonId::EditorCopy),
            "copying does not need a symbol"
        );
    }

    /// A file with problems lists them, worst line order, and the list is
    /// the same overlay references use — so Enter goes to the line.
    #[test]
    fn the_problem_list_names_every_problem_in_the_file() {
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        let mut ed = buf("src/a.rs");
        ed.set_server_diagnostics(vec![
            lsp::Diagnostic {
                line: 2,
                col: 1,
                end_col: 4,
                severity: 2,
                message: "unused".into(),
                code: None,
                source: None,
            },
            lsp::Diagnostic {
                line: 1,
                col: 1,
                end_col: 4,
                severity: 1,
                message: "no such name".into(),
                code: Some("E0425".into()),
                source: None,
            },
        ]);
        app.open_buffer(ed);
        app.open_problems();

        let Overlay::Finder(f) = &app.overlay else {
            panic!("the problems open the finder");
        };
        assert_eq!(f.mode, FinderMode::Problems);
        assert_eq!(f.rows.len(), 2);
        assert_eq!(f.rows[0].line, Some(1), "line order, not arrival order");
        assert!(f.rows[0].text.contains("no such name"));
        assert!(f.rows[0].text.contains("E0425"), "the code comes too");
        assert_eq!(f.rows[0].tag, "error");
        assert_eq!(f.rows[1].tag, "warning");
        assert!(f.note.contains("1 error"), "{}", f.note);
    }

    /// The compiler and the linter run on different clocks. One
    /// answering must never blank the other's underlines, and the merged
    /// list has to stay in line order whichever lands first.
    #[test]
    fn lint_and_compiler_problems_live_side_by_side() {
        let d = |line, severity, source: &str, message: &str| lsp::Diagnostic {
            line,
            col: 1,
            end_col: 4,
            severity,
            message: message.into(),
            code: None,
            source: Some(source.into()),
        };
        let mut ed = buf("src/a.ts");
        ed.set_server_diagnostics(vec![d(5, 1, "typescript", "type error")]);
        ed.set_lint_diagnostics(vec![
            d(9, 2, "eslint", "unused"),
            d(2, 2, "eslint", "prefer const"),
        ]);

        let lines: Vec<usize> = ed.problems().iter().map(|p| p.line).collect();
        assert_eq!(lines, vec![2, 5, 9], "one list, in line order");
        assert_eq!(
            ed.problems().iter().filter(|p| p.is_error()).count(),
            1,
            "the compiler's error is still an error"
        );
        assert_eq!(
            ed.problems().iter().filter(|p| p.is_warning()).count(),
            2,
            "and the linter's are warnings"
        );

        // A second lint result replaces only the lint half.
        ed.set_lint_diagnostics(vec![d(3, 2, "eslint", "something else")]);
        let lines: Vec<usize> = ed.problems().iter().map(|p| p.line).collect();
        assert_eq!(
            lines,
            vec![3, 5],
            "the compiler's error survives the linter running again"
        );

        // And a clean lint run leaves the compiler alone.
        ed.set_lint_diagnostics(Vec::new());
        assert_eq!(ed.problems().len(), 1);
        assert_eq!(ed.problems()[0].source.as_deref(), Some("typescript"));
    }

    /// Nothing wrong means nothing to list, and the finder does not open
    /// on an empty list.
    #[test]
    fn a_clean_file_opens_no_problem_list() {
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.open_buffer(buf("src/a.rs"));
        app.open_problems();
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.status_err, "it says so instead: {}", app.status);
    }

    /// A read-only buffer came from a commit, not the working tree, so the
    /// lines that would write to it are not offered. Reading is still on
    /// the menu — that is the whole reason the buffer is open.
    #[test]
    fn a_read_only_buffer_is_not_offered_the_lines_that_write() {
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        let mut ed = buf("src/a.rs");
        ed.read_only = true;
        app.open_buffer(ed);

        let hints: Vec<&str> = app
            .build_menu()
            .iter()
            .filter_map(|r| match r {
                MenuRow::Item(i) => Some(i.hint),
                _ => None,
            })
            .collect();

        assert!(
            !hints.contains(&"Alt+C"),
            "no commenting a file it cannot write"
        );
        assert!(!hints.contains(&"F2"), "and no renaming through it");
        assert!(hints.contains(&"F10"), "but finding every use still works");
    }

    /// Two buffers both called `mod.rs` name nothing, so a shared name
    /// costs both of them their parent directory — the rule the pins
    /// already use.
    #[test]
    fn buffer_tabs_disambiguate_a_shared_name() {
        let mut app = App::new(LaunchMode::Local, None);
        app.open_buffer(buf("src/parser/mod.rs"));
        app.open_buffer(buf("src/lexer/mod.rs"));
        app.open_buffer(buf("src/main.rs"));

        let labels: Vec<String> = app.buffer_tabs().into_iter().map(|t| t.label).collect();
        assert!(labels.iter().any(|l| l == "parser/mod.rs"), "{labels:?}");
        assert!(labels.iter().any(|l| l == "lexer/mod.rs"), "{labels:?}");
        assert!(
            labels.iter().any(|l| l == "main.rs"),
            "and a name nobody shares stays short: {labels:?}"
        );

        let active: Vec<String> = app
            .buffer_tabs()
            .into_iter()
            .filter(|t| t.active)
            .map(|t| t.label)
            .collect();
        assert_eq!(active, vec!["main.rs"], "exactly one tab is the open one");
    }

    /// The row stands still. Clicking along a row of tabs used to move
    /// each one to the far right as it was opened, because the buffer on
    /// screen lived in its own field and that field was drawn last — so
    /// the row reshuffled under the pointer and no tab was ever twice in
    /// the same place.
    #[test]
    fn the_tab_row_keeps_the_order_the_files_were_opened_in() {
        let mut app = App::new(LaunchMode::Local, None);
        app.open_buffer(buf("a.rs"));
        app.open_buffer(buf("b.rs"));
        app.open_buffer(buf("c.rs"));
        assert_eq!(labels_of(&app.buffer_tabs()), vec!["a.rs", "b.rs", "c.rs"]);

        // Click along the row in every direction. None of it reorders.
        for path in ["a.rs", "c.rs", "b.rs", "a.rs"] {
            app.switch_buffer(path);
            assert_eq!(
                labels_of(&app.buffer_tabs()),
                vec!["a.rs", "b.rs", "c.rs"],
                "opening {path} left the row alone"
            );
            let tabs = app.buffer_tabs();
            let on_screen: Vec<&str> = tabs
                .iter()
                .filter(|t| t.active)
                .map(|t| t.label.as_str())
                .collect();
            assert_eq!(on_screen.len(), 1, "exactly one tab is the open one");
            assert!(
                path.ends_with(on_screen[0]),
                "and it is {path}: {on_screen:?}"
            );
        }

        // A fourth file joins the end, where a reader looks for the file
        // they just opened.
        app.open_buffer(buf("d.rs"));
        assert_eq!(
            labels_of(&app.buffer_tabs()),
            vec!["a.rs", "b.rs", "c.rs", "d.rs"]
        );
    }

    /// A drag is the one thing that reorders the row, and it moves the
    /// tab without changing which file is on screen.
    #[test]
    fn dragging_a_tab_moves_it_along_the_row() {
        let mut app = App::new(LaunchMode::Local, None);
        app.open_buffer(buf("a.rs"));
        app.open_buffer(buf("b.rs"));
        app.open_buffer(buf("c.rs"));
        app.switch_buffer("b.rs");

        // The file on screen, dragged to the front.
        app.move_buffer(1, 0);
        assert_eq!(labels_of(&app.buffer_tabs()), vec!["b.rs", "a.rs", "c.rs"]);
        assert_eq!(
            app.editor.as_ref().unwrap().path,
            "b.rs",
            "a drag reorders the row, it does not change the file"
        );

        // And a tab that is not on screen, dragged to the end.
        app.move_buffer(1, 2);
        assert_eq!(labels_of(&app.buffer_tabs()), vec!["b.rs", "c.rs", "a.rs"]);
        assert_eq!(app.editor.as_ref().unwrap().path, "b.rs");

        // Out of range does nothing rather than panicking.
        app.move_buffer(0, 9);
        assert_eq!(labels_of(&app.buffer_tabs()), vec!["b.rs", "c.rs", "a.rs"]);
    }

    /// The ✕ shuts one tab. The rest keep their places, and unsaved work
    /// is asked about once — per tab, so arming one never arms another.
    #[test]
    fn the_close_mark_shuts_one_tab_and_asks_about_unsaved_work() {
        let mut app = App::new(LaunchMode::Local, None);
        app.open_buffer(buf("a.rs"));
        app.open_buffer(buf("b.rs"));
        app.open_buffer(buf("c.rs"));
        app.switch_buffer("c.rs");

        // The middle tab, which nobody is looking at.
        app.close_buffer_at(1);
        assert_eq!(labels_of(&app.buffer_tabs()), vec!["a.rs", "c.rs"]);
        assert_eq!(
            app.editor.as_ref().unwrap().path,
            "c.rs",
            "closing another tab did not change the file on screen"
        );

        // An unsaved one asks first, then goes on the second press.
        app.switch_buffer("c.rs");
        app.parked[0].dirty = true;
        app.close_buffer_at(0);
        assert!(app.status_err, "it asked: {}", app.status);
        assert_eq!(app.buffer_tabs().len(), 2, "and kept the file");
        app.close_buffer_at(0);
        assert_eq!(labels_of(&app.buffer_tabs()), vec!["c.rs"], "then took it");
    }

    /// Stepping wraps, and lands on the buffer the tab row shows in that
    /// position — the row and the key must not disagree about the order.
    #[test]
    fn stepping_buffers_wraps_and_follows_the_row() {
        let mut app = App::new(LaunchMode::Local, None);
        app.open_buffer(buf("a.rs"));
        app.open_buffer(buf("b.rs"));
        app.open_buffer(buf("c.rs"));
        let order: Vec<String> = app.buffer_tabs().iter().map(|t| t.label.clone()).collect();

        let active = |app: &App| app.editor.as_ref().unwrap().path.clone();
        assert_eq!(active(&app), "c.rs");

        app.step_buffer(1);
        assert_eq!(
            active(&app),
            order[0],
            "past the end comes back to the first tab in the row"
        );
        app.step_buffer(-1);
        assert_eq!(active(&app), "c.rs", "and back again");
    }

    /// `q` used to be one keypress from gone. One buffer guarded itself
    /// with Esc-then-Esc, but `q` reaches past that, and with several
    /// buffers open it could take unsaved work in files that were not even
    /// on screen.
    #[test]
    fn quitting_with_unsaved_buffers_asks_first() {
        let mut app = App::new(LaunchMode::Local, None);
        app.open_buffer(buf("a.rs"));
        app.open_buffer(buf("b.rs"));
        // The unsaved one is parked, not the one on screen.
        app.parked[0].dirty = true;

        app.request_quit();
        assert!(!app.should_quit, "it asked instead of quitting");
        assert_eq!(app.dirty_buffers(), 1);

        app.request_quit();
        assert!(app.should_quit, "and a second q goes through");
    }

    /// The arming lasts for the next key and no longer. Anything else puts
    /// the guard back, so a `q` typed a minute later still asks.
    #[test]
    fn the_quit_guard_rearms_after_any_other_key() {
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.open_buffer(buf("a.rs"));
        app.editor.as_mut().unwrap().dirty = true;

        app.request_quit();
        assert!(!app.should_quit);

        app.handle_key(key(KeyCode::Char('j')));
        app.request_quit();
        assert!(!app.should_quit, "the unrelated key disarmed it");
    }

    /// `gr` answered in the diff view and did nothing in the editor, so
    /// opening a file from the tree used to cost you the ability to ask
    /// who calls it. Both now reach the same request.
    #[test]
    fn the_editor_can_ask_for_references() {
        assert!(
            EditorRequest::References.is_explicit(),
            "a key press, so a missing server is worth saying out loud"
        );

        let mut app = App::new(LaunchMode::Local, None);
        app.files = vec![cf("src/app.rs")];
        app.rebuild_files();
        app.apply_locations(LocationsData {
            action: LspAction::References,
            word: "walk".into(),
            places: vec![
                Place {
                    loc: lsp::Loc {
                        path: "src/app.rs".into(),
                        line: 12,
                        col: 4,
                    },
                    text: "    walk(&self.root);".into(),
                },
                Place {
                    loc: lsp::Loc {
                        path: "src/ui.rs".into(),
                        line: 40,
                        col: 8,
                    },
                    text: "        walk(node);".into(),
                },
            ],
        });

        let Overlay::Finder(f) = &app.overlay else {
            panic!("references open the finder");
        };
        assert_eq!(f.mode, FinderMode::Refs);
        assert_eq!(f.preset.len(), 2);
        assert!(
            f.preset[0].in_changeset,
            "a use inside the change is marked as one"
        );
        assert!(!f.preset[1].in_changeset);
    }

    /// A file the change touches keeps its marks in either panel, and a
    /// click on it opens the diff rather than the plain file. Two lists
    /// disagreeing about the same file would be the worse answer.
    #[test]
    fn a_changed_file_keeps_its_marks_in_the_repository_list() {
        let mut app = App::new(LaunchMode::Local, None);
        app.files = vec![cf("src/app.rs")];
        app.rebuild_files();
        app.apply_repo_listing(search::RepoListing {
            paths: vec!["src/app.rs".into(), "src/untouched.rs".into()],
            ignored_from: 2,
            stubs: Vec::new(),
        });

        let changed = RowSrc::Path(0);
        let untouched = RowSrc::Path(1);
        assert_eq!(
            app.changed_of(changed).map(|f| f.path.as_str()),
            Some("src/app.rs"),
            "the repository row found the change behind it"
        );
        assert_eq!(app.changed_idx(changed), Some(0));
        assert!(
            app.changed_of(untouched).is_none(),
            "and the file beside it has no change to find"
        );
    }

    /// A refresh must not collapse the folder somebody is reading. It
    /// re-reads the repository, so the tree is replaced wholesale, and
    /// nothing but this carries the reader's place across it.
    #[test]
    fn refreshing_the_listing_keeps_open_folders_open() {
        let mut app = App::new(LaunchMode::Local, None);
        let listing = || search::RepoListing {
            paths: vec!["src/app.rs".into(), "docs/plan.md".into()],
            ignored_from: 2,
            stubs: Vec::new(),
        };
        app.apply_repo_listing(listing());
        assert!(
            app.collapsed_files.contains("src"),
            "everything starts shut"
        );

        app.collapsed_files.remove("src");
        app.apply_repo_listing(listing());

        assert!(
            !app.collapsed_files.contains("src"),
            "the folder the reader opened is still open"
        );
        assert!(
            app.collapsed_files.contains("docs"),
            "and the one they never touched is still shut"
        );
    }

    /// The panel's whole speed budget in one assertion. Absolute bounds
    /// rather than the ratio next door, so a change that makes both halves
    /// slowly worse together still fails.
    ///
    /// They are deliberately loose — about 20 times what this machine
    /// measures — because a shared CI runner is not a laptop. What they
    /// catch is a change of *shape*: a walk that became quadratic, or a
    /// subprocess that found its way onto the emit path.
    #[test]
    fn the_tree_stays_inside_its_budget() {
        let paths: Vec<String> = (0..20_000)
            .map(|i| format!("pkg{}/mod{}/file{i}.rs", i % 40, i % 400))
            .collect();
        let listing = search::RepoListing {
            ignored_from: paths.len(),
            paths,
            stubs: Vec::new(),
        };

        let t = Instant::now();
        let tree = TreeNodes::from_listing(&listing);
        let build = t.elapsed();
        assert!(
            build < Duration::from_millis(200),
            "building 20,000 paths took {build:?} — it measures about 8 ms"
        );

        let collapsed = tree.all_dirs();
        let t = Instant::now();
        let mut rows = Vec::new();
        tree.emit(&collapsed, &mut rows);
        let emit = t.elapsed();
        assert!(
            emit < Duration::from_millis(5),
            "emitting a collapsed 20,000-path tree took {emit:?} — it measures about 4 µs"
        );
    }

    /// Reading a stub must add to the list, never renumber it: a row
    /// already on screen holds an index, and inserting in the middle would
    /// quietly repoint it at a different file. It must also stop at one
    /// level — the whole reason `node_modules` arrives as a single row is
    /// that nobody wants its descendants.
    #[test]
    fn reading_a_stub_appends_and_stops_at_one_level() {
        let mut app = App::new(LaunchMode::Local, None);
        app.apply_repo_listing(search::RepoListing {
            paths: vec!["src/app.rs".into(), ".env".into()],
            ignored_from: 1,
            stubs: vec![("node_modules".to_string(), true)],
        });
        app.panel = PanelMode::Files;
        let before = app.repo_paths.clone();
        assert!(app.file_tree.is_stub("node_modules"));

        app.apply_dir_read(DirRead {
            dir: "node_modules".into(),
            files: vec!["index.js".into()],
            dirs: vec!["pkg".into()],
        });

        assert_eq!(
            &app.repo_paths[..before.len()],
            &before[..],
            "every index already on screen still means the same file"
        );
        assert!(app
            .repo_paths
            .contains(&"node_modules/index.js".to_string()));
        assert!(
            !app.file_tree.is_stub("node_modules"),
            "the directory has been read"
        );
        assert!(
            app.file_tree.is_stub("node_modules/pkg"),
            "its subdirectory has not — going deeper costs another read"
        );
        assert!(
            app.collapsed_files.contains("node_modules/pkg"),
            "and arrives collapsed, like every other directory here"
        );

        // A file inside an ignored directory is ignored, whatever its
        // position in the list. This is why the flag is per path.
        let i = app
            .repo_paths
            .iter()
            .position(|p| p == "node_modules/index.js")
            .unwrap();
        assert!(app.row_ignored(RowSrc::Path(i)));
        let tracked = app
            .repo_paths
            .iter()
            .position(|p| p == "src/app.rs")
            .unwrap();
        assert!(!app.row_ignored(RowSrc::Path(tracked)));
    }

    /// Everything collapsed is what Files mode opens onto, and the
    /// collapse set has to be built from the same walk that draws the
    /// rows. Single-child directory chains are compressed into one row
    /// with a joined label, so a set built from raw path prefixes would
    /// miss every one of them and the tree would open fully expanded.
    #[test]
    fn files_mode_opens_with_every_directory_collapsed() {
        let listing = search::RepoListing {
            paths: vec![
                "deeply/nested/only/child.rs".into(),
                "src/app.rs".into(),
                "src/ui.rs".into(),
                "README.md".into(),
            ],
            ignored_from: 4,
            stubs: vec![("node_modules".to_string(), true)],
        };
        let tree = TreeNodes::from_listing(&listing);
        let collapsed = tree.all_dirs();

        let mut rows = Vec::new();
        tree.emit(&collapsed, &mut rows);

        let dirs: Vec<&str> = rows
            .iter()
            .filter_map(|e| match e {
                FileEntry::Dir { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            dirs.contains(&"deeply/nested/only"),
            "the single-child chain is one compressed row: {dirs:?}"
        );
        assert!(dirs.contains(&"src"), "and the ordinary ones are there too");
        assert!(
            dirs.contains(&"node_modules"),
            "a directory git would not walk into still gets a row: {dirs:?}"
        );

        // Nothing below any of them, and the top-level file.
        let files: Vec<RowSrc> = rows
            .iter()
            .filter_map(|e| match e {
                FileEntry::File { src, .. } => Some(*src),
                _ => None,
            })
            .collect();
        assert_eq!(
            files.len(),
            1,
            "only the root's own file shows while everything is collapsed"
        );
    }

    /// Flipping to Files and back must not disturb the change under
    /// review, and the two lists must not share collapse state — a folder
    /// closed in one is a folder the reader never touched in the other.
    #[test]
    fn the_two_panels_keep_their_own_collapse_state() {
        let mut app = App::new(LaunchMode::Local, None);
        app.files = vec![cf("src/app.rs"), cf("src/ui.rs")];
        app.rebuild_files();
        let change_rows = app.entries.len();

        app.apply_repo_listing(search::RepoListing {
            paths: vec!["src/app.rs".into(), "docs/plan.md".into()],
            ignored_from: 2,
            stubs: Vec::new(),
        });
        app.set_panel(PanelMode::Files);
        assert!(
            app.entries.iter().any(|e| matches!(
                e,
                FileEntry::File {
                    src: RowSrc::Path(_),
                    ..
                }
            ) || matches!(e, FileEntry::Dir { .. })),
            "Files mode draws the repository"
        );

        // Open `src` here. The change panel must not notice.
        app.collapsed_files.remove("src");
        app.rebuild_entries();
        let files_rows = app.entries.len();

        app.set_panel(PanelMode::Changes);
        assert_eq!(
            app.entries.len(),
            change_rows,
            "the change under review came back exactly as it was"
        );
        assert!(
            !app.collapsed_dirs.contains("src"),
            "and the other panel's collapse never leaked into it"
        );

        app.set_panel(PanelMode::Files);
        assert_eq!(app.entries.len(), files_rows, "as did the repository list");
    }

    /// The whole point of caching the tree: a collapse toggle must not pay
    /// for the build.
    ///
    /// The bound is a ratio rather than a wall-clock number, so a slow
    /// machine does not fail the suite. What it catches is the regression
    /// that matters — someone moving the build back inside
    /// `rebuild_entries`, at which point the two times converge and the
    /// margin disappears.
    #[test]
    fn emitting_rows_does_not_rebuild_the_tree() {
        let files: Vec<ChangedFile> = (0..20_000)
            .map(|i| cf(&format!("pkg{}/mod{}/file{i}.rs", i % 40, i % 400)))
            .collect();

        let build_start = Instant::now();
        let tree = TreeNodes::build(&files);
        let build = build_start.elapsed();

        // Everything collapsed is what a reader opens onto, per D11 of the
        // plan, and it is the case a click has to stay fast in.
        let collapsed: HashSet<String> = (0..40).map(|i| format!("pkg{i}")).collect();

        let emit_start = Instant::now();
        let mut rows = Vec::new();
        for _ in 0..10 {
            rows.clear();
            tree.emit(&collapsed, &mut rows);
        }
        let emit = emit_start.elapsed();

        assert!(!rows.is_empty(), "the tree emitted nothing");
        assert!(
            emit < build,
            "ten emits ({emit:?}) should cost less than one build ({build:?}) — \
             the build is back on the toggle path"
        );
    }

    #[test]
    fn tree_groups_files_under_dirs() {
        let files = vec![cf("src/app/x.rs"), cf("src/ui/y.rs"), cf("README.md")];
        let mut entries = Vec::new();
        TreeNodes::build(&files).emit(&HashSet::new(), &mut entries);
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
                FileEntry::File {
                    src: RowSrc::Changed(0),
                    depth: 2
                },
                FileEntry::Dir {
                    label: "ui".into(),
                    path: "src/ui".into(),
                    depth: 1
                },
                FileEntry::File {
                    src: RowSrc::Changed(1),
                    depth: 2
                },
                FileEntry::File {
                    src: RowSrc::Changed(2),
                    depth: 0
                },
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
        app.rebuild_files();
        app.layout.file_list = Rect::new(0, 2, 30, 10);
        app.open_path_menu(4, 2);

        let Overlay::PathMenu(menu) = &app.overlay else {
            panic!("right-click must open the path menu");
        };
        assert_eq!(menu.path, "src/app.rs");
        assert!(!menu.is_dir);
        assert_eq!(menu.items.len(), 2);
        assert_eq!(menu.items[0].action, PathAction::Copy("src/app.rs".into()));
        // Spelled with `Path` rather than `/`: on Windows the absolute
        // form is `C:\…\src\app.rs` (canonicalize adds a `\\?\` prefix
        // as well), so a check for a leading slash would test the
        // platform instead of the menu.
        let PathAction::Copy(full) = &menu.items[1].action else {
            panic!("the second line copies the full path");
        };
        let full = std::path::Path::new(full);
        assert!(full.is_absolute(), "{full:?}");
        assert!(full.ends_with("src/app.rs"), "{full:?}");
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
        app.rebuild_files();
        app.layout.file_list = Rect::new(0, 0, 30, 10);
        app.open_path_menu(2, 0);

        let Overlay::PathMenu(menu) = &app.overlay else {
            panic!("right-click on a folder must open the path menu");
        };
        assert!(menu.is_dir);
        assert_eq!(menu.path, "src/ui");
        assert_eq!(menu.items[0].action, PathAction::Copy("src/ui".into()));
    }

    /// The menu is a menu: arrows move, Esc closes, nothing is copied.
    #[test]
    fn path_menu_moves_and_closes() {
        let mut app = App::new(LaunchMode::Local, None);
        app.repo_root = std::env::temp_dir();
        app.files = vec![cf("README.md")];
        app.tree_view = false;
        app.rebuild_files();
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
        app.rebuild_files();
        app.layout.file_list = Rect::new(0, 0, 30, 10);
        app.open_path_menu(1, 6);
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn tree_compresses_single_child_chains() {
        let files = vec![cf("a/b/c/deep.rs")];
        let mut entries = Vec::new();
        TreeNodes::build(&files).emit(&HashSet::new(), &mut entries);
        assert_eq!(
            entries,
            vec![
                FileEntry::Dir {
                    label: "a/b/c".into(),
                    path: "a/b/c".into(),
                    depth: 0
                },
                FileEntry::File {
                    src: RowSrc::Changed(0),
                    depth: 1
                },
            ]
        );
    }

    /// 20 identical lines with one change in the middle: two foldable runs.
    fn folded_app() -> App {
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.files = vec![cf("test.rs")];
        app.rebuild_files();
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
            conflict: None,
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
                conflict: None,
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
        app.rebuild_files();
        app.file_cursor = 0;
        // The reader scrolled away from the cursor to look at something.
        app.file_scroll = 2;

        app.apply_quiet(
            QuietOutcome::Local(Box::new(LocalOpenedData {
                branch: app.local_branch.clone(),
                head: Some(app.merge_base.clone()),
                files: vec![cf("a.rs"), cf("b.rs"), cf("c.rs")],
                stage: HashMap::new(),
                merge_op: None,
                tracking: None,
                stack_base: None,
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

    /// The editor does not own the window. Refreshing under it re-reads
    /// the change and leaves the buffer's own text alone — and a reader
    /// waiting on an agent's edits is exactly who needs this while a file
    /// is open.
    #[test]
    fn refresh_review_works_with_the_editor_open() {
        let mut app = idle_local_app();
        let mut ed = Editor::new("test.rs", PathBuf::from("test.rs"), "x\n");
        ed.dirty = true;
        app.editor = Some(ed);
        app.refresh_review();
        assert!(app.refreshing(), "the re-scan ran: {}", app.status);
        assert!(!app.status_err, "{}", app.status);
        assert!(
            app.editor.as_ref().is_some_and(|e| e.dirty),
            "and the unsaved buffer is untouched"
        );
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

    /// A local review with three files and a repository behind it, so the
    /// whole-index commands have an index to write to.
    fn staging_app(dir: &std::path::Path) -> App {
        init_repo(dir);
        let mut app = App::new(LaunchMode::Local, None);
        app.local = true;
        app.checked_out = true;
        app.screen = Screen::Review;
        app.repo_root = dir.to_path_buf();
        app.tree_view = false;
        app.files = ["a.rs", "b.rs", "c.rs"]
            .iter()
            .map(|p| ChangedFile {
                path: (*p).into(),
                status: "modified".into(),
                additions: 1,
                deletions: 0,
                previous: None,
                conflicted: false,
            })
            .collect();
        app.rebuild_files();
        app
    }

    /// The paths the panel lists, in the order it lists them, with each
    /// heading named. This is what the reader sees, so it is what the
    /// section tests assert on.
    fn panel_rows(app: &App) -> Vec<String> {
        app.entries
            .iter()
            .map(|e| match e {
                FileEntry::StageHeading { section, count, .. } => {
                    format!("# {} {count}", section.title())
                }
                FileEntry::ConflictHeading { count } => format!("# CONFLICT {count}"),
                FileEntry::Dir { path, .. } => format!("/ {path}"),
                FileEntry::File { src, .. } => app.row_path(*src).unwrap_or("?").to_string(),
                FileEntry::CommitRow { idx, open } => {
                    let arrow = if *open { "▾" } else { "▸" };
                    let c = &app.commits[*idx];
                    format!("{arrow} {} {}", c.short, c.subject)
                }
                FileEntry::CommitFileRow { commit, file } => app
                    .files_of_commit(*commit)
                    .and_then(|fs| fs.get(*file))
                    .map(|f| format!("    {}", f.path))
                    .unwrap_or_else(|| "    ?".into()),
                FileEntry::CommitNote(note) => format!("… {note}"),
                FileEntry::StackRow { idx } => {
                    let e = &app.stack.as_ref().unwrap().entries[*idx];
                    let here = app.stack.as_ref().unwrap().position == e.position;
                    format!(
                        "{} {} #{} {}",
                        if here { "◀" } else { " " },
                        e.position,
                        e.number,
                        e.title
                    )
                }
                FileEntry::StackTrunk => format!(
                    "⌄ {}",
                    app.stack
                        .as_ref()
                        .map(|st| st.base_ref_name.as_str())
                        .unwrap_or("?")
                ),
                FileEntry::StackNote(note) => format!("… {note}"),
            })
            .collect()
    }

    /// The whole point of the split: staging a file moves its row out of
    /// the bottom section and into the top one.
    #[test]
    fn staging_a_file_moves_it_up_into_the_staged_section() {
        let dir = TempDir::new("stage-sections");
        let mut app = staging_app(&dir.0);

        assert_eq!(
            panel_rows(&app),
            ["# STAGED 0", "# UNSTAGED 3", "a.rs", "b.rs", "c.rs"],
            "everything starts unstaged, under the bottom heading"
        );

        app.stage.insert("b.rs".into(), StageState::Staged);
        app.rebuild_entries();
        assert_eq!(
            panel_rows(&app),
            ["# STAGED 1", "b.rs", "# UNSTAGED 2", "a.rs", "c.rs"],
            "the staged file is now above the unstaged ones"
        );

        // Partly staged counts as staged: it is in the index, and the [±]
        // icon is what says the rest of it is not.
        app.stage.insert("a.rs".into(), StageState::Partial);
        app.rebuild_entries();
        assert_eq!(
            panel_rows(&app),
            ["# STAGED 2", "a.rs", "b.rs", "# UNSTAGED 1", "c.rs"],
        );
        assert_eq!(app.staged_count(), 2, "the title agrees with the heading");
    }

    /// A folded section keeps its heading and takes its files away.
    #[test]
    fn folding_a_section_hides_only_its_own_files() {
        let dir = TempDir::new("stage-fold");
        let mut app = staging_app(&dir.0);
        app.stage.insert("b.rs".into(), StageState::Staged);
        app.rebuild_entries();

        app.toggle_section(StageSection::Unstaged);
        assert_eq!(
            panel_rows(&app),
            ["# STAGED 1", "b.rs", "# UNSTAGED 2"],
            "the unstaged files are away, the staged one stays"
        );

        app.toggle_section(StageSection::Unstaged);
        assert_eq!(panel_rows(&app).len(), 5, "and they come back");
    }

    /// A directory the filter emptied is dropped rather than drawn with
    /// nothing under it.
    #[test]
    fn the_tree_drops_a_directory_a_section_emptied() {
        let dir = TempDir::new("stage-tree");
        let mut app = staging_app(&dir.0);
        app.tree_view = true;
        app.files = ["src/one.rs", "docs/two.md"]
            .iter()
            .map(|p| ChangedFile {
                path: (*p).into(),
                status: "modified".into(),
                additions: 1,
                deletions: 0,
                previous: None,
                conflicted: false,
            })
            .collect();
        app.stage.insert("src/one.rs".into(), StageState::Staged);
        app.rebuild_files();

        assert_eq!(
            panel_rows(&app),
            [
                "# STAGED 1",
                "/ src",
                "src/one.rs",
                "# UNSTAGED 1",
                "/ docs",
                "docs/two.md",
            ],
            "each section draws only the directories it has files in"
        );
    }

    /// `j` and `k` follow the rows on screen. With the sections in the way,
    /// stepping through `files` in index order would jump between them.
    #[test]
    fn the_file_cursor_follows_the_rows_not_the_list() {
        let dir = TempDir::new("stage-step");
        let mut app = staging_app(&dir.0);
        // a.rs and c.rs staged, b.rs not: index order is a, b, c but row
        // order is a, c, b.
        app.stage.insert("a.rs".into(), StageState::Staged);
        app.stage.insert("c.rs".into(), StageState::Staged);
        app.rebuild_entries();
        assert_eq!(
            panel_rows(&app),
            ["# STAGED 2", "a.rs", "c.rs", "# UNSTAGED 1", "b.rs"]
        );

        app.file_cursor = 0;
        app.step_file(1);
        assert_eq!(
            app.job.as_ref().map(|j| j.label.clone()),
            Some("Loading c.rs".to_string()),
            "the next row down is c.rs, not b.rs"
        );
    }

    /// ✚ all moves every row up at once; ↩ all sends them all back down.
    ///
    /// The panel moves before git answers, so this is the optimistic half:
    /// `the_whole_index_moves_at_once` is the half that checks git.
    #[test]
    fn the_heading_actions_move_the_whole_section() {
        let dir = TempDir::new("stage-all");
        let mut app = staging_app(&dir.0);

        app.stage_all();
        assert_eq!(app.section_count(StageSection::Staged), 3);
        assert_eq!(app.section_count(StageSection::Unstaged), 0);
        assert!(app.status.contains("Staged 3 files"), "{}", app.status);
        assert_eq!(
            panel_rows(&app),
            ["# STAGED 3", "a.rs", "b.rs", "c.rs", "# UNSTAGED 0"],
        );

        app.unstage_all();
        assert_eq!(app.section_count(StageSection::Staged), 0);
        assert!(app.status.contains("Unstaged 3 files"), "{}", app.status);
    }

    /// The git half: `git add -A` then `git reset`, against a real index.
    #[test]
    fn the_whole_index_moves_at_once() {
        let dir = TempDir::new("stage-git");
        init_repo(&dir.0);
        for name in ["a.rs", "b.rs"] {
            std::fs::write(dir.0.join(name), "one\n").unwrap();
        }

        gitops::stage_all(&dir.0).unwrap();
        let states = gitops::stage_states(&dir.0).unwrap();
        assert_eq!(states.get("a.rs"), Some(&StageState::Staged));
        assert_eq!(states.get("b.rs"), Some(&StageState::Staged));

        // Works in a repository with no commits, which is where a bare
        // `git reset` would fail for want of a HEAD to resolve.
        gitops::unstage_all(&dir.0).unwrap();
        let states = gitops::stage_states(&dir.0).unwrap();
        assert_eq!(states.get("a.rs"), Some(&StageState::Unstaged));
        // And the files are still on disk: unstaging never touches them.
        assert!(dir.0.join("a.rs").exists());
    }

    /// `X` asks the whole-change question with one key: stage what is left,
    /// or take it all back out when there is nothing left to stage.
    #[test]
    fn the_x_key_toggles_the_whole_change() {
        let dir = TempDir::new("stage-x");
        let mut app = staging_app(&dir.0);
        app.toggle_stage_all();
        assert_eq!(app.section_count(StageSection::Staged), 3);
        app.toggle_stage_all();
        assert_eq!(app.section_count(StageSection::Staged), 0);
    }

    /// Staging everything mid-merge would mark every conflict resolved
    /// without anybody resolving one, so it is refused.
    #[test]
    fn stage_all_refuses_while_a_conflict_is_open() {
        let dir = TempDir::new("stage-conflict");
        let mut app = staging_app(&dir.0);
        app.files.push(cf_conflicted("merge.txt"));
        app.rebuild_files();

        app.stage_all();
        assert!(app.status_err);
        assert!(
            app.status.contains("Resolve the merge conflicts first"),
            "{}",
            app.status
        );
        assert_eq!(app.section_count(StageSection::Staged), 0);
    }

    // ----------------------------------------------------- the commits

    /// A repository with three commits, the first of them standing in for
    /// the upstream, and an app looking at it in local review.
    fn commits_app(dir: &std::path::Path) -> (App, String) {
        init_repo(dir);
        let d = dir.to_string_lossy().into_owned();
        let commit = |name: &str, body: &str, message: &str| {
            std::fs::write(dir.join(name), body).unwrap();
            gitops::run_git(&["-C", &d, "add", "-A"]).unwrap();
            gitops::run_git(&["-C", &d, "commit", "-q", "-m", message]).unwrap();
        };
        commit("a.txt", "one\n", "first");
        let base = gitops::run_git(&["-C", &d, "rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        commit("a.txt", "two\n", "second");
        commit("b.txt", "new\n", "third");

        let mut app = App::new(LaunchMode::Local, None);
        app.local = true;
        app.checked_out = true;
        app.screen = Screen::Review;
        app.repo_root = dir.to_path_buf();
        (app, base)
    }

    /// Read the commit list the way the panel does, and wait for it.
    fn load_commits(app: &mut App, base: &str) {
        let commits = gitops::unpushed_commits(&app.repo_root, base).unwrap();
        app.apply_commits(CommitsData {
            base: Some(base.to_string()),
            commits,
        });
    }

    /// The panel lists what the upstream does not have, newest first, and
    /// each commit opens to its own files.
    #[test]
    fn the_commits_panel_opens_a_commit_to_its_files() {
        let dir = TempDir::new("commits-open");
        let (mut app, base) = commits_app(&dir.0);
        app.set_panel(PanelMode::Commits);
        load_commits(&mut app, &base);

        let rows = panel_rows(&app);
        assert_eq!(rows.len(), 2, "two commits since the base: {rows:?}");
        assert!(rows[0].starts_with('▸') && rows[0].ends_with("third"));
        assert!(rows[1].ends_with("second"));

        // Opening one reads its files and lists them under it.
        app.toggle_commit(0);
        let rows = panel_rows(&app);
        assert_eq!(
            rows.len(),
            3,
            "the newest commit's one file is under it: {rows:?}"
        );
        assert!(rows[0].starts_with('▾'));
        assert_eq!(rows[1], "    b.txt");
        assert!(rows[2].ends_with("second"), "the next commit follows it");

        // And closing it takes it away again.
        app.toggle_commit(0);
        assert_eq!(panel_rows(&app).len(), 2);
    }

    /// A file out of the panel is read against the commit's own first
    /// parent, not against the working tree — the copy on disk is a later
    /// version of the same file.
    #[test]
    fn a_commit_file_diffs_against_its_parent() {
        let dir = TempDir::new("commits-diff");
        let (mut app, base) = commits_app(&dir.0);
        app.set_panel(PanelMode::Commits);
        load_commits(&mut app, &base);
        app.toggle_commit(1);
        app.open_commit_file(1, 0);

        // The load runs on its own thread; wait for it the way the app
        // does.
        for _ in 0..200 {
            app.poll_jobs();
            if app.job.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(app.job.is_none(), "the load finished: {}", app.status);

        let oc = app.open_commit.as_ref().expect("a commit is open");
        assert_eq!(oc.file, 0);
        assert!(!oc.parent.is_empty(), "it knows its own old side");
        assert_eq!(app.open_file().map(|f| f.path.as_str()), Some("a.txt"));
        assert_eq!(app.old_content.as_deref(), Some("one\n"));
        assert_eq!(
            app.new_content.as_deref(),
            Some("two\n"),
            "the new side is the file as that commit left it"
        );

        // Nothing here is a change anyone can put back or edit.
        assert!(!app.can_revert());
        app.open_editor(None);
        assert!(app.status_err);
        assert!(app.status.contains("is a commit"), "{}", app.status);

        // Back to the change: the pane stops being a commit's.
        app.set_panel(PanelMode::Changes);
        app.files = vec![ChangedFile {
            path: "a.txt".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            previous: None,
            conflicted: false,
        }];
        app.rebuild_files();
        app.spawn_load_file(0);
        assert!(app.open_commit.is_none());
    }

    /// A branch with no upstream has nothing to measure against, and the
    /// panel says so rather than showing an empty list.
    #[test]
    fn the_commits_panel_says_why_it_is_empty() {
        let dir = TempDir::new("commits-empty");
        let (mut app, base) = commits_app(&dir.0);
        app.set_panel(PanelMode::Commits);

        app.apply_commits(CommitsData {
            base: None,
            commits: Vec::new(),
        });
        let rows = panel_rows(&app);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains("No upstream"), "{rows:?}");

        // Level with the upstream is a different sentence.
        app.apply_commits(CommitsData {
            base: Some("origin/main".into()),
            commits: Vec::new(),
        });
        assert!(panel_rows(&app)[0].contains("Nothing to push"));

        // And a real list clears it.
        load_commits(&mut app, &base);
        assert!(app.commits_note.is_none());
        assert_eq!(panel_rows(&app).len(), 2);
    }

    /// The whole thing through the real doors, against a real repository:
    /// scan the working tree, read the index, split the panel, then walk
    /// to the commits and open one.
    #[test]
    fn a_real_repository_fills_both_panels() {
        let dir = TempDir::new("end-to-end");
        let (mut app, base) = commits_app(&dir.0);
        let d = dir.0.to_string_lossy().into_owned();
        // One staged edit, one untracked file.
        std::fs::write(
            dir.0.join("a.txt"),
            "two
more
",
        )
        .unwrap();
        std::fs::write(
            dir.0.join("c.txt"),
            "loose
",
        )
        .unwrap();
        gitops::run_git(&["-C", &d, "add", "a.txt"]).unwrap();

        // The index, read by the same call the panel reads it with.
        app.files = ["a.txt", "c.txt"]
            .iter()
            .map(|p| ChangedFile {
                path: (*p).into(),
                status: "modified".into(),
                additions: 1,
                deletions: 0,
                previous: None,
                conflicted: false,
            })
            .collect();
        app.stage = gitops::stage_states(&dir.0).unwrap();
        app.rebuild_files();
        assert_eq!(
            panel_rows(&app),
            ["# STAGED 1", "a.txt", "# UNSTAGED 1", "c.txt"],
            "git status put each file in its own half"
        );
        assert_eq!(app.staged_count(), 1);

        // And the commits, read the same way the panel reads them.
        app.set_panel(PanelMode::Commits);
        load_commits(&mut app, &base);
        app.toggle_commit(0);
        let rows = panel_rows(&app);
        assert!(rows[0].ends_with("third"), "{rows:?}");
        assert_eq!(rows[1], "    b.txt", "the commit's own file: {rows:?}");

        // Back to the change, and the sections are as they were.
        app.set_panel(PanelMode::Changes);
        assert_eq!(panel_rows(&app).len(), 4);
    }

    /// F walks the three panels and comes back round.
    #[test]
    fn the_panel_key_walks_all_three() {
        let dir = TempDir::new("commits-toggle");
        let (mut app, _) = commits_app(&dir.0);
        assert_eq!(app.panel, PanelMode::Changes);
        app.toggle_panel();
        assert_eq!(app.panel, PanelMode::Files);
        app.toggle_panel();
        assert_eq!(app.panel, PanelMode::Commits);
        app.toggle_panel();
        assert_eq!(app.panel, PanelMode::Changes);
    }

    // ------------------------------------------------------------- stash

    /// The labels of every line the open menu offers.
    fn menu_labels(app: &App) -> Vec<String> {
        let Overlay::Menu(menu) = &app.overlay else {
            panic!("no menu is open");
        };
        menu.rows
            .iter()
            .map(|r| match r {
                MenuRow::Heading(h) => format!("# {h}"),
                MenuRow::Item(it) => it.label.clone(),
            })
            .collect()
    }

    /// The stash menu offers the three scopes, and greys the ones there is
    /// nothing to do — stashing the staged files with nothing staged.
    #[test]
    fn the_stash_menu_offers_three_scopes() {
        let dir = TempDir::new("stash-menu");
        let mut app = staging_app(&dir.0);
        app.open_stash_menu(0, 0);

        let labels = menu_labels(&app);
        assert!(labels
            .iter()
            .any(|l| l.contains("Stash the tracked changes")));
        assert!(labels.iter().any(|l| l.contains("untracked files too")));
        assert!(labels
            .iter()
            .any(|l| l.contains("Stash the staged files only (0)")));
        assert!(
            labels.iter().any(|l| l == "# NO STASHES YET"),
            "a fresh repository has none: {labels:?}"
        );

        let Overlay::Menu(menu) = &app.overlay else {
            panic!("the menu is open");
        };
        let staged_only = menu
            .rows
            .iter()
            .find_map(|r| match r {
                MenuRow::Item(it) if it.id == ButtonId::StashPush(StashScope::StagedOnly) => {
                    Some(it)
                }
                _ => None,
            })
            .expect("the staged-only line is there");
        assert!(
            !staged_only.enabled,
            "with nothing staged there is nothing for it to take"
        );
    }

    /// Every scope goes through the name box, and an empty box is a real
    /// answer — git names the stash itself.
    #[test]
    fn stashing_asks_for_a_name_and_takes_none() {
        let dir = TempDir::new("stash-name");
        let mut app = staging_app(&dir.0);
        app.open_stash_menu(0, 0);
        app.activate(ButtonId::StashPush(StashScope::WithUntracked));

        let Overlay::OpenPath(box_) = &app.overlay else {
            panic!("the name box opens first, status: {}", app.status);
        };
        assert_eq!(
            box_.kind,
            PathBoxKind::StashName {
                scope: StashScope::WithUntracked
            }
        );
        assert!(
            app.status
                .contains("Enter with nothing typed lets git name it"),
            "{}",
            app.status
        );

        // Enter with an empty box runs the stash rather than cancelling.
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.job.is_some(), "the stash is running");
    }

    /// Stashing belongs to local review. A pull request has no working
    /// tree of yours to put aside.
    #[test]
    fn the_stash_menu_is_local_only() {
        let dir = TempDir::new("stash-pr");
        let mut app = staging_app(&dir.0);
        app.local = false;
        app.open_stash_menu(0, 0);
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.status_err);
        assert!(app.status.contains("local changes"), "{}", app.status);
    }

    /// A stash in the list opens a second menu, so every stash carries
    /// apply, pop and drop without the first menu carrying three lines
    /// for each of them.
    #[test]
    fn picking_a_stash_offers_apply_pop_and_drop() {
        let dir = TempDir::new("stash-pick");
        let mut app = staging_app(&dir.0);
        app.stashes = vec![Stash {
            index: 0,
            name: "stash@{0}".into(),
            subject: "On main: fix the parser".into(),
            when: "2 hours ago".into(),
        }];

        app.open_stash_item_menu(0, 0, 0);
        let labels = menu_labels(&app);
        assert!(
            labels.iter().any(|l| l.contains("keep the stash")),
            "{labels:?}"
        );
        assert!(
            labels.iter().any(|l| l.contains("drop the stash")),
            "{labels:?}"
        );
        assert!(
            labels.iter().any(|l| l.contains("cannot be undone")),
            "dropping says what it costs: {labels:?}"
        );

        // A stash that went away between the two menus says so rather
        // than acting on whatever now sits at that position.
        app.stashes.clear();
        app.open_stash_item_menu(0, 0, 0);
        assert!(app.status_err);
        assert!(app.status.contains("gone"), "{}", app.status);
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

    /// A three-deep stack with the reader on the middle pull request.
    fn stack_of_three() -> github::Stack {
        let pr =
            |position: u32, number: u64, title: &str, state: &str, base: &str| github::StackPr {
                position,
                number,
                title: title.into(),
                state: state.into(),
                is_draft: false,
                review_decision: None,
                head_ref_name: format!("branch-{number}"),
                base_ref_name: base.into(),
                url: format!("https://github.com/o/r/pull/{number}"),
            };
        github::Stack {
            number: 7,
            size: 3,
            base_ref_name: "main".into(),
            entries: vec![
                pr(1, 42, "Add the lexer", "MERGED", "main"),
                pr(2, 43, "Extract the parser", "OPEN", "branch-42"),
                pr(3, 44, "Wire it up", "OPEN", "branch-43"),
            ],
            position: 2,
        }
    }

    /// A pull request review with a stack under it.
    fn stacked_app() -> App {
        let mut app = App::new(LaunchMode::Pr, None);
        app.screen = Screen::Review;
        app.local = false;
        app.checked_out = true;
        app.pr = Some(pr_detail(43));
        app.stack = Some(stack_of_three());
        app
    }

    /// A stack is drawn the way it is talked about: trunk at the bottom,
    /// each pull request resting on the one under it.
    #[test]
    fn the_stack_panel_is_a_ladder_with_the_trunk_at_its_foot() {
        let mut app = stacked_app();
        app.set_panel(PanelMode::Stack);
        assert_eq!(
            panel_rows(&app),
            vec![
                "  3 #44 Wire it up",
                "◀ 2 #43 Extract the parser",
                "  1 #42 Add the lexer",
                "⌄ main",
            ]
        );
    }

    /// The message names what this link rests on, because that is the
    /// branch its diff is read against and the pull request that has to
    /// land before it can.
    #[test]
    fn opening_the_panel_says_what_this_one_sits_on() {
        let mut app = stacked_app();
        app.set_panel(PanelMode::Stack);
        assert!(app.status.contains("2 of 3"), "{}", app.status);
        assert!(
            app.status.contains("sits on #42 Add the lexer"),
            "{}",
            app.status
        );
    }

    /// …and the one at the bottom sits on the trunk itself.
    #[test]
    fn the_bottom_of_the_stack_sits_on_the_trunk() {
        let mut app = stacked_app();
        if let Some(st) = app.stack.as_mut() {
            st.position = 1;
        }
        app.set_panel(PanelMode::Stack);
        assert!(app.status.contains("straight onto main"), "{}", app.status);
    }

    /// `F` gains a fourth stop only where there is a stack. A reader with
    /// an ordinary pull request open must not pay for the feature with an
    /// empty panel between two useful ones.
    #[test]
    fn f_reaches_the_stack_only_when_there_is_one() {
        let mut app = stacked_app();
        app.toggle_panel();
        assert_eq!(app.panel, PanelMode::Files);
        app.toggle_panel();
        assert_eq!(app.panel, PanelMode::Commits);
        app.toggle_panel();
        assert_eq!(app.panel, PanelMode::Stack);
        app.toggle_panel();
        assert_eq!(app.panel, PanelMode::Changes);

        let mut plain = stacked_app();
        plain.stack = None;
        plain.set_panel(PanelMode::Commits);
        plain.toggle_panel();
        assert_eq!(plain.panel, PanelMode::Changes, "no fourth stop");
    }

    /// Moving up a stack usually means checking the branch out, and a
    /// click that rewrote the working tree without asking would be one
    /// nobody could take back. It goes through the prompt the pull
    /// request list already uses.
    #[test]
    fn opening_another_pull_request_in_the_stack_asks_first() {
        let mut app = stacked_app();
        app.set_panel(PanelMode::Stack);
        app.open_stack_pr(2);
        assert!(
            matches!(app.overlay, Overlay::CheckoutPrompt(44)),
            "the checkout prompt opens for #44"
        );
    }

    /// The one already on screen is not a target.
    #[test]
    fn the_stack_row_you_are_on_says_so() {
        let mut app = stacked_app();
        app.set_panel(PanelMode::Stack);
        app.open_stack_pr(1);
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.status.contains("already open"), "{}", app.status);
    }

    /// `Alt+↑` and `Alt+↓` walk the chain, the way `gh stack up` and
    /// `gh stack down` do, and stop at both ends rather than wrapping.
    #[test]
    fn alt_arrows_walk_the_stack_and_stop_at_its_ends() {
        let mut app = stacked_app();
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
        assert!(
            matches!(app.overlay, Overlay::CheckoutPrompt(44)),
            "up is away from the trunk"
        );
        app.overlay = Overlay::None;
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT));
        assert!(
            matches!(app.overlay, Overlay::CheckoutPrompt(42)),
            "down is toward it"
        );

        // At the top there is nothing above.
        app.overlay = Overlay::None;
        if let Some(st) = app.stack.as_mut() {
            st.position = 3;
        }
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.status.contains("Top of the stack"), "{}", app.status);

        // …and at the bottom, nothing below.
        if let Some(st) = app.stack.as_mut() {
            st.position = 1;
        }
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT));
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.status.contains("Bottom of the stack"), "{}", app.status);
    }

    /// A plain arrow still moves the diff cursor; only the modifier means
    /// the stack.
    #[test]
    fn a_plain_arrow_is_still_the_diff_cursor() {
        let mut app = stacked_app();
        app.diff = Some(FileDiff::compute(Some("a\nb\nc\n"), Some("a\nB\nc\n")));
        app.rebuild_display();
        app.diff_cursor = 0;
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.diff_cursor, 1);
        assert!(matches!(app.overlay, Overlay::None));
    }

    /// The bug this menu exists for: the panel's button row drops what it
    /// has no room for, from the left, and the change is leftmost. A
    /// narrow panel could reach every section except the one everybody
    /// comes back to.
    #[test]
    fn the_sections_menu_lists_every_section_and_marks_the_one_you_are_on() {
        let mut app = stacked_app();
        app.set_panel(PanelMode::Commits);
        let ids: Vec<ButtonId> = app
            .section_items()
            .iter()
            .filter_map(|r| match r {
                MenuRow::Item(it) => Some(it.id),
                _ => None,
            })
            .collect();
        assert_eq!(
            ids,
            vec![
                ButtonId::PanelChanges,
                ButtonId::PanelFiles,
                ButtonId::PanelCommits,
                ButtonId::PanelStack,
            ],
            "the change is in the list, and it is first"
        );
        let checked: Vec<bool> = app
            .section_items()
            .iter()
            .filter_map(|r| match r {
                MenuRow::Item(it) => it.checked,
                _ => None,
            })
            .collect();
        assert_eq!(checked, vec![false, false, true, false]);
    }

    /// …and the way back actually works from it.
    #[test]
    fn the_sections_menu_gets_back_to_the_change() {
        let mut app = stacked_app();
        app.set_panel(PanelMode::Files);
        assert_eq!(app.panel, PanelMode::Files);
        app.activate(ButtonId::PanelChanges);
        assert_eq!(app.panel, PanelMode::Changes);
    }

    /// The stack line is only there where there is a stack.
    #[test]
    fn the_sections_menu_leaves_out_a_stack_that_is_not_there() {
        let mut app = stacked_app();
        app.stack = None;
        let ids: Vec<ButtonId> = app
            .section_items()
            .iter()
            .filter_map(|r| match r {
                MenuRow::Item(it) => Some(it.id),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 3);
        assert!(!ids.contains(&ButtonId::PanelStack));
    }

    /// The ☰ menu had no way back to the change either — its three panel
    /// lines were Files, Commits and Stack. It carries the same list now.
    #[test]
    fn the_main_menu_can_also_get_back_to_the_change() {
        let mut app = stacked_app();
        app.set_panel(PanelMode::Files);
        let has_change = app
            .build_menu()
            .iter()
            .any(|r| matches!(r, MenuRow::Item(it) if it.id == ButtonId::PanelChanges));
        assert!(has_change, "the ☰ menu offers the change");
    }

    /// The label the collapsed button wears says where you are, so the
    /// row still answers "which section is this" with one button.
    #[test]
    fn the_collapsed_button_names_the_section() {
        let mut app = stacked_app();
        assert_eq!(app.section_label(), "Change");
        app.set_panel(PanelMode::Files);
        assert_eq!(app.section_label(), "Files");
        app.set_panel(PanelMode::Commits);
        assert_eq!(app.section_label(), "Commits");
        app.set_panel(PanelMode::Stack);
        assert_eq!(app.section_label(), "Stack");
    }

    /// A refresh where the stack went — the last of the chain merged, or
    /// a swap to local review — must not leave the reader in a panel with
    /// nothing behind it.
    #[test]
    fn losing_the_stack_gives_the_panel_back() {
        let mut app = stacked_app();
        app.set_panel(PanelMode::Stack);
        assert_eq!(app.panel, PanelMode::Stack);
        app.set_stack(None);
        assert_eq!(app.panel, PanelMode::Changes);
    }

    fn pr_workspace(number: u64) -> Workspace {
        let old = "x\n".to_string();
        let new = "y\n".to_string();
        Workspace {
            local: false,
            local_branch: None,
            merge_op: None,
            tracking: None,
            conflict: None,
            pr: Some(pr_detail(number)),
            checked_out: true,
            stack: None,
            merge_base: "c".repeat(40),
            stack_base: "c".repeat(40),
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
            stack: None,
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
            conflict: None,
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
                stack: None,
            })),
            false,
        );
        assert!(app.status.contains("up to date"), "{}", app.status);
        assert!(!app.refreshing(), "no chained reload when nothing moved");
        assert!(app.diff.is_some());
    }

    /// The toggle used to refuse with the editor open. It parks the
    /// buffer instead: a file is a file whichever side opened it, and
    /// parking keeps its text, its place in the row and the dot that says
    /// it is unsaved.
    #[test]
    fn swapping_parks_the_editor_rather_than_refusing() {
        let mut app = folded_app();
        app.local = true;
        app.stash = Some(Box::new(pr_workspace(7)));
        let mut ed = Editor::new("test.rs", PathBuf::from("test.rs"), "x\n");
        ed.dirty = true;
        app.open_buffer(ed);

        app.toggle_workspace();
        assert!(!app.local, "the swap happened");
        assert!(app.editor.is_none(), "the buffer is not in the way");
        assert!(
            app.buffers().any(|e| e.path == "test.rs" && e.dirty),
            "and it is still open, still unsaved"
        );
    }

    #[test]
    fn collapsed_dir_hides_subtree() {
        let files = vec![cf("src/app/x.rs"), cf("README.md")];
        // "src" holds a single child dir and no files, so its row is the
        // compressed "src/app" — that path is also the collapse key.
        let mut collapsed = HashSet::new();
        collapsed.insert("src/app".to_string());
        let mut entries = Vec::new();
        TreeNodes::build(&files).emit(&collapsed, &mut entries);
        assert_eq!(
            entries,
            vec![
                FileEntry::Dir {
                    label: "src/app".into(),
                    path: "src/app".into(),
                    depth: 0
                },
                FileEntry::File {
                    src: RowSrc::Changed(1),
                    depth: 0
                },
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
        app.rebuild_files();
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

    // ------------------------------------------- nothing is locked down

    /// A local review with a document open over it.
    fn preview_app() -> App {
        let mut app = folded_app();
        app.local = true;
        app.repo_root = std::env::temp_dir();
        app.preview = Some(crate::preview::Preview::new(
            "PLAN.md",
            "/repo/PLAN.md".into(),
            "# Plan\n\nBody.\n",
        ));
        app.layout.badge = Rect::new(0, 0, 10, 1);
        app
    }

    /// The bug this audit started from: right-clicking the PR badge to
    /// copy the link did nothing while a document was open, because the
    /// preview answered its own clicks and never learned about the badge.
    #[test]
    fn the_badge_copies_the_pr_link_from_every_mode() {
        for (what, mut app) in [
            ("the diff", {
                let mut a = folded_app();
                a.layout.badge = Rect::new(0, 0, 10, 1);
                a
            }),
            ("a document", preview_app()),
        ] {
            // No PR here, so the answer is the refusal rather than a
            // copy — but it is an answer, which is the whole point.
            app.local = true;
            app.pr = None;
            app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Right), 2, 0));
            assert!(
                app.status.contains("No PR link here"),
                "the badge answered in {what}: {}",
                app.status
            );
        }
    }

    /// Reading a document is not a reason to be locked out of the app.
    /// Every key here used to do nothing at all with the preview open.
    #[test]
    fn the_document_answers_the_app_level_keys() {
        let mut app = preview_app();
        app.handle_key(key(KeyCode::Char('F')));
        assert_eq!(app.panel, PanelMode::Files, "F walks the panels");

        let mut app = preview_app();
        app.handle_key(key(KeyCode::Char('#')));
        assert!(
            matches!(&app.overlay, Overlay::Finder(f) if f.mode == FinderMode::Grep),
            "# opens the grep"
        );

        let mut app = preview_app();
        app.handle_key(key(KeyCode::Char('@')));
        assert!(
            matches!(&app.overlay, Overlay::Finder(_)),
            "@ opens symbols"
        );

        let mut app = preview_app();
        app.handle_key(key(KeyCode::Char('X')));
        assert!(
            app.status.contains("Staged") || app.status.contains("staged"),
            "X stages the change: {}",
            app.status
        );

        let mut app = preview_app();
        app.handle_key(key(KeyCode::Char('S')));
        assert!(
            matches!(&app.overlay, Overlay::Menu(m) if m.title.contains("Stash")),
            "S opens the stash menu"
        );

        let mut app = preview_app();
        app.handle_key(key(KeyCode::Char('b')));
        assert_eq!(app.screen, Screen::PrList, "b goes back to the list");
    }

    /// The document's own keys still win over the app's. Ctrl+B pages
    /// back there; it must not read as "back to the pull request list".
    #[test]
    fn the_documents_own_keys_come_first() {
        let mut app = preview_app();
        app.handle_key(ctrl(KeyCode::Char('b')));
        assert_eq!(app.screen, Screen::Review, "Ctrl+B paged, it did not leave");

        // Esc closes the document rather than leaving the review.
        let mut app = preview_app();
        app.handle_key(key(KeyCode::Esc));
        assert!(app.preview.is_none(), "Esc closed the document");
        assert_eq!(app.screen, Screen::Review);

        // `r` re-reads the document, not the review.
        let mut app = preview_app();
        app.handle_key(key(KeyCode::Char('r')));
        assert!(
            !app.status.contains("Rescanning"),
            "r reloaded the document: {}",
            app.status
        );
    }

    /// The editor takes bare letters as text, so the ☰ menu is how a
    /// reader in a buffer reaches everything else — and it has to be
    /// reachable without the mouse.
    #[test]
    fn the_editor_can_open_the_menu_and_it_carries_the_way_out() {
        let mut app = folded_app();
        app.local = true;
        app.checked_out = true;
        app.open_buffer(buf("test.rs"));
        assert!(app.editor.is_some(), "the editor opened: {}", app.status);

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        let Overlay::Menu(menu) = &app.overlay else {
            panic!("Alt+Enter opens the menu, status: {}", app.status);
        };
        let ids: Vec<ButtonId> = menu
            .rows
            .iter()
            .filter_map(|r| match r {
                MenuRow::Item(it) => Some(it.id),
                _ => None,
            })
            .collect();
        for want in [
            ButtonId::SwapView,
            ButtonId::BackToPrs,
            ButtonId::StageFile,
            ButtonId::StashMenu,
        ] {
            assert!(ids.contains(&want), "the editor menu offers {want:?}");
        }
    }

    /// Wait for the foreground job the way the app does.
    fn pump(app: &mut App) {
        for _ in 0..400 {
            app.poll_jobs();
            if app.job.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A local review of three real files, with a file panel to click.
    fn tabs_app(dir: &std::path::Path) -> App {
        let mut app = staging_app(dir);
        for n in ["a.rs", "b.rs", "c.rs"] {
            std::fs::write(dir.join(n), "one\ntwo\n").unwrap();
        }
        app.checked_out = true;
        app.layout.file_list = Rect::new(0, 0, 30, 10);
        app
    }

    /// The panel row a file is drawn on.
    fn file_row(app: &App, path: &str) -> u16 {
        app.entries
            .iter()
            .position(
                |e| matches!(e, FileEntry::File { src, .. } if app.row_path(*src) == Some(path)),
            )
            .unwrap_or_else(|| panic!("{path} has no row")) as u16
    }

    fn click_row(app: &mut App, path: &str) {
        let y = file_row(app, path);
        app.file_list_click(10, y);
        pump(app);
    }

    /// One click walks the change and reuses one tab; a double click says
    /// "I am coming back to this one" and the next file gets its own.
    #[test]
    fn diffs_open_in_tabs_that_a_double_click_keeps() {
        let dir = TempDir::new("difftab-keep");
        let mut app = tabs_app(&dir.0);

        click_row(&mut app, "a.rs");
        assert_eq!(app.tab_labels(), ["a.rs"], "one click, one tab");
        assert_eq!(app.peek_path(), Some("a.rs"), "and it is the peek tab");

        // A second file takes that tab over rather than adding to the row.
        click_row(&mut app, "b.rs");
        assert_eq!(
            app.tab_labels(),
            ["b.rs"],
            "the peek tab was reused, not added to"
        );

        // Kept, so the next file gets a tab of its own.
        app.keep_peek();
        click_row(&mut app, "c.rs");
        assert_eq!(app.tab_labels(), ["b.rs", "c.rs"]);
        assert!(
            app.buffer_tabs()[1].peek,
            "c.rs is the one a click replaces"
        );
    }

    /// A double click on a file still loading has to keep it too. The
    /// first click starts the read and the second lands mid-read, which
    /// is where every real double click lands.
    #[test]
    fn a_double_click_lands_mid_read_and_still_keeps() {
        let dir = TempDir::new("difftab-race");
        let mut app = tabs_app(&dir.0);
        click_row(&mut app, "a.rs");

        // Both clicks before the read is polled, the way the terminal
        // delivers them.
        let y = file_row(&app, "b.rs");
        app.file_list_click(10, y);
        app.file_list_click(10, y);
        assert!(app.job.is_some(), "the read is still in flight");
        pump(&mut app);

        assert_eq!(app.tab_labels(), ["b.rs"], "b.rs took a.rs's peek tab");
        assert_eq!(app.peek_path(), None, "and the double click kept it");

        click_row(&mut app, "c.rs");
        assert_eq!(
            app.tab_labels(),
            ["b.rs", "c.rs"],
            "so the next file got its own"
        );
    }

    /// The whole point: coming back to a tab lands where you left it.
    #[test]
    fn a_diff_tab_keeps_the_place_you_left_it() {
        let dir = TempDir::new("difftab-place");
        let mut app = tabs_app(&dir.0);
        click_row(&mut app, "a.rs");
        app.keep_peek();
        // Somewhere that is not the top.
        app.diff_cursor = 1;
        app.diff_scroll = 1;
        let folds = app.expanded_folds.clone();
        app.expanded_folds.insert(0);

        click_row(&mut app, "b.rs");
        assert_ne!(app.diff_cursor, 1, "the new file starts at its own place");
        assert_eq!(app.expanded_folds, folds, "and with its own folds");

        // Back again — by the tab, the way a click on the row does it.
        app.open_buffer_at(0);
        assert_eq!(app.diff_path(), Some("a.rs"));
        assert_eq!(app.diff_cursor, 1, "the cursor came back");
        assert_eq!(app.diff_scroll, 1, "and the scroll");
        assert!(app.expanded_folds.contains(&0), "and the folds");
        assert!(app.job.is_none(), "and none of it was read again");
    }

    /// Closing a diff tab needs no question — there is nothing unsaved in
    /// one — and leaves the reader on the tab beside it.
    #[test]
    fn closing_a_diff_tab_shows_its_neighbour() {
        let dir = TempDir::new("difftab-close");
        let mut app = tabs_app(&dir.0);
        click_row(&mut app, "a.rs");
        app.keep_peek();
        click_row(&mut app, "b.rs");
        app.keep_peek();
        assert_eq!(app.tab_labels(), ["a.rs", "b.rs"]);

        app.close_buffer_at(1);
        assert_eq!(app.tab_labels(), ["a.rs"]);
        assert_eq!(app.diff_path(), Some("a.rs"), "the neighbour is on screen");
        assert!(app.diff.is_some());
    }

    /// One tab per file, whichever way the file is being read. `e` on a
    /// diff opens the buffer in the tab the diff already had, and Esc
    /// hands it back — the tab never moves under the reader.
    #[test]
    fn one_tab_holds_both_views_of_a_file() {
        let dir = TempDir::new("difftab-both");
        let mut app = tabs_app(&dir.0);
        click_row(&mut app, "a.rs");
        app.keep_peek();
        click_row(&mut app, "b.rs");
        app.keep_peek();
        assert_eq!(app.tab_labels(), ["a.rs", "b.rs"]);

        // Edit the file the diff is showing.
        app.open_editor(None);
        assert_eq!(
            app.editor.as_ref().map(|e| e.path.as_str()),
            Some("b.rs"),
            "the editor opened: {}",
            app.status
        );
        assert_eq!(
            app.tab_labels(),
            ["a.rs", "b.rs"],
            "the buffer took the tab the diff had, it did not add one"
        );

        // And back. The tab is still b.rs's, in the same place.
        app.request_close_editor();
        assert!(app.editor.is_none());
        assert_eq!(app.tab_labels(), ["a.rs", "b.rs"]);
        assert_eq!(app.diff_path(), Some("b.rs"), "the diff is back");
    }

    /// The finder used to refuse with the editor open, so a reader in a
    /// buffer could not search the repository at all.
    #[test]
    fn the_finder_opens_over_the_editor() {
        let mut app = folded_app();
        app.local = true;
        app.checked_out = true;
        app.open_buffer(buf("test.rs"));
        app.open_finder(FinderMode::Grep);
        assert!(
            matches!(&app.overlay, Overlay::Finder(f) if f.mode == FinderMode::Grep),
            "the grep opened: {}",
            app.status
        );
        assert!(app.editor.is_some(), "and the buffer is still open");
    }

    /// Reverting a file nobody has open is nobody's business but that
    /// file's. It used to refuse whenever any buffer was open at all.
    #[test]
    fn reverting_only_stops_for_unsaved_edits_to_that_file() {
        let mut app = folded_app();
        app.local = true;
        app.checked_out = true;
        app.open_buffer(buf("elsewhere.rs"));
        app.ask_revert_file(0);
        assert!(
            matches!(app.overlay, Overlay::Revert(_)),
            "another file's buffer does not block it: {}",
            app.status
        );

        // The file itself, with unsaved edits, is the one case that does.
        app.overlay = Overlay::None;
        let mut ed = buf("test.rs");
        ed.dirty = true;
        app.open_buffer(ed);
        app.ask_revert_file(0);
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.status_err);
        assert!(app.status.contains("unsaved edits open"), "{}", app.status);
    }

    // -------------------------------------- following the terminal

    /// An app with nothing pinned, the way it starts when the config says
    /// nothing about the appearance.
    fn auto_app() -> App {
        let mut app = folded_app();
        app.appearance_auto = true;
        app.last_input = Instant::now() - Duration::from_secs(60);
        app.appearance_asked = Instant::now() - Duration::from_secs(60);
        app
    }

    /// The terminal is only asked while nobody is using it. The query
    /// reads the reply straight off the tty, so a key pressed inside that
    /// window would be eaten by it.
    #[test]
    fn the_terminal_is_only_asked_when_nobody_is_typing() {
        let mut app = auto_app();
        assert!(app.wants_appearance_check(), "idle, nothing open");

        app.last_input = Instant::now();
        assert!(!app.wants_appearance_check(), "somebody just typed");

        let mut app = auto_app();
        app.overlay = Overlay::Help;
        assert!(!app.wants_appearance_check(), "an overlay is open");

        let mut app = auto_app();
        app.open_buffer(buf("test.rs"));
        assert!(!app.wants_appearance_check(), "a buffer is open to type in");

        // And not on a timer faster than the poll interval.
        let mut app = auto_app();
        app.appearance_asked = Instant::now();
        assert!(!app.wants_appearance_check());
    }

    /// Nothing is asked at all when the appearance is pinned — that is
    /// what pinning means.
    #[test]
    fn a_pinned_appearance_is_never_asked_about() {
        let mut app = auto_app();
        app.appearance_auto = false;
        assert!(!app.wants_appearance_check());
    }

    /// The whole point: the terminal turns dark and the window follows,
    /// palette and syntax theme together, without a restart.
    #[test]
    fn the_window_follows_the_terminal() {
        let _guard = crate::highlight::test_theme_lock();
        let mut app = auto_app();
        app.themes = Themes {
            dark: crate::highlight::theme_by_name("catppuccin-mocha"),
            light: crate::highlight::theme_by_name("catppuccin-latte"),
            session: None,
        };
        crate::theme::set_appearance(Appearance::Light);
        crate::highlight::set_theme(app.themes.for_appearance(Appearance::Light));

        let moved = app.appearance_seen(Some(Appearance::Dark));
        assert!(moved, "it followed");
        assert_eq!(crate::theme::appearance(), Appearance::Dark);
        assert_eq!(
            crate::highlight::theme_key(crate::highlight::current_theme()),
            "catppuccin-mocha",
            "and took the dark theme with it"
        );
        assert!(app.status.contains("turned dark"), "{}", app.status);

        // The same answer twice changes nothing, and says nothing.
        app.status.clear();
        assert!(!app.appearance_seen(Some(Appearance::Dark)));
        assert!(app.status.is_empty());

        // No answer is not an answer.
        assert!(!app.appearance_seen(None));
        assert_eq!(crate::theme::appearance(), Appearance::Dark);
    }

    /// A slot that is set wins; an empty one takes the counterpart of the
    /// other, so Gruvbox stays Gruvbox across the change.
    #[test]
    fn each_appearance_gets_its_own_theme() {
        let _guard = crate::highlight::test_theme_lock();
        let both = Themes {
            dark: crate::highlight::theme_by_name("catppuccin-mocha"),
            light: crate::highlight::theme_by_name("catppuccin-latte"),
            session: None,
        };
        let key = |t: two_face::theme::EmbeddedThemeName| crate::highlight::theme_key(t);
        assert_eq!(
            key(both.for_appearance(Appearance::Dark)),
            "catppuccin-mocha"
        );
        assert_eq!(
            key(both.for_appearance(Appearance::Light)),
            "catppuccin-latte"
        );

        // Only one set: the other appearance takes its counterpart.
        let one = Themes {
            dark: crate::highlight::theme_by_name("gruvbox-dark"),
            light: None,
            session: None,
        };
        assert!(
            key(one.for_appearance(Appearance::Light)).starts_with("gruvbox"),
            "got {}",
            key(one.for_appearance(Appearance::Light))
        );

        // `--theme` overrides both, for this session.
        let session = Themes {
            dark: crate::highlight::theme_by_name("catppuccin-mocha"),
            light: crate::highlight::theme_by_name("catppuccin-latte"),
            session: crate::highlight::theme_by_name("nord"),
        };
        assert_eq!(key(session.for_appearance(Appearance::Light)), "nord");
        assert_eq!(key(session.for_appearance(Appearance::Dark)), "nord");
    }

    /// The way out of a pinned appearance, which before this meant
    /// knowing the config file had an `appearance` key and deleting it.
    #[test]
    fn the_picker_can_hand_the_appearance_back_to_the_terminal() {
        let _guard = crate::highlight::test_theme_lock();
        let mut app = folded_app();
        app.appearance_auto = false;
        app.open_theme_picker();
        let Overlay::ThemePicker(tp) = &app.overlay else {
            panic!("the picker is open");
        };
        assert!(!tp.auto, "it starts on what is actually in force");

        app.handle_key(key(KeyCode::Char('A')));
        let Overlay::ThemePicker(tp) = &app.overlay else {
            panic!("still open");
        };
        assert!(tp.auto, "A hands it back to the terminal");

        // And `a` takes it again.
        app.handle_key(key(KeyCode::Char('a')));
        let Overlay::ThemePicker(tp) = &app.overlay else {
            panic!("still open");
        };
        assert!(!tp.auto, "a pins the one on screen");
    }

    // ------------------------------------------ the path behind a tab

    /// The lines a path menu is offering, as (key, label, copied text).
    fn path_menu_lines(app: &App) -> Vec<(char, &'static str, String)> {
        let Overlay::PathMenu(menu) = &app.overlay else {
            panic!("no path menu is open");
        };
        menu.items
            .iter()
            .map(|it| {
                let text = match &it.action {
                    PathAction::Copy(t) => t.clone(),
                    other => format!("{other:?}"),
                };
                (it.key, it.label, text)
            })
            .collect()
    }

    /// A tab is a name, and a name is not something you can hand to a
    /// coding agent. Right-clicking one asks which form you want.
    #[test]
    fn right_clicking_a_tab_offers_both_forms_of_its_path() {
        let dir = TempDir::new("tabmenu-inside");
        let mut app = tabs_app(&dir.0);
        click_row(&mut app, "a.rs");

        app.open_tab_menu(ButtonId::BufferTab(0), 4, 1);
        let lines = path_menu_lines(&app);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0].0, 'r');
        assert_eq!(lines[0].2, "a.rs", "the relative form is what git calls it");
        assert_eq!(lines[1].0, 'f');
        assert!(
            lines[1].2.ends_with("a.rs") && lines[1].2.starts_with('/'),
            "and the full one is absolute: {:?}",
            lines[1].2
        );
    }

    /// The case the feature exists for: a file pinned from somewhere else
    /// on the machine. Its absolute path is shown nowhere else, and there
    /// is nothing for a relative one to be relative to.
    #[test]
    fn a_tab_from_outside_the_repository_offers_its_full_path() {
        let dir = TempDir::new("tabmenu-outside");
        let mut app = tabs_app(&dir.0);
        let away = std::env::temp_dir().join("loupe-tabmenu-elsewhere");
        std::fs::create_dir_all(&away).unwrap();
        let doc = away.join("agent-notes.md");
        std::fs::write(&doc, "# Notes\n").unwrap();
        app.pins
            .add(crate::pins::Pin::new(&app.repo_root, doc.clone()))
            .expect("room for one pin");
        assert!(app.pins.items[0].outside);

        app.open_tab_menu(ButtonId::PinTab(0), 4, 1);
        let lines = path_menu_lines(&app);
        assert_eq!(
            lines.len(),
            1,
            "one line, and it is the useful one: {lines:?}"
        );
        assert_eq!(lines[0].0, 'f');
        assert!(
            lines[0].2.ends_with("agent-notes.md") && lines[0].2.starts_with('/'),
            "{:?}",
            lines[0].2
        );
        std::fs::remove_dir_all(&away).ok();
    }

    /// The ✕ is part of the tab, so the right button asks about the file
    /// there too rather than closing it by surprise.
    #[test]
    fn the_close_mark_answers_the_same_question() {
        let dir = TempDir::new("tabmenu-close");
        let mut app = tabs_app(&dir.0);
        click_row(&mut app, "a.rs");
        app.open_tab_menu(ButtonId::BufferClose(0), 4, 1);
        assert_eq!(path_menu_lines(&app).len(), 2);
        assert_eq!(app.tab_labels(), ["a.rs"], "and nothing was closed");
    }

    /// The key everyone reaches for. `/` still works; Ctrl+F is what a
    /// reader who has never opened vim will try first.
    #[test]
    fn ctrl_f_opens_the_search_in_every_pane() {
        // The diff.
        let mut app = folded_app();
        app.handle_key(ctrl(KeyCode::Char('f')));
        assert!(app.find.typing, "Ctrl+F opened the diff search");
        assert_eq!(app.status, "/");

        // Cmd+F, where the terminal reports it at all.
        app.find.typing = false;
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::SUPER));
        assert!(app.find.typing, "Cmd+F does the same");

        // And with shift it is the repository, not this file.
        app.find.typing = false;
        app.handle_key(KeyEvent::new(
            KeyCode::Char('F'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert!(!app.find.typing, "shift did not open the file search");
        assert!(
            matches!(&app.overlay, Overlay::Finder(f) if f.mode == FinderMode::Grep),
            "it opened the grep instead"
        );
    }

    /// Ctrl+F no longer pages forward in the diff. Everything else that
    /// did still does, which is what makes the trade payable.
    #[test]
    fn the_other_paging_keys_survive_ctrl_f() {
        let mut app = folded_app();
        let page = app.diff_page();
        assert!(page > 1, "the fixture has a pane to page");

        for k in [ctrl(KeyCode::Char('d')), key(KeyCode::PageDown)] {
            app.diff_cursor = 0;
            app.handle_key(k);
            assert!(app.diff_cursor > 0, "{k:?} still moves the cursor down");
        }
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
        app.rebuild_files();
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
        app.rebuild_files();
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
        app.rebuild_files();
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
        app.rebuild_files();
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

    pub fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
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
    /// Row offset of the first file in the panel. Local review heads each
    /// staging section, so a test that clicks "the first file" has to ask
    /// rather than assume row zero.
    fn first_file_row(app: &App) -> u16 {
        app.entries
            .iter()
            .position(|e| matches!(e, FileEntry::File { .. }))
            .expect("the panel lists at least one file") as u16
    }

    fn revert_app() -> App {
        let mut app = App::new(LaunchMode::Local, None);
        app.screen = Screen::Review;
        app.local = true;
        // Local review always has the working tree under it.
        app.checked_out = true;
        app.files = vec![cf("a.txt")];
        app.rebuild_files();
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
        // Local review heads each staging section, so the first file is
        // not the first row.
        let row = first_file_row(&app);
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            fl.x + fl.width - 1,
            fl.y + row,
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
            conflicted: false,
        }];
        app.rebuild_files();
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
        app.rebuild_files();
        app.file_cursor = 1;
        app.diff_scroll = 4;
        let files_before = app.files.len();
        let diff_before = app.display.len();

        app.apply_external(ExternalFile {
            peek: false,
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
            peek: false,
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
        app.rebuild_files();
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
        app.rebuild_files();
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

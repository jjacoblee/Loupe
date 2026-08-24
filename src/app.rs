//! Application state and event handling.
//!
//! Blocking work (gh/git calls, diffing, highlighting) runs on background
//! threads as "jobs"; the main loop keeps drawing (spinner in the status bar)
//! and applies each job's outcome when it lands. Foreground jobs are modal —
//! input is ignored while one runs, except `c`/Esc to cancel (when the job is
//! cancellable) and `q` to quit. Viewed-state syncs to GitHub run as
//! fire-and-forget background jobs with optimistic local state.

use crate::diff::{DisplayEntry, FileDiff, RowKind, Selection, Side};
use crate::editor::Editor;
use crate::github::{self, ChangedFile, CommentSide, PrDetail, PrSummary};
use crate::gitops::{self, StageState};
use crate::highlight::{self, HlLine};
use crate::theme::Appearance;
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Instant;
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
    ViewSplit,
    ViewInline,
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
    Theme,
    ThemeApply,
    ThemeCancel,
    ThemeRow(usize),
    /// The ☀/🌙 light-dark switch in the theme picker.
    AppearanceToggle,
    /// The PR ⇄ local toggle in the review top bar (also the ` key).
    SwapView,
}

/// Clickable regions recorded during the last draw.
#[derive(Default)]
pub struct HitAreas {
    pub buttons: Vec<(Rect, ButtonId)>,
    pub pr_list: Rect,
    pub file_list: Rect,
    pub diff: Rect,
    /// The whole review body (file panel + diff), for resize arithmetic.
    pub review: Rect,
    /// The two border columns between the panels — drag to resize.
    pub divider: Rect,
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
/// Lines of context kept between the cursor and the edge of the diff pane.
const SCROLLOFF: usize = 3;
/// Columns one Left/Right key press scrolls the diff body.
const HSCROLL_STEP: i32 = 8;
/// Columns one sideways wheel notch scrolls it — smaller, because a
/// trackpad swipe delivers a stream of them.
const HSCROLL_WHEEL: i32 = 4;

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

pub enum Outcome {
    BranchPr(Option<u64>),
    LocalOpened(Box<LocalOpenedData>),
    Prs { repo: String, prs: Vec<PrSummary> },
    PrOpened(Box<PrOpenedData>),
    FileLoaded(Box<FileLoadedData>),
    CommentPosted { path: String, lo: usize, hi: usize },
    EditorSaved(Box<EditorSavedData>),
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
    /// True while the divider is being dragged.
    resizing: bool,

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
    pending_quiet: Option<QuietOutcome>,
    /// Error to re-surface once a fallback PR-list load finishes.
    post_load_err: Option<String>,
    /// One-shot note prepended to the next file-loaded status message.
    auto_open_note: Option<String>,
    pub layout: HitAreas,
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
            resizing: false,
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
            overlay: Overlay::None,
            status: String::new(),
            status_err: false,
            job: None,
            bg_jobs: Vec::new(),
            stash: None,
            quiet: None,
            pending_quiet: None,
            post_load_err: None,
            auto_open_note: None,
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

    /// True while the panel divider is being dragged (the renderer accents
    /// the seam).
    pub fn resizing(&self) -> bool {
        self.resizing
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
                    }
                }
            }
        }

        // Silent refreshes: applied in place, never modal. If a modal job is
        // mid-flight the result is parked until it finishes, so the two can't
        // interleave their writes to the same state.
        if let Some(q) = &self.quiet {
            match q.rx.try_recv() {
                Ok(r) => {
                    self.quiet = None;
                    match r {
                        Ok(o) if self.job.is_some() => self.pending_quiet = Some(o),
                        Ok(o) => self.apply_quiet(o),
                        // A failed refresh never disturbs what's on screen.
                        Err(e) => self.err(format!("Background refresh failed: {e:#}")),
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
            if let Some(o) = self.pending_quiet.take() {
                self.apply_quiet(o);
            }
        }
        true
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
                self.diff_cursor = self.first_change_display();
                self.diff_scroll = self.diff_cursor.saturating_sub(3);
                self.diff_hscroll = 0;
                self.select_mode = false;
                self.selection = None;
                self.editor = None;
                self.reveal_current_file();
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
        let content = editor.content();
        let abs_path = editor.abs_path.clone();
        let path = editor.path.clone();
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
    }

    fn cancel_theme_pick(&mut self) {
        if let Overlay::ThemePicker(tp) = &self.overlay {
            highlight::set_theme(tp.prev);
            crate::theme::set_appearance(tp.prev_appearance);
            self.overlay = Overlay::None;
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
            Overlay::None => {}
        }

        // Editor mode.
        if let Some(editor) = &mut self.editor {
            match (key.code, key.modifiers) {
                (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                    self.spawn_save_editor();
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
                if pending_g && key.code == KeyCode::Char('g') {
                    self.cursor_to(0);
                    return;
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
                    KeyCode::Enter | KeyCode::Char(' ') => {
                        let idx = self.diff_cursor;
                        if !self.toggle_fold_row(idx) {
                            self.err("Nothing to fold here — V selects lines, e edits.");
                        }
                    }
                    KeyCode::Esc if self.selection.is_some() => self.clear_selection(),
                    // --- everything that was already bound
                    KeyCode::Char('q') => self.should_quit = true,
                    KeyCode::Esc | KeyCode::Char('b') => self.back_to_pr_list(),
                    KeyCode::Char('v') => self.toggle_view(),
                    KeyCode::Char('z') => self.toggle_fold(),
                    KeyCode::Char('x') => self.toggle_file_mark(self.file_cursor),
                    KeyCode::Char('e') | KeyCode::Char('i') => {
                        let line = match self.cursor_pos() {
                            Some((Side::Right, n)) => Some(n),
                            _ => None,
                        };
                        self.open_editor(line);
                    }
                    KeyCode::Char('c') => self.open_comment(),
                    KeyCode::Char('`') => self.toggle_workspace(),
                    KeyCode::Char('r') => self.spawn_load_file(self.file_cursor),
                    KeyCode::Char('t') => self.open_theme_picker(),
                    KeyCode::Char('?') => self.overlay = Overlay::Help,
                    KeyCode::Char('<') => self.resize_file_panel(-2),
                    KeyCode::Char('>') => self.resize_file_panel(2),
                    KeyCode::Char('n') | KeyCode::Char(']') => self.step_file(1),
                    KeyCode::Char('p') | KeyCode::Char('[') => self.step_file(-1),
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
            Overlay::None => {}
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
                        Some(ButtonId::EditorSave) => {
                            self.spawn_save_editor();
                            return;
                        }
                        Some(ButtonId::EditorClose) => {
                            self.request_close_editor();
                            return;
                        }
                        Some(ButtonId::Help) => {
                            self.overlay = Overlay::Help;
                            return;
                        }
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
                    Some(ButtonId::Refresh) => {
                        self.spawn_load_prs();
                        return;
                    }
                    Some(ButtonId::LocalChanges) => {
                        self.spawn_open_local(true);
                        return;
                    }
                    Some(ButtonId::Theme) => {
                        self.open_theme_picker();
                        return;
                    }
                    Some(ButtonId::Help) => {
                        self.overlay = Overlay::Help;
                        return;
                    }
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
                    Some(ButtonId::ViewSplit) => {
                        self.set_view(ViewMode::SideBySide);
                        return;
                    }
                    Some(ButtonId::ViewInline) => {
                        self.set_view(ViewMode::Inline);
                        return;
                    }
                    Some(ButtonId::ViewTree) => {
                        self.set_tree_view(true);
                        return;
                    }
                    Some(ButtonId::ViewFlat) => {
                        self.set_tree_view(false);
                        return;
                    }
                    Some(ButtonId::FoldToggle) => {
                        self.toggle_fold();
                        return;
                    }
                    Some(ButtonId::Edit) => {
                        self.open_editor(None);
                        return;
                    }
                    Some(ButtonId::Comment) => {
                        self.open_comment();
                        return;
                    }
                    Some(ButtonId::BackToPrs) => {
                        self.back_to_pr_list();
                        return;
                    }
                    Some(ButtonId::SwapView) => {
                        self.toggle_workspace();
                        return;
                    }
                    Some(ButtonId::Theme) => {
                        self.open_theme_picker();
                        return;
                    }
                    Some(ButtonId::Help) => {
                        self.overlay = Overlay::Help;
                        return;
                    }
                    _ => {}
                }
                let fl = self.layout.file_list;
                if contains(fl, x, y) {
                    self.file_list_click(x, y);
                    return;
                }
                let dr = self.layout.diff;
                if contains(dr, x, y) {
                    self.diff_click(x, y);
                }
            }
            MouseEventKind::Drag(MouseButton::Left)
                if self.drag_select && contains(self.layout.diff, x, y) =>
            {
                if let Some((side, line)) = self.diff_pos_at(x, y) {
                    if let Some(sel) = &mut self.selection {
                        if sel.side == side {
                            sel.end = line;
                        }
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag_select = false;
            }
            MouseEventKind::Down(MouseButton::Right) => self.clear_selection(),
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
    fn divider_mouse(&mut self, m: MouseEvent, x: u16, y: u16) -> bool {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if contains(self.layout.divider, x, y) => {
                // Double-click puts it back to the default width.
                if self.double_click(x, y) {
                    self.resizing = false;
                    self.file_panel_w = self.clamp_panel_w(FILE_PANEL_DEFAULT);
                    self.ok(format!(
                        "File panel reset to {} columns.",
                        self.file_panel_w
                    ));
                } else {
                    self.resizing = true;
                    self.ok("Drag to resize the file panel — double-click the divider to reset.");
                }
                true
            }
            MouseEventKind::Drag(MouseButton::Left) if self.resizing => {
                let left = self.layout.review.x;
                // The divider's left column is the panel's last column.
                let w = x.saturating_sub(left).saturating_add(1);
                self.file_panel_w = self.clamp_panel_w(w);
                true
            }
            MouseEventKind::Up(MouseButton::Left) if self.resizing => {
                self.resizing = false;
                self.ok(format!("File panel {} columns.", self.file_panel_w));
                true
            }
            _ => false,
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
                if x >= cb_start && x < cb_start + 4 {
                    self.toggle_file_mark(idx);
                } else if idx != self.file_cursor {
                    self.spawn_load_file(idx);
                }
            }
        }
    }

    fn diff_click(&mut self, x: u16, y: u16) {
        // Clicking a fold row expands it.
        let r = self.layout.diff;
        let vis = self.diff_scroll + (y - r.y) as usize;
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
        if double && side == Side::Right {
            self.open_editor(Some(line));
            return;
        }
        self.selection = Some(Selection {
            side,
            anchor: line,
            end: line,
        });
        self.select_mode = false;
        self.drag_select = true;
        let side_name = if side == Side::Right { "new" } else { "old" };
        self.ok(format!(
            "Selected line {line} ({side_name} side) — drag to extend, then [Comment] or c. Double-click to edit."
        ));
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
                let mid = r.x + r.width / 2;
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
                sel.end = line;
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
        self.selection = Some(Selection {
            side,
            anchor: line,
            end: line,
        });
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
                "No more changes below — n for the next file."
            } else {
                "No more changes above — p for the previous file."
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
        let w = self.layout.diff.width as usize;
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

    /// Keep the panel wide enough to be useful and the diff pane alive.
    pub fn clamp_panel_w(&self, w: u16) -> u16 {
        let avail = self.layout.review.width;
        let hi = avail.saturating_sub(DIFF_MIN_W).max(FILE_PANEL_MIN);
        w.clamp(FILE_PANEL_MIN, hi)
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

    pub fn open_comment(&mut self) {
        if self.local {
            self.err("Reviewing local changes — commenting needs an open pull request (b for the PR list).");
            return;
        }
        // No explicit selection: comment on the row the cursor is on.
        let sel = match self.selection {
            Some(sel) => sel,
            None => match self.cursor_pos() {
                Some((side, line)) => Selection {
                    side,
                    anchor: line,
                    end: line,
                },
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
        self.editor = None;
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
                    self.spawn_quiet_local();
                } else {
                    let n = self.pr.as_ref().map(|p| p.number).unwrap_or(0);
                    self.ok(format!("PR #{n} — checking GitHub for updates."));
                    self.spawn_quiet_pr();
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

    /// Re-fetch the open PR's metadata and file list without blocking.
    fn spawn_quiet_pr(&mut self) {
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
        });
    }

    /// Re-scan the working tree for uncommitted changes without blocking.
    fn spawn_quiet_local(&mut self) {
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
        });
    }

    /// Reload one file in place — same work as [`Self::spawn_load_file`],
    /// but non-modal, and applied without touching the scroll position.
    fn spawn_quiet_file(&mut self, idx: usize) {
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
    fn apply_quiet(&mut self, outcome: QuietOutcome) {
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
                    self.ok(format!("✔ PR #{number} is up to date."));
                } else {
                    self.spawn_quiet_file(self.file_cursor);
                }
            }
            QuietOutcome::Local(data) => {
                if !self.local {
                    return;
                }
                let d = *data;
                let head = d.head.unwrap_or_default();
                let changed = self.files != d.files || self.merge_base != head;
                let cur = self.files.get(self.file_cursor).map(|f| f.path.clone());
                self.local_branch = d.branch;
                self.merge_base = head;
                self.files = d.files;
                self.stage = d.stage;
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
                    self.ok("Working tree clean — nothing uncommitted. ` swaps back.");
                } else if !changed && self.diff.is_some() {
                    self.ok("✔ Local changes are up to date.");
                } else {
                    self.spawn_quiet_file(self.file_cursor);
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
                if same {
                    self.ok(format!("✔ {} — up to date.", d.path));
                } else {
                    self.ok(format!("⟳ {} updated with the latest changes.", d.path));
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
        app.selection = Some(Selection {
            side: Side::Right,
            anchor: 10,
            end: 10,
        });

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
        app.apply_quiet(QuietOutcome::File(Box::new(reload(&old, &new))));
        assert_eq!(app.diff_scroll, 3);
        assert_eq!(app.diff_cursor, 4);
        assert!(app.selection.is_some());
        assert!(app.status.contains("up to date"), "{}", app.status);

        // Changed content: no jump to the top, but the selection goes —
        // its lines may not exist any more.
        let new2 = new.replace("line3\n", "line3 changed\n");
        app.apply_quiet(QuietOutcome::File(Box::new(reload(&old, &new2))));
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
        app.apply_quiet(QuietOutcome::Pr(Box::new(PrRefreshData {
            detail: pr_detail(7),
            merge_base: "c".repeat(40),
            files: vec![cf("test.rs")],
            viewed: HashSet::new(),
        })));
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
}

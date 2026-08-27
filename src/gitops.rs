//! Thin wrappers around the local `git` binary.

use crate::github::ChangedFile;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

pub fn run_git(args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .context("failed to spawn git — is git installed?")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Absolute path of the repository root (so file paths from the GitHub API,
/// which are repo-relative, resolve correctly even when loupe is launched
/// from a subdirectory).
pub fn repo_root() -> Option<PathBuf> {
    run_git(&["rev-parse", "--show-toplevel"])
        .ok()
        .map(|s| PathBuf::from(s.trim()))
}

/// Join an API-supplied repo-relative path onto `root`, rejecting anything
/// that could escape the repository. GitHub file paths are always plain
/// relative paths, so an absolute path, a `..`/`.` component, or a Windows
/// drive prefix means a malformed (or malicious) response — those must never
/// reach a filesystem read or write. Note `Path::join` alone is NOT safe
/// here: joining an absolute path replaces the root entirely.
pub fn safe_repo_path(root: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    if rel.is_empty() || !p.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(root.join(p))
}

/// Name of the currently checked-out branch, or None on detached HEAD.
pub fn current_branch() -> Option<String> {
    run_git(&["branch", "--show-current"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn is_repo() -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Best-effort merge base between two commits; falls back to `base` itself.
pub fn merge_base(base: &str, head: &str) -> String {
    run_git(&["merge-base", base, head])
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| base.to_string())
}

/// Content of `path` at `refspec`, or None if the file does not exist there.
pub fn show_file(refspec: &str, path: &str) -> Option<String> {
    let spec = format!("{refspec}:{path}");
    run_git(&["show", &spec]).ok()
}

/// Full commit id of HEAD, or None (e.g. a repository with no commits yet).
pub fn head_oid() -> Option<String> {
    run_git(&["rev-parse", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Map a `git diff --name-status` letter to the GitHub-style status strings
/// the rest of the app keys on ("added" gates the old side, "removed" the
/// new side, "renamed" carries a previous path).
fn map_status(s: &str) -> &'static str {
    match s.chars().next() {
        Some('A') => "added",
        Some('D') => "removed",
        Some('R') => "renamed",
        Some('C') => "copied",
        _ => "modified", // M, T (typechange), U, …
    }
}

/// Parse `git diff --name-status -M -z` output: NUL-separated tokens of
/// `STATUS path` — with renames/copies (`R###`/`C###`) followed by BOTH the
/// old and the new path.
fn parse_name_status_z(out: &str) -> Vec<(String, Option<String>, String)> {
    let mut files = Vec::new();
    let mut toks = out.split('\0').filter(|t| !t.is_empty());
    while let Some(status) = toks.next() {
        let mapped = map_status(status).to_string();
        if matches!(status.chars().next(), Some('R') | Some('C')) {
            let (Some(old), Some(new)) = (toks.next(), toks.next()) else {
                break;
            };
            files.push((mapped, Some(old.to_string()), new.to_string()));
        } else {
            let Some(path) = toks.next() else { break };
            files.push((mapped, None, path.to_string()));
        }
    }
    files
}

/// Parse `git diff --numstat -M -z` output into per-path (additions,
/// deletions), keyed by the NEW path. Tokens are `added<TAB>deleted<TAB>path`;
/// a rename ends the first token after the second tab and appends the old and
/// new paths as two extra tokens. Binary files report `-` — counted as 0.
fn parse_numstat_z(out: &str) -> HashMap<String, (u64, u64)> {
    let mut counts = HashMap::new();
    let mut toks = out.split('\0').filter(|t| !t.is_empty());
    while let Some(tok) = toks.next() {
        let mut parts = tok.splitn(3, '\t');
        let a = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let d = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let path = match parts.next() {
            Some("") | None => {
                // Rename record: old path, then new path, as separate tokens.
                let _old = toks.next();
                match toks.next() {
                    Some(new) => new.to_string(),
                    None => break,
                }
            }
            Some(p) => p.to_string(),
        };
        counts.insert(path, (a, d));
    }
    counts
}

/// Uncommitted changes — staged + unstaged edits vs HEAD, plus untracked
/// files (shown as additions). This is the file list for local-changes
/// review; paths are repo-root-relative, matching what the rest of the app
/// expects from the GitHub API.
pub fn local_changes(root: &Path) -> Result<Vec<ChangedFile>> {
    let mut files = Vec::new();
    // Conflicted paths, so the file list can mark them and sort them first.
    // A failed read just means no marks — never sink the whole scan.
    let unmerged = unmerged_paths(root).unwrap_or_default();
    if head_oid().is_some() {
        let ns = run_git(&["diff", "HEAD", "--name-status", "-M", "-z"])?;
        // Counts are cosmetic — don't fail the scan over them.
        let counts = run_git(&["diff", "HEAD", "--numstat", "-M", "-z"])
            .map(|o| parse_numstat_z(&o))
            .unwrap_or_default();
        for (status, previous, path) in parse_name_status_z(&ns) {
            let (additions, deletions) = counts.get(&path).copied().unwrap_or((0, 0));
            let conflicted = unmerged.contains(&path);
            files.push(ChangedFile {
                path,
                status,
                additions,
                deletions,
                previous,
                conflicted,
            });
        }
    }
    // A conflict git could not merge at all — an add/add on a path neither
    // side had, say — can be unmerged without showing in `diff HEAD`.
    for path in &unmerged {
        if !files.iter().any(|f| &f.path == path) {
            files.push(ChangedFile {
                path: path.clone(),
                status: "modified".into(),
                additions: 0,
                deletions: 0,
                previous: None,
                conflicted: true,
            });
        }
    }
    let untracked = run_git(&[
        "ls-files",
        "--others",
        "--exclude-standard",
        "--full-name",
        "-z",
    ])?;
    for path in untracked.split('\0').filter(|p| !p.is_empty()) {
        // Line count for the +N column; 0 for binary/unreadable files.
        let additions = safe_repo_path(root, path)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|c| c.lines().count() as u64)
            .unwrap_or(0);
        files.push(ChangedFile {
            path: path.to_string(),
            status: "added".into(),
            additions,
            deletions: 0,
            previous: None,
            conflicted: false,
        });
    }
    // Conflicts first, then by path. A merge conflict blocks the commit, so
    // it is the one thing in the list that has to be seen without scrolling.
    files.sort_by(|a, b| {
        b.conflicted
            .cmp(&a.conflicted)
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(files)
}

// ------------------------------------------------------------------ staging

/// How much of a file's change is in the index. Local-changes review shows
/// this in place of the PR "viewed" checkbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StageState {
    /// Nothing staged — the whole change is still only in the working tree.
    #[default]
    Unstaged,
    /// Some of it staged, more still in the working tree (`git add -p`, or
    /// an edit made after staging).
    Partial,
    /// The working tree matches the index: everything is staged.
    Staged,
    /// A merge left this path unmerged. It cannot be staged as it stands —
    /// the conflict has to be resolved first — so it gets its own state
    /// rather than one of the three above.
    Conflicted,
}

/// Parse `git status --porcelain=v1 -z -uall`. Each record is `XY <path>`,
/// where X is the index status and Y the working-tree one; a rename or copy
/// record is followed by a second record holding the original path.
pub fn parse_status_z(out: &str) -> HashMap<String, StageState> {
    let mut map = HashMap::new();
    let mut it = out.split('\0').filter(|r| !r.is_empty());
    while let Some(rec) = it.next() {
        let mut chars = rec.chars();
        let (Some(x), Some(y)) = (chars.next(), chars.next()) else {
            continue;
        };
        // "XY path" — one separating space, and paths may contain spaces.
        let Some(path) = rec.get(3..).filter(|p| !p.is_empty()) else {
            continue;
        };
        if x == 'R' || x == 'C' {
            // Consume the original path that follows a rename/copy.
            it.next();
        }
        let state = match (x, y) {
            // The six unmerged shapes git defines: DD, AU, UD, UA, DU, AA,
            // UU. Every one of them has a U on a side, or is DD or AA.
            ('U', _) | (_, 'U') | ('D', 'D') | ('A', 'A') => StageState::Conflicted,
            ('?', _) | (' ', _) | ('!', _) => StageState::Unstaged,
            (_, ' ') => StageState::Staged,
            _ => StageState::Partial,
        };
        map.insert(path.to_string(), state);
    }
    map
}

// ------------------------------------------------------------- conflicts

/// Paths git left unmerged, from the index rather than the file text. This
/// is the authority: a conflict git could not express with markers (one
/// side deleted the file, say) has no markers to find.
pub fn unmerged_paths(root: &Path) -> Result<Vec<String>> {
    let r = root.to_string_lossy().into_owned();
    let out = run_git(&["-C", &r, "diff", "--name-only", "--diff-filter=U", "-z"])?;
    Ok(out
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect())
}

/// Content of one merge stage of an unmerged path: 1 is the common
/// ancestor, 2 is ours, 3 is theirs. `None` when that stage does not exist
/// — an add/add conflict has no ancestor, and a delete/modify conflict is
/// missing whichever side did the deleting.
pub fn stage_blob(root: &Path, stage: u8, path: &str) -> Option<String> {
    let r = root.to_string_lossy().into_owned();
    let spec = format!(":{stage}:{}", checked_pathspec(root, path).ok()?);
    run_git(&["-C", &r, "show", &spec]).ok()
}

/// Write `content` over a repository file. The path is checked the same
/// way every other write is, so a path that could leave the repository is
/// refused before it reaches the filesystem.
pub fn write_repo_file(root: &Path, path: &str, content: &str) -> Result<()> {
    let abs = safe_repo_path(root, path)
        .with_context(|| format!("refusing to write suspicious path “{path}”"))?;
    if let Some(dir) = abs.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&abs, content).with_context(|| format!("writing {path}"))
}

/// Resolve a whole conflicted path from the index rather than from the
/// marker text: take stage 2 (ours) or stage 3 (theirs) exactly as git
/// recorded it, and mark the path resolved.
///
/// This is the answer for the conflicts markers cannot describe. When the
/// chosen side deleted the file, the file is removed instead of written.
/// Returns true when a file is left on disk.
pub fn take_side(root: &Path, path: &str, ours: bool) -> Result<bool> {
    let stage = if ours { 2 } else { 3 };
    let r = root.to_string_lossy().into_owned();
    let spec = checked_pathspec(root, path)?;
    match stage_blob(root, stage, path) {
        Some(content) => {
            write_repo_file(root, path, &content)?;
            run_git(&["-C", &r, "add", "--", spec])?;
            Ok(true)
        }
        None => {
            // That side deleted the file, so resolving to it deletes it.
            run_git(&["-C", &r, "rm", "-f", "-q", "--ignore-unmatch", "--", spec])?;
            if let Some(abs) = safe_repo_path(root, path) {
                if abs.symlink_metadata().is_ok() {
                    std::fs::remove_file(&abs).with_context(|| format!("removing {path}"))?;
                }
            }
            Ok(false)
        }
    }
}

/// What git is in the middle of. A conflict outside a merge still has to
/// be resolved, but the sentence that says how to finish differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeOp {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl MergeOp {
    /// The word for the top bar badge.
    pub fn badge(self) -> &'static str {
        match self {
            MergeOp::Merge => "MERGE",
            MergeOp::Rebase => "REBASE",
            MergeOp::CherryPick => "CHERRY-PICK",
            MergeOp::Revert => "REVERT",
        }
    }

    /// The word for it in a sentence.
    pub fn noun(self) -> &'static str {
        match self {
            MergeOp::Merge => "merge",
            MergeOp::Rebase => "rebase",
            MergeOp::CherryPick => "cherry-pick",
            MergeOp::Revert => "revert",
        }
    }

    /// The git command that finishes it, once every conflict is resolved.
    pub fn finish(self) -> &'static str {
        match self {
            MergeOp::Merge => "git commit",
            MergeOp::Rebase => "git rebase --continue",
            MergeOp::CherryPick => "git cherry-pick --continue",
            MergeOp::Revert => "git revert --continue",
        }
    }
}

/// Absolute path of the `.git` directory (a file in a worktree or
/// submodule, so ask git rather than joining `.git` onto the root).
pub fn git_dir(root: &Path) -> Option<PathBuf> {
    let r = root.to_string_lossy().into_owned();
    let out = run_git(&["-C", &r, "rev-parse", "--absolute-git-dir"]).ok()?;
    Some(PathBuf::from(out.trim()))
}

/// The operation in progress, read from the state files git leaves in the
/// git directory. `None` when the tree is not mid-anything.
pub fn merge_op(root: &Path) -> Option<MergeOp> {
    let dir = git_dir(root)?;
    let has = |name: &str| dir.join(name).exists();
    if has("MERGE_HEAD") {
        return Some(MergeOp::Merge);
    }
    // Both rebase backends: `rebase-merge` is the interactive one,
    // `rebase-apply` the older patch-based one.
    if has("rebase-merge") || has("rebase-apply") {
        return Some(MergeOp::Rebase);
    }
    if has("CHERRY_PICK_HEAD") {
        return Some(MergeOp::CherryPick);
    }
    if has("REVERT_HEAD") {
        return Some(MergeOp::Revert);
    }
    None
}

// -------------------------------------------------------------- tracking

/// How far the branch has drifted from the branch it tracks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracking {
    /// The upstream branch, as `origin/main`.
    pub upstream: String,
    /// Commits on this branch that the upstream does not have.
    pub ahead: usize,
    /// Commits on the upstream that this branch does not have.
    pub behind: usize,
}

impl Tracking {
    pub fn in_sync(&self) -> bool {
        self.ahead == 0 && self.behind == 0
    }
}

/// Parse `git rev-list --left-right --count <a>...<b>`: one line of two
/// counts, left first.
fn parse_left_right(out: &str) -> Option<(usize, usize)> {
    let mut it = out.split_whitespace();
    let l = it.next()?.parse().ok()?;
    let r = it.next()?.parse().ok()?;
    Some((l, r))
}

/// The upstream of the current branch, and how far HEAD is from it.
///
/// The configured upstream comes first. A branch with none falls back to
/// `origin/<branch>`, which is what a clone almost always has and what the
/// question "how far behind origin am I?" means anyway. `None` when there
/// is no branch, no origin, or nothing to compare against.
pub fn tracking(root: &Path) -> Option<Tracking> {
    let r = root.to_string_lossy().into_owned();
    let branch = current_branch()?;
    let upstream = run_git(&["-C", &r, "rev-parse", "--abbrev-ref", "@{upstream}"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let guess = format!("refs/remotes/origin/{branch}");
            run_git(&["-C", &r, "rev-parse", "--verify", "--quiet", &guess])
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(|_| format!("origin/{branch}"))
        })?;
    let range = format!("{upstream}...HEAD");
    let out = run_git(&["-C", &r, "rev-list", "--left-right", "--count", &range]).ok()?;
    // Left is the upstream side, so left is what we are behind by.
    let (behind, ahead) = parse_left_right(&out)?;
    Some(Tracking {
        upstream,
        ahead,
        behind,
    })
}

/// Index state of every changed path in the working tree.
pub fn stage_states(root: &Path) -> Result<HashMap<String, StageState>> {
    let root = root.to_string_lossy().into_owned();
    let out = run_git(&["-C", &root, "status", "--porcelain=v1", "-z", "-uall"])?;
    Ok(parse_status_z(&out))
}

/// Reject anything that could escape the repository before it reaches a
/// git pathspec (git's own output is well-behaved, but this is the same
/// guard the filesystem paths get).
fn checked_pathspec<'a>(root: &Path, rel: &'a str) -> Result<&'a str> {
    if safe_repo_path(root, rel).is_none() {
        bail!("refusing to act on suspicious path “{rel}”");
    }
    Ok(rel)
}

/// `git add` the file — and, for a rename, the original path too, so the
/// removal is staged with it. Paths are pathspecs relative to the repo
/// root, so git runs with `-C <root>` (loupe may be started in a subdir).
pub fn stage_file(root: &Path, path: &str, previous: Option<&str>) -> Result<()> {
    let r = root.to_string_lossy().into_owned();
    let mut args = vec!["-C", r.as_str(), "add", "--", checked_pathspec(root, path)?];
    if let Some(prev) = previous {
        args.push(checked_pathspec(root, prev)?);
    }
    run_git(&args).map(|_| ())
}

/// Take the file back out of the index, leaving the working tree alone.
/// `git reset` with a pathspec and no commit argument resolves against HEAD
/// where there is one and the empty tree where there isn't, so this works
/// in a repository with no commits too (unlike `rm --cached`, which refuses
/// when the index and the working tree have diverged).
pub fn unstage_file(root: &Path, path: &str, previous: Option<&str>) -> Result<()> {
    let r = root.to_string_lossy().into_owned();
    let mut args = vec![
        "-C",
        r.as_str(),
        "reset",
        "-q",
        "--",
        checked_pathspec(root, path)?,
    ];
    if let Some(prev) = previous {
        args.push(checked_pathspec(root, prev)?);
    }
    run_git(&args).map(|_| ())
}

/// `git add -A` from the repository root: every change in the working tree
/// goes into the index, untracked files included.
///
/// The file panel already lists untracked files as additions, so "stage
/// everything in the list" and `git add -A` name the same set. The `.`
/// pathspec is relative to `-C <root>`, so this stages the whole
/// repository even when loupe was started in a subdirectory.
pub fn stage_all(root: &Path) -> Result<()> {
    let r = root.to_string_lossy().into_owned();
    run_git(&["-C", &r, "add", "-A", "--", "."]).map(|_| ())
}

/// Empty the index back to HEAD, leaving every file on disk alone.
///
/// The `.` pathspec is what makes this work in a repository with no
/// commits: `git reset` with a pathspec and no commit resolves against the
/// empty tree when there is no HEAD, while a bare `git reset` fails.
pub fn unstage_all(root: &Path) -> Result<()> {
    let r = root.to_string_lossy().into_owned();
    run_git(&["-C", &r, "reset", "-q", "--", "."]).map(|_| ())
}

// ------------------------------------------------------------------- stash

/// What a stash takes with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StashScope {
    /// Tracked changes only — what `git stash push` does on its own. An
    /// untracked file stays where it is.
    Tracked,
    /// Tracked changes and untracked files (`--include-untracked`).
    /// Ignored files are still left alone.
    WithUntracked,
    /// Only what is in the index (`--staged`), which leaves the unstaged
    /// edits in the working tree. Needs git 2.35 or later.
    StagedOnly,
}

impl StashScope {
    /// What the status line calls this scope.
    pub fn label(self) -> &'static str {
        match self {
            StashScope::Tracked => "tracked changes",
            StashScope::WithUntracked => "tracked changes and untracked files",
            StashScope::StagedOnly => "the staged changes",
        }
    }
}

/// `git stash push`, with an optional name and a scope.
///
/// git refuses an empty stash, and its own message for that reads as a
/// success ("No local changes to save"), so the caller is told with an
/// error instead — a silent no-op after a menu click looks like a bug.
pub fn stash_push(root: &Path, message: Option<&str>, scope: StashScope) -> Result<()> {
    let r = root.to_string_lossy().into_owned();
    let mut args = vec!["-C", r.as_str(), "stash", "push"];
    match scope {
        StashScope::Tracked => {}
        StashScope::WithUntracked => args.push("--include-untracked"),
        StashScope::StagedOnly => args.push("--staged"),
    }
    let msg = message.map(str::trim).filter(|m| !m.is_empty());
    if let Some(m) = msg {
        args.push("-m");
        args.push(m);
    }
    let out = run_git(&args)?;
    if out.contains("No local changes to save") || out.trim().is_empty() {
        bail!("nothing to stash — {} left nothing to save", scope.label());
    }
    Ok(())
}

/// One entry of `git stash list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stash {
    /// Position in the list. `stash@{0}` is the newest.
    pub index: usize,
    /// What git calls it: `stash@{0}`.
    pub name: String,
    /// The reflog subject — the name you gave it, or the `WIP on <branch>`
    /// git writes when you gave it none.
    pub subject: String,
    /// How long ago it was made, as `2 hours ago`.
    pub when: String,
}

/// Every stash in the repository, newest first.
///
/// The fields are split on a unit separator and the records on a record
/// separator, so a stash message with a newline or a tab in it still parses.
pub fn stash_list(root: &Path) -> Result<Vec<Stash>> {
    let r = root.to_string_lossy().into_owned();
    let out = run_git(&["-C", &r, "stash", "list", "--format=%gd%x1f%gs%x1f%cr%x1e"])?;
    Ok(parse_stash_list(&out))
}

fn parse_stash_list(out: &str) -> Vec<Stash> {
    out.split('\u{1e}')
        .map(str::trim_start)
        .filter(|rec| !rec.is_empty())
        .enumerate()
        .map(|(index, rec)| {
            let mut f = rec.split('\u{1f}');
            let name = f.next().unwrap_or_default().to_string();
            let subject = f.next().unwrap_or_default().to_string();
            let when = f.next().unwrap_or_default().to_string();
            Stash {
                index,
                name,
                subject,
                when,
            }
        })
        .collect()
}

/// Put a stash back into the working tree. `pop` also drops it.
///
/// The stash is named by position rather than by the `stash@{N}` string,
/// so a list that moved under the reader cannot address the wrong one by
/// a stale name.
pub fn stash_apply(root: &Path, index: usize, pop: bool) -> Result<()> {
    let r = root.to_string_lossy().into_owned();
    let name = format!("stash@{{{index}}}");
    let verb = if pop { "pop" } else { "apply" };
    run_git(&["-C", &r, "stash", verb, "--index", &name])
        // `--index` also restores what was staged, and git refuses it when
        // the index cannot be replayed. Fall back to the plain form, which
        // restores the work as unstaged edits rather than failing.
        .or_else(|_| run_git(&["-C", &r, "stash", verb, &name]))
        .map(|_| ())
}

/// Throw one stash away for good.
pub fn stash_drop(root: &Path, index: usize) -> Result<()> {
    let r = root.to_string_lossy().into_owned();
    let name = format!("stash@{{{index}}}");
    run_git(&["-C", &r, "stash", "drop", &name]).map(|_| ())
}

// ----------------------------------------------------------------- commits

/// One commit in the list of what has not been pushed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// The full object id, which is what every later git call uses.
    pub oid: String,
    /// The abbreviated id, which is what the panel shows.
    pub short: String,
    pub subject: String,
    pub author: String,
    /// How long ago it was made, as `2 hours ago`.
    pub when: String,
}

/// Commits on HEAD that `upstream` does not have, newest first.
pub fn unpushed_commits(root: &Path, upstream: &str) -> Result<Vec<Commit>> {
    let r = root.to_string_lossy().into_owned();
    let range = format!("{upstream}..HEAD");
    let out = run_git(&[
        "-C",
        &r,
        "log",
        "--format=%H%x1f%h%x1f%s%x1f%an%x1f%cr%x1e",
        &range,
    ])?;
    Ok(parse_commit_log(&out))
}

fn parse_commit_log(out: &str) -> Vec<Commit> {
    out.split('\u{1e}')
        .map(str::trim_start)
        .filter(|rec| !rec.is_empty())
        .map(|rec| {
            let mut f = rec.split('\u{1f}');
            let oid = f.next().unwrap_or_default().to_string();
            let short = f.next().unwrap_or_default().to_string();
            let subject = f.next().unwrap_or_default().to_string();
            let author = f.next().unwrap_or_default().to_string();
            let when = f.next().unwrap_or_default().to_string();
            Commit {
                oid,
                short,
                subject,
                author,
                when,
            }
        })
        .collect()
}

/// The files one commit changed, against its first parent.
///
/// `--diff-merges=first-parent` is what makes a merge commit answer this
/// question at all: without it `git show` prints nothing for a merge, and
/// an empty file list would read as "this commit changed nothing". A root
/// commit has no parent, and `git show` lists its whole tree as added.
pub fn commit_files(root: &Path, oid: &str) -> Result<Vec<ChangedFile>> {
    let r = root.to_string_lossy().into_owned();
    let ns = run_git(&[
        "-C",
        &r,
        "show",
        "--diff-merges=first-parent",
        "--format=",
        "--name-status",
        "-M",
        "-z",
        oid,
    ])?;
    // Counts are cosmetic — a failed read costs the +/− columns, not the
    // file list.
    let counts = run_git(&[
        "-C",
        &r,
        "show",
        "--diff-merges=first-parent",
        "--format=",
        "--numstat",
        "-M",
        "-z",
        oid,
    ])
    .map(|o| parse_numstat_z(&o))
    .unwrap_or_default();
    let mut files: Vec<ChangedFile> = parse_name_status_z(&ns)
        .into_iter()
        .map(|(status, previous, path)| {
            let (additions, deletions) = counts.get(&path).copied().unwrap_or((0, 0));
            ChangedFile {
                path,
                status,
                additions,
                deletions,
                previous,
                conflicted: false,
            }
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// The first parent of a commit, which is the side its diff is read
/// against. `None` for a root commit, whose diff is against nothing.
pub fn first_parent(root: &Path, oid: &str) -> Option<String> {
    let r = root.to_string_lossy().into_owned();
    let spec = format!("{oid}^1");
    run_git(&["-C", &r, "rev-parse", "--verify", "--quiet", &spec])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ------------------------------------------------------------------ revert

/// True when `path` exists in the tree at `rev`.
fn exists_at(root: &Path, rev: &str, path: &str) -> bool {
    let r = root.to_string_lossy().into_owned();
    let spec = format!("{rev}:{path}");
    Command::new("git")
        .args(["-C", &r, "cat-file", "-e", &spec])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Put `path` back the way it looks at `rev` — index and working tree both —
/// or remove it when it does not exist there.
///
/// `git checkout <rev> -- <path>` is the whole trick: it rewrites the index
/// entry as well as the file, so a reverted file stops showing up as changed
/// instead of lingering as a staged edit whose diff is empty. `rev` is None
/// in a repository with no commits, where every file is a new one.
///
/// This throws work away for good — the caller is expected to have asked
/// first.
pub fn revert_path(root: &Path, rev: Option<&str>, path: &str) -> Result<()> {
    let r = root.to_string_lossy().into_owned();
    let spec = checked_pathspec(root, path)?;
    match rev.filter(|rev| !rev.is_empty() && exists_at(root, rev, path)) {
        Some(rev) => run_git(&["-C", &r, "checkout", rev, "--", spec]).map(|_| ()),
        None => {
            // Nothing to go back to: the change created this file, so undoing
            // it means removing it — from the index too when it is staged.
            // `--ignore-unmatch` keeps an untracked file from being an error.
            run_git(&["-C", &r, "rm", "-f", "-q", "--ignore-unmatch", "--", spec])?;
            match safe_repo_path(root, path) {
                Some(abs) if abs.symlink_metadata().is_ok() => {
                    std::fs::remove_file(&abs).with_context(|| format!("removing {path}"))
                }
                _ => Ok(()),
            }
        }
    }
}

/// True when `url` points at the GitHub repository `repo` ("owner/name") —
/// https, ssh, and scp-style remote URLs all end in the same two segments.
fn url_matches_repo(url: &str, repo: &str) -> bool {
    let t = url.trim_end_matches('/').trim_end_matches(".git");
    let mut it = t.rsplit(['/', ':']);
    let (Some(name), Some(owner)) = (it.next(), it.next()) else {
        return false;
    };
    format!("{owner}/{name}").eq_ignore_ascii_case(repo)
}

/// `git remote -v` lines: "<name>\t<url> (fetch)" — first remote whose URL
/// points at `repo`.
fn remote_matching(remotes: &str, repo: &str) -> Option<String> {
    for line in remotes.lines() {
        let mut it = line.split_whitespace();
        let (Some(name), Some(url)) = (it.next(), it.next()) else {
            continue;
        };
        if url_matches_repo(url, repo) {
            return Some(name.to_string());
        }
    }
    None
}

/// Where to fetch PR refs from: a configured remote pointing at `repo`, or
/// the repository's GitHub URL when no remote matches (fork clones with an
/// upstream org configured may have no upstream remote at all).
pub fn fetch_source(repo: &str) -> String {
    run_git(&["remote", "-v"])
        .ok()
        .and_then(|out| remote_matching(&out, repo))
        .unwrap_or_else(|| format!("https://github.com/{repo}.git"))
}

/// `owner/name` for the `origin` remote, from the URL git already has.
///
/// The blame pane needs a repository to build a pull request link out of,
/// and local review never resolves one — it has no reason to talk to
/// GitHub at all. This is the offline answer: one `git` call, no network,
/// and None for a clone with no origin or a host that is not GitHub.
pub fn origin_repo() -> Option<String> {
    let url = run_git(&["remote", "get-url", "origin"]).ok()?;
    repo_from_url(url.trim())
}

/// The `owner/name` an origin URL names, in any of the forms git accepts:
/// `https://host/owner/name(.git)`, `git@host:owner/name.git`,
/// `ssh://git@host/owner/name.git`.
fn repo_from_url(url: &str) -> Option<String> {
    if !url.contains("github") {
        return None;
    }
    let t = url.trim_end_matches('/').trim_end_matches(".git");
    let mut it = t.rsplit(['/', ':']);
    let (name, owner) = (it.next()?, it.next()?);
    if name.is_empty() || owner.is_empty() || owner.contains("://") {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

/// Fetch the base branch and the PR head ref so both commits exist locally.
/// `source` is a remote name or URL (see [`fetch_source`]). Non-fatal:
/// review can proceed for whatever objects are already present.
pub fn fetch_pr(source: &str, base_ref: &str, pr_number: u64) -> Result<()> {
    let head_spec = format!("+refs/pull/{pr_number}/head:refs/prtui/pr-{pr_number}");
    // Fully qualify the base ref: branch names may legally start with `-`
    // (git only forbids that for locally *created* branches), so passing the
    // API-supplied name bare would let a branch named like `--upload-pack=…`
    // be parsed as an option by `git fetch`.
    let base_spec = format!("refs/heads/{base_ref}");
    run_git(&["fetch", source, &base_spec, &head_spec]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_z_parses_index_and_worktree_columns() {
        // " M" unstaged edit, "M " fully staged, "MM" staged then edited
        // again, "??" untracked, "A " a staged new file, and a staged
        // rename (whose original path follows as its own record).
        let out =
            " M src/a.rs\0M  src/b.rs\0MM src/c.rs\0?? new.txt\0A  added.rs\0R  dst.rs\0src.rs\0";
        let m = parse_status_z(out);
        assert_eq!(m.get("src/a.rs"), Some(&StageState::Unstaged));
        assert_eq!(m.get("src/b.rs"), Some(&StageState::Staged));
        assert_eq!(m.get("src/c.rs"), Some(&StageState::Partial));
        assert_eq!(m.get("new.txt"), Some(&StageState::Unstaged));
        assert_eq!(m.get("added.rs"), Some(&StageState::Staged));
        assert_eq!(m.get("dst.rs"), Some(&StageState::Staged));
        // The rename's original path is consumed, not read as its own entry.
        assert_eq!(m.get("src.rs"), None);
        assert_eq!(m.len(), 6);
        // Paths with spaces survive: only the first two columns are status.
        let m = parse_status_z("M  a file.txt\0");
        assert_eq!(m.get("a file.txt"), Some(&StageState::Staged));
    }

    /// Round-trip against a real repository: git is a hard dependency of
    /// loupe, and this is the only way to catch a `git add` / `git reset`
    /// invocation that stops meaning what we think it means.
    #[test]
    fn stage_and_unstage_against_a_real_repo() {
        let root = std::env::temp_dir().join(format!("loupe-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let r = root.to_string_lossy().into_owned();
        let git = |args: &[&str]| {
            let mut full = vec!["-C", r.as_str()];
            full.extend_from_slice(args);
            run_git(&full).unwrap();
        };
        git(&["init", "-q", "."]);
        git(&["config", "user.email", "loupe@test"]);
        git(&["config", "user.name", "loupe"]);
        // Pin the line endings: git for Windows checks out CRLF by
        // default, and these tests compare file contents byte for byte.
        git(&["config", "core.autocrlf", "false"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        std::fs::write(root.join("old.txt"), "keep\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);

        // An edit and a brand-new file, both unstaged.
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("new.txt"), "fresh\n").unwrap();
        let st = stage_states(&root).unwrap();
        assert_eq!(st.get("a.txt"), Some(&StageState::Unstaged));
        assert_eq!(st.get("new.txt"), Some(&StageState::Unstaged));

        // Staging an edit, and staging an untracked file.
        stage_file(&root, "a.txt", None).unwrap();
        stage_file(&root, "new.txt", None).unwrap();
        let st = stage_states(&root).unwrap();
        assert_eq!(st.get("a.txt"), Some(&StageState::Staged));
        assert_eq!(st.get("new.txt"), Some(&StageState::Staged));

        // Editing after staging is the partial case.
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        assert_eq!(
            stage_states(&root).unwrap().get("a.txt"),
            Some(&StageState::Partial)
        );

        // Unstaging leaves the working tree alone.
        unstage_file(&root, "a.txt", None).unwrap();
        assert_eq!(
            stage_states(&root).unwrap().get("a.txt"),
            Some(&StageState::Unstaged)
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\ntwo\nthree\n"
        );
        // …and an unstaged new file is untracked again, not deleted.
        unstage_file(&root, "new.txt", None).unwrap();
        assert_eq!(
            stage_states(&root).unwrap().get("new.txt"),
            Some(&StageState::Unstaged)
        );
        assert!(root.join("new.txt").is_file());

        // A rename stages both halves through the `previous` path.
        std::fs::rename(root.join("old.txt"), root.join("renamed.txt")).unwrap();
        stage_file(&root, "renamed.txt", Some("old.txt")).unwrap();
        let st = stage_states(&root).unwrap();
        assert_eq!(st.get("renamed.txt"), Some(&StageState::Staged));
        assert_eq!(st.get("old.txt"), None, "the removal is staged with it");
        unstage_file(&root, "renamed.txt", Some("old.txt")).unwrap();
        let st = stage_states(&root).unwrap();
        assert_eq!(st.get("renamed.txt"), Some(&StageState::Unstaged));
        assert_eq!(st.get("old.txt"), Some(&StageState::Unstaged));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A repository with no commits yet: staging still means "added", and
    /// unstaging must not need a HEAD to resolve against.
    #[test]
    fn stage_round_trip_in_a_repo_with_no_commits() {
        let root = std::env::temp_dir().join(format!("loupe-stage-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let r = root.to_string_lossy().into_owned();
        run_git(&["-C", &r, "init", "-q", "."]).unwrap();
        std::fs::write(root.join("first.txt"), "hello\n").unwrap();

        stage_file(&root, "first.txt", None).unwrap();
        assert_eq!(
            stage_states(&root).unwrap().get("first.txt"),
            Some(&StageState::Staged)
        );
        unstage_file(&root, "first.txt", None).unwrap();
        assert_eq!(
            stage_states(&root).unwrap().get("first.txt"),
            Some(&StageState::Unstaged)
        );
        assert!(root.join("first.txt").is_file(), "unstaging never deletes");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Reverting against a real repository: the four shapes a changed file
    /// can have (edited, staged, deleted, brand new) all have to end with
    /// `git status` clean for that path.
    #[test]
    fn revert_puts_files_back_in_a_real_repo() {
        let root = std::env::temp_dir().join(format!("loupe-revert-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let r = root.to_string_lossy().into_owned();
        let git = |args: &[&str]| {
            let mut full = vec!["-C", r.as_str()];
            full.extend_from_slice(args);
            run_git(&full).unwrap();
        };
        git(&["init", "-q", "."]);
        git(&["config", "user.email", "loupe@test"]);
        git(&["config", "user.name", "loupe"]);
        // Pin the line endings: git for Windows checks out CRLF by
        // default, and these tests compare file contents byte for byte.
        git(&["config", "core.autocrlf", "false"]);
        std::fs::write(root.join("edited.txt"), "one\n").unwrap();
        std::fs::write(root.join("staged.txt"), "two\n").unwrap();
        std::fs::write(root.join("gone.txt"), "three\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        let head = run_git(&["-C", &r, "rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();

        // An unstaged edit, a staged edit, a deletion, and an untracked file.
        std::fs::write(root.join("edited.txt"), "one\nEDIT\n").unwrap();
        std::fs::write(root.join("staged.txt"), "two\nEDIT\n").unwrap();
        stage_file(&root, "staged.txt", None).unwrap();
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        std::fs::write(root.join("brand-new.txt"), "fresh\n").unwrap();
        assert_eq!(stage_states(&root).unwrap().len(), 4);

        for path in ["edited.txt", "staged.txt", "gone.txt", "brand-new.txt"] {
            revert_path(&root, Some(&head), path).unwrap();
        }

        assert_eq!(
            std::fs::read_to_string(root.join("edited.txt")).unwrap(),
            "one\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("staged.txt")).unwrap(),
            "two\n",
            "a staged edit is undone in the index too"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("gone.txt")).unwrap(),
            "three\n",
            "a deleted file comes back"
        );
        assert!(
            !root.join("brand-new.txt").exists(),
            "a file the change created is removed, not emptied"
        );
        assert!(
            stage_states(&root).unwrap().is_empty(),
            "nothing is left showing as changed: {:?}",
            stage_states(&root).unwrap()
        );

        // A staged new file goes out of the index as well as off the disk.
        std::fs::write(root.join("added.txt"), "x\n").unwrap();
        stage_file(&root, "added.txt", None).unwrap();
        revert_path(&root, Some(&head), "added.txt").unwrap();
        assert!(!root.join("added.txt").exists());
        assert!(stage_states(&root).unwrap().is_empty());

        // With no commit to go back to, everything is a new file.
        std::fs::write(root.join("nocommit.txt"), "x\n").unwrap();
        revert_path(&root, None, "nocommit.txt").unwrap();
        assert!(!root.join("nocommit.txt").exists());

        // And a path that tries to leave the repository is refused outright.
        assert!(revert_path(&root, Some(&head), "../escape.txt").is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn status_z_reports_every_unmerged_shape_as_a_conflict() {
        // The seven codes git defines for an unmerged path.
        let out = "DD a\0AU b\0UD c\0UA d\0DU e\0AA f\0UU g\0 M plain\0";
        let m = parse_status_z(out);
        for path in ["a", "b", "c", "d", "e", "f", "g"] {
            assert_eq!(
                m.get(path),
                Some(&StageState::Conflicted),
                "{path} should be conflicted"
            );
        }
        assert_eq!(m.get("plain"), Some(&StageState::Unstaged));
    }

    #[test]
    fn left_right_counts_parse() {
        assert_eq!(parse_left_right("2\t5\n"), Some((2, 5)));
        assert_eq!(parse_left_right("0\t0"), Some((0, 0)));
        assert_eq!(parse_left_right(""), None);
        assert_eq!(parse_left_right("nonsense"), None);
    }

    /// A merge conflict against a real repository. Everything this feature
    /// stands on — the unmerged list, the index stages, the merge state,
    /// and the whole-file resolve — is git behavior, so the only honest
    /// test is one that makes git produce a conflict.
    #[test]
    fn a_real_merge_conflict_is_found_and_resolved() {
        let root = std::env::temp_dir().join(format!("loupe-conflict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let r = root.to_string_lossy().into_owned();
        let git = |args: &[&str]| {
            let mut full = vec!["-C", r.as_str()];
            full.extend_from_slice(args);
            run_git(&full).unwrap();
        };
        git(&["init", "-q", "-b", "main", "."]);
        git(&["config", "user.email", "loupe@test"]);
        git(&["config", "user.name", "loupe"]);
        // Pin the line endings: git for Windows checks out CRLF by
        // default, and these tests compare file contents byte for byte.
        git(&["config", "core.autocrlf", "false"]);
        // Pin the conflict style: a developer with diff3 configured and one
        // without must get the same test.
        git(&["config", "merge.conflictStyle", "merge"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(root.join("only-theirs.txt"), "start\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);

        git(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(root.join("a.txt"), "one\nTHEIRS\nthree\n").unwrap();
        std::fs::write(root.join("only-theirs.txt"), "theirs\n").unwrap();
        git(&["commit", "-qam", "theirs"]);

        git(&["checkout", "-q", "main"]);
        std::fs::write(root.join("a.txt"), "one\nOURS\nthree\n").unwrap();
        std::fs::remove_file(root.join("only-theirs.txt")).unwrap();
        git(&["commit", "-qam", "ours"]);

        // The merge is expected to fail — that is the point.
        let merged = run_git(&["-C", &r, "merge", "feature"]);
        assert!(merged.is_err(), "the merge should conflict");

        assert_eq!(merge_op(&root), Some(MergeOp::Merge));
        let mut unmerged = unmerged_paths(&root).unwrap();
        unmerged.sort();
        assert_eq!(unmerged, vec!["a.txt", "only-theirs.txt"]);

        // The file list marks them and puts them first.
        let files = local_changes(&root).unwrap();
        assert!(
            files[0].conflicted && files[1].conflicted,
            "conflicts sort to the front: {files:?}"
        );
        assert_eq!(files[0].status_char(), '!');

        // The three index stages, as the diff and the whole-file resolve
        // read them.
        assert_eq!(
            stage_blob(&root, 1, "a.txt").as_deref(),
            Some("one\ntwo\nthree\n")
        );
        assert_eq!(
            stage_blob(&root, 2, "a.txt").as_deref(),
            Some("one\nOURS\nthree\n")
        );
        assert_eq!(
            stage_blob(&root, 3, "a.txt").as_deref(),
            Some("one\nTHEIRS\nthree\n")
        );
        // We deleted this one, so our side of it does not exist.
        assert_eq!(stage_blob(&root, 2, "only-theirs.txt"), None);

        // The working-tree file carries the markers the diff is built from.
        let text = std::fs::read_to_string(root.join("a.txt")).unwrap();
        assert!(
            text.contains("<<<<<<<") && text.contains(">>>>>>>"),
            "{text}"
        );

        // Taking their whole file writes their content and stages it.
        assert!(take_side(&root, "a.txt", false).unwrap());
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\nTHEIRS\nthree\n"
        );
        assert_eq!(
            stage_states(&root).unwrap().get("a.txt"),
            Some(&StageState::Staged),
            "a resolved file is no longer conflicted"
        );

        // Taking our side of the delete/modify conflict removes the file.
        assert!(!take_side(&root, "only-theirs.txt", true).unwrap());
        assert!(!root.join("only-theirs.txt").exists());
        assert!(
            unmerged_paths(&root).unwrap().is_empty(),
            "nothing is left unmerged"
        );

        // The merge is still in progress until it is committed.
        assert_eq!(merge_op(&root), Some(MergeOp::Merge));
        git(&["commit", "-qm", "merged"]);
        assert_eq!(merge_op(&root), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Ahead and behind, against a real remote-tracking ref.
    #[test]
    fn ahead_and_behind_count_against_the_upstream() {
        let root = std::env::temp_dir().join(format!("loupe-track-{}", std::process::id()));
        let up = std::env::temp_dir().join(format!("loupe-track-up-{}", std::process::id()));
        for d in [&root, &up] {
            let _ = std::fs::remove_dir_all(d);
        }
        std::fs::create_dir_all(&up).unwrap();
        let u = up.to_string_lossy().into_owned();
        let ugit = |args: &[&str]| {
            let mut full = vec!["-C", u.as_str()];
            full.extend_from_slice(args);
            run_git(&full).unwrap();
        };
        ugit(&["init", "-q", "-b", "main", "."]);
        ugit(&["config", "user.email", "loupe@test"]);
        ugit(&["config", "user.name", "loupe"]);
        std::fs::write(up.join("f.txt"), "1\n").unwrap();
        ugit(&["add", "-A"]);
        ugit(&["commit", "-qm", "one"]);

        let r = root.to_string_lossy().into_owned();
        run_git(&["clone", "-q", &u, &r]).unwrap();
        let git = |args: &[&str]| {
            let mut full = vec!["-C", r.as_str()];
            full.extend_from_slice(args);
            run_git(&full).unwrap();
        };
        git(&["config", "user.email", "loupe@test"]);
        git(&["config", "user.name", "loupe"]);
        // Pin the line endings: git for Windows checks out CRLF by
        // default, and these tests compare file contents byte for byte.
        git(&["config", "core.autocrlf", "false"]);

        // `tracking` reads the current branch, which comes from the process
        // working directory — so run this half from inside the clone.
        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&root).unwrap();

        let t = tracking(&root).expect("a fresh clone tracks its origin");
        assert_eq!(t.upstream, "origin/main");
        assert!(t.in_sync(), "{t:?}");

        // Two of ours, one of theirs.
        for n in ["2", "3"] {
            std::fs::write(root.join("f.txt"), format!("{n}\n")).unwrap();
            git(&["commit", "-qam", n]);
        }
        std::fs::write(up.join("g.txt"), "x\n").unwrap();
        ugit(&["add", "-A"]);
        ugit(&["commit", "-qm", "upstream moved"]);
        git(&["fetch", "-q", "origin"]);

        let t = tracking(&root).expect("still tracking");
        assert_eq!((t.ahead, t.behind), (2, 1));
        assert!(!t.in_sync());

        std::env::set_current_dir(cwd).unwrap();
        for d in [&root, &up] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn pathspecs_outside_the_repo_are_refused() {
        let root = Path::new("/repo");
        assert!(checked_pathspec(root, "src/main.rs").is_ok());
        assert!(checked_pathspec(root, "../etc/passwd").is_err());
        assert!(checked_pathspec(root, "/etc/passwd").is_err());
        assert!(checked_pathspec(root, "").is_err());
    }

    #[test]
    fn safe_repo_path_accepts_plain_relative_paths() {
        let root = Path::new("/repo");
        assert_eq!(
            safe_repo_path(root, "src/main.rs"),
            Some(PathBuf::from("/repo/src/main.rs"))
        );
        assert_eq!(
            safe_repo_path(root, "a b/wéird-näme.txt"),
            Some(PathBuf::from("/repo/a b/wéird-näme.txt"))
        );
    }

    #[test]
    fn remote_url_forms_match_repo() {
        for url in [
            "https://github.com/acme/tool",
            "https://github.com/acme/tool.git",
            "https://github.com/acme/tool/",
            "git@github.com:acme/tool.git",
            "ssh://git@github.com/acme/tool.git",
            "https://github.com/ACME/Tool.git", // GitHub is case-insensitive
        ] {
            assert!(url_matches_repo(url, "acme/tool"), "{url}");
        }
        assert!(!url_matches_repo(
            "https://github.com/jacob/tool.git",
            "acme/tool"
        ));
        assert!(!url_matches_repo(
            "https://github.com/acme/other.git",
            "acme/tool"
        ));
        assert!(!url_matches_repo("tool", "acme/tool"));
    }

    #[test]
    fn remote_listing_finds_matching_remote() {
        let remotes = "origin\tgit@github.com:jacob/tool.git (fetch)\n\
                       origin\tgit@github.com:jacob/tool.git (push)\n\
                       upstream\thttps://github.com/acme/tool.git (fetch)\n\
                       upstream\thttps://github.com/acme/tool.git (push)\n";
        assert_eq!(
            remote_matching(remotes, "acme/tool"),
            Some("upstream".into())
        );
        assert_eq!(
            remote_matching(remotes, "jacob/tool"),
            Some("origin".into())
        );
        assert_eq!(remote_matching(remotes, "other/tool"), None);
        assert_eq!(remote_matching("", "acme/tool"), None);
    }

    #[test]
    fn name_status_z_parses_statuses_and_renames() {
        // As emitted by `git diff HEAD --name-status -M -z`.
        let out = "D\0gone.txt\0M\0keep.txt\0R054\0oldname.txt\0newname.txt\0A\0fresh.txt\0";
        assert_eq!(
            parse_name_status_z(out),
            vec![
                ("removed".into(), None, "gone.txt".into()),
                ("modified".into(), None, "keep.txt".into()),
                (
                    "renamed".into(),
                    Some("oldname.txt".into()),
                    "newname.txt".into()
                ),
                ("added".into(), None, "fresh.txt".into()),
            ]
        );
    }

    #[test]
    fn numstat_z_parses_counts_renames_and_binary() {
        // As emitted by `git diff HEAD --numstat -M -z`; renames embed the
        // old/new paths as two extra NUL tokens, binaries report "-".
        let out = concat!(
            "0\t1\tgone.txt\0",
            "2\t1\tkeep.txt\0",
            "1\t0\t\0oldname.txt\0newname.txt\0",
            "-\t-\tlogo.png\0",
        );
        let counts = parse_numstat_z(out);
        assert_eq!(counts.get("gone.txt"), Some(&(0, 1)));
        assert_eq!(counts.get("keep.txt"), Some(&(2, 1)));
        assert_eq!(counts.get("newname.txt"), Some(&(1, 0)));
        assert_eq!(counts.get("logo.png"), Some(&(0, 0)));
        assert!(!counts.contains_key("oldname.txt"));
    }

    #[test]
    fn safe_repo_path_rejects_escapes() {
        let root = Path::new("/repo");
        assert_eq!(safe_repo_path(root, ""), None);
        assert_eq!(safe_repo_path(root, "/etc/passwd"), None);
        assert_eq!(safe_repo_path(root, "../outside"), None);
        assert_eq!(safe_repo_path(root, "src/../../outside"), None);
        assert_eq!(safe_repo_path(root, "./src/x.rs"), None);
    }

    /// The blame pane builds pull request links from the origin URL, so
    /// every form git accepts has to resolve to the same `owner/name`.
    #[test]
    fn origin_urls_resolve_to_owner_and_name() {
        for url in [
            "https://github.com/acme/tool",
            "https://github.com/acme/tool.git",
            "https://github.com/acme/tool/",
            "git@github.com:acme/tool.git",
            "ssh://git@github.com/acme/tool.git",
        ] {
            assert_eq!(repo_from_url(url).as_deref(), Some("acme/tool"), "{url}");
        }
        // Not GitHub, and nothing to build a link from.
        assert_eq!(repo_from_url("git@gitlab.com:acme/tool.git"), None);
        assert_eq!(repo_from_url(""), None);
        assert_eq!(repo_from_url("https://github.com/"), None);
    }

    /// A stash message is free text. The separators are a unit and a
    /// record separator so a name with a newline or a tab in it still
    /// lands in one field.
    #[test]
    fn stash_records_survive_an_awkward_name() {
        let out = "stash@{0}\u{1f}On main: fix\nthe\tparser\u{1f}2 hours ago\u{1e}\n\
                   stash@{1}\u{1f}WIP on main: 4683983 something\u{1f}yesterday\u{1e}\n";
        let list = parse_stash_list(out);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].index, 0);
        assert_eq!(list[0].name, "stash@{0}");
        assert_eq!(list[0].subject, "On main: fix\nthe\tparser");
        assert_eq!(list[0].when, "2 hours ago");
        assert_eq!(list[1].index, 1, "the index is the position in the list");
        assert!(parse_stash_list("").is_empty());
    }

    /// A commit subject is free text too, and gets the same treatment.
    #[test]
    fn commit_records_survive_an_awkward_subject() {
        let out = "aaa111\u{1f}aaa\u{1f}Fix\tthe\nparser\u{1f}Jacob Lee\u{1f}2 hours ago\u{1e}\n\
                   bbb222\u{1f}bbb\u{1f}Second\u{1f}Someone Else\u{1f}yesterday\u{1e}\n";
        let log = parse_commit_log(out);
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].oid, "aaa111");
        assert_eq!(log[0].short, "aaa");
        assert_eq!(log[0].subject, "Fix\tthe\nparser");
        assert_eq!(log[0].author, "Jacob Lee");
        assert_eq!(log[1].oid, "bbb222");
        assert!(parse_commit_log("").is_empty());
    }

    /// A scratch repository, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("loupe-gitops-{name}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch directory");
            let d = dir.to_string_lossy().into_owned();
            for args in [
                vec!["-C", d.as_str(), "init", "-q", "-b", "main", "."],
                vec!["-C", d.as_str(), "config", "user.email", "loupe@test"],
                vec!["-C", d.as_str(), "config", "user.name", "loupe"],
            ] {
                run_git(&args).expect("git init");
            }
            Scratch(dir)
        }

        fn write(&self, name: &str, body: &str) {
            std::fs::write(self.0.join(name), body).expect("fixture written");
        }

        fn commit(&self, message: &str) {
            let d = self.0.to_string_lossy().into_owned();
            run_git(&["-C", &d, "add", "-A"]).unwrap();
            run_git(&["-C", &d, "commit", "-q", "-m", message]).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The three scopes take three different sets of work, and the name
    /// given to a stash comes back in the list.
    #[test]
    fn the_three_stash_scopes_take_different_work() {
        let s = Scratch::new("stash-scopes");
        s.write("a.txt", "one\n");
        s.commit("first");
        s.write("a.txt", "two\n");
        s.write("untracked.txt", "new\n");
        stage_all(&s.0).unwrap();
        // Only a.txt in the index for this one; untracked.txt was added
        // by `stage_all`, so take it back out first.
        run_git(&[
            "-C",
            &s.0.to_string_lossy(),
            "reset",
            "-q",
            "--",
            "untracked.txt",
        ])
        .unwrap();

        stash_push(&s.0, Some("just the index"), StashScope::StagedOnly).unwrap();
        let states = stage_states(&s.0).unwrap();
        assert_eq!(states.get("a.txt"), None, "the staged edit went away");
        assert_eq!(
            states.get("untracked.txt"),
            Some(&StageState::Unstaged),
            "the untracked file stayed"
        );

        let list = stash_list(&s.0).unwrap();
        assert_eq!(list.len(), 1);
        assert!(
            list[0].subject.contains("just the index"),
            "the name given is the name kept: {:?}",
            list[0].subject
        );

        // Untracked files need asking for by name.
        stash_push(&s.0, None, StashScope::Tracked).unwrap_err();
        stash_push(&s.0, Some("everything"), StashScope::WithUntracked).unwrap();
        assert!(!s.0.join("untracked.txt").exists());

        // And back out again, newest first.
        stash_apply(&s.0, 0, true).unwrap();
        assert!(s.0.join("untracked.txt").exists());
        assert_eq!(stash_list(&s.0).unwrap().len(), 1, "pop dropped it");

        stash_drop(&s.0, 0).unwrap();
        assert!(stash_list(&s.0).unwrap().is_empty());
    }

    /// git calls an empty stash a success and says so on stdout. The
    /// caller gets an error instead, so a menu click that did nothing
    /// does not look like one that worked.
    #[test]
    fn stashing_nothing_is_an_error() {
        let s = Scratch::new("stash-empty");
        s.write("a.txt", "one\n");
        s.commit("first");
        let err = stash_push(&s.0, None, StashScope::Tracked).unwrap_err();
        assert!(err.to_string().contains("nothing to stash"), "{err}");
    }

    /// The commit list is what the upstream does not have, and each
    /// commit can name its own files.
    #[test]
    fn unpushed_commits_list_what_the_upstream_lacks() {
        let s = Scratch::new("commits");
        s.write("a.txt", "one\n");
        s.commit("first");
        let base = head_at(&s.0);
        s.write("a.txt", "two\n");
        s.write("b.txt", "new\n");
        s.commit("second");
        s.write("c.txt", "third\n");
        s.commit("third");

        let commits = unpushed_commits(&s.0, &base).unwrap();
        assert_eq!(commits.len(), 2, "two commits since the base");
        assert_eq!(commits[0].subject, "third", "newest first");
        assert_eq!(commits[1].subject, "second");
        assert_eq!(commits[0].author, "loupe");
        assert!(!commits[0].short.is_empty());

        let files = commit_files(&s.0, &commits[1].oid).unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(names, ["a.txt", "b.txt"]);
        assert_eq!(files[0].status, "modified");
        assert_eq!(files[1].status, "added");
        assert_eq!(files[1].additions, 1);

        // A root commit has no parent to diff against, and lists its whole
        // tree as added rather than failing.
        let all = unpushed_commits(&s.0, "HEAD~2").unwrap();
        assert_eq!(all.len(), 2);
        assert!(first_parent(&s.0, &commits[0].oid).is_some());
    }

    fn head_at(root: &Path) -> String {
        run_git(&["-C", &root.to_string_lossy(), "rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string()
    }
}

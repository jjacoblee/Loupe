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
    if head_oid().is_some() {
        let ns = run_git(&["diff", "HEAD", "--name-status", "-M", "-z"])?;
        // Counts are cosmetic — don't fail the scan over them.
        let counts = run_git(&["diff", "HEAD", "--numstat", "-M", "-z"])
            .map(|o| parse_numstat_z(&o))
            .unwrap_or_default();
        for (status, previous, path) in parse_name_status_z(&ns) {
            let (additions, deletions) = counts.get(&path).copied().unwrap_or((0, 0));
            files.push(ChangedFile {
                path,
                status,
                additions,
                deletions,
                previous,
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
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
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
            ('?', _) | (' ', _) | ('!', _) => StageState::Unstaged,
            (_, ' ') => StageState::Staged,
            _ => StageState::Partial,
        };
        map.insert(path.to_string(), state);
    }
    map
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
}

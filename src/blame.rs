//! `git blame` for the pane between the file panel and the diff.
//!
//! The pane answers one question the diff cannot: is the code around this
//! change old and settled, or did it move recently — and was that me? So
//! every line carries three things: an author, an age, and the pull
//! request the commit belongs to.
//!
//! Three classes of line are colored apart from the age ramp, because
//! they mean different things to a reviewer:
//!
//! - **Uncommitted** — `git blame` gave the zero sha. Your working tree.
//! - **In this change** — the commit belongs to the change under review
//!   (see [`change_set`]). These are the lines this pull request moved.
//! - **History** — everything else, on the [`Heat`] ramp by age.
//!
//! Parsing is `git blame --porcelain`, which emits the full commit header
//! the *first* time a commit appears and the hash alone after that. The
//! parser therefore keeps a hash → [`Commit`] map and looks later lines up
//! in it; a per-line header (`--line-porcelain`) would be simpler and
//! several times larger on a file whose history is one commit deep.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use crate::gitops::run_git;

/// The zero sha `git blame` reports for a line that is not committed yet.
const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

/// Commits to read when working out which ones belong to a local branch.
/// A branch with more unpushed commits than this is not a review unit any
/// more, and the cap keeps the call bounded on a huge repository.
const LOCAL_CHANGE_CAP: &str = "500";

/// One commit, shared by every line it is responsible for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    pub author: String,
    pub author_email: String,
    /// Author time, as a Unix timestamp.
    pub author_time: i64,
    /// The first line of the commit message.
    pub summary: String,
    /// Pull request number read out of [`Commit::summary`]. A GitHub
    /// lookup fills in the rest later (see [`crate::github`]).
    pub pr: Option<u64>,
}

impl Commit {
    /// The abbreviated hash, the length `git log --oneline` uses.
    pub fn short(&self) -> &str {
        let n = self.sha.len().min(8);
        &self.sha[..n]
    }

    pub fn uncommitted(&self) -> bool {
        self.sha == ZERO_SHA
    }
}

/// What one line of a file is blamed on.
#[derive(Debug, Clone)]
pub struct BlameLine {
    pub commit: Arc<Commit>,
}

/// The blame of one file, indexed by line.
#[derive(Debug, Clone, Default)]
pub struct Blame {
    /// One entry per line of the blamed file, in order. Line *n* of the
    /// file is `lines[n - 1]` — the diff row model counts from 1.
    pub lines: Vec<BlameLine>,
}

impl Blame {
    /// The commit responsible for 1-based file line `n`.
    pub fn at(&self, n: usize) -> Option<&Arc<Commit>> {
        self.lines.get(n.checked_sub(1)?).map(|l| &l.commit)
    }

    /// Every distinct commit in the file, newest first. The pull request
    /// backfill asks GitHub about these.
    pub fn commits(&self) -> Vec<Arc<Commit>> {
        let mut seen = HashSet::new();
        let mut out: Vec<Arc<Commit>> = Vec::new();
        for line in &self.lines {
            if seen.insert(line.commit.sha.clone()) {
                out.push(line.commit.clone());
            }
        }
        out.sort_by_key(|c| std::cmp::Reverse(c.author_time));
        out
    }
}

/// How old a line is, in the steps the heat map paints. The ramp is
/// **absolute**, not relative to the file: the same color means the same
/// age everywhere, so the scale is learned once rather than per file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heat {
    /// Not committed — your working tree, right now.
    Uncommitted,
    /// The commit belongs to the change under review.
    InChange,
    /// Committed history, `0` newest (under a day) to `5` oldest (over a
    /// year). Indexes [`crate::theme::Palette::blame_heat`].
    Age(usize),
}

/// Age boundaries of the [`Heat::Age`] steps, in seconds.
const DAY: i64 = 60 * 60 * 24;
const AGE_STEPS: [i64; 5] = [DAY, DAY * 7, DAY * 30, DAY * 90, DAY * 365];

/// Which step of the ramp a commit sits on, given the time now.
pub fn heat(commit: &Commit, now: i64, in_change: bool) -> Heat {
    if commit.uncommitted() {
        return Heat::Uncommitted;
    }
    if in_change {
        return Heat::InChange;
    }
    let age = (now - commit.author_time).max(0);
    Heat::Age(AGE_STEPS.iter().position(|b| age < *b).unwrap_or(5))
}

/// A short age, the width the pane has room for: `2h`, `3d`, `5mo`, `2y`.
pub fn ago(time: i64, now: i64) -> String {
    let s = (now - time).max(0);
    match s {
        s if s < 60 => "now".into(),
        s if s < 60 * 60 => format!("{}m", s / 60),
        s if s < DAY => format!("{}h", s / (60 * 60)),
        s if s < DAY * 30 => format!("{}d", s / DAY),
        s if s < DAY * 365 => format!("{}mo", s / (DAY * 30)),
        s => format!("{}y", s / (DAY * 365)),
    }
}

/// The pull request number a squash or merge commit names in its subject.
///
/// GitHub writes `Some change (#412)` for a squash merge and `Merge pull
/// request #412 from owner/branch` for a merge commit, so most history
/// resolves with no network call at all. Anything else — a rebase merge,
/// a direct push — gives None and waits for the GitHub lookup.
///
/// The match is deliberately narrow: `#5` inside `arr[#5]` is not a pull
/// request, and a wrong number would send the reader to the wrong page.
pub fn pr_from_summary(summary: &str) -> Option<u64> {
    // `Merge pull request #412 from ...`
    if let Some(rest) = summary.strip_prefix("Merge pull request #") {
        return leading_number(rest);
    }
    // `... (#412)`, at the end of the subject, where GitHub puts it.
    let trimmed = summary.trim_end();
    let inner = trimmed.strip_suffix(')')?;
    let at = inner.rfind("(#")?;
    // The marker has to be its own word, not the tail of `foo(#1`.
    if at > 0 && !inner[..at].ends_with(' ') {
        return None;
    }
    let digits = &inner[at + 2..];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn leading_number(s: &str) -> Option<u64> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    s[..end].parse().ok()
}

/// Parse `git blame --porcelain` output into one entry per line.
///
/// The format: a header line `<sha> <orig-line> <final-line> [<count>]`,
/// then — only the first time that sha appears — key/value lines for the
/// commit, then the file line itself prefixed with a tab. Later lines of
/// the same commit carry the header alone, which is why the commit map
/// exists.
pub fn parse_porcelain(out: &str) -> Blame {
    let mut commits: HashMap<String, Arc<Commit>> = HashMap::new();
    let mut lines: Vec<BlameLine> = Vec::new();
    // The commit currently being described, and the fields seen so far.
    let mut sha = String::new();
    let mut author = String::new();
    let mut email = String::new();
    let mut time: i64 = 0;
    let mut summary = String::new();

    for line in out.lines() {
        // The file's own text — the end of the current entry.
        if let Some(_text) = line.strip_prefix('\t') {
            if !commits.contains_key(&sha) {
                let commit = Arc::new(Commit {
                    sha: sha.clone(),
                    author: std::mem::take(&mut author),
                    author_email: std::mem::take(&mut email),
                    author_time: time,
                    pr: pr_from_summary(&summary),
                    summary: std::mem::take(&mut summary),
                });
                commits.insert(sha.clone(), commit);
            }
            if let Some(commit) = commits.get(&sha) {
                lines.push(BlameLine {
                    commit: commit.clone(),
                });
            }
            continue;
        }
        match line.split_once(' ') {
            // A header line: the first field is 40 or 64 hex characters.
            Some((first, _))
                if first.len() >= 40
                    && first.bytes().all(|b| b.is_ascii_hexdigit())
                    && line.split(' ').count() >= 3 =>
            {
                sha = first.to_string();
            }
            Some(("author", v)) => author = v.to_string(),
            Some(("author-mail", v)) => email = v.trim_matches(['<', '>']).to_string(),
            Some(("author-time", v)) => time = v.trim().parse().unwrap_or(0),
            Some(("summary", v)) => summary = v.to_string(),
            _ => {}
        }
    }
    Blame { lines }
}

/// Blame `path` at `rev`, or in the working tree when `rev` is None.
///
/// Blaming the working tree is what marks your uncommitted edits as
/// uncommitted; blaming a commit is what a pull request whose branch is
/// not checked out needs, because the file on disk belongs to some other
/// branch.
///
/// `--no-progress` keeps a slow blame from writing to the terminal Loupe
/// has in raw mode. Returns None rather than an error for a path with no
/// history — a file added by this change has nothing to blame.
pub fn blame_file(root: &Path, rev: Option<&str>, path: &str) -> Option<Blame> {
    let root = root.to_string_lossy().into_owned();
    let mut args: Vec<&str> = vec!["-C", &root, "blame", "--porcelain", "--no-progress"];
    if let Some(rev) = rev {
        // An empty oid would make git read the index instead of a commit.
        if rev.is_empty() {
            return None;
        }
        args.push(rev);
    }
    args.push("--");
    args.push(path);
    let out = run_git(&args).ok()?;
    let blame = parse_porcelain(&out);
    (!blame.lines.is_empty()).then_some(blame)
}

/// The commits that belong to the change under review — the set that
/// paints a line as [`Heat::InChange`].
///
/// For a pull request that is `base..head`, exactly the commits the pull
/// request adds. For local review there is no pull request to bound it,
/// so it is HEAD minus every remote: the work you have made and not
/// pushed, which is the same question in the local shape.
pub fn change_set(root: &Path, merge_base: &str, head: &str, local: bool) -> HashSet<String> {
    let root = root.to_string_lossy().into_owned();
    // `rev-list a..b` is one argument, so the range is built before the
    // argument list borrows it.
    let range = format!("{merge_base}..{head}");
    let args: Vec<&str> = if local {
        vec![
            "-C",
            &root,
            "rev-list",
            "-n",
            LOCAL_CHANGE_CAP,
            "HEAD",
            "--not",
            "--remotes",
        ]
    } else if merge_base.is_empty() || head.is_empty() {
        return HashSet::new();
    } else {
        vec!["-C", &root, "rev-list", &range]
    };
    run_git(&args)
        .map(|out| out.lines().map(|l| l.trim().to_string()).collect())
        .unwrap_or_default()
}

/// The email git commits as, so blame can tell the reader's own work
/// apart from everyone else's. None when git has no `user.email` set.
pub fn my_email(root: &Path) -> Option<String> {
    let root = root.to_string_lossy().into_owned();
    run_git(&["-C", &root, "config", "user.email"])
        .ok()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
}

/// Seconds since the Unix epoch, for the age arithmetic.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A calendar date for the commit popup, where there is room to be exact.
/// Plain civil-date arithmetic from the timestamp: no chrono, and the
/// popup is the only place that needs it.
pub fn date(time: i64) -> String {
    let days = time.div_euclid(DAY);
    let secs = time.rem_euclid(DAY);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        secs % 3600 / 60
    )
}

/// Days since the epoch to a civil date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `git blame --porcelain` output: the first commit carries a
    /// full header, its later lines carry the hash alone, and the
    /// working-tree lines come back on the zero sha.
    const PORCELAIN: &str = "\
4349b2a0b02c7d355f97e0089828e6285f6fdb4c 1 1 1
author Tester
author-mail <a@b.c>
author-time 1700000000
author-tz -0500
committer Tester
committer-mail <a@b.c>
committer-time 1700000000
committer-tz -0500
summary first (#7)
boundary
filename f.txt
\tone
0000000000000000000000000000000000000000 2 2 2
author Not Committed Yet
author-mail <not.committed.yet>
author-time 1700000900
author-tz -0500
committer Not Committed Yet
committer-mail <not.committed.yet>
committer-time 1700000900
committer-tz -0500
summary Version of f.txt from f.txt
previous 4349b2a0b02c7d355f97e0089828e6285f6fdb4c f.txt
filename f.txt
\ttwoX
0000000000000000000000000000000000000000 3 3
\tthree
";

    #[test]
    fn porcelain_parses_headers_and_the_repeated_short_form() {
        let b = parse_porcelain(PORCELAIN);
        assert_eq!(b.lines.len(), 3, "one entry per file line");

        let first = b.at(1).expect("line 1");
        assert_eq!(first.author, "Tester");
        assert_eq!(first.author_email, "a@b.c");
        assert_eq!(first.author_time, 1_700_000_000);
        assert_eq!(first.summary, "first (#7)");
        assert_eq!(first.pr, Some(7), "the subject names the pull request");
        assert!(!first.uncommitted());
        assert_eq!(first.short(), "4349b2a0");

        // Line 3 repeats the zero sha with no header of its own.
        assert!(b.at(2).unwrap().uncommitted());
        assert!(b.at(3).unwrap().uncommitted());
        assert_eq!(
            b.at(2).unwrap().sha,
            b.at(3).unwrap().sha,
            "the short form resolves through the commit map"
        );
        // The `boundary` and `previous` lines are not fields; they must
        // not be mistaken for one.
        assert_eq!(b.at(2).unwrap().author, "Not Committed Yet");
    }

    #[test]
    fn distinct_commits_come_back_newest_first() {
        let b = parse_porcelain(PORCELAIN);
        let cs = b.commits();
        assert_eq!(cs.len(), 2, "three lines, two commits");
        assert!(cs[0].uncommitted(), "the newer one leads");
    }

    #[test]
    fn a_line_number_past_the_end_has_no_commit() {
        let b = parse_porcelain(PORCELAIN);
        assert!(b.at(4).is_none());
        assert!(b.at(0).is_none(), "file lines count from 1");
    }

    #[test]
    fn heat_steps_at_every_boundary() {
        let now = 1_800_000_000;
        let at = |age: i64| {
            let c = Commit {
                sha: "a".repeat(40),
                author: String::new(),
                author_email: String::new(),
                author_time: now - age,
                summary: String::new(),
                pr: None,
            };
            heat(&c, now, false)
        };
        assert_eq!(at(0), Heat::Age(0), "just committed");
        assert_eq!(at(DAY - 1), Heat::Age(0));
        assert_eq!(at(DAY), Heat::Age(1), "a day old steps down");
        assert_eq!(at(DAY * 7), Heat::Age(2));
        assert_eq!(at(DAY * 30), Heat::Age(3));
        assert_eq!(at(DAY * 90), Heat::Age(4));
        assert_eq!(at(DAY * 365), Heat::Age(5));
        assert_eq!(at(DAY * 4000), Heat::Age(5), "the ramp bottoms out");
        // A clock that disagrees must not underflow into the hot end.
        assert_eq!(at(-DAY), Heat::Age(0));
    }

    #[test]
    fn uncommitted_and_in_change_outrank_age() {
        let old = Commit {
            sha: "b".repeat(40),
            author: String::new(),
            author_email: String::new(),
            author_time: 0,
            summary: String::new(),
            pr: None,
        };
        assert_eq!(heat(&old, 1_800_000_000, true), Heat::InChange);
        let zero = Commit {
            sha: ZERO_SHA.into(),
            ..old.clone()
        };
        assert_eq!(
            heat(&zero, 1_800_000_000, false),
            Heat::Uncommitted,
            "the working tree wins over any age"
        );
    }

    #[test]
    fn pull_request_numbers_come_out_of_the_subject() {
        assert_eq!(pr_from_summary("Fix the parser (#412)"), Some(412));
        assert_eq!(
            pr_from_summary("Merge pull request #412 from acme/branch"),
            Some(412)
        );
        // Not markers.
        assert_eq!(pr_from_summary("no marker here"), None);
        assert_eq!(pr_from_summary("index arr[#5]"), None);
        assert_eq!(pr_from_summary("call foo(#1)"), None, "not its own word");
        assert_eq!(pr_from_summary("Fix (#not-a-number)"), None);
        assert_eq!(pr_from_summary("Fix (#)"), None);
        assert_eq!(pr_from_summary(""), None);
    }

    #[test]
    fn ages_read_short_enough_for_the_pane() {
        let now = 1_800_000_000;
        assert_eq!(ago(now, now), "now");
        assert_eq!(ago(now - 90, now), "1m");
        assert_eq!(ago(now - 60 * 60 * 5, now), "5h");
        assert_eq!(ago(now - DAY * 3, now), "3d");
        assert_eq!(ago(now - DAY * 60, now), "2mo");
        assert_eq!(ago(now - DAY * 800, now), "2y");
        assert!(ago(now - DAY * 800, now).len() <= 4, "the column is narrow");
    }

    #[test]
    fn dates_render_for_the_popup() {
        // 2023-11-14 22:13:20 UTC
        assert_eq!(date(1_700_000_000), "2023-11-14 22:13");
        assert_eq!(date(0), "1970-01-01 00:00");
    }

    #[test]
    fn an_empty_blame_yields_no_lines() {
        assert!(parse_porcelain("").lines.is_empty());
    }

    /// Round-trip against a real repository. The porcelain parser is
    /// tested above against fixed output; this is the only check that the
    /// `git blame` invocation still produces that shape, and that
    /// `change_set` names the commits a branch actually adds.
    #[test]
    fn blame_and_change_set_against_a_real_repo() {
        let root = std::env::temp_dir().join(format!("loupe-blame-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let r = root.to_string_lossy().into_owned();
        let git = |args: &[&str]| {
            let mut full = vec!["-C", r.as_str()];
            full.extend_from_slice(args);
            run_git(&full).unwrap_or_else(|e| panic!("git {args:?}: {e:#}"))
        };
        git(&["init", "-q", "."]);
        git(&["config", "user.email", "first@test"]);
        git(&["config", "user.name", "First"]);
        std::fs::write(root.join("f.txt"), "one\ntwo\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base commit"]);
        let base = git(&["rev-parse", "HEAD"]).trim().to_string();

        // A second commit, by a different author, touching line 2 only.
        git(&["config", "user.email", "second@test"]);
        git(&["config", "user.name", "Second"]);
        std::fs::write(root.join("f.txt"), "one\nTWO\n").unwrap();
        git(&["commit", "-qam", "change the second line (#31)"]);
        let head = git(&["rev-parse", "HEAD"]).trim().to_string();

        // …and an uncommitted third line on top.
        std::fs::write(root.join("f.txt"), "one\nTWO\nthree\n").unwrap();

        let b = blame_file(&root, None, "f.txt").expect("the working tree blames");
        assert_eq!(b.lines.len(), 3);
        assert_eq!(b.at(1).unwrap().author, "First", "line 1 is untouched");
        assert_eq!(b.at(2).unwrap().author, "Second");
        assert_eq!(b.at(2).unwrap().author_email, "second@test");
        assert_eq!(b.at(2).unwrap().pr, Some(31), "read out of the subject");
        assert!(b.at(3).unwrap().uncommitted(), "line 3 is not committed");

        // Blaming the commit instead sees only what was committed.
        let at_head = blame_file(&root, Some(&head), "f.txt").expect("HEAD blames");
        assert_eq!(at_head.lines.len(), 2, "the third line is not in HEAD");
        assert!(at_head.lines.iter().all(|l| !l.commit.uncommitted()));

        // The change set for "this branch adds one commit on top of base".
        let set = change_set(&root, &base, &head, false);
        assert_eq!(set.len(), 1);
        assert!(set.contains(&head));
        assert!(!set.contains(&base), "the base is not part of the change");

        // A path with no history is not an error — it has nothing to say.
        assert!(blame_file(&root, None, "missing.txt").is_none());
        // An empty rev must never be passed through: git would read the
        // index rather than a commit.
        assert!(blame_file(&root, Some(""), "f.txt").is_none());

        assert_eq!(my_email(&root).as_deref(), Some("second@test"));
        let _ = std::fs::remove_dir_all(&root);
    }
}

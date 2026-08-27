//! Finding things: fuzzy path matching, a dependency-free definition
//! scanner, and `git grep`.
//!
//! Three questions the finder answers, in rising order of cost:
//!
//! 1. *Which file?* — [`fuzzy`] ranks paths the way an fzf-style matcher
//!    does, in memory, with no subprocess at all.
//! 2. *Where in this file?* — [`symbols`] scans one file's text for lines
//!    that look like definitions.
//! 3. *Where in the repository?* — [`grep`] shells out to `git grep` once
//!    and parses its NUL-delimited output.
//!
//! Everything here is pure except [`grep`] and [`list_files`], so the
//! ranking and the scanner are unit-testable without a repository.
//!
//! ## Why `git grep` and not our own walker
//!
//! `git grep` already knows what is tracked, what is ignored, what is
//! binary, and how to read a *commit* rather than the working tree — that
//! last one matters, because in PR review the file on disk is not always
//! the file being reviewed. One subprocess beats walking the tree
//! ourselves, and it costs no dependency.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Most hits a single search reports. Past this the list stops being a
/// list and starts being a haystack of its own; the UI says it truncated.
pub const RESULT_LIMIT: usize = 400;

/// One matching line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub path: String,
    /// 1-based line number.
    pub line: usize,
    pub text: String,
    /// Char offset and length of the first match within `text`, for
    /// highlighting (0/0 when the pattern was a regex we don't re-run).
    pub col: usize,
    pub len: usize,
    /// The line looks like the definition of the thing searched for.
    pub definition: bool,
}

/// What to search, and where.
#[derive(Debug, Clone)]
pub struct GrepRequest {
    pub root: PathBuf,
    pub query: String,
    /// Commit to search. `None` searches the working tree — which is what
    /// local review wants, and what PR review must *not* use (the tree may
    /// be on another branch entirely).
    pub rev: Option<String>,
    /// Restrict to these paths; empty means the whole tree.
    pub paths: Vec<String>,
    pub regex: bool,
    /// Include files git doesn't track yet (working-tree searches only).
    pub untracked: bool,
}

/// Run one `git grep`. Returns the hits and whether they were truncated.
///
/// Exit status 1 means "no matches", which is an answer, not a failure —
/// only status ≥ 2 is an error worth showing.
pub fn grep(req: &GrepRequest) -> Result<(Vec<Hit>, bool)> {
    if req.query.is_empty() {
        return Ok((Vec::new(), false));
    }
    let root = req.root.to_string_lossy().to_string();
    let mut args: Vec<String> = vec![
        "-C".into(),
        root,
        "grep".into(),
        // NUL-delimited (`path\0lineno\0text`) so a path with a colon in
        // it can't be mistaken for the line-number separator.
        "-z".into(),
        "-n".into(),
        "-I".into(), // never binary files
        "--no-color".into(),
        "--full-name".into(),
    ];
    args.push(if req.regex { "-E" } else { "-F" }.into());
    if smart_case(&req.query) {
        args.push("-i".into());
    }
    if req.rev.is_none() && req.untracked {
        args.push("--untracked".into());
    }
    // `-e` keeps a query starting with `-` from parsing as an option.
    args.push("-e".into());
    args.push(req.query.clone());
    if let Some(rev) = &req.rev {
        args.push(rev.clone());
    }
    args.push("--".into());
    if req.paths.is_empty() {
        args.push(".".into());
    } else {
        // `:(literal)` disables pathspec globbing: a file called `a[1].ts`
        // is a filename, not a character class.
        for p in &req.paths {
            args.push(format!(":(literal){p}"));
        }
    }

    let out = Command::new("git")
        .args(&args)
        .output()
        .context("failed to spawn git — is git installed?")?;
    match out.status.code() {
        Some(0) | Some(1) => {}
        _ => {
            anyhow::bail!(
                "git grep failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let prefix = req.rev.as_ref().map(|r| format!("{r}:"));
    let mut hits = Vec::new();
    let mut truncated = false;
    for record in text.split('\n').filter(|r| !r.is_empty()) {
        if hits.len() >= RESULT_LIMIT {
            truncated = true;
            break;
        }
        let Some(hit) = parse_record(record, prefix.as_deref(), &req.query, req.regex) else {
            continue;
        };
        hits.push(hit);
    }
    Ok((hits, truncated))
}

/// One `path\0lineno\0text` record (with `rev:` still on the path when the
/// search was against a commit).
fn parse_record(record: &str, prefix: Option<&str>, query: &str, regex: bool) -> Option<Hit> {
    let mut parts = record.splitn(3, '\0');
    let path = parts.next()?;
    let line: usize = parts.next()?.parse().ok()?;
    let text = parts.next()?;
    let path = match prefix {
        Some(p) => path.strip_prefix(p)?,
        None => path,
    };
    // A regex match's extent is git's business, not ours — highlight only
    // what we can locate ourselves.
    let (col, len) = if regex {
        (0, 0)
    } else {
        find_ranges(text, query)
            .first()
            .map(|(s, e)| (*s, e - s))
            .unwrap_or((0, 0))
    };
    Some(Hit {
        path: path.to_string(),
        line,
        text: text.to_string(),
        col,
        len,
        definition: !regex && defines(path, text, query),
    })
}

/// What `git ls-files` found in a repository.
///
/// The two lists are kept apart because git reports two different kinds
/// of thing and the caller wants only one of them. See [`list_files`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RepoFiles {
    /// Every file, as a repo-relative path.
    pub files: Vec<String>,
    /// Directories git reported instead of walking into them: a nested
    /// repository, or a linked worktree someone put inside the clone.
    /// Stored without the trailing slash git writes.
    pub stubs: Vec<String>,
}

/// Every file in the repository — the haystack for a repo-wide fuzzy file
/// search. Reads a commit when given one, so it matches what the diff
/// view is showing.
///
/// ## Why the result is split
///
/// `git ls-files` refuses to walk into a nested repository boundary and
/// reports the directory itself, with a trailing slash:
///
/// ```text
/// wt/feat/          <- a worktree someone put inside the clone
/// .gitignore
/// src/main.rs
/// ```
///
/// Mixed into the file list, `wt/feat/` reaches the tree builder, which
/// splits on `/` and pops an empty last component — a file row with no
/// name, pointing at a directory. So the stubs come back on their own:
/// the file finder drops them, and the file tree draws them as folders it
/// has not opened yet.
pub fn list_files(root: &Path, rev: Option<&str>) -> Result<RepoFiles> {
    match rev {
        Some(rev) => run_ls(root, &["ls-tree", "-r", "--name-only", "-z", rev]),
        None => run_ls(
            root,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        ),
    }
}

/// The files git is ignoring, and the directories it is ignoring whole.
///
/// `--directory` is what makes this affordable. Without it git walks into
/// every ignored directory and reports its contents: on one real Astro
/// repository that is 17,504 entries and 147 ms, nearly all of it
/// `node_modules`. With it, git stops at a directory whose whole contents
/// are ignored and reports the directory — 10 entries and 14 ms:
///
/// ```text
/// .env
/// debug.log
/// dist/
/// node_modules/
/// ```
///
/// Which is exactly the split the file tree wants. An ignored *file* is
/// something a developer needs to open — `.env` above — so it is listed.
/// An ignored *directory* is `node_modules`, so it costs one row until
/// somebody asks for more.
pub fn list_ignored(root: &Path) -> Result<RepoFiles> {
    run_ls(
        root,
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "--no-empty-directory",
        ],
    )
}

/// Run one `git ls-files`/`ls-tree` and split its output into files and
/// the directories git declined to walk into. See [`RepoFiles`].
fn run_ls(root: &Path, args: &[&str]) -> Result<RepoFiles> {
    let root = root.to_string_lossy().to_string();
    let mut full: Vec<&str> = vec!["-C", &root];
    full.extend_from_slice(args);
    let out = Command::new("git")
        .args(&full)
        .output()
        .context("failed to spawn git — is git installed?")?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.first().unwrap_or(&"ls-files"),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let mut found = RepoFiles::default();
    for entry in String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|s| !s.is_empty())
    {
        match entry.strip_suffix('/') {
            Some(dir) => found.stubs.push(dir.to_string()),
            None => found.files.push(entry.to_string()),
        }
    }
    Ok(found)
}

/// Everything the file tree draws for one repository.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RepoListing {
    /// Every path: the tracked and untracked ones first, then the ignored
    /// ones. One list rather than two so a row is one index.
    pub paths: Vec<String>,
    /// Where the ignored paths begin in `paths`. Rows at or past this
    /// index are drawn dim: git is ignoring them, and a reader should see
    /// that before opening one.
    pub ignored_from: usize,
    /// Directories git would not walk into, and whether each is one it is
    /// ignoring. A nested repository or a linked worktree inside the clone
    /// is *not* ignored — its contents are ordinary files — while
    /// `node_modules` is. The row that reads a stub needs to know which,
    /// or everything inside a worktree comes back drawn as ignored.
    pub stubs: Vec<(String, bool)>,
}

/// Both halves of the listing, for the file tree.
///
/// Two `git ls-files` calls: about 93 ms for 16,761 tracked files, and
/// about 14 ms for the ignored half. Far past a frame, so this belongs on
/// a worker thread.
pub fn list_repo(root: &Path) -> Result<RepoListing> {
    let tracked = list_files(root, None)?;
    let ignored = list_ignored(root)?;
    let mut paths = tracked.files;
    let ignored_from = paths.len();
    paths.extend(ignored.files);
    let mut stubs: Vec<(String, bool)> = tracked.stubs.into_iter().map(|d| (d, false)).collect();
    stubs.extend(ignored.stubs.into_iter().map(|d| (d, true)));
    stubs.sort();
    stubs.dedup_by(|a, b| a.0 == b.0);
    Ok(RepoListing {
        paths,
        ignored_from,
        stubs,
    })
}

// ------------------------------------------------------------------ matching

/// Smart case, the way every good search box does it: an all-lowercase
/// query ignores case, a query with a capital in it means that capital.
pub fn smart_case(query: &str) -> bool {
    !query.chars().any(char::is_uppercase)
}

/// Char ranges of every occurrence of `needle` in `text`, honouring
/// [`smart_case`]. Char indices, not bytes — the renderer counts columns.
pub fn find_ranges(text: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let fold = smart_case(needle);
    let hay: Vec<char> = if fold {
        text.chars().flat_map(char::to_lowercase).collect()
    } else {
        text.chars().collect()
    };
    let pat: Vec<char> = if fold {
        needle.chars().flat_map(char::to_lowercase).collect()
    } else {
        needle.chars().collect()
    };
    // Lowercasing can change char counts (ẞ → ss), which would shift every
    // column. Rare enough to simply not highlight.
    if fold && hay.len() != text.chars().count() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if pat.len() > hay.len() {
        return out;
    }
    let mut i = 0;
    while i + pat.len() <= hay.len() {
        if hay[i..i + pat.len()] == pat[..] {
            out.push((i, i + pat.len()));
            i += pat.len();
        } else {
            i += 1;
        }
    }
    out
}

// Scoring weights, in fzf's proportions. What matters is the ratios: a
// boundary bonus that outweighed a whole extra match would rank
// `a/p/parse.rs` above `src/app.rs` for "app", which is not what anyone
// means when they type "app".
const SCORE_MATCH: i32 = 16;
/// First character of the string.
const BONUS_START: i32 = 12;
/// After a path separator.
const BONUS_SEGMENT: i32 = 10;
/// After `_`, `-`, `.` or a space.
const BONUS_WORD: i32 = 8;
/// A lowercase→uppercase step, i.e. the hump in camelCase.
const BONUS_CAMEL: i32 = 7;
/// Floor for a match adjacent to the previous one; a run also inherits
/// the bonus of the character that started it, so `app` in `src/app.rs`
/// keeps the "start of a path segment" credit across all three chars.
const BONUS_CONSECUTIVE: i32 = 4;
const GAP_START: i32 = -3;
const GAP_EXTENSION: i32 = -1;
/// Longest haystack we score. Past this a path is pathological; a plain
/// subsequence test still lets it match, it just doesn't get ranked.
const MAX_HAY: usize = 320;

/// Per-position bonus: how much this char looks like the start of
/// something a person would type.
fn boundary_bonus(chars: &[char], j: usize) -> i32 {
    if j == 0 {
        return BONUS_START;
    }
    let prev = chars[j - 1];
    let cur = chars[j];
    if prev == '/' || prev == '\\' {
        BONUS_SEGMENT
    } else if prev == '_' || prev == '-' || prev == '.' || prev == ' ' {
        BONUS_WORD
    } else if prev.is_lowercase() && cur.is_uppercase() {
        BONUS_CAMEL
    } else {
        0
    }
}

/// Score `needle` against `hay`, fzf-style: `None` when the needle isn't a
/// subsequence at all, otherwise the score and the matched char indices
/// (so the UI can bold exactly what matched).
///
/// This is a full dynamic program rather than a greedy left-to-right walk
/// because greedy gets `app/model.rs` vs `a/p/proto.rs` wrong: the first
/// place each char *fits* is rarely the place it *belongs*.
pub fn fuzzy(needle: &str, hay: &str) -> Option<(i32, Vec<usize>)> {
    if needle.is_empty() {
        return Some((0, Vec::new()));
    }
    let fold = smart_case(needle);
    let hay_chars: Vec<char> = hay.chars().collect();
    let pat: Vec<char> = needle.chars().collect();
    if pat.len() > hay_chars.len() {
        return None;
    }
    let lower_hay: Vec<char> = if fold {
        hay_chars
            .iter()
            .map(|c| c.to_lowercase().next().unwrap_or(*c))
            .collect()
    } else {
        hay_chars.clone()
    };
    let lower_pat: Vec<char> = if fold {
        pat.iter()
            .map(|c| c.to_lowercase().next().unwrap_or(*c))
            .collect()
    } else {
        pat.clone()
    };
    // Cheap reject before the O(n·m) part.
    {
        let mut it = lower_hay.iter();
        if !lower_pat.iter().all(|c| it.any(|h| h == c)) {
            return None;
        }
    }
    if hay_chars.len() > MAX_HAY {
        return Some((0, Vec::new()));
    }

    let n = lower_pat.len();
    let m = lower_hay.len();
    let bonus: Vec<i32> = (0..m).map(|j| boundary_bonus(&hay_chars, j)).collect();

    // `h[i][j]`: best score for the first `i+1` pattern chars consumed
    // within `hay[..=j]`. `run[i][j]`: length of the consecutive-match run
    // ending at `j` (0 when this cell took a gap). `took_match` is the
    // traceback — which branch won — so the matched positions come back
    // exactly, not by re-deriving them.
    let mut h = vec![0i32; n * m];
    let mut run = vec![0u16; n * m];
    let mut took_match = vec![false; n * m];

    #[allow(clippy::needless_range_loop)] // `i` indexes three tables, not one
    for i in 0..n {
        let mut in_gap = false;
        for j in 0..m {
            let row = i * m;
            // Branch 1: skip hay[j] and keep whatever we had.
            let gap_score = if j > 0 {
                h[row + j - 1] + if in_gap { GAP_EXTENSION } else { GAP_START }
            } else {
                i32::MIN / 4
            };
            // Branch 2: match pattern[i] here.
            let mut match_score = i32::MIN / 4;
            let mut run_len = 0u16;
            if lower_hay[j] == lower_pat[i] {
                let prev = if i == 0 {
                    Some(0)
                } else if j == 0 {
                    None
                } else {
                    Some(h[row - m + j - 1])
                };
                if let Some(prev) = prev {
                    let prev_run = if i == 0 || j == 0 {
                        0
                    } else {
                        run[row - m + j - 1]
                    };
                    run_len = prev_run + 1;
                    let mut b = bonus[j];
                    if run_len > 1 {
                        let first = bonus[j + 1 - run_len as usize];
                        if b >= BONUS_SEGMENT && b > first {
                            // This char starts a *better* boundary than the
                            // run did — treat it as a fresh run.
                            run_len = 1;
                        } else {
                            b = b.max(first.max(BONUS_CONSECUTIVE));
                        }
                    }
                    if i == 0 {
                        // Where the match starts says the most about intent.
                        b *= 2;
                    }
                    match_score = prev + SCORE_MATCH + b;
                }
            }
            let idx = row + j;
            if match_score >= gap_score {
                h[idx] = match_score.max(0);
                run[idx] = run_len;
                took_match[idx] = match_score > 0;
                in_gap = false;
            } else {
                h[idx] = gap_score.max(0);
                run[idx] = 0;
                in_gap = true;
            }
        }
    }

    let last = (0..m).max_by_key(|j| (h[(n - 1) * m + j], std::cmp::Reverse(*j)))?;
    let total = h[(n - 1) * m + last];
    if total <= 0 {
        return None;
    }
    let mut idx = vec![0usize; n];
    let mut i = n as i64 - 1;
    let mut j = last as i64;
    while i >= 0 && j >= 0 {
        if took_match[i as usize * m + j as usize] {
            idx[i as usize] = j as usize;
            i -= 1;
        }
        j -= 1;
    }
    if i >= 0 {
        // Traceback fell off the front (only possible on a degenerate
        // scoring path) — the score still stands, the highlight doesn't.
        return Some((total, Vec::new()));
    }
    // Shorter haystacks win ties: `ui.rs` should beat `vendor/x/ui.rs`.
    Some((total - (m as i32) / 4, idx))
}

// ------------------------------------------------------------------ symbols

/// A line that looks like it defines something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// 1-based line number.
    pub line: usize,
    pub name: String,
    /// Short label: `fn`, `class`, `type`, …
    pub kind: &'static str,
    /// The source line itself, trimmed.
    pub text: String,
}

/// Language families that share definition syntax closely enough to share
/// a matcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang {
    Rust,
    /// TypeScript, JavaScript, and their JSX flavors.
    Web,
    Python,
    Go,
    /// C, C++, Java, C#, Swift, Kotlin, Scala, PHP — brace languages with
    /// `type name(args) {` methods.
    Brace,
    Ruby,
    Shell,
    Other,
}

fn lang_of(path: &str) -> Lang {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => Lang::Rust,
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" | "svelte" | "vue" => Lang::Web,
        "py" | "pyi" => Lang::Python,
        "go" => Lang::Go,
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "java" | "cs" | "swift" | "kt"
        | "kts" | "scala" | "php" | "m" | "mm" => Lang::Brace,
        "rb" | "rake" => Lang::Ruby,
        "sh" | "bash" | "zsh" | "fish" => Lang::Shell,
        _ => Lang::Other,
    }
}

/// Words that can appear before the real keyword and mean nothing to us.
const MODIFIERS: &[&str] = &[
    "pub",
    "export",
    "default",
    "async",
    "static",
    "public",
    "private",
    "protected",
    "internal",
    "final",
    "abstract",
    "override",
    "open",
    "sealed",
    "unsafe",
    "extern",
    "declare",
    "inline",
    "virtual",
    "explicit",
    "constexpr",
    "readonly",
];

/// Keyword → the label shown next to the symbol.
const KEYWORDS: &[(&str, &str)] = &[
    ("fn", "fn"),
    ("func", "fn"),
    ("function", "fn"),
    ("def", "fn"),
    ("defp", "fn"),
    ("class", "class"),
    ("struct", "struct"),
    ("enum", "enum"),
    ("trait", "trait"),
    ("interface", "interface"),
    ("protocol", "interface"),
    ("impl", "impl"),
    ("type", "type"),
    ("typedef", "type"),
    ("mod", "mod"),
    ("module", "mod"),
    ("namespace", "mod"),
    ("union", "struct"),
    ("record", "class"),
    ("object", "class"),
    ("macro_rules!", "macro"),
];

/// Statement keywords that start a *call site*, never a definition — the
/// guard that keeps `if (foo(x)) {` out of the symbol list.
const CONTROL: &[&str] = &[
    "if", "else", "for", "while", "switch", "case", "catch", "do", "return", "match", "when",
    "with", "try", "throw", "await", "yield", "new", "delete", "using", "import", "from",
    "require", "assert", "print", "echo", "elif", "except", "finally", "in", "of", "and", "or",
    "not",
];

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Leading identifier of `s`, stripped of generics and punctuation.
fn ident(s: &str) -> Option<String> {
    let name: String = s.chars().take_while(|c| is_ident_char(*c)).collect();
    if name.is_empty() || name.chars().next()?.is_ascii_digit() {
        None
    } else {
        Some(name)
    }
}

fn is_comment(t: &str, lang: Lang) -> bool {
    t.starts_with("//")
        || t.starts_with("/*")
        || t.starts_with('*')
        || t.starts_with("--")
        || (matches!(lang, Lang::Python | Lang::Ruby | Lang::Shell | Lang::Other)
            && t.starts_with('#'))
}

/// The definition this line makes, if it makes one: `(name, kind)`.
///
/// Deliberately a pattern matcher and not a parser. It is allowed to miss
/// an exotic definition; it must not *invent* one, because a wrong symbol
/// sends the reader to the wrong line. When this project grows a real
/// language service (the TypeScript one first), it plugs in behind this
/// same signature.
pub fn definition(path: &str, line: &str) -> Option<(String, &'static str)> {
    let lang = lang_of(path);
    let t = line.trim();
    if t.is_empty() || is_comment(t, lang) {
        return None;
    }
    // Strip the modifiers first, so `export const handleClick = …` and
    // `pub(crate) fn handle_click` both reduce to their interesting part.
    let mut stripped = t;
    loop {
        let head = stripped.split_whitespace().next().unwrap_or("");
        // `pub(crate)`, `pub(super)` and friends.
        let bare = head.split('(').next().unwrap_or(head);
        if head.is_empty() || !MODIFIERS.contains(&bare) {
            break;
        }
        stripped = stripped[head.len()..].trim_start();
    }

    // Keyword form: <keyword> <name>
    let mut toks = stripped.split_whitespace().peekable();
    if let Some(tok) = toks.peek().copied() {
        // `fn foo(` arrives as one token when there is no space.
        let head: String = tok.chars().take_while(|c| *c != '(' && *c != '<').collect();
        if let Some((_, kind)) = KEYWORDS.iter().find(|(k, _)| *k == head || *k == tok) {
            toks.next();
            // Go methods: `func (s *Server) Handle(w, r)`.
            let mut next = toks.next().unwrap_or("");
            if next.starts_with('(') {
                for tok in toks.by_ref() {
                    if tok.ends_with(')') || tok.ends_with(')') {
                        break;
                    }
                }
                next = toks.next().unwrap_or("");
            }
            // `fn foo(` with no space: the name rode along on the keyword.
            let rest = tok.strip_prefix(&head).unwrap_or("");
            let candidate = if rest.len() > 1 { &rest[1..] } else { next };
            if let Some(name) = ident(candidate) {
                if !CONTROL.contains(&name.as_str()) {
                    return Some((name, kind));
                }
            }
            return None;
        }
    }

    // Assignment form: `const handleClick = (…) => {`, `foo = function() {`.
    if matches!(lang, Lang::Web | Lang::Other | Lang::Python | Lang::Ruby) {
        if let Some(sym) = assignment_definition(stripped) {
            return Some(sym);
        }
    }

    // Brace-language method: `void handleClick(Event e) {`.
    if matches!(lang, Lang::Brace | Lang::Web) {
        if let Some(name) = brace_method(stripped) {
            return Some((name, "fn"));
        }
    }
    None
}

/// `const foo = () => {` / `foo = function (…) {` / `foo: (…) => {`.
fn assignment_definition(t: &str) -> Option<(String, &'static str)> {
    let rest = ["const ", "let ", "var "]
        .iter()
        .find_map(|kw| t.strip_prefix(kw))
        .unwrap_or(t);
    let name = ident(rest.trim_start())?;
    let after = rest.trim_start().get(name.len()..)?.trim_start();
    let value = after
        .strip_prefix('=')
        .or_else(|| after.strip_prefix(':'))?
        .trim_start();
    // Only *functions* are worth listing; a plain constant is noise.
    let is_fn = value.contains("=>")
        || value.starts_with("function")
        || value.starts_with("async function")
        || value.starts_with("async (");
    if is_fn && !CONTROL.contains(&name.as_str()) {
        Some((name, "fn"))
    } else {
        None
    }
}

/// A C-style method or function header: the identifier immediately before
/// the argument list, on a line that opens a body rather than calling
/// something.
fn brace_method(t: &str) -> Option<String> {
    if t.ends_with(';') || t.ends_with(',') {
        return None; // declaration or argument, not a definition
    }
    if !t.ends_with('{') && !t.ends_with(')') {
        return None;
    }
    let open = t.find('(')?;
    let before = &t[..open];
    // An assignment or a comparison before the parenthesis means this line
    // is calling something, not defining it.
    if before.contains('=') || before.contains("=>") || before.contains('.') {
        return None;
    }
    let name: String = before
        .chars()
        .rev()
        .take_while(|c| is_ident_char(*c))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if name.is_empty() || CONTROL.contains(&name.as_str()) {
        return None;
    }
    // `foo()` alone is a call; a definition names a type or a modifier
    // first, or is a method inside a class body (indented, no leading dot).
    let head = before[..before.len() - name.len()].trim();
    if head.is_empty() && !t.ends_with('{') {
        return None;
    }
    if head.split_whitespace().any(|w| CONTROL.contains(&w)) {
        return None;
    }
    Some(name)
}

/// The identifier surrounding char index `i`: where it starts and what it
/// is. `None` when the cursor isn't on one.
pub fn word_at(text: &str, i: usize) -> Option<(usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    if i >= chars.len() || !is_ident_char(chars[i]) {
        return None;
    }
    let mut start = i;
    while start > 0 && is_ident_char(chars[start - 1]) {
        start -= 1;
    }
    let mut end = i;
    while end + 1 < chars.len() && is_ident_char(chars[end + 1]) {
        end += 1;
    }
    Some((start, chars[start..=end].iter().collect()))
}

/// Every identifier on a line worth asking a language server about —
/// keywords and bare numbers are dropped, since "what is `return`?" has
/// no useful answer.
pub fn identifiers(text: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !is_ident_char(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_ident_char(chars[i]) {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        let boring = word.chars().next().is_some_and(|c| c.is_ascii_digit())
            || CONTROL.contains(&word.as_str())
            || MODIFIERS.contains(&word.as_str())
            || KEYWORDS.iter().any(|(k, _)| *k == word)
            || matches!(
                word.as_str(),
                "true"
                    | "false"
                    | "null"
                    | "nil"
                    | "None"
                    | "self"
                    | "this"
                    | "let"
                    | "var"
                    | "const"
                    | "string"
                    | "number"
                    | "boolean"
                    | "void"
                    | "int"
                    | "bool"
            );
        if !boring {
            out.push((start, word));
        }
    }
    out
}

/// Every definition in a file, in line order.
pub fn symbols(path: &str, content: &str) -> Vec<Symbol> {
    content
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            definition(path, line).map(|(name, kind)| Symbol {
                line: i + 1,
                name,
                kind,
                text: line.trim().to_string(),
            })
        })
        .collect()
}

/// Does this line define `name`? Used to mark the definition among a
/// pile of grep hits — the seam a real reference provider slots into.
pub fn defines(path: &str, line: &str, name: &str) -> bool {
    definition(path, line)
        .map(|(n, _)| n.eq_ignore_ascii_case(name.trim()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_prefers_path_and_word_boundaries() {
        // Both contain the letters; the one where they start segments wins.
        let (a, _) = fuzzy("uir", "src/ui/render.rs").unwrap();
        let (b, _) = fuzzy("uir", "src/builder/quiet.rs").unwrap();
        assert!(a > b, "boundary match {a} should beat interior match {b}");
    }

    #[test]
    fn fuzzy_prefers_consecutive_runs() {
        let (run, _) = fuzzy("app", "src/app.rs").unwrap();
        let (spread, _) = fuzzy("app", "a/p/parse.rs").unwrap();
        assert!(run > spread);
    }

    #[test]
    fn fuzzy_reports_the_matched_positions() {
        let (_, idx) = fuzzy("apr", "app.rs").unwrap();
        let chars: Vec<char> = "app.rs".chars().collect();
        let got: String = idx.iter().map(|i| chars[*i]).collect();
        assert_eq!(got, "apr");
        assert_eq!(idx.len(), 3);
    }

    #[test]
    fn fuzzy_rejects_non_subsequences() {
        assert!(fuzzy("zqx", "src/app.rs").is_none());
        // Order matters: the chars are all there, in the wrong sequence.
        assert!(fuzzy("psa", "app.rs").is_none());
    }

    #[test]
    fn fuzzy_is_case_insensitive_until_you_type_a_capital() {
        assert!(fuzzy("app", "src/App.tsx").is_some());
        assert!(fuzzy("App", "src/app.tsx").is_none());
        assert!(fuzzy("App", "src/App.tsx").is_some());
    }

    #[test]
    fn ranges_find_every_occurrence() {
        assert_eq!(find_ranges("a foo b foo", "foo"), vec![(2, 5), (8, 11)]);
        // Smart case again: lowercase query, case-blind search.
        assert_eq!(find_ranges("Foo", "foo"), vec![(0, 3)]);
        assert!(find_ranges("foo", "Foo").is_empty());
        assert!(find_ranges("foo", "").is_empty());
    }

    #[test]
    fn ranges_are_char_indices_not_bytes() {
        // Two-byte chars ahead of the match must not shift the column.
        assert_eq!(find_ranges("ααα foo", "foo"), vec![(4, 7)]);
    }

    #[test]
    fn finds_definitions_across_languages() {
        let cases = [
            ("a.rs", "pub fn handle_click(&self) {", "handle_click", "fn"),
            ("a.rs", "pub(crate) struct Finder {", "Finder", "struct"),
            ("a.rs", "impl App {", "App", "impl"),
            (
                "a.ts",
                "export const handleClick = () => {",
                "handleClick",
                "fn",
            ),
            ("a.ts", "export default function main() {", "main", "fn"),
            ("a.ts", "  interface Props {", "Props", "interface"),
            (
                "a.tsx",
                "  handleClick(event: Event) {",
                "handleClick",
                "fn",
            ),
            ("a.py", "def handle_click(self):", "handle_click", "fn"),
            ("a.py", "class Finder(Base):", "Finder", "class"),
            (
                "a.go",
                "func (s *Server) Handle(w http.ResponseWriter) {",
                "Handle",
                "fn",
            ),
            ("a.go", "func New() *Server {", "New", "fn"),
            (
                "a.java",
                "  public void handleClick(Event e) {",
                "handleClick",
                "fn",
            ),
        ];
        for (path, line, name, kind) in cases {
            let got = definition(path, line);
            assert_eq!(
                got,
                Some((name.to_string(), kind)),
                "{path}: {line} — got {got:?}"
            );
        }
    }

    #[test]
    fn call_sites_and_comments_are_not_definitions() {
        let cases = [
            ("a.ts", "  handleClick(event);"),
            ("a.ts", "  if (handleClick(e)) {"),
            ("a.ts", "  // function handleClick() {"),
            ("a.ts", "  const total = 3;"),
            ("a.ts", "  return handleClick(e);"),
            ("a.rs", "    // fn handle_click()"),
            ("a.rs", "    self.handle_click();"),
            ("a.py", "    # def handle_click"),
            ("a.java", "    while (running) {"),
        ];
        for (path, line) in cases {
            assert_eq!(definition(path, line), None, "{path}: {line}");
        }
    }

    #[test]
    fn symbols_are_listed_in_line_order() {
        let src = "use std::fmt;\n\nstruct A;\n\nimpl A {\n    fn one(&self) {}\n    fn two(&self) {}\n}\n";
        let syms = symbols("x.rs", src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["A", "A", "one", "two"]);
        assert_eq!(syms[0].line, 3);
        assert_eq!(syms[3].line, 7);
    }

    #[test]
    fn defines_marks_only_the_definition() {
        assert!(defines("a.rs", "pub fn handle(&self) {", "handle"));
        assert!(!defines("a.rs", "    self.handle();", "handle"));
    }

    #[test]
    fn smart_case_follows_the_query() {
        assert!(smart_case("handle"));
        assert!(!smart_case("Handle"));
    }

    /// `.env` is ignored and a developer needs it. `node_modules` is
    /// ignored and is 17,000 rows of noise. `--directory` is what tells
    /// them apart: git stops at a directory whose whole contents are
    /// ignored and names the directory, but an ignored file it names
    /// outright.
    #[test]
    fn an_ignored_file_is_listed_and_an_ignored_directory_is_not_walked() {
        let root = std::env::temp_dir().join(format!("loupe-ign-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let r = root.to_string_lossy().into_owned();
        let git = |args: &[&str]| {
            let out = Command::new("git").args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        git(&["-C", &r, "init", "-q", "."]);
        std::fs::write(root.join(".gitignore"), "node_modules/\n.env\n").unwrap();
        std::fs::write(root.join(".env"), "SECRET=1\n").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("node_modules/pkg/a.js"), "1\n").unwrap();
        git(&["-C", &r, "add", "-A"]);
        git(&[
            "-C",
            &r,
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "x",
        ]);

        let listing = list_repo(&root).unwrap();

        assert!(
            listing.paths.contains(&".env".to_string()),
            "an ignored file a developer needs is listed: {:?}",
            listing.paths
        );
        assert!(
            listing
                .paths
                .iter()
                .all(|p| !p.starts_with("node_modules/")),
            "git never walked into the ignored directory: {:?}",
            listing.paths
        );
        assert!(
            listing
                .stubs
                .iter()
                .any(|(d, ignored)| d == "node_modules" && *ignored),
            "it came back as one ignored directory instead: {:?}",
            listing.stubs
        );

        // The split point is what draws the dim rows, so it has to land
        // between the two kinds rather than at either end.
        let (tracked, ignored) = listing.paths.split_at(listing.ignored_from);
        assert!(tracked.contains(&"src/main.rs".to_string()));
        assert!(!tracked.contains(&".env".to_string()));
        assert!(ignored.contains(&".env".to_string()));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A nested repository — the shape a linked worktree takes when
    /// someone puts it inside the clone — makes `git ls-files` report a
    /// directory instead of walking into it. That entry must never reach
    /// the file list, or the tree builder turns it into a nameless row.
    #[test]
    fn a_directory_git_would_not_walk_into_is_not_a_file() {
        let root = std::env::temp_dir().join(format!("loupe-stub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("wt/feat")).unwrap();
        let r = root.to_string_lossy().into_owned();
        let git = |args: &[&str]| {
            let out = Command::new("git").args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        git(&["-C", &r, "init", "-q", "."]);
        std::fs::write(root.join("src.rs"), "fn main() {}\n").unwrap();
        // A repository inside the repository: git stops at the boundary.
        git(&[
            "-C",
            &root.join("wt/feat").to_string_lossy(),
            "init",
            "-q",
            ".",
        ]);
        std::fs::write(root.join("wt/feat/inner.rs"), "fn inner() {}\n").unwrap();

        let found = list_files(&root, None).unwrap();

        assert!(
            found.files.iter().all(|f| !f.ends_with('/')),
            "a trailing slash means a directory, not a file: {:?}",
            found.files
        );
        assert!(
            found.files.contains(&"src.rs".to_string()),
            "ordinary files still come back: {:?}",
            found.files
        );
        assert!(
            !found.files.iter().any(|f| f.starts_with("wt/feat/")),
            "git never walked in, so nothing inside is listed: {:?}",
            found.files
        );
        assert_eq!(
            found.stubs,
            vec!["wt/feat".to_string()],
            "the boundary comes back on its own, without the slash"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The parser is written against real `git grep -z -n` output, so the
    /// test drives a real repository rather than a fixture string.
    #[test]
    fn grep_reads_a_real_repository() {
        let root = std::env::temp_dir().join(format!("loupe-grep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let r = root.to_string_lossy().into_owned();
        let git = |args: &[&str]| {
            let out = Command::new("git").args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        git(&["-C", &r, "init", "-q", "."]);
        std::fs::write(
            root.join("src/a.ts"),
            "export const handleClick = () => {\n  return 1;\n}\n",
        )
        .unwrap();
        std::fs::write(root.join("src/b.ts"), "  handleClick();\n").unwrap();
        git(&["-C", &r, "add", "-A"]);
        git(&[
            "-C",
            &r,
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "x",
        ]);

        let req = GrepRequest {
            root: root.clone(),
            query: "handleClick".into(),
            rev: None,
            paths: Vec::new(),
            regex: false,
            untracked: false,
        };
        let (hits, truncated) = grep(&req).unwrap();
        assert!(!truncated);
        assert_eq!(hits.len(), 2);
        let def = hits.iter().find(|h| h.path == "src/a.ts").unwrap();
        assert_eq!(def.line, 1);
        assert!(def.definition, "the arrow function is the definition");
        assert_eq!(&def.text[def.col..def.col + def.len], "handleClick");
        let call = hits.iter().find(|h| h.path == "src/b.ts").unwrap();
        assert!(!call.definition, "a call site is not a definition");

        // Restricted to one path.
        let scoped = GrepRequest {
            paths: vec!["src/b.ts".into()],
            ..req.clone()
        };
        assert_eq!(grep(&scoped).unwrap().0.len(), 1);

        // Searching the commit rather than the working tree: an uncommitted
        // edit is invisible, which is the point in PR review.
        std::fs::write(root.join("src/c.ts"), "handleClick;\n").unwrap();
        let head = String::from_utf8_lossy(
            &Command::new("git")
                .args(["-C", &r, "rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        let at_head = GrepRequest {
            rev: Some(head),
            ..req.clone()
        };
        let (hits, _) = grep(&at_head).unwrap();
        assert_eq!(hits.len(), 2, "the untracked file is not in the commit");
        assert!(hits.iter().all(|h| !h.path.contains(':')), "rev stripped");

        // Untracked files, on the other hand, are exactly what local
        // review needs to see.
        let untracked = GrepRequest {
            untracked: true,
            ..req.clone()
        };
        assert_eq!(grep(&untracked).unwrap().0.len(), 3);

        // No match is an answer, not an error.
        let miss = GrepRequest {
            query: "nothingHereAtAll".into(),
            ..req.clone()
        };
        assert!(grep(&miss).unwrap().0.is_empty());

        // A file list, for whole-repo file matching.
        let files = list_files(&root, None).unwrap();
        assert!(files.files.contains(&"src/a.ts".to_string()));

        let _ = std::fs::remove_dir_all(&root);
    }
}

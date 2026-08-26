//! The context provider: what the reader is looking at, for an agent.
//!
//! Loupe knows something no other process on the machine knows — the exact
//! lines a human is reading and judging right now. An agent in another pane
//! has to guess at it, and pays for the guess in grep calls.
//!
//! This module publishes that knowledge. Loupe binds a unix socket keyed to
//! the repository root; `loupe ctl context` connects from wherever the agent
//! runs, asks one question, and prints the answer. The agent's own
//! `UserPromptSubmit` hook runs that command and feeds the output to the
//! model, so the context arrives without a key press.
//!
//! The repository root is the whole addressing scheme. Neither side learns
//! about panes, multiplexers, or window ids, because neither side needs to.
//!
//! Unix only for now: the standard library has no unix sockets on Windows.

use anyhow::{bail, Context, Result};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// A hunk longer than this is cut down before it goes to the model. Codex
/// gives hook output a 2500-token budget by default and spills the rest to
/// a temp file; a diff that blows past that costs the reader nothing and
/// buys the model nothing.
const MAX_HUNK_LINES: usize = 60;
/// How much of an over-long hunk survives, at each end.
const HUNK_HEAD: usize = 34;
const HUNK_TAIL: usize = 16;

/// Everything the context block can say. The UI thread refreshes this; the
/// socket thread reads it. Owned strings throughout, so the socket thread
/// never borrows from `App`.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub repo: Option<String>,
    pub branch: Option<String>,
    /// PR number and title, when a pull request is open.
    pub pr: Option<(u64, String)>,
    pub local: bool,
    /// Path of the open file, relative to the repository root.
    pub file: Option<String>,
    /// "old" or "new", and the inclusive line range the user selected.
    pub selection: Option<(&'static str, usize, usize)>,
    /// The selected lines themselves.
    pub hunk: Option<String>,
    /// Changed files the user has not marked viewed.
    pub unviewed: Vec<String>,
    /// Held review comments, as path and line.
    pub held: Vec<(String, usize)>,
}

impl Snapshot {
    /// Render the block that goes to the model.
    ///
    /// Two rules earn their place here. The `file:line` pointer always
    /// survives truncation, because it is the sentence the whole block
    /// exists to say. And every field passes through [`safe`] first: a
    /// branch name and a PR title are attacker-reachable text about to be
    /// read by something that follows instructions.
    pub fn render(&self) -> String {
        let mut out = String::from("## Loupe — what the user is looking at\n\n");

        let repo = self.repo.as_deref().unwrap_or("this repository");
        let mut line = format!("Repo: {}", safe(repo));
        if let Some(branch) = &self.branch {
            let _ = write!(line, " (branch: {})", safe(branch));
        }
        let _ = writeln!(out, "{line}");

        match (&self.pr, self.local) {
            (Some((n, title)), _) => {
                let _ = writeln!(out, "Mode: pull request review — #{n} \"{}\"", safe(title));
            }
            (None, true) => out.push_str("Mode: local review of uncommitted changes\n"),
            (None, false) => out.push_str("Mode: review\n"),
        }

        match (&self.file, self.selection) {
            (Some(path), Some((side, a, b))) => {
                let range = if a == b {
                    format!("{a}")
                } else {
                    format!("{a}-{b}")
                };
                let _ = writeln!(out, "\n{}:{range}  (selected, {side} side)", safe(path));
            }
            (Some(path), None) => {
                let _ = writeln!(out, "\n{}  (open, nothing selected)", safe(path));
            }
            (None, _) => out.push_str("\nNo file is open.\n"),
        }

        if let Some(hunk) = &self.hunk {
            let text = elide(hunk);
            if !text.trim().is_empty() {
                let _ = write!(out, "\n```\n{}\n```\n", safe(&text));
            }
        }

        if !self.held.is_empty() {
            let list: Vec<String> = self
                .held
                .iter()
                .map(|(p, l)| format!("{}:{l}", safe(p)))
                .collect();
            let _ = writeln!(out, "\nHeld review comments: {}", list.join(", "));
        }

        if !self.unviewed.is_empty() {
            let shown: Vec<String> = self.unviewed.iter().take(8).map(|p| safe(p)).collect();
            let more = self.unviewed.len().saturating_sub(shown.len());
            let tail = if more > 0 {
                format!(" (+{more} more)")
            } else {
                String::new()
            };
            let _ = writeln!(out, "Not yet marked viewed: {}{tail}", shown.join(", "));
        }

        out
    }
}

/// Strip what must never reach a terminal or a model unrevealed.
///
/// Control characters can reshape an escape sequence; bidi and invisible
/// format characters can reorder text so that what a reviewer reads and what
/// a model reads disagree — the Trojan Source family. Newlines and tabs are
/// content here, so they stay.
fn safe(text: &str) -> String {
    text.chars()
        .filter(|c| {
            if *c == '\n' || *c == '\t' {
                return true;
            }
            !c.is_control() && !matches!(*c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
        })
        .collect()
}

/// Keep a long hunk's two ends and say how much went missing.
fn elide(hunk: &str) -> String {
    let lines: Vec<&str> = hunk.lines().collect();
    if lines.len() <= MAX_HUNK_LINES {
        return hunk.to_string();
    }
    let gone = lines.len() - HUNK_HEAD - HUNK_TAIL;
    let mut out = lines[..HUNK_HEAD].join("\n");
    let _ = write!(out, "\n… {gone} lines elided …\n");
    out.push_str(&lines[lines.len() - HUNK_TAIL..].join("\n"));
    out
}

// ------------------------------------------------------------------ socket

/// The directory that holds one socket per repository. Owner-only.
fn socket_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .context("no XDG_STATE_HOME and no HOME")?;
    Ok(base.join("loupe/sock"))
}

/// The socket path for one repository root.
///
/// The path *is* the discovery mechanism: both sides derive it from the same
/// repository root, so there is no registry to keep in step and nothing to
/// clean up when loupe is killed. Whether anyone is listening is answered by
/// trying to connect.
pub fn socket_path(repo_root: &Path) -> Result<PathBuf> {
    let key = digest(repo_root.to_string_lossy().as_bytes());
    Ok(socket_dir()?.join(format!("{key}.sock")))
}

/// A short, stable, filename-safe key for a path. FNV-1a is enough: this
/// names a file, it does not protect anything.
fn digest(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// What the UI thread publishes and the socket thread serves.
    pub type Shared = Arc<Mutex<Snapshot>>;

    /// Create the socket directory, owner-only, and repair a wrong mode.
    fn private_dir(dir: &Path) -> Result<()> {
        if !dir.exists() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
            return Ok(());
        }
        let meta = std::fs::symlink_metadata(dir)?;
        if !meta.is_dir() {
            bail!("{} exists and is not a directory", dir.display());
        }
        if meta.permissions().mode() & 0o777 != 0o700 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// Decide what to do about a socket path that already exists.
    ///
    /// Codex settles this by trying to connect rather than by reading a pid
    /// file, and it is right: a pid file lies once the number is reused,
    /// while a refused connection is proof that nobody is home.
    fn clear_stale(path: &Path) -> Result<()> {
        match UnixStream::connect(path) {
            // Someone answered. A second loupe on the same repository would
            // fight this one for the socket, so leave it alone.
            Ok(_) => bail!("another loupe already serves {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            // Nobody listens: the file outlived the process that made it.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                let _ = std::fs::remove_file(path);
                Ok(())
            }
            Err(_) if !path.exists() => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Start serving the context for `repo_root`.
    ///
    /// Returns the handle the UI thread writes snapshots into. A failure
    /// here is never fatal: loupe is a review tool first, and it must open
    /// even when the socket cannot.
    pub fn serve(repo_root: &Path) -> Result<Shared> {
        let path = socket_path(repo_root)?;
        private_dir(path.parent().context("socket path has no parent")?)?;
        clear_stale(&path)?;

        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;

        let shared: Shared = Arc::new(Mutex::new(Snapshot::default()));
        let served = Arc::clone(&shared);
        let guard = path.clone();

        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut verb = String::new();
                if BufReader::new(
                    stream.try_clone().expect("unix stream should clone"),
                )
                .read_line(&mut verb)
                .is_err()
                {
                    continue;
                }
                let body = match verb.trim() {
                    "" | "context" => served
                        .lock()
                        .map(|s| s.render())
                        .unwrap_or_else(|_| String::new()),
                    other => format!("loupe: unknown request {:?}\n", safe(other)),
                };
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
            // The listener only ends when loupe does; take the file with it.
            let _ = std::fs::remove_file(&guard);
        });

        Ok(shared)
    }

    /// Ask the loupe that serves `repo_root` what is on screen.
    ///
    /// No session means no output and a clean exit. A hook that prints
    /// nothing does nothing, which is exactly right when loupe is closed.
    pub fn ask(repo_root: &Path) -> Result<Option<String>> {
        let path = socket_path(repo_root)?;
        let mut stream = match UnixStream::connect(&path) {
            Ok(s) => s,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None)
            }
            Err(e) => return Err(e.into()),
        };
        stream.write_all(b"context\n")?;
        stream.flush()?;
        let mut body = String::new();
        stream.read_to_string(&mut body)?;
        Ok(Some(body))
    }
}

#[cfg(unix)]
pub use imp::{ask, serve, Shared};

#[cfg(not(unix))]
mod imp {
    use super::*;
    use std::sync::{Arc, Mutex};

    pub type Shared = Arc<Mutex<Snapshot>>;

    /// Windows has no unix sockets in the standard library. Loupe still
    /// runs; only the context provider is absent.
    pub fn serve(_repo_root: &Path) -> Result<Shared> {
        bail!("the context socket needs a unix platform")
    }

    pub fn ask(_repo_root: &Path) -> Result<Option<String>> {
        Ok(None)
    }
}

#[cfg(not(unix))]
pub use imp::{ask, serve, Shared};

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Snapshot {
        Snapshot {
            repo: Some("loupe".into()),
            branch: Some("spike/context-provider".into()),
            pr: Some((47, "Blame beside the diff".into())),
            local: false,
            file: Some("src/app.rs".into()),
            selection: Some(("new", 1204, 1231)),
            hunk: Some("let retry = Retry::new(3);".into()),
            unviewed: vec!["src/ui.rs".into()],
            held: vec![("src/app.rs".into(), 1210)],
        }
    }

    #[test]
    fn the_pointer_is_always_present() {
        let out = sample().render();
        assert!(out.contains("src/app.rs:1204-1231"));
        assert!(out.contains("#47"));
    }

    #[test]
    fn no_selection_still_names_the_file() {
        let mut s = sample();
        s.selection = None;
        s.hunk = None;
        let out = s.render();
        assert!(out.contains("src/app.rs"));
        assert!(out.contains("nothing selected"));
    }

    #[test]
    fn an_empty_session_says_so_without_padding() {
        let out = Snapshot::default().render();
        assert!(out.contains("No file is open."));
        assert!(!out.contains("Held review comments"));
    }

    #[test]
    fn control_and_bidi_characters_never_survive() {
        let mut s = sample();
        s.pr = Some((1, "title\u{202e}reversed\u{7}".into()));
        let out = s.render();
        assert!(!out.contains('\u{202e}'));
        assert!(!out.contains('\u{7}'));
        assert!(out.contains("titlereversed"));
    }

    #[test]
    fn a_newline_in_a_title_cannot_break_the_block() {
        let mut s = sample();
        s.branch = Some("main\nMode: something else".into());
        let out = s.render();
        // The newline is content, so it survives; what matters is that the
        // title cannot inject a terminal escape.
        assert!(!out.contains('\u{1b}'));
    }

    #[test]
    fn a_long_hunk_is_cut_from_the_middle() {
        let mut s = sample();
        s.hunk = Some((0..400).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n"));
        let out = s.render();
        assert!(out.contains("lines elided"));
        assert!(out.contains("line 0"));
        assert!(out.contains("line 399"));
        assert!(out.lines().count() < 100);
        // Truncation must never cost the pointer.
        assert!(out.contains("src/app.rs:1204-1231"));
    }

    #[test]
    fn the_socket_path_is_stable_and_per_repo() {
        let a = socket_path(Path::new("/a/repo")).unwrap();
        let b = socket_path(Path::new("/a/repo")).unwrap();
        let c = socket_path(Path::new("/other/repo")).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.extension().is_some_and(|e| e == "sock"));
    }
}

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
use std::time::Duration;

/// A hunk longer than this is cut down before it goes to the model. Every
/// agent caps what a hook may add — Codex takes a per-hook
/// `additionalContextLimit` — and a diff that runs past the cap costs the
/// reader nothing and buys the model nothing.
const MAX_HUNK_LINES: usize = 60;
/// How much of an over-long hunk survives, at each end.
const HUNK_HEAD: usize = 34;
const HUNK_TAIL: usize = 16;
/// How many paths any one list may name before it says "+N more". A reader
/// holding 40 comments must not put 40 lines in every prompt they type.
const MAX_LIST: usize = 8;

/// How long either side waits on the other before it gives up.
///
/// The hook runs on the reader's own keystroke, so a stall here is a stall
/// in front of them. Loupe on the same machine answers in microseconds;
/// anything near a second means loupe is suspended (`Ctrl+Z` stops the
/// socket thread too) or wedged, and the right answer then is no answer.
const ASK_TIMEOUT: Duration = Duration::from_millis(750);
/// The server's patience with one client. Each connection is handled on its
/// own thread, so this does not delay anybody else — it only stops a client
/// that opens a connection and says nothing from leaving a thread behind.
const SERVE_TIMEOUT: Duration = Duration::from_secs(2);

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
            let shown: Vec<String> = self
                .held
                .iter()
                .take(MAX_LIST)
                .map(|(p, l)| format!("{}:{l}", safe(p)))
                .collect();
            let _ = writeln!(
                out,
                "\nHeld review comments: {}{}",
                shown.join(", "),
                more_than(self.held.len(), shown.len())
            );
        }

        if !self.unviewed.is_empty() {
            let shown: Vec<String> = self
                .unviewed
                .iter()
                .take(MAX_LIST)
                .map(|p| safe(p))
                .collect();
            let _ = writeln!(
                out,
                "Not yet marked viewed: {}{}",
                shown.join(", "),
                more_than(self.unviewed.len(), shown.len())
            );
        }

        out
    }
}

/// Say how many entries a list left out, or nothing when it left out none.
fn more_than(total: usize, shown: usize) -> String {
    match total.saturating_sub(shown) {
        0 => String::new(),
        more => format!(" (+{more} more)"),
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
            // Nobody answers. Whatever is at that path outlived the process
            // that made it, or was never a socket — a stray regular file
            // gets the same treatment, because the alternative is a loupe
            // that can never serve this repository again. The directory is
            // loupe's own and owner-only, so nothing here is the reader's.
            Err(_) => {
                let _ = std::fs::remove_file(path);
                Ok(())
            }
        }
    }

    /// Start serving the context for `repo_root`.
    ///
    /// Returns the handle the UI thread writes snapshots into. A failure
    /// here is never fatal: loupe is a review tool first, and it must open
    /// even when the socket cannot.
    pub fn serve(repo_root: &Path) -> Result<Shared> {
        let path = socket_path(repo_root)?;
        let dir = path.parent().context("socket path has no parent")?;
        private_dir(dir)?;
        // A loupe that is killed, or that crashes, leaves its socket file
        // behind: the accept loop below only ends when the process does, so
        // nothing on the way out can remove it. Clearing the whole
        // directory here covers those, and covers a repository the reader
        // never opens again.
        sweep(dir);
        serve_at(&path)
    }

    /// Serve one socket path. Split out from [`serve`] so a test can name
    /// its own path and never touch the reader's environment.
    pub(super) fn serve_at(path: &Path) -> Result<Shared> {
        clear_stale(path)?;

        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;

        let shared: Shared = Arc::new(Mutex::new(Snapshot::default()));
        let served = Arc::clone(&shared);

        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                // One thread for each connection. Answering them in turn
                // instead would let a client that connects and then says
                // nothing hold up every client behind it — and the client
                // behind it is a reader waiting on their own prompt.
                // Connections arrive one per prompt, so the cost is
                // nothing next to what it prevents.
                let served = Arc::clone(&served);
                thread::spawn(move || answer(stream, &served));
            }
        });

        Ok(shared)
    }

    /// Answer one question, then hang up.
    fn answer(mut stream: UnixStream, served: &Shared) {
        // Bound the wait so a client that says nothing does not leave this
        // thread alive for as long as loupe runs.
        let _ = stream.set_read_timeout(Some(SERVE_TIMEOUT));
        let _ = stream.set_write_timeout(Some(SERVE_TIMEOUT));
        let Ok(clone) = stream.try_clone() else {
            return;
        };
        let mut verb = String::new();
        if BufReader::new(clone).read_line(&mut verb).is_err() {
            return;
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

    /// Remove every socket in `dir` that nobody answers.
    ///
    /// A socket another loupe is serving answers, and is left alone. Errors
    /// are ignored throughout: this is housekeeping, and failing to tidy up
    /// must never stop a review from opening.
    pub(super) fn sweep(dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "sock")
                && matches!(
                    UnixStream::connect(&path).map_err(|e| e.kind()),
                    Err(std::io::ErrorKind::ConnectionRefused)
                )
            {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    /// Ask the loupe that serves `repo_root` what is on screen.
    ///
    /// No session means no output and a clean exit. A hook that prints
    /// nothing does nothing, which is exactly right when loupe is closed.
    pub fn ask(repo_root: &Path) -> Result<Option<String>> {
        ask_at(&socket_path(repo_root)?)
    }

    /// Ask one socket path. Split out from [`ask`] for the same reason
    /// [`serve_at`] is.
    pub(super) fn ask_at(path: &Path) -> Result<Option<String>> {
        let mut stream = match UnixStream::connect(path) {
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
        // Silence beats delay. A loupe that does not answer promptly is a
        // loupe that is suspended or wedged, and the reader is waiting on
        // their own prompt while this blocks.
        stream.set_read_timeout(Some(ASK_TIMEOUT))?;
        stream.set_write_timeout(Some(ASK_TIMEOUT))?;
        if stream.write_all(b"context\n").is_err() || stream.flush().is_err() {
            return Ok(None);
        }
        let mut body = String::new();
        match stream.read_to_string(&mut body) {
            Ok(_) => Ok(Some(body)),
            // A timeout arrives as `WouldBlock` on some platforms and
            // `TimedOut` on others; both mean the same thing here, and so
            // does a half-read answer — an incomplete block is worse than
            // none, because the reader cannot tell it was cut.
            Err(_) => Ok(None),
        }
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

    /// A directory of our own under the system temp directory, named so two
    /// tests — or two test runs — never collide. Unix socket paths are
    /// short by nature, so the name is kept short too.
    /// Wait until nothing answers at `path`.
    ///
    /// Closing a listener is not instant under load: a connection still
    /// lands in the dying backlog for a moment afterwards. A test that
    /// races the kernel measures the clock instead of loupe.
    #[cfg(unix)]
    fn wait_until_dead(path: &Path) {
        for _ in 0..200 {
            if std::os::unix::net::UnixStream::connect(path).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{} still answers after two seconds", path.display());
    }

    #[cfg(unix)]
    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("lp{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

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
    fn a_long_list_says_how_much_it_left_out() {
        let mut s = sample();
        s.unviewed = (0..20).map(|i| format!("src/f{i}.rs")).collect();
        s.held = (0..20).map(|i| (format!("src/f{i}.rs"), i)).collect();
        let out = s.render();
        // Eight of each, and the count of what did not fit. A reader
        // holding twenty comments must not put twenty lines in every
        // prompt they type.
        assert!(out.contains("(+12 more)"), "{out}");
        assert_eq!(out.matches("(+12 more)").count(), 2, "{out}");
        assert!(!out.contains("src/f8.rs"), "{out}");
        assert!(out.contains("src/f7.rs"), "{out}");
    }

    #[test]
    fn a_short_list_says_nothing_about_leftovers() {
        let out = sample().render();
        assert!(!out.contains("more)"), "{out}");
    }

    #[test]
    fn a_long_hunk_is_cut_from_the_middle() {
        let mut s = sample();
        s.hunk = Some(
            (0..400)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let out = s.render();
        assert!(out.contains("lines elided"));
        assert!(out.contains("line 0"));
        assert!(out.contains("line 399"));
        assert!(out.lines().count() < 100);
        // Truncation must never cost the pointer.
        assert!(out.contains("src/app.rs:1204-1231"));
    }

    #[cfg(unix)]
    #[test]
    fn the_socket_path_is_stable_and_per_repo() {
        let a = socket_path(Path::new("/a/repo")).unwrap();
        let b = socket_path(Path::new("/a/repo")).unwrap();
        let c = socket_path(Path::new("/other/repo")).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.extension().is_some_and(|e| e == "sock"));
    }
    // ------------------------------------------------------- the socket
    //
    // `serve` and `ask` are the two functions that fail in the reader's
    // hands, so they are exercised over a real socket rather than mocked.

    #[cfg(unix)]
    #[test]
    fn a_question_over_the_socket_gets_what_is_on_screen() {
        let dir = scratch("rt");
        let path = dir.join("s.sock");
        let shared = imp::serve_at(&path).expect("serve");
        *shared.lock().unwrap() = sample();

        let body = imp::ask_at(&path)
            .expect("ask")
            .expect("a live socket answers");
        assert_eq!(body, sample().render());
        assert!(body.contains("src/app.rs:1204-1231"));

        // The snapshot the UI thread publishes is what the next question
        // gets — the socket never serves a stale copy.
        shared.lock().unwrap().file = Some("src/ui.rs".into());
        let again = imp::ask_at(&path).expect("ask").expect("still live");
        assert!(again.contains("src/ui.rs"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn no_loupe_means_no_output_and_no_error() {
        let dir = scratch("none");
        // Nothing has ever bound this path.
        assert!(imp::ask_at(&dir.join("s.sock")).expect("ask").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_socket_left_by_a_dead_loupe_does_not_block_the_next_one() {
        use std::os::unix::net::UnixListener;
        let dir = scratch("stale");
        let path = dir.join("s.sock");
        // Dropping a listener leaves its file behind, which is exactly what
        // a killed loupe leaves behind.
        drop(UnixListener::bind(&path).expect("bind"));
        assert!(path.exists(), "the file outlives the listener");
        wait_until_dead(&path);

        let shared = imp::serve_at(&path).expect("serve over a stale socket");
        *shared.lock().unwrap() = sample();
        assert!(imp::ask_at(&path).expect("ask").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn junk_at_the_socket_path_is_cleared_rather_than_fatal() {
        let dir = scratch("junk");
        let path = dir.join("s.sock");
        // Not a socket at all. Refusing to start here would leave the
        // reader with a repository that never gets a context provider
        // again, and no way to find out why.
        std::fs::write(&path, "not a socket").expect("write");

        let shared = imp::serve_at(&path).expect("serve over junk");
        *shared.lock().unwrap() = sample();
        assert!(imp::ask_at(&path).expect("ask").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_second_loupe_leaves_the_first_ones_socket_alone() {
        let dir = scratch("second");
        let path = dir.join("s.sock");
        let first = imp::serve_at(&path).expect("first serve");
        *first.lock().unwrap() = sample();

        assert!(imp::serve_at(&path).is_err(), "the second must refuse");
        // And the first is still answering.
        assert!(imp::ask_at(&path).expect("ask").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_client_that_says_nothing_does_not_wedge_the_server() {
        use std::os::unix::net::UnixStream;
        let dir = scratch("mute");
        let path = dir.join("s.sock");
        let shared = imp::serve_at(&path).expect("serve");
        *shared.lock().unwrap() = sample();

        // Connect and never send a verb. Without a read timeout on the
        // server this holds the accept loop forever and every later
        // question hangs with it.
        let mute = UnixStream::connect(&path).expect("connect");
        let answer = imp::ask_at(&path).expect("ask");
        assert!(
            answer.is_some(),
            "a mute client must not block the next one"
        );
        drop(mute);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_dead_socket_is_swept_and_a_live_one_is_kept() {
        use std::os::unix::net::UnixListener;
        let dir = scratch("sweep");
        let dead = dir.join("dead.sock");
        let live = dir.join("live.sock");
        let other = dir.join("keep.txt");
        drop(UnixListener::bind(&dead).expect("bind"));
        let _listener = UnixListener::bind(&live).expect("bind");
        std::fs::write(&other, "not a socket").expect("write");
        wait_until_dead(&dead);

        imp::sweep(&dir);

        assert!(!dead.exists(), "a socket nobody answers must go");
        assert!(live.exists(), "another loupe's socket must stay");
        assert!(other.exists(), "the sweep touches only .sock files");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

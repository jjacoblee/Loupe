//! Language servers — symbols, definitions, references and hover, from
//! whatever the developer already has installed.
//!
//! Loupe ships no language intelligence of its own and bundles no servers.
//! It looks on `PATH` for the ones a person working in that language
//! already has (`typescript-language-server`, `gopls`, `rust-analyzer`),
//! starts one lazily the first time a question is asked about a file it
//! handles, and keeps it for the session. Nothing is installed, nothing is
//! downloaded, and a language with no server simply falls back to the
//! pattern matcher in [`crate::search`].
//!
//! ## Why the buffer is sent, not the path
//!
//! Every request opens the document with the *text loupe is showing*
//! (`textDocument/didOpen`), not by pointing the server at the file. In PR
//! review the working tree can be a different branch entirely — reading
//! the file from disk would answer a question about code that isn't on
//! screen. Cross-file answers (a definition in an untouched file) still
//! come from the server's own view of the project, which is the best any
//! tool can do without checking the PR out.
//!
//! ## Threading
//!
//! Every call here **blocks** waiting on a subprocess, so every call must
//! happen on a worker thread — see the job engine in [`crate::app`]. The
//! registry is cloneable and internally locked so a job can carry it
//! across a thread boundary; per-server locking means two questions about
//! the same language queue up, which is what the protocol wants anyway.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Starting a server can mean a project-wide index; the first request
/// after that pays for it.
const INIT_TIMEOUT: Duration = Duration::from_secs(20);
/// A warm server answers in milliseconds. This is the "something is
/// wrong" bound, not an expected wait.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Lines of a server's stderr kept for the "it died and here's why"
/// message.
const STDERR_TAIL: usize = 10;
/// How long to keep re-asking while a server says it is still indexing.
/// rust-analyzer answers `documentSymbol` immediately but returns
/// *nothing* for references until its index is built — without this, the
/// first `gr` after launch would report "no references" and be wrong.
const INDEXING_BUDGET: Duration = Duration::from_secs(12);

/// A language server loupe knows how to talk to.
pub struct ServerSpec {
    /// What to call it in messages to the user.
    pub lang: &'static str,
    pub exts: &'static [&'static str],
    pub cmd: &'static str,
    pub args: &'static [&'static str],
    /// What to run if it isn't installed.
    pub install: &'static str,
}

/// The three languages this supports today. Adding a fourth is a row
/// here plus a `language_id` arm — the protocol is the same for all of
/// them.
pub const SERVERS: &[ServerSpec] = &[
    ServerSpec {
        lang: "TypeScript",
        exts: &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"],
        cmd: "typescript-language-server",
        args: &["--stdio"],
        // `tsserver` itself does NOT speak LSP — this wrapper is what does.
        install: "npm install -g typescript-language-server typescript",
    },
    ServerSpec {
        lang: "Go",
        exts: &["go"],
        cmd: "gopls",
        args: &[],
        install: "go install golang.org/x/tools/gopls@latest",
    },
    ServerSpec {
        lang: "Rust",
        exts: &["rs"],
        cmd: "rust-analyzer",
        args: &[],
        install: "rustup component add rust-analyzer",
    },
];

pub fn spec_for(path: &str) -> Option<&'static ServerSpec> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    SERVERS.iter().find(|s| s.exts.contains(&ext.as_str()))
}

/// The `languageId` a server expects in `didOpen`.
fn language_id(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "go" => "go",
        "rs" => "rust",
        other => Box::leak(other.to_string().into_boxed_str()),
    }
}

/// Server-specific `initializationOptions`.
///
/// Only TypeScript needs any: `typescript-language-server` is a wrapper
/// that has to be pointed at an actual `tsserver.js`, and it refuses to
/// start without one. It looks in the workspace, which covers a project
/// with TypeScript as a dependency and misses everything else — including
/// a perfectly good global install. So loupe looks too.
fn init_options(spec: &ServerSpec, root: &Path) -> Value {
    if spec.lang != "TypeScript" {
        return Value::Null;
    }
    match tsserver_path(root) {
        Some(path) => json!({"tsserver": {"path": path.to_string_lossy()}}),
        None => Value::Null,
    }
}

/// Where this project's `tsserver.js` lives, preferring the workspace's
/// own copy — a project pinned to an older TypeScript should be analyzed
/// by *that* TypeScript, not by whatever is installed globally.
fn tsserver_path(root: &Path) -> Option<PathBuf> {
    let local = root.join("node_modules/typescript/lib/tsserver.js");
    if local.is_file() {
        return Some(local);
    }
    // A global install, found through the `tsserver` shim on PATH: resolve
    // the symlink, then walk up to the package root.
    if let Some(bin) = which("tsserver").and_then(|p| std::fs::canonicalize(p).ok()) {
        if let Some(pkg) = bin
            .ancestors()
            .find(|a| a.file_name().is_some_and(|n| n == "typescript"))
        {
            let candidate = pkg.join("lib/tsserver.js");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    // Last resort: ask npm where it puts global packages.
    let out = Command::new("npm").args(["root", "-g"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    let candidate = dir.join("typescript/lib/tsserver.js");
    candidate.is_file().then_some(candidate)
}

/// Find an executable on `PATH`, the way a shell would — and, for the one
/// case where a shell would be wrong, check that it is really there.
///
/// rustup keeps a proxy in `~/.cargo/bin` for every tool it *could*
/// provide, installed or not. On a machine that has never added the
/// `rust-analyzer` component, `~/.cargo/bin/rust-analyzer` is still
/// sitting there as a link to `rustup` itself, and running it prints
/// "Unknown binary" and exits 1. A file at that path is therefore not the
/// same thing as the tool being installed. Loupe used to believe it:
/// `loupe --lsp` printed a ✓ beside Rust, the help overlay called it
/// installed, and `gd` / `gr` / `K` on a `.rs` file failed with rustup's
/// error instead of quietly falling back to pattern matching.
pub fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let found = std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(cmd);
        candidate.is_file().then_some(candidate)
    })?;
    if is_rustup_proxy(&found) {
        // Ask rustup which one this is. It answers with the real path
        // when the component is installed, and fails when it is not.
        return rustup_which(cmd);
    }
    Some(found)
}

/// True when `path` is one of rustup's stand-ins rather than a real tool.
///
/// rustup installs a stand-in for every tool it *could* provide, whether
/// the component is there or not, so the file existing proves nothing. It
/// makes them three ways, and only the first is a symlink:
///
/// 1. A **symlink** to `rustup` — `canonicalize` lands on the name.
/// 2. A **hard link** to it, which is what a normal install makes. The
///    link *is* the same file, so `canonicalize` gives back the
///    stand-in's own name and tells us nothing; the inode is the thing
///    that matches.
///
/// Case 2 is why this cannot just read the resolved name: on a
/// hard-linked install loupe called the stand-in "installed" and then
/// failed at the first request with rustup's "Unknown binary" error.
///
/// The inode test is anchored to the `rustup` binary in the same
/// directory, and is exact — a real tool that happens to live in
/// `~/.cargo/bin`, one `cargo install` put there, is a different file
/// and is taken at face value.
fn is_rustup_proxy(path: &Path) -> bool {
    if let Ok(real) = std::fs::canonicalize(path) {
        if real
            .file_stem()
            .is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case("rustup"))
        {
            return true;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Some(dir) = path.parent() else {
            return false;
        };
        let Some(rustup) = ["rustup", "rustup.exe"]
            .iter()
            .map(|n| dir.join(n))
            .find(|p| p.is_file())
        else {
            return false;
        };
        if let (Ok(a), Ok(b)) = (std::fs::metadata(path), std::fs::metadata(&rustup)) {
            return a.dev() == b.dev() && a.ino() == b.ino();
        }
    }
    false
}

/// What `rustup which <cmd>` says, or `None` when rustup does not have it.
///
/// The answer is kept for the session. [`which`] is asked on every frame
/// the help overlay is open, and a subprocess per frame is not something
/// to pay for a question whose answer almost never changes. The price is
/// that a component installed while loupe is running is not noticed until
/// it restarts.
fn rustup_which(cmd: &str) -> Option<PathBuf> {
    static ANSWERS: Mutex<Option<HashMap<String, Option<PathBuf>>>> = Mutex::new(None);
    let mut guard = lock(&ANSWERS);
    let cache = guard.get_or_insert_with(HashMap::new);
    if let Some(hit) = cache.get(cmd) {
        return hit.clone();
    }
    let answer = Command::new("rustup")
        .args(["which", cmd])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string()))
        .filter(|p| p.is_file());
    cache.insert(cmd.to_string(), answer.clone());
    answer
}

/// What `loupe --lsp` reports: every supported server and where (or
/// whether) it was found.
pub fn doctor() -> Vec<(&'static ServerSpec, Option<PathBuf>)> {
    SERVERS.iter().map(|s| (s, which(s.cmd))).collect()
}

// ------------------------------------------------------------------ results

/// A place in the repository a request pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loc {
    /// Repo-relative when it is inside the repository, absolute if not.
    pub path: String,
    /// 1-based.
    pub line: usize,
    /// 1-based character column.
    pub col: usize,
}

/// A problem the server found in a file it has open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// 1-based line the marker sits on.
    pub line: usize,
    /// 1-based char columns of the span, on `line`.
    pub col: usize,
    pub end_col: usize,
    /// 1 error, 2 warning, 3 information, 4 hint.
    pub severity: u8,
    pub message: String,
    /// "ts(2552)", "unused_variables" — what to search for.
    pub code: Option<String>,
}

impl Diagnostic {
    pub fn is_error(&self) -> bool {
        self.severity <= 1
    }

    pub fn label(&self) -> &'static str {
        match self.severity {
            1 => "error",
            2 => "warning",
            3 => "info",
            _ => "hint",
        }
    }
}

/// One suggestion from `textDocument/completion`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// What the list shows.
    pub label: String,
    /// What gets typed when it is accepted.
    pub insert: String,
    /// Signature or type, shown next to the label.
    pub detail: Option<String>,
    pub kind: &'static str,
    /// The server's own ordering key; it knows better than we do.
    pub sort: String,
    /// An explicit range to replace, when the server gave one — it knows
    /// where the word it is completing actually starts.
    pub replace: Option<(usize, usize, usize, usize)>,
}

/// A range of text to replace, from formatting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    /// 0-based line/char, as LSP gives them, converted to chars.
    pub start: (usize, usize),
    pub end: (usize, usize),
    pub text: String,
}

/// <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#completionItemKind>
fn completion_kind(n: u64) -> &'static str {
    match n {
        2 => "method",
        3 => "fn",
        4 => "ctor",
        5 => "field",
        6 => "var",
        7 => "class",
        8 => "interface",
        9 => "module",
        10 => "property",
        13 => "enum",
        14 => "keyword",
        15 => "snippet",
        21 => "const",
        22 => "struct",
        25 => "type",
        _ => "",
    }
}

/// One entry from `textDocument/documentSymbol`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sym {
    pub name: String,
    pub kind: &'static str,
    /// 1-based.
    pub line: usize,
    /// The enclosing symbol, when the server reports a tree.
    pub container: Option<String>,
}

/// <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#symbolKind>
fn symbol_kind(n: u64) -> &'static str {
    match n {
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "ctor",
        10 => "enum",
        11 => "interface",
        12 => "fn",
        13 => "var",
        14 => "const",
        18 => "array",
        22 => "enum",
        23 => "struct",
        26 => "type",
        _ => "sym",
    }
}

// ------------------------------------------------------------------ uris

/// `file://` URI for a path, percent-encoding what has to be encoded.
/// Hand-rolled rather than pulling in a URL crate for one function.
fn uri_of(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::from("file://");
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn path_of_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///path` — anything between the // and the next / is a host,
    // which for our purposes is always empty.
    let rest = match rest.find('/') {
        Some(i) => &rest[i..],
        None => rest,
    };
    let bytes = rest.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

/// LSP counts characters in UTF-16 code units; loupe counts them in
/// chars. They differ the moment an emoji or a CJK character appears
/// before the cursor on a line.
fn utf16_col(line: &str, char_col: usize) -> usize {
    line.chars().take(char_col).map(char::len_utf16).sum()
}

/// The inverse: a server's UTF-16 column back into a char index, so a
/// location can be pointed at in text loupe measures its own way.
pub fn char_column(line: &str, utf16: usize) -> usize {
    char_col(line, utf16)
}

fn char_col(line: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (i, ch) in line.chars().enumerate() {
        if units >= utf16 {
            return i;
        }
        units += ch.len_utf16();
    }
    line.chars().count()
}

fn nth_line(text: &str, line0: usize) -> &str {
    text.lines().nth(line0).unwrap_or("")
}

// ------------------------------------------------------------------ client

/// One running server process.
struct Client {
    lang: &'static str,
    cmd: &'static str,
    child: Child,
    /// Frames waiting to go to the server, and the thread that writes
    /// them. See [`Client::send`].
    out: mpsc::Sender<Vec<u8>>,
    rx: Receiver<Value>,
    next_id: i64,
    /// uri → (version, hash of the text we last sent).
    opened: HashMap<String, (i64, u64)>,
    /// Work-done progress tokens the server has begun and not ended —
    /// i.e. whether it is still chewing on the project.
    progress: std::collections::HashSet<String>,
    /// What the server said it can do, from the initialize result. Asking
    /// for completion from a server that has none just wastes a round
    /// trip and an error message.
    caps: Value,
    /// Latest diagnostics per document URI. These arrive as *pushed*
    /// notifications rather than answers, so they are collected whenever
    /// the stream is read and kept until replaced.
    diagnostics: HashMap<String, Vec<Diagnostic>>,
    /// Bumped whenever any diagnostics change, so the UI knows to redraw
    /// without diffing the lists itself.
    diag_version: u64,
    /// The tail of the server's stderr. Kept because when a server dies
    /// on startup this is the only place the reason exists — a rustup
    /// shim for an uninstalled component, a bad config, a missing
    /// runtime. Without it the user gets "no answer" and no idea why.
    stderr: Arc<Mutex<Vec<String>>>,
}

impl Drop for Client {
    fn drop(&mut self) {
        // Politeness is not worth a hang on quit: ask, don't wait.
        let _ = self.notify("exit", json!({}));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Stack-frame lines from a panicking or erroring server: `   3: foo::bar`,
/// `   at ./src/main.rs:1`, and the header above them.
fn is_backtrace_noise(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("at ") || t.contains("backtrace") || t.contains("Backtrace") {
        return true;
    }
    // `<digits>: <symbol>`
    let mut chars = t.chars();
    let digits: String = chars.by_ref().take_while(|c| c.is_ascii_digit()).collect();
    !digits.is_empty() && t[digits.len()..].starts_with(':')
}

fn read_message(r: &mut impl BufRead) -> Result<Option<Value>> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(None); // server closed its stdout
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            len = v.trim().parse().ok();
        }
    }
    let Some(len) = len else { return Ok(None) };
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(Some(serde_json::from_slice(&buf)?))
}

impl Client {
    fn start(spec: &'static ServerSpec, root: &Path) -> Result<Client> {
        // Spawn what `which` resolved, not the bare name. They differ for
        // a rustup stand-in: `which` asks rustup for the real binary,
        // while the bare name goes back through the stand-in and fails
        // with "Unknown binary" when the component is missing from the
        // active toolchain. Starting the resolved path is what makes the
        // "is it installed?" answer and the spawn agree.
        let Some(bin) = which(spec.cmd) else {
            bail!(
                "{} is not installed (no `{}` on PATH). Install it with:  {}",
                spec.lang,
                spec.cmd,
                spec.install
            );
        };
        let mut child = Command::new(&bin)
            .args(spec.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Read (and mostly discard) stderr: an unread pipe would
            // eventually block a chatty server mid-answer.
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to start {}", spec.cmd))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("{} produced no stdout", spec.cmd))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("{} accepts no stdin", spec.cmd))?;
        // One thread owns the pipe, and everybody else reaches it through
        // this channel. See `Client::send` for why.
        let (out, outbox) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            for frame in outbox {
                if stdin.write_all(&frame).is_err() || stdin.flush().is_err() {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(Vec::new()));
        if let Some(pipe) = child.stderr.take() {
            let sink = stderr.clone();
            thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    // Backtrace frames crowd out the one line that says
                    // what actually went wrong.
                    if is_backtrace_noise(&line) {
                        continue;
                    }
                    let mut tail = lock(&sink);
                    if tail.len() == STDERR_TAIL {
                        tail.remove(0);
                    }
                    tail.push(line);
                }
            });
        }
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(msg)) = read_message(&mut reader) {
                if tx.send(msg).is_err() {
                    break; // the client went away
                }
            }
        });
        let mut client = Client {
            lang: spec.lang,
            cmd: spec.cmd,
            child,
            out,
            rx,
            next_id: 0,
            opened: HashMap::new(),
            progress: std::collections::HashSet::new(),
            caps: Value::Null,
            diagnostics: HashMap::new(),
            diag_version: 0,
            stderr,
        };
        let uri = uri_of(root);
        let init = client.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "clientInfo": {"name": "loupe", "version": env!("CARGO_PKG_VERSION")},
                "rootUri": uri,
                "workspaceFolders": [{"uri": uri, "name": "workspace"}],
                "initializationOptions": init_options(spec, root),
                "capabilities": {
                    "textDocument": {
                        "synchronization": {"dynamicRegistration": false, "didSave": false},
                        "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                        "definition": {"linkSupport": true},
                        "references": {},
                        "hover": {"contentFormat": ["plaintext", "markdown"]},
                        "formatting": {},
                        "completion": {
                            "completionItem": {
                                // Loupe inserts text literally, so a
                                // server must not send `${1:placeholder}`
                                // snippets it expects us to expand.
                                "snippetSupport": false,
                                "insertReplaceSupport": false,
                                "documentationFormat": ["plaintext"],
                            },
                            "contextSupport": true,
                        },
                        "publishDiagnostics": {"relatedInformation": false},
                    },
                    "workspace": {"workspaceFolders": true, "configuration": true},
                    // Without this a server never sends `$/progress`, and
                    // loupe can't tell "no references" from "not indexed
                    // yet" — see `request_when_ready`.
                    "window": {"workDoneProgress": true},
                },
            }),
            INIT_TIMEOUT,
        )?;
        client.caps = init.get("capabilities").cloned().unwrap_or(Value::Null);
        client.notify("initialized", json!({}))?;
        Ok(client)
    }

    /// Hand one JSON-RPC frame to the writer thread.
    ///
    /// This must not block, and writing to the pipe here would. A pipe
    /// holds 64 KB on macOS; `didChange` on a 536 KB buffer is 553 KB of
    /// escaped JSON, so nine of those writes wait on the server draining
    /// the other end — and a server busy indexing a project does not.
    /// `sync_open` is called from the idle tick on the drawing thread, so
    /// that wait was a frozen window.
    ///
    /// One channel per server rather than a thread per message, because
    /// the protocol is ordered: `didOpen` must reach the server before the
    /// `didChange` that builds on it, and version 2 before version 3. Two
    /// threads racing on the same pipe would corrupt the stream outright.
    ///
    /// A broken pipe surfaces one message late — the writer thread ends,
    /// which drops the receiver, which fails the next send. That is soon
    /// enough for something already this far gone.
    fn send(&mut self, msg: &Value) -> Result<()> {
        let body = serde_json::to_vec(msg)?;
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(&body);
        self.out
            .send(frame)
            .map_err(|_| anyhow!("{} stopped reading its input", self.cmd))
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    /// Deal with one incoming message. Returns it only if it is a
    /// *response* (something a caller might be waiting for); everything
    /// else is absorbed here.
    ///
    /// This is the single place notifications are read, which is what
    /// makes diagnostics possible: they are pushed, not answered, so if
    /// this dropped them the way an early version did they would only
    /// ever be seen by accident, while some unrelated request happened to
    /// be in flight.
    fn handle_incoming(&mut self, msg: Value) -> Option<Value> {
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            return Some(msg);
        };
        match method {
            "$/progress" => self.note_progress(&msg),
            "textDocument/publishDiagnostics" => self.note_diagnostics(&msg),
            _ => {}
        }
        // A message with both a method and an id is a request *from* the
        // server, and several will block until answered — gopls will not
        // finish starting until `workspace/configuration` comes back.
        if let Some(id) = msg.get("id").cloned() {
            let result = match method {
                "workspace/configuration" => {
                    let n = msg["params"]["items"]
                        .as_array()
                        .map(|a| a.len())
                        .unwrap_or(1);
                    Value::Array(vec![json!({}); n])
                }
                _ => Value::Null,
            };
            let _ = self.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
        }
        None
    }

    /// Send a request and pump the incoming stream until its answer shows
    /// up.
    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| anyhow!("{} did not answer {method} in time", self.lang))?;
            let msg = match self.rx.recv_timeout(left) {
                Ok(msg) => msg,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    bail!("{} did not answer {method} in time", self.lang)
                }
                // The process is gone. Whatever it printed on the way out
                // is the actual explanation.
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!(self.died()),
            };
            let Some(msg) = self.handle_incoming(msg) else {
                continue;
            };
            if msg.get("id").and_then(Value::as_i64) != Some(id) {
                continue; // an answer to something else
            }
            if let Some(err) = msg.get("error") {
                bail!(
                    "{}: {}",
                    self.lang,
                    err.get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("error")
                );
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Read whatever is already waiting, without blocking. This is how
    /// pushed diagnostics reach the UI between requests.
    fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle_incoming(msg);
        }
    }

    fn note_diagnostics(&mut self, msg: &Value) {
        let Some(uri) = msg["params"]["uri"].as_str() else {
            return;
        };
        let items = msg["params"]["diagnostics"].as_array();
        let list: Vec<Diagnostic> = items
            .map(|a| a.iter().filter_map(parse_diagnostic).collect())
            .unwrap_or_default();
        let uri = uri.to_string();
        // An empty list means "this file is clean now" and has to replace
        // the old one — dropping it would leave stale red on screen.
        if self.diagnostics.get(&uri) == Some(&list) {
            return;
        }
        self.diagnostics.insert(uri, list);
        self.diag_version += 1;
    }

    /// Make the server's copy of a document match the text on screen.
    fn sync(&mut self, uri: &str, path: &str, text: &str) -> Result<()> {
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        let hash = hasher.finish();
        match self.opened.get(uri).copied() {
            Some((_, prev)) if prev == hash => Ok(()),
            Some((version, _)) => {
                let version = version + 1;
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": {"uri": uri, "version": version},
                        // Full-document sync: loupe never streams edits, it
                        // hands over a finished buffer.
                        "contentChanges": [{"text": text}],
                    }),
                )?;
                self.opened.insert(uri.to_string(), (version, hash));
                Ok(())
            }
            None => {
                self.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id(path),
                            "version": 1,
                            "text": text,
                        }
                    }),
                )?;
                self.opened.insert(uri.to_string(), (1, hash));
                Ok(())
            }
        }
    }

    /// Remember whether a long-running server task is in flight.
    fn note_progress(&mut self, msg: &Value) {
        let token = match &msg["params"]["token"] {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => return,
        };
        match msg["params"]["value"]["kind"].as_str() {
            Some("begin") => {
                self.progress.insert(token);
            }
            Some("end") => {
                self.progress.remove(&token);
            }
            _ => {}
        }
    }

    /// True while the server has told us it is busy with something.
    fn indexing(&self) -> bool {
        !self.progress.is_empty()
    }

    /// Read whatever the server has to say for a while, without asking it
    /// anything — how progress notifications get noticed between requests.
    fn pump(&mut self, budget: Duration) {
        let deadline = Instant::now() + budget;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let Ok(msg) = self.rx.recv_timeout(left) else {
                return;
            };
            self.handle_incoming(msg);
        }
    }

    /// A request whose empty answer might only mean "not ready yet".
    /// Retried while the server reports work in progress, so the first
    /// question after launch gets a real answer instead of a wrong one.
    fn request_when_ready(&mut self, method: &str, params: Value) -> Result<Value> {
        let mut result = self.request(method, params.clone(), REQUEST_TIMEOUT)?;
        let started = Instant::now();
        while is_empty_answer(&result) && self.indexing() && started.elapsed() < INDEXING_BUDGET {
            self.pump(Duration::from_millis(300));
            result = self.request(method, params.clone(), REQUEST_TIMEOUT)?;
        }
        Ok(result)
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Why the server is no longer running, in the words it used itself.
    fn died(&mut self) -> String {
        let tail = lock(&self.stderr);
        let said: Vec<&str> = tail
            .iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .take(2)
            .collect();
        let status = match self.child.try_wait() {
            Ok(Some(code)) => format!(" ({code})"),
            _ => String::new(),
        };
        if said.is_empty() {
            format!(
                "{} exited{status} without answering. Check that `{}` runs on its own.",
                self.lang, self.cmd
            )
        } else {
            format!("{} exited{status}: {}", self.lang, said.join(" — "))
        }
    }
}

// ---------------------------------------------------------------- registry

enum Slot {
    /// Not started yet.
    Off,
    Ready(Box<Client>),
    /// Tried and couldn't — the message says what to install.
    Failed(String),
}

/// The set of servers this session is using. Cheap to clone; a clone is
/// what a background job carries.
/// One lazily-started server per language, each behind its own lock so a
/// slow question about Rust doesn't hold up a question about TypeScript.
type Slots = HashMap<&'static str, Arc<Mutex<Slot>>>;

#[derive(Clone, Default)]
pub struct Lsp {
    slots: Arc<Mutex<Slots>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A panicking job must not take the language server down with it.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Lsp {
    /// Whether a file's language has a server loupe knows about — cheap,
    /// and used to decide if starting one is even worth offering.
    pub fn supports(path: &str) -> Option<&'static ServerSpec> {
        spec_for(path)
    }

    fn slot(&self, spec: &'static ServerSpec) -> Arc<Mutex<Slot>> {
        let mut map = lock(&self.slots);
        map.entry(spec.lang)
            .or_insert_with(|| Arc::new(Mutex::new(Slot::Off)))
            .clone()
    }

    /// Run something against the server for `path`, starting it if this is
    /// the first question about that language. **Blocks** — worker threads
    /// only.
    fn with_client<T>(
        &self,
        root: &Path,
        path: &str,
        f: impl FnOnce(&mut Client) -> Result<T>,
    ) -> Result<T> {
        let spec = spec_for(path).ok_or_else(|| {
            anyhow!(
                "No language server for {} — loupe drives TypeScript, Go and Rust; \
                 everything else falls back to pattern matching.",
                Path::new(path)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| format!(".{e} files"))
                    .unwrap_or_else(|| "this file type".into())
            )
        })?;
        let slot = self.slot(spec);
        let mut guard = lock(&slot);
        if let Slot::Off = &*guard {
            match Client::start(spec, root) {
                Ok(client) => *guard = Slot::Ready(Box::new(client)),
                Err(e) => {
                    let msg = format!("{e:#}");
                    *guard = Slot::Failed(msg.clone());
                    bail!(msg);
                }
            }
        }
        match &mut *guard {
            Slot::Failed(msg) => bail!(msg.clone()),
            Slot::Ready(client) => {
                let out = f(client);
                // A server that died takes its slot with it, so the next
                // question starts a fresh one instead of failing forever.
                if out.is_err() && !client.alive() {
                    *guard = Slot::Off;
                }
                out
            }
            Slot::Off => unreachable!("started above"),
        }
    }

    /// Start the server for this file in the background, so the first real
    /// question doesn't pay for the handshake. Errors are swallowed: this
    /// is an optimization, not a feature.
    pub fn warm(&self, root: &Path, path: &str, text: &str) {
        let Some(spec) = spec_for(path) else { return };
        // Don't fire up a process on the strength of a file we can't even
        // find a binary for.
        if which(spec.cmd).is_none() {
            return;
        }
        let (this, root, path, text) = (
            self.clone(),
            root.to_path_buf(),
            path.to_string(),
            text.to_string(),
        );
        thread::spawn(move || {
            let _ = this.with_client(&root, &path, |c| {
                let uri = uri_of(&root.join(&path));
                c.sync(&uri, &path, &text)
            });
        });
    }

    /// Every definition in one file.
    pub fn symbols(&self, root: &Path, path: &str, text: &str) -> Result<Vec<Sym>> {
        let uri = uri_of(&root.join(path));
        self.with_client(root, path, |c| {
            c.sync(&uri, path, text)?;
            let result = c.request_when_ready(
                "textDocument/documentSymbol",
                json!({"textDocument": {"uri": uri}}),
            )?;
            let mut out = Vec::new();
            collect_symbols(&result, None, &mut out);
            out.sort_by_key(|s| s.line);
            Ok(out)
        })
    }

    pub fn definition(
        &self,
        root: &Path,
        path: &str,
        text: &str,
        at: (usize, usize),
    ) -> Result<Vec<Loc>> {
        self.locations(root, path, text, at, "textDocument/definition", json!({}))
    }

    pub fn references(
        &self,
        root: &Path,
        path: &str,
        text: &str,
        at: (usize, usize),
    ) -> Result<Vec<Loc>> {
        self.locations(
            root,
            path,
            text,
            at,
            "textDocument/references",
            json!({"context": {"includeDeclaration": true}}),
        )
    }

    /// Shared shape of definition and references: a position in, a list of
    /// places out.
    fn locations(
        &self,
        root: &Path,
        path: &str,
        text: &str,
        (line, col): (usize, usize),
        method: &'static str,
        extra: Value,
    ) -> Result<Vec<Loc>> {
        let uri = uri_of(&root.join(path));
        let character = utf16_col(
            nth_line(text, line.saturating_sub(1)),
            col.saturating_sub(1),
        );
        let root = root.to_path_buf();
        self.with_client(&root.clone(), path, move |c| {
            c.sync(&uri, path, text)?;
            let mut params = json!({
                "textDocument": {"uri": uri},
                "position": {"line": line.saturating_sub(1), "character": character},
            });
            if let (Some(p), Some(e)) = (params.as_object_mut(), extra.as_object()) {
                for (k, v) in e {
                    p.insert(k.clone(), v.clone());
                }
            }
            let result = c.request_when_ready(method, params)?;
            Ok(parse_locations(&result, &root))
        })
    }

    /// The signature-and-docs blurb for a symbol, as plain text.
    pub fn hover(
        &self,
        root: &Path,
        path: &str,
        text: &str,
        (line, col): (usize, usize),
    ) -> Result<Option<String>> {
        let uri = uri_of(&root.join(path));
        let character = utf16_col(
            nth_line(text, line.saturating_sub(1)),
            col.saturating_sub(1),
        );
        self.with_client(root, path, |c| {
            c.sync(&uri, path, text)?;
            let result = c.request_when_ready(
                "textDocument/hover",
                json!({
                    "textDocument": {"uri": uri},
                    "position": {"line": line.saturating_sub(1), "character": character},
                }),
            )?;
            Ok(hover_text(&result))
        })
    }

    /// Read anything the servers have pushed, without blocking the UI
    /// thread for a moment.
    ///
    /// `try_lock` throughout: a worker thread may be mid-request holding
    /// a server's lock, and a redraw must never wait on a language
    /// server. A skipped tick costs nothing — the next one picks the
    /// messages up.
    ///
    /// Returns the total diagnostics version, so the caller can tell
    /// whether anything actually changed.
    pub fn poll(&self) -> u64 {
        let Ok(map) = self.slots.try_lock() else {
            return 0;
        };
        let mut version = 0;
        for slot in map.values() {
            let Ok(mut guard) = slot.try_lock() else {
                continue;
            };
            if let Slot::Ready(client) = &mut *guard {
                client.drain();
                version += client.diag_version;
            }
        }
        version
    }

    /// The problems the server last reported for one file. Never blocks;
    /// an empty answer may just mean "busy, ask again next frame".
    pub fn diagnostics(&self, root: &Path, path: &str) -> Vec<Diagnostic> {
        let Some(spec) = spec_for(path) else {
            return Vec::new();
        };
        let Ok(map) = self.slots.try_lock() else {
            return Vec::new();
        };
        let Some(slot) = map.get(spec.lang) else {
            return Vec::new();
        };
        let Ok(guard) = slot.try_lock() else {
            return Vec::new();
        };
        let Slot::Ready(client) = &*guard else {
            return Vec::new();
        };
        client
            .diagnostics
            .get(&uri_of(&root.join(path)))
            .cloned()
            .unwrap_or_default()
    }

    /// Push the buffer to a server that is *already running*, without
    /// blocking and without starting one.
    ///
    /// This is what makes editing live: diagnostics for text that only
    /// exists in the editor, and completions that know about the line
    /// being typed. `didChange` is a notification — a pipe write — so the
    /// cost here is the lock, not the server.
    pub fn sync_open(&self, root: &Path, path: &str, text: &str) -> bool {
        let Some(spec) = spec_for(path) else {
            return false;
        };
        let Ok(map) = self.slots.try_lock() else {
            return false;
        };
        let Some(slot) = map.get(spec.lang) else {
            return false;
        };
        let Ok(mut guard) = slot.try_lock() else {
            return false;
        };
        let Slot::Ready(client) = &mut *guard else {
            return false;
        };
        let uri = uri_of(&root.join(path));
        client.sync(&uri, path, text).is_ok()
    }

    /// Suggestions at a position. Returns an empty list (not an error)
    /// when the server offers no completion at all.
    pub fn complete(
        &self,
        root: &Path,
        path: &str,
        text: &str,
        (line, col): (usize, usize),
    ) -> Result<Vec<Completion>> {
        let uri = uri_of(&root.join(path));
        let character = utf16_col(
            nth_line(text, line.saturating_sub(1)),
            col.saturating_sub(1),
        );
        self.with_client(root, path, |c| {
            if c.caps.get("completionProvider").is_none() {
                return Ok(Vec::new());
            }
            c.sync(&uri, path, text)?;
            let result = c.request(
                "textDocument/completion",
                json!({
                    "textDocument": {"uri": uri},
                    "position": {"line": line.saturating_sub(1), "character": character},
                    "context": {"triggerKind": 1},
                }),
                REQUEST_TIMEOUT,
            )?;
            Ok(parse_completions(&result))
        })
    }

    /// Format the whole document. `Ok(None)` means the server does not
    /// format this language — a different thing from "no changes".
    pub fn format(
        &self,
        root: &Path,
        path: &str,
        text: &str,
        tab_size: usize,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri = uri_of(&root.join(path));
        self.with_client(root, path, |c| {
            if c.caps.get("documentFormattingProvider").is_none() {
                return Ok(None);
            }
            c.sync(&uri, path, text)?;
            let result = c.request(
                "textDocument/formatting",
                json!({
                    "textDocument": {"uri": uri},
                    "options": {"tabSize": tab_size, "insertSpaces": true},
                }),
                REQUEST_TIMEOUT,
            )?;
            Ok(Some(parse_edits(&result)))
        })
    }

    /// One line per language: running, not installed, or never asked.
    pub fn status(&self) -> Vec<(&'static str, String)> {
        let map = lock(&self.slots);
        SERVERS
            .iter()
            .map(|spec| {
                let state = match map.get(spec.lang) {
                    None => match which(spec.cmd) {
                        Some(_) => "installed, not started yet".to_string(),
                        None => format!("not installed — {}", spec.install),
                    },
                    Some(slot) => match &*lock(slot) {
                        Slot::Off => "not started yet".to_string(),
                        Slot::Ready(_) => "running".to_string(),
                        Slot::Failed(e) => e.clone(),
                    },
                };
                (spec.lang, state)
            })
            .collect()
    }

    /// Stop every server (each `Client`'s `Drop` does the killing).
    pub fn shutdown(&self) {
        let mut map = lock(&self.slots);
        map.clear();
    }
}

// ------------------------------------------------------------------ parsing

/// One `publishDiagnostics` entry. Positions arrive 0-based and in
/// UTF-16 units; they are converted where the text is known (the caller
/// has the buffer, this does not), so what comes out here is 1-based
/// lines and raw UTF-16 columns.
fn parse_diagnostic(v: &Value) -> Option<Diagnostic> {
    let range = v.get("range")?;
    let line = range.pointer("/start/line")?.as_u64()? as usize;
    let col = range.pointer("/start/character")?.as_u64()? as usize;
    let end_line = range.pointer("/end/line")?.as_u64()? as usize;
    let end_col = range.pointer("/end/character")?.as_u64()? as usize;
    let message = v.get("message")?.as_str()?.trim().to_string();
    // A multi-line span (a whole unclosed block, say) is marked on its
    // first line only; the message says the rest.
    let end_col = if end_line == line {
        end_col
    } else {
        usize::MAX
    };
    let code = match v.get("code") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    };
    let source = v.get("source").and_then(Value::as_str);
    Some(Diagnostic {
        line: line + 1,
        col: col + 1,
        end_col,
        severity: v.get("severity").and_then(Value::as_u64).unwrap_or(1) as u8,
        message,
        code: match (source, code) {
            (Some(s), Some(c)) => Some(format!("{s}({c})")),
            (None, Some(c)) => Some(c),
            (Some(s), None) => Some(s.to_string()),
            (None, None) => None,
        },
    })
}

/// `completion` answers with a bare list or a `CompletionList`.
fn parse_completions(value: &Value) -> Vec<Completion> {
    let items = match value {
        Value::Array(a) => a.clone(),
        Value::Object(o) => o
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let mut out: Vec<Completion> = items
        .iter()
        .filter_map(|item| {
            let label = item.get("label")?.as_str()?.trim().to_string();
            if label.is_empty() {
                return None;
            }
            // `textEdit` knows where the word being completed starts,
            // which matters when it began before the cursor.
            let edit = item.get("textEdit");
            let replace = edit.and_then(|e| {
                let r = e.get("range").or_else(|| e.get("replace"))?;
                Some((
                    r.pointer("/start/line")?.as_u64()? as usize,
                    r.pointer("/start/character")?.as_u64()? as usize,
                    r.pointer("/end/line")?.as_u64()? as usize,
                    r.pointer("/end/character")?.as_u64()? as usize,
                ))
            });
            let insert = edit
                .and_then(|e| e.get("newText"))
                .or_else(|| item.get("insertText"))
                .and_then(Value::as_str)
                .unwrap_or(&label)
                .to_string();
            Some(Completion {
                kind: completion_kind(item.get("kind").and_then(Value::as_u64).unwrap_or(0)),
                detail: item
                    .get("detail")
                    .and_then(Value::as_str)
                    .map(|d| d.trim().to_string())
                    .filter(|d| !d.is_empty()),
                sort: item
                    .get("sortText")
                    .and_then(Value::as_str)
                    .unwrap_or(&label)
                    .to_string(),
                label,
                insert,
                replace,
            })
        })
        .collect();
    // The server's ordering is the useful one — it knows which member you
    // probably meant. Alphabetical would throw that away.
    out.sort_by(|a, b| a.sort.cmp(&b.sort).then(a.label.cmp(&b.label)));
    out
}

fn parse_edits(value: &Value) -> Vec<TextEdit> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|e| {
            let r = e.get("range")?;
            Some(TextEdit {
                start: (
                    r.pointer("/start/line")?.as_u64()? as usize,
                    r.pointer("/start/character")?.as_u64()? as usize,
                ),
                end: (
                    r.pointer("/end/line")?.as_u64()? as usize,
                    r.pointer("/end/character")?.as_u64()? as usize,
                ),
                text: e.get("newText")?.as_str()?.to_string(),
            })
        })
        .collect()
}

/// Whether an answer is "nothing" — which from a server that is still
/// indexing means "not yet" rather than "there is none".
fn is_empty_answer(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// `documentSymbol` answers with either a tree (`DocumentSymbol[]`) or a
/// flat list (`SymbolInformation[]`), and which one depends on the server.
fn collect_symbols(value: &Value, container: Option<&str>, out: &mut Vec<Sym>) {
    let Some(items) = value.as_array() else {
        return;
    };
    for item in items {
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            continue;
        };
        let kind = symbol_kind(item.get("kind").and_then(Value::as_u64).unwrap_or(0));
        // Hierarchical: `selectionRange` is the name itself. Flat:
        // `location.range`.
        let line = item
            .get("selectionRange")
            .or_else(|| item.get("range"))
            .or_else(|| item.pointer("/location/range"))
            .and_then(|r| r.pointer("/start/line"))
            .and_then(Value::as_u64)
            .map(|l| l as usize + 1);
        let Some(line) = line else { continue };
        out.push(Sym {
            name: name.to_string(),
            kind,
            line,
            container: item
                .get("containerName")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| container.map(str::to_string)),
        });
        if let Some(children) = item.get("children") {
            collect_symbols(children, Some(name), out);
        }
    }
}

/// `definition` can answer with a single Location, a list of them, or a
/// list of LocationLinks — all three appear in the wild.
fn parse_locations(value: &Value, root: &Path) -> Vec<Loc> {
    let mut out = Vec::new();
    let items: Vec<&Value> = match value {
        Value::Array(a) => a.iter().collect(),
        Value::Null => Vec::new(),
        other => vec![other],
    };
    for item in items {
        let uri = item
            .get("uri")
            .or_else(|| item.get("targetUri"))
            .and_then(Value::as_str);
        let range = item
            .get("range")
            .or_else(|| item.get("targetSelectionRange"))
            .or_else(|| item.get("targetRange"));
        let (Some(uri), Some(range)) = (uri, range) else {
            continue;
        };
        let Some(path) = path_of_uri(uri) else {
            continue;
        };
        let line = range
            .pointer("/start/line")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let character = range
            .pointer("/start/character")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        // Repo-relative paths are what the rest of loupe speaks; a result
        // outside the repository (into a dependency, or the standard
        // library) keeps its absolute path and is shown as-is.
        let rel = path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string_lossy().to_string());
        out.push(Loc {
            path: rel,
            line: line + 1,
            // Converted back to chars only when the text is known; for a
            // cross-file result this is close enough to place a cursor.
            col: character + 1,
        });
    }
    out
}

/// Hover contents come as a string, a `{language, value}` pair, an array
/// of either, or a `MarkupContent`. Flatten to plain text and strip the
/// code fences, which a terminal panel has no use for.
fn hover_text(value: &Value) -> Option<String> {
    fn one(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => out.push(s.clone()),
            Value::Array(a) => a.iter().for_each(|i| one(i, out)),
            Value::Object(o) => {
                if let Some(Value::String(s)) = o.get("value") {
                    out.push(s.clone());
                }
            }
            _ => {}
        }
    }
    let contents = value.get("contents")?;
    let mut parts = Vec::new();
    one(contents, &mut parts);
    let text = parts.join("\n");
    let cleaned: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect();
    let text = cleaned.join("\n").trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this replaced: `send` wrote straight to the server's pipe
    /// from whichever thread called it, and `sync_open` calls it from the
    /// idle tick on the drawing thread. A pipe holds 64 KB; a `didChange`
    /// on a large buffer is several times that, so the write waited on a
    /// server that was busy indexing, and the window stopped.
    ///
    /// This drives the same shape without a language server: a pipe
    /// nobody reads, and frames far larger than it holds. If `send` ever
    /// writes to the pipe again rather than handing it to the writer
    /// thread, this blocks forever instead of failing.
    #[test]
    fn sending_does_not_wait_for_the_server_to_read() {
        use std::sync::mpsc::RecvTimeoutError;

        // `cat` with its stdout going nowhere we read: its input pipe
        // fills and stays full.
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("cat is on every machine this runs on");
        let mut stdin = child.stdin.take().expect("piped");
        let (out, outbox) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            for frame in outbox {
                if stdin.write_all(&frame).is_err() || stdin.flush().is_err() {
                    break;
                }
            }
        });

        // Ten megabytes, in a pipe that holds 64 kilobytes.
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            for _ in 0..10 {
                if out.send(vec![b'x'; 1024 * 1024]).is_err() {
                    break;
                }
            }
            let _ = done_tx.send(());
        });

        assert!(
            !matches!(
                done_rx.recv_timeout(Duration::from_secs(5)),
                Err(RecvTimeoutError::Timeout)
            ),
            "handing frames over blocked on the pipe"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn maps_extensions_to_servers() {
        assert_eq!(spec_for("src/a.tsx").unwrap().lang, "TypeScript");
        assert_eq!(spec_for("main.go").unwrap().lang, "Go");
        assert_eq!(spec_for("src/app.rs").unwrap().lang, "Rust");
        assert!(spec_for("notes.md").is_none());
        assert_eq!(language_id("a.tsx"), "typescriptreact");
        assert_eq!(language_id("a.mjs"), "javascript");
    }

    /// rustup leaves a stand-in on `PATH` for every tool it could
    /// provide, so "there is a file here" does not mean the tool is
    /// installed. Anything that resolves to rustup itself is one of
    /// those, and has to be checked with rustup rather than believed.
    #[cfg(unix)]
    #[test]
    fn a_rustup_stand_in_is_not_a_real_tool() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("loupe-which-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // What `~/.cargo/bin` looks like: rustup, and a link to it named
        // after a component that may or may not be installed.
        let rustup = dir.join("rustup");
        std::fs::write(&rustup, "#!/bin/sh\nexit 1\n").unwrap();
        let proxy = dir.join("some-analyzer");
        symlink(&rustup, &proxy).unwrap();
        assert!(is_rustup_proxy(&proxy), "a link to rustup is a stand-in");

        // A real tool is itself, and is taken at face value.
        let real = dir.join("real-server");
        std::fs::write(&real, "#!/bin/sh\nexit 0\n").unwrap();
        assert!(!is_rustup_proxy(&real));

        // The shape a normal install actually has: rustup hard-links its
        // stand-ins rather than symlinking them, so the resolved name is
        // the stand-in's own and only the inode gives it away. Reading
        // the name alone called this one a real tool, and loupe then
        // offered gd/gr/K and failed at the first request.
        #[cfg(unix)]
        {
            let linked = dir.join("hardlinked-analyzer");
            std::fs::hard_link(&rustup, &linked).unwrap();
            assert!(
                is_rustup_proxy(&linked),
                "a hard link to rustup is a stand-in too"
            );
        }

        // Windows keeps a copy rather than a link, and it is still rustup.
        let copied = dir.join("rustup.exe");
        std::fs::write(&copied, "x").unwrap();
        assert!(is_rustup_proxy(&copied), "the name is what gives it away");

        // A path that is not there at all answers no rather than panicking.
        assert!(!is_rustup_proxy(&dir.join("nothing-here")));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The bug this all comes from: on a machine with rustup but without
    /// the component, loupe reported rust-analyzer as installed, offered
    /// `gd` / `gr` / `K`, and failed at the first request with rustup's
    /// "Unknown binary" error. Whatever the answer is here, it has to
    /// match what actually happens when the binary runs.
    #[test]
    fn rust_analyzer_is_reported_only_when_it_runs() {
        // The resolved path, because that is what `Client::start` spawns.
        // Running the bare name instead would go back through the rustup
        // stand-in and could fail on a machine where loupe works.
        let found = which("rust-analyzer");
        let runs = found.as_ref().is_some_and(|bin| {
            Command::new(bin)
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
        });
        assert_eq!(
            found.is_some(),
            runs,
            "which() said {found:?} but running it said {runs}"
        );
    }

    /// The other half of it: a rustup stand-in whose component *is*
    /// installed must still resolve, and to the real binary rather than
    /// to the stand-in. `rustfmt` is the one to test with — it goes
    /// through the same proxy and every Rust toolchain has it.
    #[test]
    fn an_installed_rustup_component_still_resolves() {
        let Some(found) = which("rustfmt") else {
            eprintln!("skipping: no rustfmt on PATH");
            return;
        };
        assert!(found.is_file(), "it points at something real: {found:?}");
        assert!(
            !is_rustup_proxy(&found),
            "and at the binary itself, not the stand-in: {found:?}"
        );
    }

    #[test]
    fn nothing_is_found_for_a_command_that_does_not_exist() {
        assert!(which("loupe-no-such-binary-xyzzy").is_none());
    }

    #[test]
    fn uris_round_trip_through_awkward_paths() {
        let p = Path::new("/tmp/my repo/src/a b#c.ts");
        let uri = uri_of(p);
        assert!(uri.starts_with("file:///tmp/my%20repo/"));
        assert!(uri.contains("%23"), "# must be encoded: {uri}");
        assert_eq!(path_of_uri(&uri).as_deref(), Some(p));
    }

    #[test]
    fn columns_convert_between_chars_and_utf16() {
        // An emoji is one char but two UTF-16 units — a server counting
        // the second way would point at the wrong place without this.
        let line = "let 🌍 = handleClick;";
        assert_eq!(utf16_col(line, 4), 4);
        assert_eq!(utf16_col(line, 5), 6);
        assert_eq!(char_col(line, 6), 5);
        assert_eq!(char_col(line, 0), 0);
        // Past the end clamps rather than panicking.
        assert_eq!(char_col(line, 999), line.chars().count());
    }

    #[test]
    fn parses_hierarchical_symbols() {
        let value = json!([
            {
                "name": "App", "kind": 5,
                "range": {"start": {"line": 4, "character": 0}, "end": {"line": 20, "character": 1}},
                "selectionRange": {"start": {"line": 4, "character": 6}, "end": {"line": 4, "character": 9}},
                "children": [
                    {
                        "name": "handleClick", "kind": 6,
                        "selectionRange": {"start": {"line": 9, "character": 2}, "end": {"line": 9, "character": 13}}
                    }
                ]
            }
        ]);
        let mut out = Vec::new();
        collect_symbols(&value, None, &mut out);
        assert_eq!(
            out,
            vec![
                Sym {
                    name: "App".into(),
                    kind: "class",
                    line: 5,
                    container: None
                },
                Sym {
                    name: "handleClick".into(),
                    kind: "method",
                    line: 10,
                    container: Some("App".into())
                },
            ]
        );
    }

    #[test]
    fn parses_flat_symbols() {
        let value = json!([
            {
                "name": "main", "kind": 12, "containerName": "",
                "location": {
                    "uri": "file:///r/main.go",
                    "range": {"start": {"line": 2, "character": 5}, "end": {"line": 2, "character": 9}}
                }
            }
        ]);
        let mut out = Vec::new();
        collect_symbols(&value, None, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].line, 3);
        assert_eq!(out[0].kind, "fn");
    }

    #[test]
    fn parses_every_definition_shape() {
        let root = Path::new("/r");
        // A bare Location.
        let single = json!({
            "uri": "file:///r/src/a.ts",
            "range": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 17}}
        });
        assert_eq!(
            parse_locations(&single, root),
            vec![Loc {
                path: "src/a.ts".into(),
                line: 1,
                col: 7
            }]
        );
        // A LocationLink.
        let link = json!([{
            "targetUri": "file:///r/src/b.ts",
            "targetSelectionRange": {"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 4}}
        }]);
        assert_eq!(
            parse_locations(&link, root),
            vec![Loc {
                path: "src/b.ts".into(),
                line: 4,
                col: 1
            }]
        );
        // Outside the repository: kept absolute rather than mangled.
        let outside = json!([{
            "uri": "file:///usr/lib/node_modules/x/index.d.ts",
            "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}}
        }]);
        assert_eq!(
            parse_locations(&outside, root)[0].path,
            "/usr/lib/node_modules/x/index.d.ts"
        );
        assert!(parse_locations(&Value::Null, root).is_empty());
    }

    #[test]
    fn flattens_hover_contents() {
        let markup = json!({"contents": {"kind": "markdown", "value": "```ts\nconst x: number\n```\nA number."}});
        assert_eq!(
            hover_text(&markup).as_deref(),
            Some("const x: number\nA number.")
        );
        let list = json!({"contents": [{"language": "rust", "value": "fn main()"}, "docs"]});
        assert_eq!(hover_text(&list).as_deref(), Some("fn main()\ndocs"));
        assert!(hover_text(&json!({"contents": ""})).is_none());
        assert!(hover_text(&Value::Null).is_none());
    }

    #[test]
    fn message_framing_reads_a_whole_message() {
        let raw = "Content-Length: 17\r\n\r\n{\"jsonrpc\":\"2.0\"}";
        let mut r = BufReader::new(raw.as_bytes());
        let msg = read_message(&mut r).unwrap().unwrap();
        assert_eq!(msg["jsonrpc"], "2.0");
        // End of stream is None, not an error.
        assert!(read_message(&mut r).unwrap().is_none());
    }

    /// True when the error says the server never started, rather than that
    /// it answered loupe's question wrongly.
    ///
    /// Finding the binary on `PATH` proves less than it looks. The server
    /// still needs a `typescript` package it can load, and TypeScript 7
    /// ships no `tsserver.js` for typescript-language-server 6 to use — so
    /// a machine can have both installed and still have no working server.
    /// That machine is a supported one: loupe falls back to pattern
    /// matching. Only a failure to start is skipped; every other error
    /// still fails the test, because that one would be loupe's fault.
    fn server_unavailable(e: &anyhow::Error) -> bool {
        format!("{e:#}").contains("Request initialize failed")
    }

    /// The real thing: start `typescript-language-server`, ask it the four
    /// questions loupe asks, and check the answers line up with the file.
    ///
    /// Skipped (not failed) when no working server is installed — the whole
    /// design is that loupe uses what you already have, so a machine
    /// without one is a supported state, not a broken one.
    #[test]
    fn talks_to_a_real_typescript_server() {
        if which("typescript-language-server").is_none() {
            eprintln!("skipping: typescript-language-server is not installed");
            return;
        }
        let root = std::env::temp_dir().join(format!("loupe-lsp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("tsconfig.json"),
            r#"{"compilerOptions":{"strict":true}}"#,
        )
        .unwrap();
        let src = "export function handleClick(count: number): number {\n  return count + 1;\n}\n\nconst first = handleClick(1);\nconst second = handleClick(2);\n";
        std::fs::write(root.join("a.ts"), src).unwrap();

        let lsp = Lsp::default();
        let syms = match lsp.symbols(&root, "a.ts", src) {
            Ok(syms) => syms,
            Err(e) if server_unavailable(&e) => {
                eprintln!("skipping: {e}");
                let _ = std::fs::remove_dir_all(&root);
                return;
            }
            Err(e) => panic!("documentSymbol: {e}"),
        };
        assert!(
            syms.iter().any(|s| s.name == "handleClick" && s.line == 1),
            "expected handleClick among {syms:?}"
        );

        // "handleClick" starts at column 15 of line 5.
        let call = (5, 15);
        let def = lsp
            .definition(&root, "a.ts", src, call)
            .expect("definition");
        assert_eq!(def.len(), 1, "one definition: {def:?}");
        assert_eq!(def[0].path, "a.ts");
        assert_eq!(def[0].line, 1, "the definition is on line 1");

        let refs = lsp
            .references(&root, "a.ts", src, call)
            .expect("references");
        let lines: Vec<usize> = {
            let mut l: Vec<usize> = refs.iter().map(|r| r.line).collect();
            l.sort_unstable();
            l
        };
        assert_eq!(lines, vec![1, 5, 6], "declaration plus both call sites");

        let hover = lsp.hover(&root, "a.ts", src, call).expect("hover");
        let hover = hover.expect("some hover text");
        assert!(
            hover.contains("handleClick"),
            "hover says what it is: {hover}"
        );

        // The buffer, not the file, is what gets analyzed: ask about text
        // that exists nowhere on disk and the answer still tracks it.
        let edited = format!("// a line that only exists in the buffer\n{src}");
        let syms = lsp.symbols(&root, "a.ts", &edited).expect("re-sync");
        assert!(
            syms.iter().any(|s| s.name == "handleClick" && s.line == 2),
            "the definition moved down a line with the edit: {syms:?}"
        );

        assert!(lsp
            .status()
            .iter()
            .any(|(lang, state)| *lang == "TypeScript" && state == "running"));
        lsp.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The second server, for the same four questions — different
    /// implementation, different quirks (this one answers `documentSymbol`
    /// instantly but needs its index before it can find a reference).
    #[test]
    fn talks_to_a_real_rust_server() {
        if which("rust-analyzer").is_none() {
            eprintln!("skipping: rust-analyzer is not installed");
            return;
        }
        let root = std::env::temp_dir().join(format!("loupe-ra-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let src = "fn handle_click(count: usize) -> usize {\n    count + 1\n}\n\nfn main() {\n    let a = handle_click(1);\n    let b = handle_click(2);\n    println!(\"{a} {b}\");\n}\n";
        std::fs::write(root.join("src/main.rs"), src).unwrap();

        let lsp = Lsp::default();
        let syms = lsp.symbols(&root, "src/main.rs", src).expect("symbols");
        assert!(syms.iter().any(|s| s.name == "handle_click" && s.line == 1));

        // Column 13 of line 6 is inside the first `handle_click(1)` call.
        let at = (6, 13);
        let refs = lsp
            .references(&root, "src/main.rs", src, at)
            .expect("references");
        let mut lines: Vec<usize> = refs.iter().map(|r| r.line).collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            vec![1, 6, 7],
            "the declaration and both calls — an empty answer here means the \
             indexing wait regressed"
        );

        let hover = lsp.hover(&root, "src/main.rs", src, at).expect("hover");
        assert!(
            hover.unwrap_or_default().contains("fn handle_click"),
            "hover carries the signature"
        );
        lsp.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Diagnostics, completion and formatting against a real server —
    /// the three editor features, end to end. Skipped when no working
    /// typescript-language-server is installed.
    #[test]
    fn the_editor_features_work_against_a_real_server() {
        if which("typescript-language-server").is_none() {
            eprintln!("skipping: typescript-language-server is not installed");
            return;
        }
        let root = std::env::temp_dir().join(format!("loupe-ed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("tsconfig.json"),
            r#"{"compilerOptions":{"strict":true}}"#,
        )
        .unwrap();
        // `totl` is a typo the server should complain about.
        let src = "export function add(count: number): number {\n  const total = count + 1;\n  return totl;\n}\n";
        std::fs::write(root.join("a.ts"), src).unwrap();

        let lsp = Lsp::default();
        // Any request opens the document, which is what makes the server
        // start publishing diagnostics for it.
        match lsp.symbols(&root, "a.ts", src) {
            Ok(_) => {}
            Err(e) if server_unavailable(&e) => {
                eprintln!("skipping: {e}");
                let _ = std::fs::remove_dir_all(&root);
                return;
            }
            Err(e) => panic!("symbols: {e}"),
        }

        // Diagnostics are pushed, so they arrive on their own schedule.
        let mut found = Vec::new();
        for _ in 0..60 {
            lsp.poll();
            found = lsp.diagnostics(&root, "a.ts");
            if !found.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        assert!(
            found
                .iter()
                .any(|d| d.message.contains("totl") && d.line == 3),
            "expected a complaint about `totl` on line 3, got {found:?}"
        );
        let d = found.iter().find(|d| d.line == 3).unwrap();
        assert!(d.is_error());
        assert!(
            d.code.as_deref().is_some_and(|c| c.contains("2552")),
            "the code travels with the message: {:?}",
            d.code
        );
        assert!(
            d.col >= 10 && d.end_col > d.col,
            "the span covers the word: {d:?}"
        );

        // Completion after a dot, on text that only exists in the buffer.
        let typing = "export function add(count: number): number {\n  const total = count + 1;\n  return total.\n}\n";
        let items = lsp
            .complete(&root, "a.ts", typing, (3, 16))
            .expect("completion");
        assert!(
            items.iter().any(|c| c.label == "toFixed"),
            "number members offered: {:?}",
            items.iter().take(8).map(|c| &c.label).collect::<Vec<_>>()
        );

        // Formatting returns edits for badly-spaced source.
        let ugly = "export function add(count:number):number{\nreturn count+1;\n}\n";
        let edits = lsp
            .format(&root, "a.ts", ugly, 2)
            .expect("formatting")
            .expect("this server formats");
        assert!(!edits.is_empty(), "something to tidy");
        let formatted = crate::editor::apply_text_edits(ugly, &edits);
        assert!(
            formatted.contains("count: number") && formatted.contains("  return"),
            "formatted output: {formatted:?}"
        );

        lsp.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_server_reports_how_to_install_it() {
        let lsp = Lsp::default();
        let err = lsp
            .symbols(Path::new("/tmp"), "x.unknownext", "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("pattern matching"), "{err}");
        // Every supported language has an install line to offer.
        for (spec, _) in doctor() {
            assert!(!spec.install.is_empty());
        }
    }
}

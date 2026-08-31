//! Pinned files, and the row of tabs that holds them.
//!
//! A review sends the reader away from the file they were reading: to a
//! plan file an agent is still writing, to the design note that says why
//! the change looks like this, to the second markdown file that answers
//! the first. The file panel only lists what the change touches, so none
//! of those are one key away, and half of them are not in the repository
//! at all.
//!
//! A pin puts one file in a tab at the top of the window. The tab stays
//! there while the reader goes somewhere else, and one click — or `Alt`
//! and the tab's number — brings the file back. Pins live under the git
//! directory, so they survive quitting loupe and never reach a commit.
//!
//! The path may be anywhere on this machine. Drop a file on the terminal
//! window and the terminal writes its path into loupe as a paste; loupe
//! reads the path, pins it, and renders it. That is how a document in
//! `~/Downloads` gets read beside the review without first being moved
//! into the repository.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How many tabs the row holds. `Alt` and a digit reach the first 9; a
/// click reaches the rest. Past this the row stops being a row of tabs
/// and starts being a file panel, which loupe already has.
pub const MAX_PINS: usize = 12;

/// The largest file loupe reads into a tab. A dropped path is whatever
/// the reader dragged, so the size is not known in advance, and reading
/// a video into a markdown renderer helps nobody.
pub const MAX_BYTES: u64 = 16 * 1024 * 1024;

/// One pinned file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pin {
    /// Repo-relative when the file is in the repository, and the absolute
    /// path when it is not. This is the name loupe shows and the name the
    /// rest of loupe opens files by.
    pub path: String,
    /// The absolute path on disk, for reading and writing.
    pub abs_path: PathBuf,
    /// The file lives outside the repository root. The tab marks these,
    /// because a comment about "the plan file" means a different file
    /// depending on the answer.
    #[serde(default)]
    pub outside: bool,
}

impl Pin {
    /// Build a pin for `abs`, named relative to `root` when it sits
    /// inside the repository.
    ///
    /// Both paths are resolved first. A dropped path arrives already
    /// resolved and a path built from the file panel does not, and one
    /// symlink between them is enough to give one file two tabs — or to
    /// file a repository file as an outside one. On macOS `/tmp` alone
    /// is such a symlink.
    pub fn new(root: &Path, abs: PathBuf) -> Pin {
        let abs = std::fs::canonicalize(&abs).unwrap_or(abs);
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        match abs.strip_prefix(&root) {
            Ok(rel) => Pin {
                path: rel.to_string_lossy().replace('\\', "/"),
                abs_path: abs,
                outside: false,
            },
            Err(_) => Pin {
                path: abs.to_string_lossy().into_owned(),
                abs_path: abs,
                outside: true,
            },
        }
    }

    /// The file name on its own — what the tab shows when it is the only
    /// pin with that name.
    pub fn file_name(&self) -> String {
        self.abs_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.clone())
    }

    /// The parent directory and the file name — what the tab shows when
    /// another pin has the same file name. Agents write a great many
    /// files called `PLAN.md`.
    pub fn parent_and_name(&self) -> String {
        let name = self.file_name();
        match self.abs_path.parent().and_then(|p| p.file_name()) {
            Some(dir) => format!("{}/{name}", dir.to_string_lossy()),
            None => name,
        }
    }
}

/// The pinned files.
///
/// Which tab is *open* is deliberately not kept here. It is derived from
/// the file that is on screen (`App::active_pin`), because a reader
/// leaves a document by a dozen different doors — Esc, the file panel,
/// a search result, a jump to a definition — and a remembered index
/// would have to be cleared in every one of them. Derived, it cannot go
/// stale.
#[derive(Default)]
pub struct Pins {
    pub items: Vec<Pin>,
    /// First tab drawn, when the row is too narrow for all of them.
    /// The draw keeps the open tab inside the row.
    pub scroll: usize,
}

impl Pins {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// The tab holding `abs`, if there is one. Matching on the absolute
    /// path rather than the display name means a repository file and an
    /// outside file that share a name stay two different pins.
    pub fn find(&self, abs: &Path) -> Option<usize> {
        self.items.iter().position(|p| p.abs_path == abs)
    }

    /// Add a pin, or return the tab that already holds that file. `Err`
    /// when the row is full.
    pub fn add(&mut self, pin: Pin) -> Result<usize, usize> {
        if let Some(i) = self.find(&pin.abs_path) {
            return Ok(i);
        }
        if self.items.len() >= MAX_PINS {
            return Err(MAX_PINS);
        }
        self.items.push(pin);
        Ok(self.items.len() - 1)
    }

    /// Drop a tab. The file itself is untouched — a pin is a bookmark,
    /// not a copy.
    pub fn remove(&mut self, idx: usize) -> Option<Pin> {
        if idx >= self.items.len() {
            return None;
        }
        let gone = self.items.remove(idx);
        self.scroll = self.scroll.min(self.items.len().saturating_sub(1));
        Some(gone)
    }

    /// What each tab is called. A name shared by two pins gains its
    /// parent directory, so no two tabs read the same.
    pub fn labels(&self) -> Vec<String> {
        let names: Vec<String> = self.items.iter().map(|p| p.file_name()).collect();
        names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let shared = names.iter().enumerate().any(|(j, m)| j != i && m == name);
                if shared {
                    self.items[i].parent_and_name()
                } else {
                    name.clone()
                }
            })
            .collect()
    }

    /// Step to the next or previous tab from `from`, wrapping at each
    /// end. `None` when there is nothing pinned.
    pub fn step(&self, delta: i32, from: Option<usize>) -> Option<usize> {
        if self.items.is_empty() {
            return None;
        }
        let n = self.items.len() as i32;
        let Some(at) = from else {
            // No tab open: forward means the first, back means the last,
            // which is what a reader expects of both keys.
            return Some(if delta > 0 { 0 } else { (n - 1) as usize });
        };
        Some((at as i32 + delta).rem_euclid(n) as usize)
    }
}

// ------------------------------------------------------------ persistence

/// Where the pins of one clone live between runs. Under the git
/// directory, for the reasons the held comments are there: it is
/// per-clone state, it must never be committed by accident, and it
/// belongs to this checkout.
pub fn state_path(git_dir: &Path) -> Option<PathBuf> {
    let dir = git_dir.join("loupe");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("pins.json"))
}

/// Read the pins back. A pin whose file is gone is dropped rather than
/// drawn as a tab that fails on every click.
pub fn load(path: &Path) -> Vec<Pin> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut items: Vec<Pin> = serde_json::from_str(&text).unwrap_or_default();
    items.retain(|p| p.abs_path.is_file());
    items.truncate(MAX_PINS);
    items
}

/// Write the pins out. Called after every change to them, so quitting
/// never costs the reader their tabs.
pub fn save(path: &Path, items: &[Pin]) -> std::io::Result<()> {
    if items.is_empty() {
        // Nothing pinned: leave no file behind.
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => return Ok(()),
        }
    }
    let text = serde_json::to_string(items).unwrap_or_else(|_| "[]".into());
    std::fs::write(path, text)
}

// ---------------------------------------------------------- dropped paths

/// The paths named by text the terminal wrote when a file was dropped on
/// the window, or `None` when the text is not a drop.
///
/// Every terminal loupe runs in answers a drop the same way: it writes
/// the file's absolute path into the program as if it had been typed,
/// shell-escaped or quoted, and some write a `file://` URL instead. With
/// bracketed paste on, that arrives as one paste event rather than a
/// burst of key presses.
///
/// A paste is only read as a drop when *every* token in it is an
/// absolute path to a file that exists. That rule is what keeps ordinary
/// pasted text — a snippet of code, a URL, a sentence — out of the tab
/// row, and it costs nothing, because a drop is always absolute.
pub fn dropped_paths(text: &str) -> Option<Vec<PathBuf>> {
    let tokens = tokens(text.trim());
    if tokens.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for token in tokens {
        let raw = match token.strip_prefix("file://") {
            Some(rest) => {
                // `file://host/path`; the host is empty for a local file,
                // and `localhost` on the terminals that spell it out.
                let slash = rest.find('/')?;
                percent_decode(&rest[slash..])
            }
            None => token,
        };
        let path = PathBuf::from(raw);
        if !path.is_absolute() || !path.is_file() {
            return None;
        }
        out.push(path);
    }
    Some(out)
}

/// Split a command line the way a shell does: quotes hold a token
/// together, a backslash escapes the character after it, and unquoted
/// whitespace ends the token. A dropped path with a space in it arrives
/// in one of those three forms depending on the terminal.
///
/// On Windows a backslash is the path separator, not an escape — reading
/// `C:\Users\me\notes.md` as escapes yields `C:Usersmenotes.md`, which is
/// not absolute, so every drop was refused. Windows terminals quote a
/// path with spaces instead, which the quote handling below already
/// covers.
fn tokens(text: &str) -> Vec<String> {
    let escapes = !cfg!(windows);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            // A backslash is a literal inside single quotes, as in a shell.
            '\\' if escapes && quote != Some('\'') => escaped = true,
            '\'' | '"' => match quote {
                Some(q) if q == ch => quote = None,
                Some(_) => cur.push(ch),
                None => quote = Some(ch),
            },
            c if c.is_whitespace() && quote.is_none() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Turn `%20` and its kind back into bytes. Only `file://` URLs need
/// this, and only for the characters a path is allowed to hold.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Expand a leading `~` against the home directory, so a path typed into
/// the open-a-path box works the way it does in a shell.
pub fn expand_home(input: &str) -> PathBuf {
    let trimmed = input.trim();
    let Some(rest) = trimmed.strip_prefix('~') else {
        return PathBuf::from(trimmed);
    };
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return PathBuf::from(trimmed);
    };
    match rest.strip_prefix('/') {
        Some(tail) => home.join(tail),
        // A bare `~` is the home directory; `~other` is someone else's
        // home, which loupe does not try to resolve.
        None if rest.is_empty() => home,
        None => PathBuf::from(trimmed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A drop of one plain path, the common case on every terminal.
    #[test]
    fn a_plain_path_is_a_drop() {
        let dir = std::env::temp_dir().join("loupe-pins-plain");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("plan.md");
        std::fs::write(&file, "# hi").unwrap();
        let text = file.to_string_lossy().into_owned();
        assert_eq!(dropped_paths(&text), Some(vec![file.clone()]));
        // iTerm2 adds a trailing space after the path it writes.
        assert_eq!(dropped_paths(&format!("{text} ")), Some(vec![file]));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A path with a space arrives escaped, quoted, or as a URL. Every
    /// spelling the running platform can produce names the same file.
    #[test]
    fn spaces_survive_every_spelling() {
        let dir = std::env::temp_dir().join("loupe-pins-spaces");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("my notes.md");
        std::fs::write(&file, "# hi").unwrap();
        let raw = file.to_string_lossy().into_owned();
        let quoted = format!("'{raw}'");
        let mut spellings = vec![quoted];
        // Escaping and `file://` are POSIX spellings. On Windows the
        // backslash is the path separator, so an escaped path is not a
        // thing a terminal there can send — quoting is how it spells a
        // space, and reading `\` as an escape is what broke every drop.
        if !cfg!(windows) {
            spellings.push(raw.replace(' ', "\\ "));
            spellings.push(format!("file://{}", raw.replace(' ', "%20")));
        }
        for text in spellings {
            assert_eq!(dropped_paths(&text), Some(vec![file.clone()]), "{text}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Two files dropped together become two pins.
    #[test]
    fn several_files_drop_together() {
        let dir = std::env::temp_dir().join("loupe-pins-many");
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.md");
        let b = dir.join("b.md");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        let text = format!("{} {}", a.to_string_lossy(), b.to_string_lossy());
        assert_eq!(dropped_paths(&text), Some(vec![a, b]));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Ordinary pasted text is not a drop. This is the rule that lets the
    /// editor keep paste for pasting.
    #[test]
    fn ordinary_text_is_not_a_drop() {
        assert_eq!(dropped_paths("let x = 1;"), None);
        assert_eq!(dropped_paths(""), None);
        assert_eq!(dropped_paths("   "), None);
        assert_eq!(dropped_paths("https://example.com/a.md"), None);
        // Relative, so not a drop even if it happens to exist.
        assert_eq!(dropped_paths("src/app.rs"), None);
        // Absolute but not there.
        assert_eq!(dropped_paths("/definitely/not/here.md"), None);
    }

    /// A directory is not a document. Dropping one says so rather than
    /// opening an empty tab.
    #[test]
    fn a_directory_is_not_a_drop() {
        let dir = std::env::temp_dir().join("loupe-pins-dir");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(dropped_paths(&dir.to_string_lossy()), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// One dropped path that does not exist voids the whole drop, rather
    /// than pinning half of it.
    #[test]
    fn a_mixed_paste_is_not_a_drop() {
        let dir = std::env::temp_dir().join("loupe-pins-mixed");
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.md");
        std::fs::write(&a, "a").unwrap();
        let text = format!("{} /nope/b.md", a.to_string_lossy());
        assert_eq!(dropped_paths(&text), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A pin inside the repository is named relative to it; one outside
    /// keeps its whole path and is marked.
    #[test]
    fn pins_are_named_by_where_they_live() {
        let root = Path::new("/repo");
        let inside = Pin::new(root, PathBuf::from("/repo/docs/plan.md"));
        assert_eq!(inside.path, "docs/plan.md");
        assert!(!inside.outside);
        let outside = Pin::new(root, PathBuf::from("/home/me/Downloads/plan.md"));
        assert_eq!(outside.path, "/home/me/Downloads/plan.md");
        assert!(outside.outside);
    }

    /// Two tabs never read the same. The parent directory separates them.
    #[test]
    fn tabs_with_one_name_gain_their_directory() {
        let root = Path::new("/repo");
        let mut pins = Pins::default();
        pins.add(Pin::new(root, PathBuf::from("/repo/a/PLAN.md")))
            .unwrap();
        pins.add(Pin::new(root, PathBuf::from("/repo/b/PLAN.md")))
            .unwrap();
        pins.add(Pin::new(root, PathBuf::from("/repo/notes.md")))
            .unwrap();
        assert_eq!(pins.labels(), vec!["a/PLAN.md", "b/PLAN.md", "notes.md"]);
    }

    /// Pinning the same file twice opens the tab that already holds it.
    #[test]
    fn pinning_twice_is_one_tab() {
        let root = Path::new("/repo");
        let mut pins = Pins::default();
        let pin = Pin::new(root, PathBuf::from("/repo/a.md"));
        assert_eq!(pins.add(pin.clone()), Ok(0));
        assert_eq!(pins.add(pin), Ok(0));
        assert_eq!(pins.len(), 1);
    }

    /// Closing a tab takes that file out and leaves the rest in order.
    #[test]
    fn closing_a_tab_leaves_the_rest_alone() {
        let root = Path::new("/repo");
        let mut pins = Pins::default();
        for name in ["a.md", "b.md", "c.md"] {
            pins.add(Pin::new(root, PathBuf::from(format!("/repo/{name}"))))
                .unwrap();
        }
        assert_eq!(pins.remove(0).map(|p| p.path), Some("a.md".to_string()));
        assert_eq!(pins.len(), 2);
        assert_eq!(pins.items[0].path, "b.md");
        assert_eq!(pins.items[1].path, "c.md");
        assert!(pins.remove(9).is_none());
    }

    /// The tab keys wrap, and reach the ends when nothing is open.
    #[test]
    fn stepping_wraps_around_the_row() {
        let root = Path::new("/repo");
        let mut pins = Pins::default();
        for name in ["a.md", "b.md", "c.md"] {
            pins.add(Pin::new(root, PathBuf::from(format!("/repo/{name}"))))
                .unwrap();
        }
        assert_eq!(pins.step(1, None), Some(0));
        assert_eq!(pins.step(-1, None), Some(2));
        assert_eq!(pins.step(1, Some(2)), Some(0));
        assert_eq!(pins.step(-1, Some(2)), Some(1));
        assert_eq!(Pins::default().step(1, None), None);
    }

    /// The row is not a file panel: past its size, pinning says so.
    #[test]
    fn the_row_has_a_limit() {
        let root = Path::new("/repo");
        let mut pins = Pins::default();
        for i in 0..MAX_PINS {
            pins.add(Pin::new(root, PathBuf::from(format!("/repo/{i}.md"))))
                .unwrap();
        }
        let over = Pin::new(root, PathBuf::from("/repo/one-too-many.md"));
        assert_eq!(pins.add(over), Err(MAX_PINS));
    }

    /// The tabs come back next time, and a pin whose file has since been
    /// deleted quietly drops out rather than becoming a tab that fails on
    /// every click.
    #[test]
    fn pins_survive_a_restart() {
        let dir = std::env::temp_dir().join("loupe-pins-state");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let kept = dir.join("kept.md");
        let gone = dir.join("gone.md");
        std::fs::write(&kept, "# kept").unwrap();
        std::fs::write(&gone, "# gone").unwrap();
        let state = dir.join("pins.json");
        let root = Path::new("/nowhere");
        let items = vec![Pin::new(root, kept.clone()), Pin::new(root, gone.clone())];

        save(&state, &items).unwrap();
        assert_eq!(load(&state), items, "both come back");

        std::fs::remove_file(&gone).unwrap();
        let back = load(&state);
        assert_eq!(back.len(), 1, "the deleted one is dropped");
        assert_eq!(back[0].abs_path, items[0].abs_path);

        // Nothing pinned leaves no file behind.
        save(&state, &[]).unwrap();
        assert!(!state.exists());
        // …and saving an empty list twice is not an error.
        save(&state, &[]).unwrap();
        assert!(load(&state).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_leading_tilde_means_home() {
        std::env::set_var("HOME", "/home/me");
        assert_eq!(
            expand_home("~/docs/a.md"),
            PathBuf::from("/home/me/docs/a.md")
        );
        assert_eq!(expand_home("~"), PathBuf::from("/home/me"));
        assert_eq!(expand_home("/tmp/a.md"), PathBuf::from("/tmp/a.md"));
    }
}

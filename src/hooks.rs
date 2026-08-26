//! Install the `UserPromptSubmit` hook that hands an agent what the reader
//! is looking at.
//!
//! [`crate::ctx`] answers the question; this module wires up the asking.
//! Both Claude Code and Codex read their hooks from a JSON file of the same
//! shape, so one merge covers both. Codex needs one extra line of TOML,
//! because it runs no hook at all until the feature is on.
//!
//! Every write here lands in a file the reader owns and did not ask loupe
//! to manage. Three rules follow from that, and they are the reason this
//! module exists instead of a `println!` in the docs:
//!
//! 1. Merge, never replace. Other hooks in the file survive untouched.
//! 2. Keep a copy. The old file is saved beside the new one.
//! 3. Leave a marker, so a second run finds its own work instead of
//!    installing the hook twice.

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// Appended to the hook command so a later run can recognize its own work.
/// Both agents hand the command to a shell, where `#` starts a comment, so
/// the marker never reaches loupe.
pub const MARKER: &str = "# loupe-context-hook";

/// The event both agents fire when the reader submits a prompt.
const EVENT: &str = "UserPromptSubmit";

/// Seconds an agent waits for `loupe ctl context`. The command reads a unix
/// socket on the same machine, so this only ever catches a stall.
const TIMEOUT: u64 = 5;

/// Suffix for the copy taken before a write.
const BACKUP: &str = ".loupe.bak";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Claude,
    Codex,
}

/// An agent found on this machine.
#[derive(Clone, Debug)]
pub struct Agent {
    pub kind: Kind,
    pub name: &'static str,
    /// The agent's own directory, such as `~/.claude`.
    pub home: PathBuf,
}

impl Agent {
    /// The file that holds the agent's hooks.
    pub fn hooks_file(&self) -> PathBuf {
        match self.kind {
            // Claude Code keeps hooks among its other settings; Codex keeps
            // them in a file of their own.
            Kind::Claude => self.home.join("settings.json"),
            Kind::Codex => self.home.join("hooks.json"),
        }
    }

    /// True when loupe's hook is already in the file.
    pub fn installed(&self) -> bool {
        std::fs::read_to_string(self.hooks_file())
            .map(|text| installed_in(&text))
            .unwrap_or(false)
    }
}

/// Which agents this machine has. Absence of a directory is the whole test:
/// an agent that has never run has nothing for loupe to write into.
///
/// Always empty off unix. The hook asks loupe a question over a unix
/// socket, and [`crate::ctx`] cannot serve one on Windows, so a hook
/// installed there would call out to nothing.
pub fn detected() -> Vec<Agent> {
    if !cfg!(unix) {
        return Vec::new();
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    [
        (Kind::Claude, ".claude", "Claude Code"),
        (Kind::Codex, ".codex", "Codex"),
    ]
    .into_iter()
    .filter_map(|(kind, dir, name)| {
        let home = home.join(dir);
        home.is_dir().then_some(Agent { kind, name, home })
    })
    .collect()
}

/// Loupe's own path, so the hook keeps working when loupe is not on the
/// agent's `PATH` — an agent started from a desktop launcher often is not.
///
/// A path inside a Cargo build directory is refused. It works today and
/// stops working at the next `cargo clean` or branch switch, and it fails
/// the way hooks fail: quietly, in somebody else's process.
pub fn exe() -> Result<PathBuf> {
    let path = std::env::current_exe().context("cannot find loupe's own path")?;
    if from_build_dir(&path) {
        bail!(
            "this loupe runs from a build directory ({}), and a hook pointing there \
             would break at the next `cargo clean`.\n  Install it first, then retry: \
             cargo install --path .",
            path.display()
        );
    }
    Ok(path)
}

/// True when a path sits in a Cargo build directory.
fn from_build_dir(path: &Path) -> bool {
    let parts: Vec<_> = path.components().map(|c| c.as_os_str()).collect();
    parts
        .windows(2)
        .any(|w| w[0] == "target" && (w[1] == "debug" || w[1] == "release"))
}

/// The command line the agent runs.
///
/// The two agents take the answer differently. Claude Code reads a hook's
/// plain stdout as context. Codex ignores stdout and reads one JSON object,
/// so its hook asks for `--json`. Codex accepts nothing else: a hook that
/// prints the block on its own runs, reports success, and is thrown away.
pub fn command(exe: &Path, kind: Kind) -> String {
    match kind {
        Kind::Claude => format!("{} ctl context {MARKER}", exe.display()),
        Kind::Codex => format!("{} ctl context --json {MARKER}", exe.display()),
    }
}

// -------------------------------------------------------------- the merge

fn document(text: &str) -> Result<Map<String, Value>> {
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str(text).context("the file is not valid JSON — fix it and retry")? {
        Value::Object(map) => Ok(map),
        _ => bail!("the file is valid JSON but not an object"),
    }
}

fn render(doc: Map<String, Value>) -> Result<String> {
    let mut text = serde_json::to_string_pretty(&Value::Object(doc))?;
    text.push('\n');
    Ok(text)
}

/// True when this entry is loupe's.
fn ours(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains(MARKER))
            })
        })
}

/// True when loupe's hook is already in this JSON text.
pub fn installed_in(text: &str) -> bool {
    document(text)
        .ok()
        .and_then(|doc| {
            let list = doc.get("hooks")?.get(EVENT)?.as_array()?;
            Some(list.iter().any(ours))
        })
        .unwrap_or(false)
}

/// Add loupe's hook to an agent's JSON file, and leave every other hook as
/// it was. A hook already there is replaced, so a reader who moves the
/// loupe binary can re-run setup and get the new path.
pub fn install_json(text: &str, exe: &Path, kind: Kind) -> Result<String> {
    let mut doc = document(text)?;
    let hooks = doc
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("`hooks` is not an object")?;
    let list = hooks
        .entry(EVENT)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .with_context(|| format!("`hooks.{EVENT}` is not an array"))?;
    list.retain(|entry| !ours(entry));
    list.push(json!({
        "hooks": [{
            "type": "command",
            "command": command(exe, kind),
            "timeout": TIMEOUT,
        }]
    }));
    render(doc)
}

/// Take loupe's hook back out. Creates nothing: an agent with no hooks at
/// all comes back unchanged.
pub fn uninstall_json(text: &str) -> Result<String> {
    let mut doc = document(text)?;
    let Some(hooks) = doc.get_mut("hooks").and_then(Value::as_object_mut) else {
        return render(doc);
    };
    let Some(list) = hooks.get_mut(EVENT).and_then(Value::as_array_mut) else {
        return render(doc);
    };
    list.retain(|entry| !ours(entry));
    // An event left holding nothing is loupe's own leftover, so take the
    // key with it rather than leave an empty array behind.
    if list.is_empty() {
        hooks.remove(EVENT);
    }
    if hooks.is_empty() {
        doc.remove("hooks");
    }
    render(doc)
}

/// Codex runs no hook until the feature is on. Every comment and every
/// other key in the file survives.
pub fn enable_codex_hooks(text: &str) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .context("config.toml is not valid TOML — fix it and retry")?;
    let features = doc
        .entry("features")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    features
        .as_table_like_mut()
        .context("`features` in config.toml is not a table")?
        .insert("hooks", toml_edit::value(true));
    Ok(doc.to_string())
}

// -------------------------------------------------------------- the files

fn read_or_empty(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write `text` to `path`, after saving what was there. A reader who does
/// not like the result can put the old file back by hand.
fn write_with_backup(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    if path.is_file() {
        let mut name = path
            .file_name()
            .context("the hook file has no name")?
            .to_os_string();
        name.push(BACKUP);
        let backup = path.with_file_name(name);
        std::fs::copy(path, &backup)
            .with_context(|| format!("saving a copy as {}", backup.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// Apply a merge to one file, and skip the write when nothing changed.
fn edit(path: &Path, f: impl Fn(&str) -> Result<String>) -> Result<()> {
    let text = read_or_empty(path)?;
    let updated = f(&text).with_context(|| format!("in {}", path.display()))?;
    if updated == text {
        return Ok(());
    }
    write_with_backup(path, &updated)
}

/// Put the hook in place for one agent.
pub fn install(agent: &Agent, exe: &Path) -> Result<()> {
    edit(&agent.hooks_file(), |text| {
        install_json(text, exe, agent.kind)
    })?;
    if agent.kind == Kind::Codex {
        edit(&agent.home.join("config.toml"), enable_codex_hooks)?;
    }
    Ok(())
}

/// Take the hook back out for one agent.
///
/// Codex keeps `features.hooks = true`: other hooks may rely on it, and
/// turning it off would break them.
pub fn uninstall(agent: &Agent) -> Result<()> {
    edit(&agent.hooks_file(), uninstall_json)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe() -> PathBuf {
        PathBuf::from("/usr/local/bin/loupe")
    }

    #[test]
    fn a_path_inside_a_build_directory_is_refused() {
        assert!(from_build_dir(Path::new(
            "/home/me/src/loupe/target/debug/loupe"
        )));
        assert!(from_build_dir(Path::new(
            "/home/me/src/loupe/target/release/loupe"
        )));
        // The installed binary, and a directory that only looks like one.
        assert!(!from_build_dir(Path::new("/home/me/.cargo/bin/loupe")));
        assert!(!from_build_dir(Path::new("/usr/local/bin/loupe")));
        assert!(!from_build_dir(Path::new("/home/me/target/loupe")));
        assert!(!from_build_dir(Path::new("/home/me/debug/loupe")));
    }

    #[test]
    fn codex_is_told_to_answer_in_json_and_claude_is_not() {
        let codex = install_json("", &exe(), Kind::Codex).unwrap();
        let claude = install_json("", &exe(), Kind::Claude).unwrap();
        assert!(codex.contains("ctl context --json"));
        assert!(!claude.contains("--json"));
        // Both are still loupe's, so either can be replaced or removed.
        assert!(installed_in(&codex) && installed_in(&claude));
    }

    #[test]
    fn an_empty_file_becomes_a_whole_hook_file() {
        let out = install_json("", &exe(), Kind::Claude).unwrap();
        assert!(installed_in(&out));
        assert!(out.contains("/usr/local/bin/loupe ctl context"));
        assert!(out.contains(MARKER));
    }

    #[test]
    fn a_hook_that_is_already_there_is_not_added_twice() {
        let once = install_json("", &exe(), Kind::Claude).unwrap();
        let twice = install_json(&once, &exe(), Kind::Claude).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn a_moved_binary_is_picked_up_on_the_next_run() {
        let old = install_json("", &PathBuf::from("/old/loupe"), Kind::Claude).unwrap();
        let new = install_json(&old, &exe(), Kind::Claude).unwrap();
        assert!(!new.contains("/old/loupe"));
        assert!(new.contains("/usr/local/bin/loupe"));
    }

    #[test]
    fn every_other_hook_survives_the_merge() {
        let before = r#"{
  "model": "opus",
  "hooks": {
    "Stop": [{"hooks": [{"type": "command", "command": "say done"}]}],
    "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "notify.sh"}]}]
  }
}"#;
        let after = install_json(before, &exe(), Kind::Claude).unwrap();
        assert!(after.contains("say done"));
        assert!(after.contains("notify.sh"));
        assert!(after.contains("\"model\": \"opus\""));
        assert!(installed_in(&after));
    }

    #[test]
    fn the_key_order_of_the_readers_file_is_kept() {
        // Alphabetical order would rewrite the whole file and bury loupe's
        // one line in a diff nobody asked for.
        let before = r#"{"zeta": 1, "alpha": 2, "model": "opus"}"#;
        let after = install_json(before, &exe(), Kind::Claude).unwrap();
        let zeta = after.find("zeta").unwrap();
        let alpha = after.find("alpha").unwrap();
        assert!(zeta < alpha, "keys were reordered:\n{after}");
    }

    #[test]
    fn uninstall_leaves_the_other_hooks_alone() {
        let before = r#"{
  "hooks": {
    "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "notify.sh"}]}]
  }
}"#;
        let with = install_json(before, &exe(), Kind::Claude).unwrap();
        let without = uninstall_json(&with).unwrap();
        assert!(!installed_in(&without));
        assert!(without.contains("notify.sh"));
    }

    #[test]
    fn uninstall_clears_up_after_itself() {
        let with = install_json("", &exe(), Kind::Claude).unwrap();
        let without = uninstall_json(&with).unwrap();
        assert_eq!(without.trim(), "{}");
    }

    #[test]
    fn uninstall_creates_nothing_when_there_is_nothing_to_remove() {
        let before = r#"{"model": "opus"}"#;
        let after = uninstall_json(before).unwrap();
        assert!(!after.contains("hooks"));
    }

    #[test]
    fn a_broken_file_is_reported_and_not_overwritten() {
        let err = install_json("{ not json", &exe(), Kind::Claude).unwrap_err();
        assert!(err.to_string().contains("valid JSON"));
    }

    #[test]
    fn codex_keeps_its_comments_when_the_feature_goes_on() {
        let before = "# my settings\nmodel = \"gpt\"\n\n[features]\njs_repl = false\n";
        let after = enable_codex_hooks(before).unwrap();
        assert!(after.contains("# my settings"));
        assert!(after.contains("js_repl = false"));
        assert!(after.contains("hooks = true"));
    }

    #[test]
    fn codex_gets_a_features_table_when_it_has_none() {
        let after = enable_codex_hooks("model = \"gpt\"\n").unwrap();
        assert!(after.contains("[features]"));
        assert!(after.contains("hooks = true"));
        assert!(enable_codex_hooks(&after).unwrap() == after);
    }
}

//! User configuration.
//!
//! Two TOML files, both optional:
//!
//! * global — `$LOUPE_CONFIG` if set, else `$XDG_CONFIG_HOME/loupe/config.toml`,
//!   else `~/.config/loupe/config.toml`
//! * per-repo — `.loupe.toml` at the repository root; its values win over the
//!   global file (handy for pinning a different upstream org per project)
//!
//! Unknown keys and malformed values are hard errors (reported before the TUI
//! starts) rather than silently ignored — a typo'd config that "works" is
//! worse than one that complains.

use crate::app::LaunchMode;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Upstream GitHub organization (or user) that pull requests are opened
    /// against. When set, loupe lists and opens PRs on `<org>/<repo-name>`
    /// instead of the clone's own owner — the fork/multi-org workflow.
    pub org: Option<String>,
    /// Startup mode. Command-line flags (`--pr` / `--local`) still win.
    pub mode: Option<ConfigMode>,
    /// Syntax-highlighting theme, by name (e.g. "one-half-dark").
    /// `loupe --themes` lists every valid name.
    pub theme: Option<String>,
    /// Starting width of the file panel, in columns. Clamped to what the
    /// terminal can actually give; the divider can still be dragged.
    pub file_panel_width: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigMode {
    Auto,
    Pr,
    Local,
}

impl From<ConfigMode> for LaunchMode {
    fn from(m: ConfigMode) -> Self {
        match m {
            ConfigMode::Auto => LaunchMode::Auto,
            ConfigMode::Pr => LaunchMode::Pr,
            ConfigMode::Local => LaunchMode::Local,
        }
    }
}

impl Config {
    /// Overlay `over` on top of `self`: any key `over` sets replaces ours.
    fn merged(self, over: Config) -> Config {
        Config {
            org: over.org.or(self.org),
            mode: over.mode.or(self.mode),
            theme: over.theme.or(self.theme),
            file_panel_width: over.file_panel_width.or(self.file_panel_width),
        }
    }
}

fn parse(text: &str) -> Result<Config> {
    toml::from_str(text).map_err(anyhow::Error::from)
}

/// Where the global config file lives (whether or not it exists).
pub fn global_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LOUPE_CONFIG") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("loupe").join("config.toml"))
}

/// Load global config, then overlay the repo-local `.loupe.toml` (if inside
/// a repository). Missing files are fine; unreadable or invalid ones error.
pub fn load(repo_root: Option<&Path>) -> Result<Config> {
    let mut cfg = Config::default();
    if let Some(path) = global_path() {
        if path.is_file() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            cfg = parse(&text).with_context(|| format!("in {}", path.display()))?;
        }
    }
    if let Some(root) = repo_root {
        let path = root.join(".loupe.toml");
        if path.is_file() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let local = parse(&text).with_context(|| format!("in {}", path.display()))?;
            cfg = cfg.merged(local);
        }
    }
    Ok(cfg)
}

// ------------------------------------------------------------------ writing

/// Set string keys in a TOML document, preserving every comment and all
/// formatting the user already has. Pure so it's easy to test.
fn upsert(text: &str, pairs: &[(&str, &str)]) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .context("existing config is not valid TOML — fix it (or delete it) and retry")?;
    for (key, value) in pairs {
        doc[key] = toml_edit::value(*value);
    }
    Ok(doc.to_string())
}

/// Write string keys into the global config file, creating it (and its
/// directory) if needed and preserving the rest of the file. Returns the
/// path written. Values here are always validated by the caller first.
pub fn save_global(pairs: &[(&str, &str)]) -> Result<PathBuf> {
    let path = global_path().context("no config location — neither $LOUPE_CONFIG nor $HOME set")?;
    let existing = if path.is_file() {
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new()
    };
    let updated = upsert(&existing, pairs).with_context(|| format!("in {}", path.display()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&path, updated).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Create the global config file with a commented header if it does not
/// exist yet — the marker that first-run setup already happened (even when
/// the user skipped it). Never touches an existing file.
pub fn ensure_global_exists() -> Result<PathBuf> {
    let path = global_path().context("no config location — neither $LOUPE_CONFIG nor $HOME set")?;
    if !path.is_file() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(
            &path,
            "# loupe configuration — every key is optional; `loupe --help` documents them.\n\
             # theme = \"catppuccin-mocha\"   (`loupe --themes` lists all; press t in loupe)\n\
             # mode = \"auto\"                (\"auto\" | \"pr\" | \"local\")\n",
        )
        .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_keys() {
        let cfg = parse("org = \"acme\"\nmode = \"pr\"\ntheme = \"nord\"\nfile_panel_width = 40\n")
            .unwrap();
        assert_eq!(
            cfg,
            Config {
                org: Some("acme".into()),
                mode: Some(ConfigMode::Pr),
                theme: Some("nord".into()),
                file_panel_width: Some(40),
            }
        );
    }

    #[test]
    fn empty_and_partial_files_are_fine() {
        assert_eq!(parse("").unwrap(), Config::default());
        let cfg = parse("mode = \"local\"\n").unwrap();
        assert_eq!(cfg.mode, Some(ConfigMode::Local));
        assert_eq!(cfg.org, None);
    }

    #[test]
    fn unknown_keys_and_bad_values_error() {
        assert!(parse("orgg = \"typo\"\n").is_err());
        assert!(parse("mode = \"both\"\n").is_err());
        assert!(parse("mode = 3\n").is_err());
        assert!(parse("file_panel_width = \"wide\"\n").is_err());
    }

    #[test]
    fn repo_local_overrides_global() {
        let global = parse("org = \"acme\"\nmode = \"pr\"\ntheme = \"nord\"\n").unwrap();
        let local = parse("org = \"other-co\"\nfile_panel_width = 28\n").unwrap();
        let merged = global.merged(local);
        assert_eq!(merged.org.as_deref(), Some("other-co"));
        assert_eq!(merged.file_panel_width, Some(28));
        // Keys the local file doesn't set fall through to the global ones.
        assert_eq!(merged.mode, Some(ConfigMode::Pr));
        assert_eq!(merged.theme.as_deref(), Some("nord"));
    }

    #[test]
    fn upsert_preserves_comments_and_other_keys() {
        let existing = "# my config\norg = \"acme\"  # upstream\n\n# panel\nfile_panel_width = 40\ntheme = \"nord\"\n";
        let out = upsert(existing, &[("theme", "catppuccin-mocha")]).unwrap();
        assert!(out.contains("# my config"), "{out}");
        assert!(out.contains("org = \"acme\"  # upstream"), "{out}");
        assert!(out.contains("file_panel_width = 40"), "{out}");
        assert!(out.contains("theme = \"catppuccin-mocha\""), "{out}");
        assert!(!out.contains("\"nord\""), "{out}");
        // The result must still parse as a valid loupe config.
        assert_eq!(
            parse(&out).unwrap().theme.as_deref(),
            Some("catppuccin-mocha")
        );
    }

    #[test]
    fn upsert_adds_missing_keys_and_rejects_broken_files() {
        let out = upsert("", &[("theme", "dracula"), ("mode", "local")]).unwrap();
        let cfg = parse(&out).unwrap();
        assert_eq!(cfg.theme.as_deref(), Some("dracula"));
        assert_eq!(cfg.mode, Some(ConfigMode::Local));
        assert!(upsert("theme = ", &[("theme", "nord")]).is_err());
    }

    #[test]
    fn mode_maps_to_launch_mode() {
        assert_eq!(LaunchMode::from(ConfigMode::Auto), LaunchMode::Auto);
        assert_eq!(LaunchMode::from(ConfigMode::Pr), LaunchMode::Pr);
        assert_eq!(LaunchMode::from(ConfigMode::Local), LaunchMode::Local);
    }
}

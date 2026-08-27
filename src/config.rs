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
use crate::theme::Appearance;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// What to call it in messages to the reader.
    pub lang: String,
    /// File extensions it handles, without the dot.
    pub extensions: Vec<String>,
    /// The program to run.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// What to tell somebody who does not have it.
    #[serde(default)]
    pub install: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Upstream GitHub organization (or user) that pull requests are opened
    /// against. When set, loupe lists and opens PRs on `<org>/<repo-name>`
    /// instead of the clone's own owner — the fork/multi-org workflow.
    pub org: Option<String>,
    /// Startup mode. Command-line flags (`--pr` / `--local`) still win.
    pub mode: Option<ConfigMode>,
    /// Syntax-highlighting theme for a dark terminal, by name (e.g.
    /// "one-half-dark"). `loupe --themes` lists every valid name.
    pub theme: Option<String>,
    /// The same, for a light terminal. Unset means "the light counterpart
    /// of `theme`" — so Gruvbox stays Gruvbox either way.
    pub light_theme: Option<String>,
    /// Light or dark colors. Unset (or "auto") asks the terminal.
    pub appearance: Option<ConfigAppearance>,
    /// Starting width of the file panel, in columns. Clamped to what the
    /// terminal can actually give; the divider can still be dragged.
    pub file_panel_width: Option<u16>,
    /// Whether loupe may start language servers it finds on PATH, for
    /// go-to-definition, find-references and hover. Default true; nothing
    /// is ever installed, and a missing server just falls back to pattern
    /// matching. `loupe --lsp` reports what was found.
    pub language_servers: Option<bool>,
    /// Run the language server's formatter (prettier, gofmt, rustfmt —
    /// whatever it drives) every time the editor saves. Off by default:
    /// reformatting a file mid-review would add changes to the diff that
    /// nobody asked for. `Ctrl+T` (or the ⇥ Format button) formats on
    /// demand either way.
    pub format_on_save: Option<bool>,
    /// Re-scan the working tree while local review sits idle, so edits
    /// made by an agent (or a second terminal) show up without a key
    /// press. Default true. It only ever runs after input stops, never
    /// with the editor or an overlay open, and never for a pull request —
    /// that side is refreshed on demand with `r` or the ⟳ button.
    pub auto_refresh: Option<bool>,
    /// Show the blame pane between the file panel and the diff from the
    /// start. Default false: it costs a `git blame` per file and about
    /// 30 columns of width, and not every review wants it. `B` (or the
    /// ☰ menu) turns it on for one session either way.
    pub blame: Option<bool>,
    /// Starting width of the blame pane, in columns. Clamped to what the
    /// terminal can give; the second divider can still be dragged.
    pub blame_width: Option<u16>,
    /// Ask GitHub which pull request a blamed commit belongs to, for the
    /// commits whose subject does not already say. Default true. One
    /// batched `gh` call per file, cached for the session; set false to
    /// stay entirely offline and rely on the subject alone.
    pub blame_pr_lookup: Option<bool>,
    /// Extra language servers, as `[[server]]` tables. Each one needs a
    /// `lang`, the `extensions` it handles and the `command` to run;
    /// `args` and `install` are optional. An extension a built-in server
    /// already claims goes to the one configured here.
    #[serde(default, rename = "server")]
    pub servers: Vec<ServerConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigMode {
    Auto,
    Pr,
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigAppearance {
    /// Ask the terminal what its background is (the default).
    Auto,
    Light,
    Dark,
}

impl ConfigAppearance {
    /// `None` for "auto" — the caller then runs detection.
    pub fn resolved(self) -> Option<Appearance> {
        match self {
            ConfigAppearance::Auto => None,
            ConfigAppearance::Light => Some(Appearance::Light),
            ConfigAppearance::Dark => Some(Appearance::Dark),
        }
    }
}

/// Which config key holds the theme for an appearance. The two slots are
/// independent so moving between a light and a dark terminal doesn't
/// overwrite the choice made on the other one.
pub fn theme_key_for(appearance: Appearance) -> &'static str {
    match appearance {
        Appearance::Dark => "theme",
        Appearance::Light => "light_theme",
    }
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
            light_theme: over.light_theme.or(self.light_theme),
            appearance: over.appearance.or(self.appearance),
            file_panel_width: over.file_panel_width.or(self.file_panel_width),
            language_servers: over.language_servers.or(self.language_servers),
            format_on_save: over.format_on_save.or(self.format_on_save),
            auto_refresh: over.auto_refresh.or(self.auto_refresh),
            blame: over.blame.or(self.blame),
            blame_width: over.blame_width.or(self.blame_width),
            blame_pr_lookup: over.blame_pr_lookup.or(self.blame_pr_lookup),
            // The nearer file replaces the list rather than adding to it.
            // Two files each naming a Python server would otherwise start
            // whichever one `spec_for` happened to reach first.
            servers: if over.servers.is_empty() {
                self.servers
            } else {
                over.servers
            },
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

    /// A `[[server]]` table adds a language, and the nearer config file
    /// replaces the list rather than adding to it — two files each naming
    /// a Python server would otherwise start whichever one `spec_for`
    /// happened to reach first.
    #[test]
    fn a_config_file_can_add_a_language_server() {
        let global: Config = parse(concat!(
            "[[server]]\n",
            "lang = \"Python\"\n",
            "extensions = [\"py\"]\n",
            "command = \"pyright-langserver\"\n",
            "args = [\"--stdio\"]\n",
            "install = \"npm install -g pyright\"\n",
        ))
        .unwrap();
        assert_eq!(global.servers.len(), 1);
        assert_eq!(global.servers[0].lang, "Python");
        assert_eq!(global.servers[0].args, vec!["--stdio"]);

        // `args` and `install` are optional; a server that takes neither
        // should not have to say so.
        let bare: Config = parse(concat!(
            "[[server]]\n",
            "lang = \"Ruby\"\n",
            "extensions = [\"rb\"]\n",
            "command = \"ruby-lsp\"\n",
        ))
        .unwrap();
        assert!(bare.servers[0].args.is_empty());

        let merged = global.merged(bare);
        assert_eq!(merged.servers.len(), 1, "the nearer file replaces the list");
        assert_eq!(merged.servers[0].lang, "Ruby");
    }

    #[test]
    fn parses_all_keys() {
        let cfg = parse(concat!(
            "org = \"acme\"\n",
            "mode = \"pr\"\n",
            "theme = \"nord\"\n",
            "light_theme = \"github\"\n",
            "appearance = \"light\"\n",
            "file_panel_width = 40\n",
            "language_servers = false\n",
            "format_on_save = true\n",
            "auto_refresh = false\n",
            "blame = true\n",
            "blame_width = 26\n",
            "blame_pr_lookup = false\n",
        ))
        .unwrap();
        assert_eq!(
            cfg,
            Config {
                org: Some("acme".into()),
                mode: Some(ConfigMode::Pr),
                theme: Some("nord".into()),
                light_theme: Some("github".into()),
                appearance: Some(ConfigAppearance::Light),
                file_panel_width: Some(40),
                language_servers: Some(false),
                format_on_save: Some(true),
                auto_refresh: Some(false),
                blame: Some(true),
                blame_width: Some(26),
                blame_pr_lookup: Some(false),
                servers: Vec::new(),
            }
        );
    }

    #[test]
    fn appearance_values() {
        assert_eq!(
            parse("appearance = \"dark\"\n").unwrap().appearance,
            Some(ConfigAppearance::Dark)
        );
        // "auto" parses, and means "ask the terminal".
        let auto = parse("appearance = \"auto\"\n").unwrap().appearance;
        assert_eq!(auto, Some(ConfigAppearance::Auto));
        assert_eq!(auto.unwrap().resolved(), None);
        assert_eq!(ConfigAppearance::Light.resolved(), Some(Appearance::Light));
        assert!(parse("appearance = \"beige\"\n").is_err());
        assert_eq!(theme_key_for(Appearance::Dark), "theme");
        assert_eq!(theme_key_for(Appearance::Light), "light_theme");
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

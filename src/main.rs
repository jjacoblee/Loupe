mod app;
mod clipboard;
mod config;
mod diff;
mod editor;
mod github;
mod gitops;
mod highlight;
mod lsp;
mod search;
mod theme;
mod ui;
mod wizard;

use anyhow::Result;
use app::{App, LaunchMode};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use crossterm::execute;
use std::io::stdout;
use std::time::Duration;
use theme::Appearance;

const USAGE: &str = "loupe — a mouse-first TUI for reviewing GitHub pull requests and local changes

USAGE: loupe [--pr | --local | --auto] [--theme <name>] [--light | --dark]
       loupe set-theme [--light] <name>
       loupe setup

  (no flag)  use the configured default mode (see CONFIG below), or auto:
             review uncommitted local changes if there are any,
             otherwise fall through to the pull-request flow
  --pr       skip the local scan and go straight to pull requests
  --local    review local changes only (even when the tree is clean)
  --auto     the auto behavior above (overrides a configured default)
  --theme <name>
             use this syntax theme for the session, without saving it
  --light    force light colors for this session
  --dark     force dark colors (the default is to ask the terminal)
  --themes   list syntax-theme names
  --lsp      report which language servers loupe can find
  --help     show this help

  set-theme [--light] <name>
                     save <name> as your theme (in the global config);
                     --light saves it as the light-terminal theme
  appearance         report what your terminal says its background is
                     (run this if loupe guessed light/dark wrong)
  setup              re-run the first-launch setup wizard

Inside loupe, press t (or click 🎨 Theme) for the theme picker — it
previews live and saves your choice. The wizard runs on first launch.

CONFIG (TOML): ~/.config/loupe/config.toml (or $LOUPE_CONFIG), plus a
per-repository .loupe.toml at the repo root whose values win. All keys
are optional:

  org   = \"acme\"           # upstream org PRs are opened against — PRs
                           # load from acme/<repo-name> instead of the
                           # clone's own owner (fork / multi-org setups)
  mode  = \"auto\"           # default mode: \"auto\" | \"pr\" | \"local\"
  appearance = \"auto\"      # \"auto\" | \"light\" | \"dark\" — auto asks the
                           # terminal for its background color and tunes
                           # the diff colors to match
  theme = \"catppuccin-mocha\"      # syntax theme on a dark terminal
  light_theme = \"catppuccin-latte\" # …and on a light one (defaults to the
                           # light counterpart of `theme`)
  file_panel_width = 34    # starting width of the file panel, in columns
                           # (drag the divider, or < / >, to change it)";

enum CliCmd {
    /// Start the TUI. `mode`/`theme` override the configured defaults for
    /// this session; `setup` forces the setup wizard to run first.
    Run {
        mode: Option<LaunchMode>,
        theme: Option<String>,
        appearance: Option<Appearance>,
        setup: bool,
    },
    Help,
    Themes,
    /// Report which language servers are installed.
    Lsp,
    /// Save a theme into one of the two slots (`light` picks which).
    SetTheme {
        name: String,
        light: bool,
    },
    /// Report what the terminal says its background is.
    Appearance,
}

/// Parse argv. Err carries the offending argument (or a message).
fn parse_cli<I: Iterator<Item = String>>(args: I) -> Result<CliCmd, String> {
    let mut args = args;
    let mut mode = None;
    let mut theme = None;
    let mut appearance = None;
    let mut setup = false;
    let mut set_theme = false;
    let mut set_theme_name: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--pr" | "-p" | "pr" => mode = Some(LaunchMode::Pr),
            "--local" | "-l" | "local" => mode = Some(LaunchMode::Local),
            "--auto" | "auto" => mode = Some(LaunchMode::Auto),
            "--theme" => match args.next() {
                Some(name) => theme = Some(name),
                None => return Err("--theme needs a theme name".to_string()),
            },
            "--light" => appearance = Some(Appearance::Light),
            "--dark" => appearance = Some(Appearance::Dark),
            "setup" => setup = true,
            // `--light` may come before or after the name, so the rest of
            // the arguments are parsed normally and resolved at the end.
            "set-theme" => set_theme = true,
            "--themes" => return Ok(CliCmd::Themes),
            "--lsp" => return Ok(CliCmd::Lsp),
            "appearance" => return Ok(CliCmd::Appearance),
            "--help" | "-h" => return Ok(CliCmd::Help),
            other => match other.strip_prefix("--theme=") {
                Some(name) if !name.is_empty() => theme = Some(name.to_string()),
                _ if set_theme && set_theme_name.is_none() && !other.starts_with('-') => {
                    set_theme_name = Some(other.to_string());
                }
                _ => return Err(other.to_string()),
            },
        }
    }
    if set_theme {
        return match set_theme_name {
            Some(name) => Ok(CliCmd::SetTheme {
                name,
                light: appearance == Some(Appearance::Light),
            }),
            None => Err("set-theme needs a theme name".to_string()),
        };
    }
    Ok(CliCmd::Run {
        mode,
        theme,
        appearance,
        setup,
    })
}

/// `loupe appearance`: say what the terminal reports and what loupe would
/// do with it. Detection needs raw mode, but nothing else here does, so it
/// is turned on for the query alone.
fn report_appearance() {
    let detected = match crossterm::terminal::enable_raw_mode() {
        Ok(()) => {
            let d = theme::detect_detailed();
            let _ = crossterm::terminal::disable_raw_mode();
            d
        }
        Err(_) => None,
    };
    match detected {
        Some(theme::Detected::Terminal(r, g, b)) => println!(
            "terminal background: rgb({r}, {g}, {b}) — that reads as {}",
            theme::Appearance::of_background(r, g, b).key()
        ),
        Some(theme::Detected::ColorFgBg(a)) => println!(
            "terminal background: no answer to the OSC 11 query, but \
             COLORFGBG={} says {}",
            std::env::var("COLORFGBG").unwrap_or_default(),
            a.key()
        ),
        None => println!(
            "terminal background: unknown — this terminal answers neither the \
             OSC 11 query nor COLORFGBG"
        ),
    }
    let appearance = detected.map(|d| d.appearance());
    match appearance {
        Some(a) => println!("loupe would use: {} colors", a.key()),
        None => println!(
            "loupe would use: dark colors — set `appearance` in your config, \
             or pass --light / --dark, if that is wrong"
        ),
    }
}

/// Resolve a theme name or exit with the standard hint.
fn theme_or_exit(name: &str) -> two_face::theme::EmbeddedThemeName {
    match highlight::theme_by_name(name) {
        Some(theme) => theme,
        None => {
            eprintln!("loupe: unknown theme “{name}” — run `loupe --themes` for the list");
            std::process::exit(2);
        }
    }
}

/// `loupe --lsp`: what is installed, what isn't, and what to run to fix
/// that. Loupe never installs anything itself, so this is the whole
/// story of why `gd` did or didn't work.
fn report_language_servers() {
    println!("Language servers loupe can drive (it starts what you already have):\n");
    let mut missing = Vec::new();
    for (spec, found) in lsp::doctor() {
        let exts: Vec<String> = spec.exts.iter().map(|e| format!(".{e}")).collect();
        match found {
            Some(path) => println!(
                "  ✓ {:<12} {}\n      {}",
                spec.lang,
                path.display(),
                exts.join(" ")
            ),
            None => {
                println!(
                    "  ✗ {:<12} {} not found on PATH\n      {}",
                    spec.lang,
                    spec.cmd,
                    exts.join(" ")
                );
                missing.push(spec);
            }
        }
    }
    if !missing.is_empty() {
        println!("\nTo add the missing ones:");
        for spec in &missing {
            println!("  {:<12} {}", spec.lang, spec.install);
        }
    }
    println!(
        "\nInside loupe: gd goes to a definition, gr lists references, K shows a type.\n\
         Anything else falls back to pattern matching, which still finds most definitions.\n\
         Set `language_servers = false` in your config to turn this off entirely."
    );
}

/// Everything resolved before the terminal is touched. The appearance and
/// the theme cannot be settled here: deciding those means asking the
/// terminal for its background color, which needs raw mode.
struct Startup {
    mode: LaunchMode,
    org: Option<String>,
    file_panel_width: Option<u16>,
    /// `language_servers` — off means loupe never starts one.
    language_servers: bool,
    /// `format_on_save` — run the server's formatter on every save.
    format_on_save: bool,
    wizard: bool,
    /// Forced by `--light`/`--dark` or the `appearance` config key; `None`
    /// means "ask the terminal".
    appearance: Option<Appearance>,
    /// The configured theme for each appearance, already validated.
    dark_theme: Option<two_face::theme::EmbeddedThemeName>,
    light_theme: Option<two_face::theme::EmbeddedThemeName>,
    /// `--theme`: overrides whichever slot the appearance selects, for this
    /// session only.
    session_theme: Option<two_face::theme::EmbeddedThemeName>,
}

impl Startup {
    /// The syntax theme to highlight with, given the resolved appearance.
    ///
    /// The two config slots are independent, so the usual case is a plain
    /// lookup. Only when the slot for this appearance is empty does the
    /// other one get adapted — Gruvbox Dark becomes Gruvbox Light rather
    /// than something unrelated.
    fn theme(&self, appearance: Appearance) -> two_face::theme::EmbeddedThemeName {
        if let Some(theme) = self.session_theme {
            return theme;
        }
        let (own, other) = match appearance {
            Appearance::Dark => (self.dark_theme, self.light_theme),
            Appearance::Light => (self.light_theme, self.dark_theme),
        };
        own.unwrap_or_else(|| match other {
            Some(theme) => highlight::for_appearance(theme, appearance),
            None => highlight::default_theme(appearance),
        })
    }

    /// Light or dark, in precedence order: an explicit setting, then the
    /// terminal's own background, then the background whatever theme was
    /// asked for was designed for, and finally dark.
    fn appearance(&self) -> Appearance {
        self.appearance_from(theme::detect())
    }

    /// [`Startup::appearance`] with detection already done — pure, so the
    /// precedence can be tested without a terminal to interrogate.
    fn appearance_from(&self, detected: Option<Appearance>) -> Appearance {
        self.appearance
            .or(detected)
            .or_else(|| self.theme_appearance())
            .unwrap_or(Appearance::Dark)
    }

    /// The background the configured theme was built for, when that is a
    /// signal at all.
    ///
    /// It is the last thing left when a terminal answers neither the OSC 11
    /// query nor `COLORFGBG` (plain `screen`, some CI runners), and it
    /// matters most on upgrade: someone who set `theme = "github"` back
    /// when there was only one slot would otherwise get Github's dark
    /// foregrounds on near-black diff tints — the very pairing this is all
    /// meant to prevent. Two configured slots say nothing, since that is
    /// someone who uses both kinds of terminal.
    fn theme_appearance(&self) -> Option<Appearance> {
        let theme = match (self.session_theme, self.dark_theme, self.light_theme) {
            (Some(t), _, _) => t,
            (None, Some(t), None) => t,
            (None, None, Some(t)) => t,
            _ => return None,
        };
        Some(if highlight::theme_is_light(theme) {
            Appearance::Light
        } else {
            Appearance::Dark
        })
    }
}

fn main() -> Result<()> {
    let (cli_mode, cli_theme, cli_appearance, force_setup) = match parse_cli(
        std::env::args().skip(1),
    ) {
        Ok(CliCmd::Run {
            mode,
            theme,
            appearance,
            setup,
        }) => (mode, theme, appearance, setup),
        Ok(CliCmd::Help) => {
            println!("{USAGE}");
            return Ok(());
        }
        Ok(CliCmd::Themes) => {
            for (name, _) in highlight::THEMES {
                println!("{name}");
            }
            return Ok(());
        }
        Ok(CliCmd::Lsp) => {
            report_language_servers();
            return Ok(());
        }
        Ok(CliCmd::Appearance) => {
            report_appearance();
            return Ok(());
        }
        Ok(CliCmd::SetTheme { name, light }) => {
            let key = highlight::theme_key(theme_or_exit(&name));
            let slot = config::theme_key_for(if light {
                Appearance::Light
            } else {
                Appearance::Dark
            });
            let which = if light { "light" } else { "dark" };
            // Saving a dark theme as the light-terminal one is legal
            // but almost always a slip — say so, rather than letting it
            // surface as unreadable code on the next light terminal.
            if highlight::theme_is_light(theme_or_exit(&name)) != light {
                eprintln!(
                        "loupe: warning — this theme is designed for a {} background, but you are saving it as your {which}-terminal theme",
                        if light { "dark" } else { "light" }
                    );
            }
            let path = config::save_global(&[(slot, key)])?;
            println!(
                "loupe: theme “{key}” saved as your {which}-terminal theme in {}",
                path.display()
            );
            return Ok(());
        }
        Err(bad) => {
            eprintln!("loupe: unknown argument “{bad}”\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    // Config problems are reported before the terminal goes raw — a typo'd
    // file should fail loudly, not be silently ignored.
    let cfg = match config::load(gitops::repo_root().as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("loupe: config error: {e:#}");
            std::process::exit(2);
        }
    };
    // Theme names are validated here, before the terminal goes raw, so a
    // typo is a plain error message rather than a flash of alternate screen.
    let startup = Startup {
        // Precedence: command line > config file > built-in auto.
        mode: cli_mode
            .or(cfg.mode.map(LaunchMode::from))
            .unwrap_or(LaunchMode::Auto),
        org: cfg.org,
        file_panel_width: cfg.file_panel_width,
        language_servers: cfg.language_servers.unwrap_or(true),
        format_on_save: cfg.format_on_save.unwrap_or(false),
        // First launch (no global config yet) — or an explicit `loupe setup`
        // — runs the wizard before anything else.
        wizard: force_setup || !config::global_path().is_some_and(|p| p.is_file()),
        appearance: cli_appearance.or_else(|| cfg.appearance.and_then(|a| a.resolved())),
        dark_theme: cfg.theme.as_deref().map(theme_or_exit),
        light_theme: cfg.light_theme.as_deref().map(theme_or_exit),
        session_theme: cli_theme.as_deref().map(theme_or_exit),
    };
    run_tui(startup)
}

fn run_tui(startup: Startup) -> Result<()> {
    // ratatui::init installs a panic hook that restores the terminal; we add
    // mouse capture on top and make sure it is torn down too.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        default_hook(info);
    }));

    let mut terminal = ratatui::init();
    // Raw mode is on (ratatui::init), and nothing is reading events yet —
    // the only window in which the terminal can be asked about its
    // background without the reply being mistaken for a keystroke.
    let appearance = startup.appearance();
    theme::set_appearance(appearance);
    highlight::set_theme(startup.theme(appearance));
    execute!(stdout(), EnableMouseCapture)?;

    let result = (|| {
        if startup.wizard {
            match wizard::run(&mut terminal)? {
                wizard::WizardEnd::Quit => return Ok(()),
                wizard::WizardEnd::Done | wizard::WizardEnd::Skipped => {}
            }
        }
        run(
            &mut terminal,
            startup.mode,
            startup.org,
            startup.file_panel_width,
            startup.language_servers,
            startup.format_on_save,
        )
    })();

    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    mode: LaunchMode,
    org: Option<String>,
    file_panel_width: Option<u16>,
    language_servers: bool,
    format_on_save: bool,
) -> Result<()> {
    let mut app = App::new(mode, org);
    app.lsp_enabled = language_servers;
    app.format_on_save = format_on_save;
    if let Some(w) = file_panel_width {
        // The real clamp happens at draw time, once the area is known.
        app.file_panel_w = w.max(app::FILE_PANEL_MIN);
    }
    app.start();
    let mut dirty = true;

    loop {
        // Blocking work runs on background threads; pick up results here.
        // While a foreground job runs the spinner animates, so keep drawing;
        // otherwise only redraw when state actually changed — an idle loupe
        // costs ~zero CPU.
        if app.poll_jobs() || app.busy() || app.refreshing() || app.searching() {
            dirty = true;
        }

        if dirty {
            terminal.draw(|f| ui::draw(f, &mut app))?;
            dirty = false;
        }

        if app.should_quit {
            // Language servers are children of this process; don't leave
            // them running after the terminal is handed back.
            app.lsp.shutdown();
            return Ok(());
        }

        // Short timeout while busy keeps the spinner animating and picks up
        // job results promptly; longer otherwise to stay idle-friendly.
        let timeout = Duration::from_millis(if app.busy() || app.refreshing() || app.searching() {
            80
        } else {
            250
        });
        if event::poll(timeout)? {
            // Drain every pending event before the next draw: a fast mouse
            // drag or wheel flick delivers dozens of events, which would
            // otherwise each pay a full redraw.
            loop {
                match event::read()? {
                    Event::Key(key) if key.kind != event::KeyEventKind::Release => {
                        app.handle_key(key)
                    }
                    Event::Mouse(m) => app.handle_mouse(m),
                    _ => {}
                }
                dirty = true;
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parsing() {
        let p = |args: &[&str]| parse_cli(args.iter().map(|s| s.to_string()));
        // No flag: defer to config (None), don't force auto.
        assert!(matches!(p(&[]), Ok(CliCmd::Run { mode: None, .. })));
        let mode_of = |args: &[&str]| match p(args) {
            Ok(CliCmd::Run { mode, .. }) => mode,
            _ => panic!("expected Run"),
        };
        assert_eq!(mode_of(&["--pr"]), Some(LaunchMode::Pr));
        assert_eq!(mode_of(&["pr"]), Some(LaunchMode::Pr));
        assert_eq!(mode_of(&["--local"]), Some(LaunchMode::Local));
        assert_eq!(mode_of(&["-l"]), Some(LaunchMode::Local));
        assert_eq!(mode_of(&["--auto"]), Some(LaunchMode::Auto));
        assert!(matches!(p(&["--help"]), Ok(CliCmd::Help)));
        assert!(matches!(p(&["--themes"]), Ok(CliCmd::Themes)));
        assert_eq!(p(&["--nope"]).err(), Some("--nope".to_string()));
        // Last flag wins.
        assert_eq!(mode_of(&["--pr", "--local"]), Some(LaunchMode::Local));
    }

    fn startup(dark: Option<&str>, light: Option<&str>, session: Option<&str>) -> Startup {
        let by = |n: Option<&str>| n.map(|n| highlight::theme_by_name(n).unwrap());
        Startup {
            mode: LaunchMode::Auto,
            org: None,
            file_panel_width: None,
            language_servers: true,
            format_on_save: false,
            wizard: false,
            appearance: None,
            dark_theme: by(dark),
            light_theme: by(light),
            session_theme: by(session),
        }
    }

    /// The two theme slots are independent, and an empty one borrows from
    /// the other rather than jumping to something unrelated.
    #[test]
    fn theme_slots_resolve_per_appearance() {
        let key = |s: &Startup, a| highlight::theme_key(s.theme(a));

        // Both configured: each appearance uses its own.
        let s = startup(Some("nord"), Some("github"), None);
        assert_eq!(key(&s, Appearance::Dark), "nord");
        assert_eq!(key(&s, Appearance::Light), "github");

        // Only the dark slot set — the light one adapts it.
        let s = startup(Some("gruvbox-dark"), None, None);
        assert_eq!(key(&s, Appearance::Dark), "gruvbox-dark");
        assert_eq!(key(&s, Appearance::Light), "gruvbox-light");

        // Only the light slot set — same in reverse.
        let s = startup(None, Some("solarized-light"), None);
        assert_eq!(key(&s, Appearance::Light), "solarized-light");
        assert_eq!(key(&s, Appearance::Dark), "solarized-dark");

        // Nothing configured: the built-in defaults, one per appearance.
        let s = startup(None, None, None);
        assert_eq!(key(&s, Appearance::Dark), "catppuccin-mocha");
        assert_eq!(key(&s, Appearance::Light), "catppuccin-latte");

        // `--theme` is for this session and wins over both slots.
        let s = startup(Some("nord"), Some("github"), Some("dracula"));
        assert_eq!(key(&s, Appearance::Dark), "dracula");
        assert_eq!(key(&s, Appearance::Light), "dracula");
    }

    /// An explicit `--light`/`--dark` beats detection, detection beats the
    /// configured theme's own background, and with nothing at all to go on
    /// the answer is dark — what loupe did before any of this existed.
    #[test]
    fn appearance_precedence() {
        let (light, dark) = (Some(Appearance::Light), Some(Appearance::Dark));

        // An explicit setting wins even against a terminal saying otherwise.
        let mut s = startup(Some("nord"), None, None);
        s.appearance = light;
        assert_eq!(s.appearance_from(dark), Appearance::Light);
        s.appearance = dark;
        assert_eq!(s.appearance_from(light), Appearance::Dark);

        // Detection wins over the theme's own background.
        let s = startup(Some("github"), None, None);
        assert_eq!(s.appearance_from(dark), Appearance::Dark);

        // Nothing detected: fall back to what the configured theme wants.
        // This is the upgrade path — one `theme` key, set before there were
        // two — and getting it wrong means dark tints under light syntax.
        assert_eq!(
            startup(Some("github"), None, None).appearance_from(None),
            Appearance::Light
        );
        assert_eq!(
            startup(Some("nord"), None, None).appearance_from(None),
            Appearance::Dark
        );
        assert_eq!(
            startup(None, Some("gruvbox-light"), None).appearance_from(None),
            Appearance::Light
        );
        // `--theme` is the strongest of the theme signals.
        assert_eq!(
            startup(Some("nord"), None, Some("github")).appearance_from(None),
            Appearance::Light
        );
        // Both slots configured says nothing — that is someone who uses
        // both kinds of terminal.
        assert_eq!(
            startup(Some("nord"), Some("github"), None).appearance_from(None),
            Appearance::Dark
        );
        // And with no signal at all, dark.
        assert_eq!(
            startup(None, None, None).appearance_from(None),
            Appearance::Dark
        );
    }

    #[test]
    fn appearance_flags_parse() {
        let p = |args: &[&str]| parse_cli(args.iter().map(|s| s.to_string()));
        let appearance_of = |args: &[&str]| match p(args) {
            Ok(CliCmd::Run { appearance, .. }) => appearance,
            _ => panic!("expected Run"),
        };
        assert_eq!(appearance_of(&[]), None, "unset means ask the terminal");
        assert_eq!(appearance_of(&["--light"]), Some(Appearance::Light));
        assert_eq!(appearance_of(&["--dark"]), Some(Appearance::Dark));
        // Last one wins, like the mode flags.
        assert_eq!(
            appearance_of(&["--light", "--dark"]),
            Some(Appearance::Dark)
        );
        // Composes with everything else.
        match p(&["--local", "--light", "--theme", "github"]) {
            Ok(CliCmd::Run {
                mode,
                theme,
                appearance,
                ..
            }) => {
                assert_eq!(mode, Some(LaunchMode::Local));
                assert_eq!(theme.as_deref(), Some("github"));
                assert_eq!(appearance, Some(Appearance::Light));
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn theme_and_setup_parsing() {
        let p = |args: &[&str]| parse_cli(args.iter().map(|s| s.to_string()));
        match p(&["--theme", "nord"]) {
            Ok(CliCmd::Run { theme, .. }) => assert_eq!(theme.as_deref(), Some("nord")),
            _ => panic!("expected Run"),
        }
        match p(&["--theme=dracula", "--pr"]) {
            Ok(CliCmd::Run { theme, mode, .. }) => {
                assert_eq!(theme.as_deref(), Some("dracula"));
                assert_eq!(mode, Some(LaunchMode::Pr));
            }
            _ => panic!("expected Run"),
        }
        assert!(p(&["--theme"]).is_err());
        match p(&["set-theme", "catppuccin-mocha"]) {
            Ok(CliCmd::SetTheme { name, light }) => {
                assert_eq!(name, "catppuccin-mocha");
                assert!(!light, "no --light means the dark slot");
            }
            _ => panic!("expected SetTheme"),
        }
        // `--light` picks the other slot, before or after the name.
        for args in [
            ["set-theme", "--light", "github"],
            ["set-theme", "github", "--light"],
        ] {
            match p(&args) {
                Ok(CliCmd::SetTheme { name, light }) => {
                    assert_eq!(name, "github");
                    assert!(light);
                }
                other => panic!("expected SetTheme, got {:?}", other.is_ok()),
            }
        }
        assert!(p(&["set-theme"]).is_err());
        assert!(matches!(p(&["setup"]), Ok(CliCmd::Run { setup: true, .. })));
        assert!(matches!(
            p(&["setup", "--local"]),
            Ok(CliCmd::Run {
                setup: true,
                mode: Some(LaunchMode::Local),
                ..
            })
        ));
    }
}

mod app;
mod config;
mod diff;
mod editor;
mod github;
mod gitops;
mod highlight;
mod ui;
mod wizard;

use anyhow::Result;
use app::{App, LaunchMode};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use crossterm::execute;
use std::io::stdout;
use std::time::Duration;

const USAGE: &str = "loupe — a mouse-first TUI for reviewing GitHub pull requests and local changes

USAGE: loupe [--pr | --local | --auto] [--theme <name>]
       loupe set-theme <name>
       loupe setup

  (no flag)  use the configured default mode (see CONFIG below), or auto:
             review uncommitted local changes if there are any,
             otherwise fall through to the pull-request flow
  --pr       skip the local scan and go straight to pull requests
  --local    review local changes only (even when the tree is clean)
  --auto     the auto behavior above (overrides a configured default)
  --theme <name>
             use this syntax theme for the session, without saving it
  --themes   list syntax-theme names
  --help     show this help

  set-theme <name>   save <name> as your theme (in the global config)
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
  theme = \"catppuccin-mocha\"  # syntax theme (loupe --themes lists them)
  file_panel_width = 34    # starting width of the file panel, in columns
                           # (drag the divider, or < / >, to change it)";

enum CliCmd {
    /// Start the TUI. `mode`/`theme` override the configured defaults for
    /// this session; `setup` forces the setup wizard to run first.
    Run {
        mode: Option<LaunchMode>,
        theme: Option<String>,
        setup: bool,
    },
    Help,
    Themes,
    SetTheme(String),
}

/// Parse argv. Err carries the offending argument (or a message).
fn parse_cli<I: Iterator<Item = String>>(args: I) -> Result<CliCmd, String> {
    let mut args = args;
    let mut mode = None;
    let mut theme = None;
    let mut setup = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--pr" | "-p" | "pr" => mode = Some(LaunchMode::Pr),
            "--local" | "-l" | "local" => mode = Some(LaunchMode::Local),
            "--auto" | "auto" => mode = Some(LaunchMode::Auto),
            "--theme" => match args.next() {
                Some(name) => theme = Some(name),
                None => return Err("--theme needs a theme name".to_string()),
            },
            "setup" => setup = true,
            "set-theme" => {
                return match args.next() {
                    Some(name) => Ok(CliCmd::SetTheme(name)),
                    None => Err("set-theme needs a theme name".to_string()),
                };
            }
            "--themes" => return Ok(CliCmd::Themes),
            "--help" | "-h" => return Ok(CliCmd::Help),
            other => match other.strip_prefix("--theme=") {
                Some(name) if !name.is_empty() => theme = Some(name.to_string()),
                _ => return Err(other.to_string()),
            },
        }
    }
    Ok(CliCmd::Run { mode, theme, setup })
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

fn main() -> Result<()> {
    let (cli_mode, cli_theme, force_setup) = match parse_cli(std::env::args().skip(1)) {
        Ok(CliCmd::Run { mode, theme, setup }) => (mode, theme, setup),
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
        Ok(CliCmd::SetTheme(name)) => {
            let _ = theme_or_exit(&name);
            let key = highlight::theme_key(theme_or_exit(&name));
            let path = config::save_global(&[("theme", key)])?;
            println!("loupe: theme “{key}” saved to {}", path.display());
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
    // Theme precedence: --theme > config > built-in default. Names are
    // validated before the terminal goes raw.
    if let Some(name) = cli_theme.as_deref().or(cfg.theme.as_deref()) {
        highlight::set_theme(theme_or_exit(name));
    }
    // First launch (no global config yet) — or an explicit `loupe setup` —
    // runs the wizard before anything else.
    let wizard = force_setup || !config::global_path().is_some_and(|p| p.is_file());
    // Precedence: command line > config file > built-in auto.
    let mode = cli_mode
        .or(cfg.mode.map(LaunchMode::from))
        .unwrap_or(LaunchMode::Auto);
    run_tui(mode, cfg.org, cfg.file_panel_width, wizard)
}

fn run_tui(
    mode: LaunchMode,
    org: Option<String>,
    file_panel_width: Option<u16>,
    wizard: bool,
) -> Result<()> {
    // ratatui::init installs a panic hook that restores the terminal; we add
    // mouse capture on top and make sure it is torn down too.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        default_hook(info);
    }));

    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;

    let result = (|| {
        if wizard {
            match wizard::run(&mut terminal)? {
                wizard::WizardEnd::Quit => return Ok(()),
                wizard::WizardEnd::Done | wizard::WizardEnd::Skipped => {}
            }
        }
        run(&mut terminal, mode, org, file_panel_width)
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
) -> Result<()> {
    let mut app = App::new(mode, org);
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
        if app.poll_jobs() || app.busy() || app.refreshing() {
            dirty = true;
        }

        if dirty {
            terminal.draw(|f| ui::draw(f, &mut app))?;
            dirty = false;
        }

        if app.should_quit {
            return Ok(());
        }

        // Short timeout while busy keeps the spinner animating and picks up
        // job results promptly; longer otherwise to stay idle-friendly.
        let timeout = Duration::from_millis(if app.busy() || app.refreshing() {
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
            Ok(CliCmd::SetTheme(name)) => assert_eq!(name, "catppuccin-mocha"),
            _ => panic!("expected SetTheme"),
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

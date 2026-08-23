# Configuration

Loupe is configured with a small TOML file — but you rarely have to edit
it by hand: the first launch runs a **setup wizard** that writes it for
you, the in-app **theme picker** (`t` or the 🎨 button) saves your theme
choice itself, and `loupe set-theme <name>` does the same from the shell.
Every key is optional — with no config at all you get auto mode, the
`catppuccin-mocha` theme, and a 34-column file panel.

## The setup wizard

The first time you run `loupe` (no global config file yet), a short
wizard walks you through picking a syntax theme — with a live preview —
and a default startup mode, then saves both to the global config.
Re-run it any time with:

```sh
loupe setup
```

Skipping the wizard (Esc) writes a commented placeholder config so it
won't ask again.

## Where config lives

Loupe reads, in order:

1. **Global config** — the first of:
   - `$LOUPE_CONFIG` (explicit path, highest priority),
   - `$XDG_CONFIG_HOME/loupe/config.toml`,
   - `~/.config/loupe/config.toml`.
2. **Per-repository config** — `.loupe.toml` at the repository root.
   Keys set here **win** over the global file, which is how you pin
   settings per project (a different upstream `org`, a different default
   mode, …).

A commented [`config.example.toml`](../config.example.toml) ships in the
repository — copy it to either location and uncomment what you need.

Unknown keys are **hard errors**: a typo'd config is reported on stderr
(file and key) before the terminal enters raw mode, and Loupe exits with
status 2. Your config never silently half-applies.

## Keys

### `org` — upstream organization

```toml
org = "acme"
```

The GitHub organization (or user) pull requests are opened against. When
set, Loupe lists and opens PRs on `<org>/<repo-name>` instead of the
clone's own owner — for fork workflows and for people working across
several organizations. PR-ref fetches automatically use the git remote
whose URL matches that repository (https, ssh, and scp-style URLs all
match), falling back to `https://github.com/<org>/<repo>.git`.

Typically set per-project in `.loupe.toml`.

With an `org` configured, branch auto-open matches upstream PRs by your
current branch's name — exact for same-repo branches, best-effort when
the PR head lives in a fork.

### `mode` — default startup mode

```toml
mode = "auto"    # "auto" | "pr" | "local"
```

- `auto` — review uncommitted local changes if there are any, otherwise
  fall through to the pull-request flow (the built-in default).
- `pr` — always go straight to pull requests.
- `local` — always review local changes.

Precedence: command line (`--pr` / `--local` / `--auto`) > repo
`.loupe.toml` > global config > built-in `auto`.

### `theme` — syntax-highlighting theme

```toml
theme = "catppuccin-mocha"
```

Any of the 32 bundled themes (the same extended set
[bat](https://github.com/sharkdp/bat) uses) — all four Catppuccin
flavors, Dracula, Nord, Gruvbox, GitHub, and more, including several
light themes. The default is `catppuccin-mocha`.

The comfortable way to choose: press **`t`** (or click **🎨 Theme**)
inside loupe. The picker previews each theme live on a code sample —
with diff backgrounds — as you move through the list; Enter keeps the
selection *and saves it to this config key for you*, Esc puts everything
back. Alternatives:

```sh
loupe --themes              # list every valid name
loupe --theme nord          # try one for a single session, unsaved
loupe set-theme nord        # save it to the global config from the shell
```

An unknown theme name is reported and Loupe exits before starting the
TUI.

### `file_panel_width` — starting panel width

```toml
file_panel_width = 34
```

Starting width of the file panel, in columns. While running you can drag
the divider between the panels, press `<` / `>`, or double-click the
divider to return to the default of 34. The width is re-clamped on every
draw, so a narrow terminal can never squeeze the diff out of existence.

## Command line

```
loupe [--pr | --local | --auto] [--theme <name>]
loupe set-theme <name>
loupe setup

  (no flag)  use the configured default mode, or auto
  --pr, -p   skip the local scan and go straight to pull requests
  --local,-l review local changes only (even when the tree is clean)
  --auto     review local changes if any, else PRs (overrides config)
  --theme <name>  use this theme for the session, without saving it
  --themes   list syntax-theme names
  --help,-h  show usage

  set-theme <name>   save <name> as your theme in the global config
  setup              re-run the first-launch setup wizard
```

When several mode flags are given, the last one wins.

## Environment

| Variable | Effect |
| --- | --- |
| `LOUPE_CONFIG` | Absolute path to the global config file, overriding the XDG lookup |
| `XDG_CONFIG_HOME` | Respected for the default config location |

Loupe performs all GitHub access through the `gh` CLI, so `gh`'s own
configuration (default host, auth) applies as-is.

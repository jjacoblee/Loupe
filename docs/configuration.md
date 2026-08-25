# Configuration

Loupe is configured with a small TOML file — but you rarely have to edit
it by hand: the first launch runs a **setup wizard** that writes it for
you, the in-app **theme picker** (`t` or the 🎨 button) saves your theme
choice itself, and `loupe set-theme <name>` does the same from the shell.
Every key is optional — with no config at all you get auto mode, a
34-column file panel, and colors matched to your terminal's background
(`catppuccin-mocha` on a dark terminal, `catppuccin-latte` on a light
one).

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

### `appearance` — light or dark colors

```toml
appearance = "auto"    # "auto" | "light" | "dark"
```

Loupe paints its own colors — the green and red diff backgrounds above
all — and those have to suit the background they sit on. On `auto` (the
default) it asks your terminal what its background color is at startup,
using the OSC 11 escape sequence that every current terminal answers
(iTerm2, Ghostty, kitty, WezTerm, Alacritty, foot, xterm, Windows
Terminal, Terminal.app). A terminal that stays silent is asked again via
`COLORFGBG`, and if that is missing too, Loupe assumes dark.

The whole query is bounded: a device-attributes request is sent right
behind it as a sentinel, so a terminal that doesn't implement OSC 11
costs a millisecond rather than a timeout, and the worst case — a
terminal that answers nothing at all — is 120 ms, once, at startup.

Set `light` or `dark` to skip detection entirely. `--light` / `--dark`
do the same for one session, and pressing `a` in the theme picker flips
it live and saves it.

To see what your terminal reports:

```sh
loupe appearance
```

### `theme` / `light_theme` — syntax-highlighting themes

```toml
theme       = "catppuccin-mocha"   # used on a dark terminal
light_theme = "catppuccin-latte"   # used on a light one
```

Any of the 32 bundled themes (the same extended set
[bat](https://github.com/sharkdp/bat) uses) — all four Catppuccin
flavors, Dracula, Nord, Gruvbox, GitHub, and more, including nine light
themes.

There are two keys because a syntax theme is only readable on the
background it was designed for. Loupe picks the one matching the
resolved appearance, so moving between a light and a dark terminal —
or between a light and a dark day — doesn't overwrite the choice you
made on the other.

Setting only one is fine: the empty slot borrows from the one you set,
staying in the same family where a counterpart exists. `theme =
"gruvbox-dark"` alone gives you `gruvbox-light` on a light terminal, and
`solarized-light` alone gives you `solarized-dark` on a dark one. Themes
with no counterpart fall back to the default for that appearance.

The comfortable way to choose: press **`t`** (or pick **Theme** from **☰**)
inside loupe. The picker previews each theme live on a code sample —
against the theme's own background, with diff backgrounds — as you move
through the list; Enter keeps the selection *and saves it into the slot
for your current appearance*, Esc puts everything back. `a` switches
light ⇄ dark, carrying the selection to the counterpart theme so the two
never disagree. Alternatives:

```sh
loupe --themes                    # list every valid name
loupe --theme nord                # try one for a single session, unsaved
loupe set-theme nord              # save it as your dark-terminal theme
loupe set-theme --light github    # …and as your light-terminal one
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

### `language_servers` — go to definition, references, hover

```toml
language_servers = true   # the default
```

Loupe drives a language server for `gd`, `gr` and `K`. It ships none and
installs none: it looks on your `PATH` for the one your language already
uses, starts it the first time you ask a question about a file it
handles, and stops it when Loupe exits.

| Language | Binary | Install |
| --- | --- | --- |
| TypeScript / JavaScript | `typescript-language-server` | `npm install -g typescript-language-server typescript` |
| Go | `gopls` | `go install golang.org/x/tools/gopls@latest` |
| Rust | `rust-analyzer` | `rustup component add rust-analyzer` |

Run `loupe --lsp` to see which of those are installed and what to run
for the rest. Note that `tsserver` alone is not enough — it speaks its
own protocol rather than LSP, and `typescript-language-server` is the
wrapper that bridges the two. Loupe pairs it with the `tsserver.js` from
your project's `node_modules` when there is one, so a project pinned to
an older TypeScript is analyzed by that version.

The server always receives **the text on your screen**, not the file on
disk. In PR review those differ — the working tree may be on another
branch entirely — and answering from the wrong copy would be worse than
not answering.

Set this to `false` and Loupe never starts a subprocess for any of it;
`@` (symbols in this file) keeps working from pattern matching, and
`gd` / `gr` / `K` say why they can't.

### `format_on_save` — run the formatter when the editor saves

```toml
format_on_save = false   # the default
```

With this on, `Ctrl+S` in the editor runs `textDocument/formatting`
first — prettier, gofmt or rustfmt, whichever the language server drives
— and saves the result.

It is off by default on purpose: Loupe is a review tool, and reformatting
a file in the middle of a review would add changes to the diff that
nobody asked for. `Ctrl+T` (or the `⇥ Format` button) formats on demand
whatever this is set to, and `Ctrl+Z` undoes a format in one keystroke.

### `auto_refresh` — re-scan local changes while you sit idle

```toml
auto_refresh = true   # the default
```

Local review re-reads the working tree about 2 seconds after your last
key press or mouse move, and at most once every 5 seconds. This is what
keeps the diff current while an agent (or a second terminal) writes to
your files — nothing in a terminal tells Loupe that happened.

It never interrupts you. The re-scan stands down while the editor is
open, while any overlay or menu is open, while lines are selected, and
during a drag or a panel resize. A re-scan that finds nothing says
nothing and moves nothing; one that finds a change reloads the open file
in place, keeping your scroll position, cursor row and folds.

Pull requests are never polled whatever this is set to: a PR head lives
on GitHub, and checking it on a timer would spend API calls on a commit
that moves a few times a day. `r` (or the `⟳` button) fetches it, and
`r` refreshes local review on demand too.

Set `false` to turn the idle re-scan off for good, or flip it for one
session from `☰ → Refresh while idle`.

### `blame` / `blame_width` / `blame_pr_lookup` — the blame pane

```toml
blame = false           # the default
blame_width = 30
blame_pr_lookup = true  # the default
```

`blame` opens the pane between the file panel and the diff from the
start. It is off by default: it costs a `git blame` per file and about 30
columns of width, and not every review wants it. `B` (or
`☰ → Blame column`) turns it on for one session either way.

`blame_width` is its starting width in columns. Drag the second divider
to change it while running, or double-click that divider for 30. Below
about 22 columns the pull request number drops out, then the age. A
terminal too narrow for three panes shows two — the diff keeps its width.

`blame_pr_lookup` decides whether Loupe asks GitHub which pull request a
blamed commit belongs to. A squash or merge commit names its own number
in its subject, which Loupe reads for free and offline; a rebase merge
does not, and those are what the lookup is for — one batched `gh` call
per file, cached by commit for the session. Set `false` to stay entirely
offline and rely on the subject alone.

See [The blame pane](keys-and-mouse.md#the-blame-pane) for what the
colors mean and what a click on a row offers.

## Command line

```
loupe [--pr | --local | --auto] [--theme <name>] [--light | --dark]
loupe md <file.md>
loupe set-theme [--light] <name>
loupe appearance
loupe setup

  (no flag)  use the configured default mode, or auto
  --pr, -p   skip the local scan and go straight to pull requests
  --local,-l review local changes only (even when the tree is clean)
  --auto     review local changes if any, else PRs (overrides config)
  --theme <name>  use this theme for the session, without saving it
  --light    force light colors for this session
  --dark     force dark colors (the default is to ask the terminal)
  --themes   list syntax-theme names
  --lsp      report which language servers loupe can find
  --help,-h  show usage

  md <file.md>       read one markdown file in the preview, with no
                     review beside it. The path may be anywhere on the
                     machine. P shows its source, Ctrl+S saves, q quits.
  set-theme [--light] <name>
                     save <name> as your dark- (or light-) terminal theme
  appearance         report what your terminal says its background is
  setup              re-run the first-launch setup wizard
```

When several mode flags are given, the last one wins; the same goes for
`--light` / `--dark`.

## Environment

| Variable | Effect |
| --- | --- |
| `LOUPE_CONFIG` | Absolute path to the global config file, overriding the XDG lookup |
| `XDG_CONFIG_HOME` | Respected for the default config location |

Loupe performs all GitHub access through the `gh` CLI, so `gh`'s own
configuration (default host, auth) applies as-is.

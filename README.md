<!-- The same block-letter logo the setup wizard draws (src/wizard.rs). -->

```text
██╗      ██████╗ ██╗   ██╗██████╗ ███████╗
██║     ██╔═══██╗██║   ██║██╔══██╗██╔════╝
██║     ██║   ██║██║   ██║██████╔╝█████╗
██║     ██║   ██║██║   ██║██╔═══╝ ██╔══╝
███████╗╚██████╔╝╚██████╔╝██║     ███████╗
╚══════╝ ╚═════╝  ╚═════╝ ╚═╝     ╚══════╝
```

# 🔍 Loupe — mouse-first PR review in the terminal

[![CI](https://github.com/jjacoblee/Loupe/actions/workflows/ci.yml/badge.svg)](https://github.com/jjacoblee/Loupe/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/jjacoblee/Loupe)](https://github.com/jjacoblee/Loupe/releases/latest)
[![License: Apache 2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

Loupe is a clickable terminal UI for code review. It reviews GitHub pull
requests and your own uncommitted changes. You get file trees,
syntax-highlighted side-by-side diffs, inline comments, viewed checkboxes
that sync to GitHub, and a real editor — all in one terminal window.
Built with Rust and [ratatui](https://ratatui.rs).

It is also built for work with coding agents. Loupe tells your agent
which code you are looking at, so you can ask questions about the exact
lines on your screen.

![Loupe reviewing local changes — file tree with staging icons and a syntax-highlighted side-by-side diff](docs/assets/local-review.png)

## Talk to your agent about the code on your screen

Loupe is the only process on your machine that knows which lines you are
reading right now. An agent in another pane normally has to grep for
that context. Loupe removes the guesswork:

1. Select code in the diff — click a line, drag a range, or use `V` +
   motions.
2. Type an instruction to your agent in its own pane: *"rename this"*,
   *"why does this branch exist?"*, *"is this change safe?"*.
3. The agent receives the file, the line range, the diff hunk on both
   sides, your held review comments, and the files you have not marked
   viewed — with no key press and no copy-paste.

One command wires it up:

```sh
loupe ctl install
```

This adds a `UserPromptSubmit` hook to every coding agent on the machine
(Claude Code and Codex are both supported). The first-launch wizard
offers the same setup. The hook is silent when no Loupe is open. `loupe
ctl uninstall` removes it again.

No panes, no tmux ids, no configuration per session — the repository
root is the address, so terminal splits and separate windows all work
the same. Over SSH, or with an agent that has no hooks, press `Y` to
copy the same context block by hand.

Full details: [docs/agent-context.md](docs/agent-context.md).

## Review pull requests

- **One command, the right view.** `loupe` opens your uncommitted
  changes if you have any. Otherwise it opens the current branch's open
  PR, or a clickable picker of the repo's open PRs.
- **Comments where you point.** Click or drag across diff lines, then
  press `c`. Loupe posts single-line or multi-line review comments to
  the PR through `gh`.
- **One review, not ten notifications.** `Ctrl+S` *holds* a comment
  instead of posting it. Held comments show as `💬` and survive a
  restart. `R` opens the review box: write a summary, pick **Comment**,
  **Approve**, or **Request changes**, and send everything as a single
  GitHub review. Loupe shows you exactly what will go before it goes.
- **Viewed checkboxes that sync.** Mark a file viewed in Loupe and it
  shows as viewed on github.com — and the other way around.
- **Stacked PRs as a ladder.** The badge shows your rung (`PR #43 ·
  2/3`). Click it to see the whole chain and jump to any rung, or walk
  the chain with `Alt+↑` / `Alt+↓`. No extension needed — Loupe reads
  the chain from the GitHub API.
- **No tokens to manage.** All GitHub access goes through your existing
  [`gh`](https://cli.github.com/) login.

## Review your own changes — and keep up with an agent

- **Local review with staging.** The file panel stages in local mode:
  `[+]` unstaged, `[±]` partially staged, `[✓]` staged. Review your diff
  and build the commit in one pass.
- **Live refresh.** Loupe re-scans the working tree whenever you pause,
  so files an agent rewrites appear without a key press. It never
  interrupts you and never moves your place. `r` refreshes now.
- **Three layers of your change, in three colors.** Press `s` and every
  row gets the color of the step that wrote it: purple for pushed and
  reviewed, green and red for new work, and amber for a line written
  *twice*. When an agent hands you a hundred lines, this answers the one
  question no other diff tool does: is this new work, or a redo?
- **Merge conflicts, resolved in the diff.** Conflicted files sort to
  the top. The diff shows your version on the left and theirs on the
  right, with the marker lines stripped. Press `o` to take ours, theirs,
  both, the ancestor — or edit by hand. The last resolved conflict
  stages the file.
- **Undo without leaving the review.** Every changed section and every
  file row has a `↺`. Click it to put those lines — or the whole file —
  back. Both ask first.
- **Two reviews, one window.** `` ` `` swaps between the pull request
  and your own uncommitted changes. Both keep their open file, cursor,
  scroll, folds, and marks.

## Read code like an editor

- **Code intelligence from the tools you already have.** `gd` goes to a
  definition, `gr` lists references, `K` shows types and docs. Loupe
  drives whichever language server is on your PATH
  (`typescript-language-server`, `gopls`, `rust-analyzer`) and hands it
  the text on your screen, not the file on disk.
- **Blame beside the diff.** `B` shows who last touched each line, how
  long ago, and the PR it landed in — with an age heat map, and colors
  for lines that this change or your working tree moved.
- **Find anything.** `/` searches the open diff. `Ctrl+P` fuzzy-matches
  a file. `#` greps every changed file or the whole repository. `@`
  lists what the file defines.
- **Read the markdown, not the markup.** `P` renders a `.md` file as a
  document — headings, tables, task lists, colored code blocks. It
  re-renders on its own when an agent rewrites the file. Drag a `.md`
  file from anywhere onto the window to read it beside the review.
- **Pinned tabs.** `=` pins the file in front of you, `1`–`9` open a
  tab. Tabs are per worktree and survive a restart, so the plan file you
  read twenty times a day is one key away.
- **The whole repository, not just the change.** `F` swaps the file
  panel to every file in the repository, with ignored files drawn dim.
- **Copy out of either side.** Drag to select exact characters on the
  old side or the new, then press `y`. The removed text is still there
  to take. Works over SSH.

## Fix nits in place

Double-click a line to open a real in-terminal editor:

- Live syntax highlighting, completion (`Ctrl+Space`), and diagnostics
  as you type.
- Go-to-definition (`F12`), every use (`F10`), rename everywhere
  (`Alt+M`), and the fixes the language server offers (`Alt+.`).
- `eslint` and `ruff` run over the buffer beside the compiler, with the
  tool named on each finding.
- A rename opens every touched file as an unsaved buffer, so you read
  what happened before it lands.
- `Ctrl+S` refreshes the diff. You commit when you are ready.

## Comfortable in any terminal

- **Mouse and keyboard, never in conflict.** Click a line or press
  `j`/`k` — the same cursor moves. Vim motions work throughout.
- **Light and dark, without being told.** Loupe asks your terminal for
  its background color at startup and tunes everything to it. Override
  with `--light` / `--dark` or `a` in the theme picker.
- **32 syntax themes** (all four Catppuccin flavors, Dracula, Nord,
  Gruvbox, …) with a live preview: press `t`.
- **Fast and non-blocking.** Every fetch runs in the background with a
  cancellable spinner. Idle CPU is near zero. PR content is treated as
  untrusted input throughout.
- **Zero-config start.** The first launch runs a setup wizard and writes
  your config for you.

![The theme picker — every theme previews live](docs/assets/theme-picker.png)

## Install

**Prebuilt binaries** for Linux, macOS, and Windows are on the
[releases page](https://github.com/jjacoblee/Loupe/releases/latest) —
unpack and put `loupe` on your `PATH`.

**With cargo** (any recent stable [Rust](https://rustup.rs)):

```sh
cargo install --git https://github.com/jjacoblee/Loupe
```

> Loupe is not on crates.io yet — the crate name `loupe` there belongs
> to an unrelated project, so use the `--git` form.

**From source:**

```sh
git clone https://github.com/jjacoblee/Loupe && cd Loupe
cargo build --release   # binary at target/release/loupe
```

You also need `git`, an authenticated GitHub CLI (`gh auth login`), and
a modern terminal with mouse support.

## Quick start

```sh
cd your-repo
loupe                # local changes if any, else the PR flow
loupe --pr           # straight to pull requests
loupe --local        # review uncommitted work even when the tree is clean
loupe md PLAN.md     # read one markdown file, with no review beside it
loupe ctl install    # give your coding agents your on-screen context
```

When you open a PR from the picker, Loupe offers **Checkout & review**
(switches your working tree, enables the editor) or **Review only** (no
checkout — comments work, the editor is off).

The basics: click files, read diffs (`v` toggles split/inline, `z`
folds), select lines and press `c` to comment, `e` to edit, `x` to mark
viewed or stage, `B` for blame, `?` for help, `q` to quit. The full
reference is in [docs/keys-and-mouse.md](docs/keys-and-mouse.md).

## Documentation

| | |
| --- | --- |
| [Documentation index](docs/README.md) | Every page, and a table of where each feature is written up |
| [Getting started](docs/getting-started.md) | Requirements, installation, first run, a tour, troubleshooting |
| [Agent context](docs/agent-context.md) | Give your coding agent the file and lines you are reading, with one hook |
| [Reviewing pull requests](docs/reviewing-prs.md) | Picker, checkout modes, comments, the review box, viewed sync, editing |
| [Reviewing local changes](docs/local-changes.md) | Local mode, the staging column, merge conflicts, keeping up with an agent |
| [Configuration](docs/configuration.md) | Config file, all keys, CLI flags, themes |
| [Keyboard & mouse reference](docs/keys-and-mouse.md) | Every binding and clickable control, and how each feature works |
| [Architecture](docs/architecture.md) | How Loupe works inside — start here to contribute |

Configuration lives in `~/.config/loupe/config.toml` plus an optional
per-repo `.loupe.toml` (see
[`config.example.toml`](config.example.toml)). You will rarely edit it
by hand: the setup wizard and theme picker write it for you, and `loupe
setup` re-runs the wizard at any time.

## Contributing

Contributions are welcome — bug reports, docs, and code. Start with the
[contribution guide](CONTRIBUTING.md) and the
[architecture overview](docs/architecture.md). This project follows a
[code of conduct](CODE_OF_CONDUCT.md); security issues go through the
[security policy](SECURITY.md).

## License

[Apache License 2.0](LICENSE) — free to use, modify, and distribute, for
personal projects and at work, commercially or not.

Two things come with it. The license grants patent rights alongside
copyright, and it asks that the [NOTICE](NOTICE) file travel with any
derivative work. So if you build a product or a hosted service on Loupe,
keep the NOTICE — that is the credit this project asks for.

If you ship something built on Loupe, a link back to
[the repository](https://github.com/jjacoblee/Loupe) is appreciated, and
I would love to hear about it.

### Third-party code

Loupe stands on other people's work. The Rust crates it links are listed
with their licenses in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md),
which ships in every release archive. The syntax definitions and color
themes are embedded in the binary itself by way of
[two-face](https://codeberg.org/CosmicHarper/two-face) and the
[bat](https://github.com/sharkdp/bat) project, and carry the copyrights
of their individual authors:

```sh
loupe --acknowledgements
```

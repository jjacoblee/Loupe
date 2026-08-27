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
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A rich, clickable TUI for reviewing GitHub pull requests **and your own
uncommitted changes**, with the file trees, inline comments and viewed
checkboxes of a graphical review tool. Built with Rust +
[ratatui](https://ratatui.rs).

Click through file trees, read syntax-highlighted side-by-side diffs,
post review comments, mark files viewed (synced to GitHub), stage your
work file by file, and fix nits in a real in-terminal editor — without
leaving your terminal or touching a browser.

![Loupe reviewing local changes — file tree with staging icons and a syntax-highlighted side-by-side diff](docs/assets/local-review.png)

## Highlights

- **One command, the right view** — `loupe` opens your uncommitted
  changes if you have any, the current branch's open PR if there is one,
  or a clickable picker of the repo's open PRs.
- **Real review tools** — click or drag across diff lines and post
  single- or multi-line review comments straight to the PR via `gh`.
- **Viewed checkboxes that sync** — mark files viewed here, see them
  checked on github.com, and vice versa.
- **A whole review, not ten notifications** — `Ctrl+S` in a comment
  *holds* it instead of posting it. Held comments show as `💬` in the
  change bar and beside the file name, and survive quitting loupe. `R`
  opens the review box under the file panel: write the summary, pick
  **Comment**, **Approve**, or **Request changes** from the button's
  dropdown, and `Ctrl+S` sends the lot as a single GitHub review — after
  showing you exactly what is about to go. `Ctrl+Enter` still posts one
  comment on its own when that is all you meant.
- **Local review with staging** — in local mode the file panel stages
  instead: `[+]` unstaged, `[±]` partially staged, `[✓]` staged. Review
  your diff and build the commit in one pass; unstaging never touches
  your files.
- **Merge conflicts, resolved in the diff** — conflicted files sort to
  the top of the file panel under a red `⚠ N MERGE CONFLICTS` heading,
  and the top bar turns into a `⚠ MERGE` (or `REBASE`, or `CHERRY-PICK`)
  badge that says how to finish. Open one and the diff shows **your
  version on the left and theirs on the right** with the marker lines
  stripped out, so each conflict is an ordinary changed section: `}` and
  `{` walk them, long agreed stretches fold away, and search and syntax
  colors work as usual. `o` — or a click on the `⚑` in the change bar —
  offers **take ours**, **take theirs**, **take both**, the common
  ancestor where git wrote one, and *edit it by hand*. Each choice
  rewrites only that conflict; the last one resolved stages the file so
  git knows it is settled. A conflict git could not write markers for
  (one side deleted the file) resolves whole from the index instead.
- **How far you have drifted** — `↑3 ↓2 origin/main` beside the branch
  name: commits you have not pushed, and commits waiting for you. `≡`
  when you are level with the upstream.
- **Undo a change without leaving the review** — every changed section
  of the diff has a `↺` in the change bar and every file row has one of
  its own. Click the section marker to put just those lines back
  (working tree only — what you staged stays staged), or the file marker
  to take the whole file back to where it started. Both ask first.
- **Blame beside the diff** — `B` opens a pane between the file panel
  and the diff: who last touched each line, how long ago, and the pull
  request it landed in. An age heat map — one hue, lightness only — means
  you read the shape of the file's history before you read a word, and
  two colors sit above it —
  one for your uncommitted lines, one for lines the change under review
  moved — so *"is this related to what I am doing now?"* is answered at a
  glance. Click a row for the commit itself, and `o` opens its pull
  request. Works in a PR review, in local review, and beside the editor.
- **Read the markdown, not the markup** — `P` renders a `.md` file as a
  document in the pane the diff uses: headings, wrapped paragraphs,
  nested and task lists, tables, block quotes, front matter, and fenced
  code blocks colored by your syntax theme. `P` again opens its source in
  the editor, on the line you were reading, so you can change a plan file
  and look at the result without saving in between. It re-renders on its
  own when an agent rewrites the file. `Ctrl+P` reaches any `.md` file in
  the repository, and `loupe md <path>` reads one from anywhere on the
  machine.
- **Pin the files you keep coming back to** — a row of tabs under the top
  bar. `=` pins whatever is in front of you, `1`–`9` open a tab, `,` and
  `.` step through them, and `-` unpins the one you are reading. The tabs
  are per worktree and survive quitting loupe, so the plan file you read
  twenty times a day is one key away every morning.
- **Drag a file onto loupe and read it** — drop a `.md` file anywhere on
  the window and loupe pins it and renders it, wherever it lives on the
  machine. A design note an agent wrote in `~/Documents`, a write-up
  someone sent you that is still in `~/Downloads` — read it beside the
  review without first copying it into the repository and then
  remembering not to commit it. Files outside the repository are marked
  `↗` in the row so the two never blur together. Drop several at once and
  they all get a tab. It works whether your terminal marks the drop as a
  paste (Ghostty, iTerm2, Terminal.app) or types the path in one
  character at a time (Warp). `Ctrl+O` does the same by typing or pasting
  a path.
- **Keeps up with an agent** — local review re-scans the working tree
  whenever you pause, so files a coding agent rewrites under an open
  review appear without a key press. It never interrupts (no re-scan
  with the editor, a menu or a selection open) and never moves your
  place. `r` or the `⟳` button pulls changes in right now.
- **Your agent knows what you are looking at** — loupe is the only
  process on the machine that knows which lines a human is reading and
  judging. `loupe ctl context` publishes them: the file, the line range,
  the hunk on both sides, the comments you are holding, and the files you
  have not marked viewed. `loupe ctl install` wires it into the
  `UserPromptSubmit` hook that Claude Code and Codex both support — the
  setup wizard offers the same thing on first launch — and every
  instruction you type carries that block, so *"rename this"* means the
  lines under your cursor and the agent stops grepping for what you
  already had on screen.
  Nothing is typed into your prompt and no pane ids are involved — the
  repository root is the address, so tmux, terminal splits, and separate
  windows all behave the same. `Y` copies the same block by hand, for an
  agent with no hooks or a review on the far end of an SSH connection.
  See [docs/agent-context.md](docs/agent-context.md).
- **Two reviews, one window** — a review asks two questions: *what does
  this branch do?* and *what have I changed in reply?* `` ` `` swaps
  between the pull request and your own uncommitted changes and keeps
  both whole — the open file, the cursor row, the scroll position, the
  folds, and what you marked viewed or staged. Swap back and it is the
  screen you left, not a reload of it.
- **A top bar that fits** — the toolbar shows the two or three things
  that match what you are doing; `☰` holds the rest, grouped and
  labelled with the key that does the same thing.
- **Editor-grade diffs** — side-by-side or inline, syntax highlighting
  from bat's extended syntax set (32 themes), unchanged stretches folded
  away, pinned gutters with horizontal scrolling for wide lines,
  resizable panels.
- **Vim motions too** — the diff has a cursor row driven by `j`/`k`,
  `Ctrl+D`/`Ctrl+U`, `gg`/`G`, `{`/`}`, `H`/`M`/`L` and friends, with
  `V` line selections for commenting — clicking a line places the same
  cursor, so mouse and keyboard never disagree.
- **Find anything** — `/` searches the open diff as you type (`n`/`N`
  step the matches); `Ctrl+P` fuzzy-matches a file; `#` greps every
  changed file — or the whole repository — in one `git grep`, with
  definitions sorted to the top; `@` lists what's defined in the file.
  A hit in a file the PR doesn't touch opens in the editor, and `Esc`
  puts you back in the diff exactly where you were.
- **Copy out of either side** — drag through the diff to select exactly
  the characters you want, on the old side or the new, and `y` copies
  them. The removed text is still there to take even though it exists
  nowhere on disk. Works over SSH.
- **Real code intelligence, from the tools you already have** — `gd`
  goes to a definition, `gr` lists every reference, `K` shows the type
  and its docs. Loupe drives whichever language server is on your PATH
  (`typescript-language-server`, `gopls`, `rust-analyzer`), starts it on
  demand, and hands it *the text on your screen* rather than the file on
  disk — which matters when the working tree is on another branch.
  Nothing is bundled or installed; `loupe --lsp` says what it found, and
  a language without a server falls back to pattern matching.
- **The whole repository, not just the change** — `F` swaps the file
  panel between the files this change touches and every file in the
  repository. Ignored *files* are listed and drawn dim, because `.env`
  is worth opening and worth knowing is not committed; ignored
  *directories* cost one row, so `node_modules` stays a single folder
  until you open it. A file the change touches keeps its stage and
  viewed marks in both lists, and opens its diff rather than the plain
  file. Right-click to make, rename or delete one.
- **Edit in place, with the language server attached** — double-click a
  new-side line to open an editor with live incremental highlighting,
  completion (`Ctrl+Space`), type-and-docs at the cursor (`Ctrl+G`),
  go-to-definition (`F12`), find-every-use (`F10`), the signature
  of the call you are in (`Alt+S`), formatting (`Ctrl+T`), and
  diagnostics that appear as you type rather than after you push.
  `Alt+F` finds and replaces, `Alt+C` comments a line or a selection,
  and `Enter` keeps your indent. `Ctrl+S` refreshes the diff; you commit
  when you're ready.
- **Double-click a word, right-click for the rest** — a double click
  takes the whole identifier and lights up every other place it appears
  in the file; a right click opens a menu about that word, from go-to-
  definition down to the fixes the server offers. The function keys are
  where an editor puts them: `F12` definition, `F10` every use, `F2`
  rename, `F8` the next problem, `F1` the help card.
- **Suggestions as you type** — the popup opens on its own after a
  character of a name, and after a character the server asks to be told
  about, so `object.` in TypeScript lists the object's fields. `Tab`
  takes one. Loupe reads the trigger characters from the server rather
  than guessing them.
- **Problems you can read** — the span is colored *and* underlined, the
  gutter carries `✗` or `▲`, and the message itself sits in the margin
  past the end of the line. Errors red, warnings yellow. `Alt+X` lays
  one out properly: TypeScript folds its reasoning into a single
  sentence, and the panel puts each reason under the one it explains.
- **Lint beside the compiler** — `eslint` and `ruff` run over the buffer
  as you edit, the project's own copy first, and their findings sit
  alongside the language server's with the tool named on each:
  `eslint(no-undef)`, `typescript(2552)`.
- **Walk what is wrong** — `F8` and `Shift+F8` step through the
  problems in the file in line order, and `Alt+E` lists them all with
  the server's own codes, to filter and jump from.
- **Rename a symbol and read every file it touched** — `Alt+M` renames
  it everywhere the server knows about, and `Alt+.` offers the fixes and
  refactors on hand. Neither writes to disk. Every file the change
  reaches opens as an unsaved buffer with a `●` in the tab row, so you
  read what happened before it lands and save the ones you accept. A
  tool that silently rewrites twelve files is a tool nobody can review.
- **More than one file at a time** — opening a second file parks the
  first instead of closing it. Both sit in the tab row, `Alt+]` and
  `Alt+[` step between them, and coming back to one finds the cursor,
  the scroll and any unsaved edits exactly where you left them. `q` says
  how many are unsaved before it takes them.
- **Fast and non-blocking** — every fetch runs in the background with a
  cancellable spinner; idle CPU is ~zero; PR content is treated as
  untrusted input throughout.
- **Light and dark, without being told** — Loupe asks your terminal for
  its background color at startup and tunes everything to it: diff
  greens and reds that read on white, gutters and chrome that don't
  vanish, and a syntax theme from the matching half of the set. Override
  it with `--light` / `--dark`, the `appearance` config key, or `a` in
  the theme picker.
- **Zero-config start** — the first launch opens a setup wizard: pick a
  theme from a live preview, pick a default mode, and it writes your
  config for you. Press `t` any time for the in-app theme picker — 32
  themes (all four Catppuccin flavors, Dracula, Nord, Gruvbox, …),
  previewed live and saved on Enter.
- **No tokens to manage** — all GitHub access goes through your existing
  [`gh`](https://cli.github.com/) login.

![The theme picker — every theme previews live](docs/assets/theme-picker.png)

## Install

**Prebuilt binaries** for Linux, macOS, and Windows are on the
[releases page](https://github.com/jjacoblee/Loupe/releases/latest) —
unpack and put `loupe` on your `PATH`.

**With cargo** (any recent stable [Rust](https://rustup.rs)):

```sh
cargo install --git https://github.com/jjacoblee/Loupe
```

> Loupe is not on crates.io yet — the crate name `loupe` there belongs to
> an unrelated project, so use the `--git` form.

**From source:**

```sh
git clone https://github.com/jjacoblee/Loupe && cd Loupe
cargo build --release   # binary at target/release/loupe
```

You'll also need `git` and an authenticated GitHub CLI (`gh auth login`),
plus any modern terminal with mouse support.

## Quick start

```sh
cd your-repo
loupe           # local changes if any, else the PR flow
loupe --pr      # straight to pull requests
loupe --local   # review uncommitted work even when the tree is clean
loupe md PLAN.md   # read one markdown file, with no review beside it
```

Opening a PR from the picker offers **Checkout & review** (switches your
working tree, enables editing) or **Review only** (no checkout —
commenting works, editing is off). If your current branch already has an
open PR — say, in a per-branch worktree — it opens directly; press `b`
for the list.

The first launch runs a quick setup wizard (theme + default mode, saved
for you). Then: click files, read diffs (`v` toggles split/inline, `z`
folds), select lines (click, drag, or `V` + motions) and `c` to comment,
`e` to edit, `P` to read a markdown file as a document, `x` to mark
viewed or stage, `B` for blame, `r` to refresh, `m` (or `☰`)
for the menu, `?` for help, `q` to quit — the full keyboard and mouse
reference is in
[docs/keys-and-mouse.md](docs/keys-and-mouse.md).

## Documentation

| | |
| --- | --- |
| [Documentation index](docs/README.md) | Every page, and a table of where each feature is written up |
| [Getting started](docs/getting-started.md) | Requirements, installation, first run, a tour, troubleshooting |
| [Reviewing pull requests](docs/reviewing-prs.md) | Picker, checkout modes, comments, the review box, viewed sync, editing |
| [Reviewing local changes](docs/local-changes.md) | Local mode, the staging column, merge conflicts, keeping up with an agent |
| [Configuration](docs/configuration.md) | Config file, all keys, CLI flags, themes |
| [Keyboard & mouse reference](docs/keys-and-mouse.md) | Every binding and clickable control, and how each feature works |
| [Agent context](docs/agent-context.md) | Give your coding agent the file and lines you are reading, with one hook |
| [Architecture](docs/architecture.md) | How Loupe works inside — start here to contribute |

The features that work in both review modes — [pinned
files](docs/keys-and-mouse.md#pinned-files), the [markdown
preview](docs/keys-and-mouse.md#markdown-preview), the [blame
pane](docs/keys-and-mouse.md#the-blame-pane),
[find](docs/keys-and-mouse.md#find), [language
servers](docs/keys-and-mouse.md#language-servers), the
[editor](docs/keys-and-mouse.md#editor) and [agent
context](docs/agent-context.md) — are written up in the
reference and listed in the [documentation index](docs/README.md).

Configuration lives in `~/.config/loupe/config.toml` plus an optional
per-repo `.loupe.toml` (see
[`config.example.toml`](config.example.toml)) — upstream `org` for fork
workflows, default mode, light/dark `appearance`, syntax themes, panel
width, the blame pane, language servers, and the idle re-scan. You'll
rarely edit it by hand: the setup wizard and theme picker write it for
you, and `loupe setup` re-runs the wizard whenever you want.

## Contributing

Contributions are welcome — bug reports, docs, and code. Start with the
[contribution guide](CONTRIBUTING.md) and the
[architecture overview](docs/architecture.md). This project follows a
[code of conduct](CODE_OF_CONDUCT.md); security issues go through the
[security policy](SECURITY.md).

## License

[MIT](LICENSE)

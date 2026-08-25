# Changelog

All notable changes to Loupe are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Right-click a file-panel row to copy its path**: a small menu opens
  at the pointer with *Copy relative path* (`r`) — `src/app.rs`, the way
  git spells it — and *Copy full path* (`f`), the same path from the
  root of the disk. Folder rows work too. Paths are the one thing in the
  file panel that dragging cannot copy, because the panel shows them
  shortened and split across tree rows, and they are what a shell
  command or an agent prompt wants. The menu also opens while the editor
  is up, since copying a path changes nothing on screen.

- **Reverting changes from inside the review**: a `↺` in the change bar
  beside every changed section of the diff, and one at the end of every
  file row. Clicking a section marker — or `u` — puts that run of lines
  back and leaves the rest of the file, and the index, alone; the file
  marker — or `U` — reverts the whole file through
  `git checkout <old> -- <path>`, index included, and deletes a file the
  change had created. Both confirm first (this is the one thing git
  can't undo for you), a file that moved on disk since it was loaded is
  refused rather than overwritten, and a read-only PR review offers
  neither and spends none of the width.
- **Search**, in three shapes behind one overlay (`Ctrl+P`, or the
  `🔍 Find` button): fuzzy file matching over the changeset — `Tab`
  widens it to the whole repository — `#` to grep inside those files via
  a single `git grep`, and `@` to list what a file defines. Results
  carry the source line, mark which hits are definitions, and say which
  files are part of the change. Searches read the commit under review
  rather than the working tree, so what you find is what you're reading.
- **`/` searches the open diff** incrementally, highlighting every match
  in place; `n` / `N` step through them and wrap, and jumping into a
  collapsed section expands it on the way. `Esc` unwinds one layer at a
  time — selection, then search, then back.
- **Opening a file that isn't in the change** — from a search result or
  a jump to a definition — opens it in the editor rather than a diff of
  the file against itself. The review underneath is untouched, so `Esc`
  needs no reload and loses nothing. A file read from the commit
  (because the branch isn't checked out) opens read-only and says so.
- **Copying**, with character-level selection: drag through the diff to
  select exactly the characters you want — mid-word, across lines, on
  either side — and `y`, `Ctrl+C`, or the `⧉ Copy` button puts precisely
  that on the clipboard. Dragging past the edge scrolls and keeps
  selecting; the selection stays pinned to the side it started on, since
  the two panes are different documents. A plain click still selects the
  whole line, which is what review comments anchor to, and `V` still
  starts a keyboard line selection. Copying uses
  `pbcopy`/`wl-copy`/`xclip`/`xsel` when available and falls back to
  OSC 52 so it works over SSH; `Ctrl+C` copies in the editor too.
- **Language servers** for go-to-definition (`gd`), find-all-references
  (`gr`) and hover (`K`), driving whatever is already installed —
  `typescript-language-server`, `gopls` or `rust-analyzer`. Loupe
  bundles nothing and installs nothing: it starts the server on demand,
  sends it the buffer on screen rather than the file on disk (in PR
  review those differ), waits out a cold index instead of reporting a
  wrong empty answer, and falls back to pattern matching where there is
  no server. `loupe --lsp` reports what was found and how to install the
  rest; `language_servers = false` turns it all off.
- **Light-terminal support**: Loupe now asks the terminal for its
  background color at startup (OSC 11, with a `COLORFGBG` fallback and a
  device-attributes sentinel so a terminal that ignores the query costs
  a millisecond rather than a timeout) and switches its whole palette to
  match. The diff backgrounds were the worst of it — near-black green
  and red slabs on a white terminal — but gutters, fold banners,
  buttons, badges, the divider, and every status color are tuned per
  appearance now too.
- **Two theme slots**: `theme` (dark terminals) and `light_theme` (light
  ones), so switching terminals doesn't overwrite the other choice. An
  unset slot borrows from the one that is set and stays in the same
  family where a counterpart exists — `gruvbox-dark` implies
  `gruvbox-light`.
- **Appearance overrides**: `appearance = "auto" | "light" | "dark"` in
  the config, `--light` / `--dark` for one session, and `a` in the theme
  picker (and in the wizard) to flip live — which carries the theme
  selection to its counterpart and saves both.
- **`loupe appearance`**: prints the background color your terminal
  reported and what Loupe would do with it, for when the guess is wrong.
- **`loupe set-theme --light <name>`**: save the light-terminal theme
  from the shell.
- **First-launch setup wizard**: a welcome screen, a theme choice with a
  live-highlighted preview, and a default-mode choice, saved to the
  config file automatically. Runs once (skippable), and on demand via
  `loupe setup`.
- **In-app theme picker** (`t` or the 🎨 Theme button, in both the PR
  list and review screens): previews each of the 32 themes live on a
  code sample with diff backgrounds; Enter applies it, re-highlights the
  open file, and persists the choice to the global config (preserving
  comments and unrelated keys); Esc restores the previous theme.
- **Theme CLI**: `loupe --theme <name>` for a one-session override and
  `loupe set-theme <name>` to persist a theme from the shell.

### Fixed

- The top bar no longer paints its buttons over the PR title or branch
  name on a narrow terminal: the row now reserves space for what is on
  its left and drops the leftmost buttons when there isn't room. The
  one-column gaps between buttons are painted too, so the text
  underneath stops showing through them a letter at a time.
- The finder overlay sizes itself to its results instead of always
  taking the full height.

- **The editor is connected to the language server too**, not just the
  diff view:
  - **Diagnostics as you type**, without saving — `●`/`▲` in the gutter,
    the offending span colored, the message in the status bar for the
    cursor line and a count for the file otherwise. The buffer is pushed
    to the server 220 ms after you stop typing, so what it checks is what
    is on screen rather than what is on disk.
  - **Completion** (`Ctrl+Space`, and automatically after `.`, `:` or
    `>`): a popup under the cursor, narrowing as you type without another
    round trip, `Tab`/`Enter` to accept, `Esc` to dismiss.
  - **`Ctrl+G`** for the type and docs at the cursor and **`Ctrl+]`** to
    go to the definition — the diff view's `K` and `gd` can't work in an
    editor, where plain letters are text.
  - **`Ctrl+T`** (or the `⇥ Format` button) formats the file;
    `format_on_save = true` does it on every save. Off by default, since
    reformatting mid-review adds diff noise nobody asked for.

### Fixed

- The help overlay said undo was `Ctrl+Z` / `Ctrl+Y`. `tui-textarea`
  binds `Ctrl+U` / `Ctrl+R`; `Ctrl+Z` is now also bound to undo, and the
  documented keys match what the editor actually does.
- Undoing a format takes one keystroke. Replacing the whole buffer costs
  two of the editor's undo steps, so a single undo used to leave the file
  looking empty.

### Changed

- `n` / `p` no longer step through files — they are next / previous
  search match, as in vim. `]` / `[`, which always did the same job,
  remain the file keys.

- Default theme is now **catppuccin-mocha** on a dark terminal and
  **catppuccin-latte** on a light one (was one-half-dark unconditionally;
  both still available by name).
- The theme picker's sample panel is painted in the previewed theme's own
  background, so a light theme looks light before you commit to it.
- Themes switch at runtime — the syntax theme is no longer baked in at
  startup.


## [0.1.0] - 2026-08-23

Initial public release.

### Added

- **Pull request review**: clickable picker of open PRs; branch auto-open
  (the current branch's open PR opens directly in editable mode);
  Checkout & review vs. Review only (no working-tree changes) modes.
- **Local-changes review**: with no flags, uncommitted work (staged +
  unstaged + untracked) opens for review; the file panel stages files in
  place (`[+]` / `[±]` / `[✓]`), driven by real `git status` state, with
  rename-aware staging and unstaging that never touches file contents.
- **Diff view**: side-by-side and inline layouts; syntax highlighting via
  bat's extended syntax and theme set (32 themes, pure-Rust engine);
  folding of unchanged runs with per-run expand/collapse; pinned-gutter
  horizontal scrolling for wide lines; resizable file panel with
  drag/keyboard/double-click-reset.
- **Vim-style keyboard navigation**: an underlined cursor row in the
  diff driven by `j`/`k`, `Ctrl+D`/`Ctrl+U`, `Ctrl+F`/`Ctrl+B`,
  `gg`/`G`, `{`/`}`, `H`/`M`/`L`, `Ctrl+E`/`Ctrl+Y`, `0`/`$`; `V` line
  selections; fold/expand and edit at the cursor — the whole review flow
  works mouse-free, and clicking a line places the same cursor.
- **Review comments**: single- and multi-line comments on either diff
  side, posted through `gh`; comment anchoring guarded against local
  edits diverging from the PR head.
- **Viewed checkboxes** synced bidirectionally with GitHub's per-file
  viewed state.
- **In-place editor** on the new side with live incremental syntax
  highlighting, mouse selection, undo/redo, and save-refreshes-diff.
- **Configuration**: global config file plus per-repo `.loupe.toml`
  (upstream `org` for fork/multi-org workflows, default `mode`, syntax
  `theme`, `file_panel_width`); strict unknown-key rejection; `--pr`,
  `--local`, `--auto`, `--themes` CLI flags.
- **Responsiveness**: all git/GitHub/diff/highlight work on background
  threads with a cancellable spinner; near-zero idle CPU.
- **Hardening**: PR-controlled paths, oids, and refs validated before
  reaching `git`; no shell execution anywhere; symlink-safe editing.

[Unreleased]: https://github.com/jjacoblee/Loupe/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/jjacoblee/Loupe/releases/tag/v0.1.0

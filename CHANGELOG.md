# Changelog

All notable changes to Loupe are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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

### Changed

- Default theme is now **catppuccin-mocha** (was one-half-dark; still
  available by name).
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

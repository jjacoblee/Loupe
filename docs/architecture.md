# Architecture

An orientation for contributors. Loupe is a single Rust crate with a
deliberately small dependency set; this page maps the modules, the
threading model, and the invariants that are easy to break by accident.

## Module map

| Module | Responsibility |
| --- | --- |
| `main.rs` | CLI parsing, config load, terminal setup/teardown, the event loop |
| `app.rs` | All application state (`App`), input handling, background-job orchestration |
| `ui.rs` | Rendering: layout, file panel, diff view, buttons, overlays, status bar |
| `diff.rs` | The diff engine: `FileDiff` computation, folding, display entries, line widths |
| `editor.rs` | The in-place editor: a custom renderer over `tui-textarea` |
| `gitops.rs` | Everything that shells out to `git`: local scans, staging, refs, file content |
| `github.rs` | Everything that shells out to `gh`: PR lists, details, comments, viewed sync |
| `highlight.rs` | Syntax highlighting via syntect + two-face: themes, caching, incremental editor highlighting |
| `config.rs` | TOML config discovery, parsing, and merging (global + per-repo) |

There is no shell anywhere: `git` and `gh` are always invoked with
argv-style `Command` arguments, never through `sh -c`.

## The event loop and the dirty flag

`main.rs` runs a poll loop: pick up finished background jobs
(`app.poll_jobs()`), draw if anything changed, then wait for input with a
short timeout while busy (spinner animation) and a longer one when idle.
Pending input events are **drained** before the next draw, so a fast
mouse drag pays for one redraw, not one per event. Idle CPU is
approximately zero.

**Invariant:** anything that changes visible state must arrive through an
input event, make `poll_jobs()` return `true`, or be covered by
`busy()` — a new mutation path outside those must set the dirty flag
itself, or the change won't render until the next unrelated event.

## Background jobs

All blocking work — `gh` calls, `git` calls, diff computation, syntax
highlighting — runs on background threads that send an `Outcome` over an
mpsc channel; `poll_jobs` applies results on the UI thread.

- **Foreground jobs** are modal: a spinner and a job label show in the
  status bar, and cancellable jobs die on `c`/`Esc` (the result is
  dropped; the worker thread finishes on its own).
- **Fire-and-forget jobs** (viewed-state sync, staging) apply an
  optimistic local change first and revert it if the job fails. Staging
  results carry a fresh read of the index, which is adopted wholesale —
  only a real `git status` knows about partial staging.

## The diff pipeline

1. `gitops`/`github` produce old/new file contents (old side = merge base
   of the PR's base and head; in local mode, `HEAD`).
2. `diff.rs` computes a `FileDiff` with the `similar` crate under a
   500 ms timeout — a pathological file degrades to a coarser diff
   instead of hanging the load job.
3. Old- and new-side syntax highlighting run **in parallel** in the load
   job (they dominate open latency: ~370 ms per side on a 3k-line file).
4. Rendering walks a `DisplayEntry` layer (rows, `Fold` markers, `Unfold`
   headers) so folding is a view-time concern, and overlays green/red/
   selection backgrounds on top of the highlight spans.

Tab expansion and `FileDiff::max_width` share `diff::TAB_WIDTH` — the
horizontal-scroll clamp and the renderer must agree on tab width or
scrolling drifts on tab-indented files.

## Syntax highlighting

The stock syntect syntax set is missing TypeScript, TSX, TOML, Dockerfile
and more, and its stock themes are muted — Loupe uses
[two-face](https://crates.io/crates/two-face) (bat's extended syntax and
theme set) with the pure-Rust `fancy-regex` engine, so there is no C
dependency. Full-file highlighting is *never* done per keystroke:

- Diff views highlight once, in the background load job.
- The editor uses `EditorHighlight`, an incremental per-line cache: on an
  edit it re-highlights from the first changed line only until the
  parser state re-converges with the cached suffix (~0.2 ms per
  keystroke vs. 280 ms for a full 3k-line recompute). Files over 8,000
  lines skip editor highlighting entirely to avoid a multi-second open.

`highlight::warm()` starts deserializing the syntax set during the first
network call so the first file open doesn't pay for it.

## The editor

`tui-textarea` owns editing state (buffer, cursor, selection, undo), but
its widget rendering is replaced entirely: `editor.rs` renders the buffer
itself to get per-token syntax colors, selection highlighting, and the
cursor. Because `tui-textarea` exposes no viewport getter, `editor.rs`
replicates its deterministic scroll logic as a **shadow viewport** used
for mouse→cursor mapping.

**Invariant:** `PgUp`/`PgDn`/`Ctrl+V`/`Alt+V` must be intercepted
*before* `textarea.input()` — its internal scroll handling would desync
the shadow viewport.

## Talking to git and GitHub

- All GitHub access goes through the `gh` CLI — the user's existing
  login; Loupe never sees a token.
- Paginated `gh api` output is consumed as NDJSON via `--jq`, never by
  string-concatenating pages (patch text can contain `][`).
- PR content is treated as untrusted input. `safe_repo_path` rejects
  absolute/`..`/prefix path components before joining API-provided paths
  onto the repo root; head/base oids are validated as 40/64-char hex
  before being spliced into `git show`/`git merge-base`; fetched base
  refs are qualified as `refs/heads/…` so a branch name can't parse as an
  option; and the editor refuses to open or save through a symlink. See
  [SECURITY.md](../SECURITY.md).
- `git show <oid>:<path>` with an **empty** oid reads the index, not
  HEAD — any call that could receive an empty oid must guard against it.
- File paths from the GitHub API are repo-root-relative; filesystem
  operations join them onto `git rev-parse --show-toplevel`, and
  staging/pathspec operations pass `-C <repo_root>` because pathspecs are
  cwd-relative.

## Testing

`cargo test` runs the full suite (no network, no GitHub): diff engine and
folding semantics, tree building, highlight (including incremental ==
fresh equivalence), `-z` porcelain parsers verified against real git
output, config parsing and merge precedence, CLI parsing, scroll and
resize clamps — plus `TestBackend` render tests that assert syntax colors
actually reach the diff cells, and two tests that drive a **real
temporary git repository** to keep the staging plumbing honest. Clippy is
kept at zero warnings across all targets.

## Performance ground rules

Measured baselines live in the commit history; the standing rules:

- Nothing blocking on the UI thread — if it can take more than a frame,
  it's a job.
- No full-file highlight per keystroke, ever.
- Idle means idle: don't add periodic redraws; respect the dirty flag.
- Renderers coalesce same-style runs into single spans — no per-character
  `Span` allocation in hot paths.

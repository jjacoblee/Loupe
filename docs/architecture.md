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
| `theme.rs` | Light/dark appearance: terminal background detection and the two UI color palettes |
| `config.rs` | TOML config discovery, parsing, and merging (global + per-repo) |
| `search.rs` | Fuzzy path matching, the pattern-based definition scanner, and `git grep` |
| `lsp.rs` | Language servers: process lifecycle, JSON-RPC over stdio, symbols/definition/references/hover |
| `clipboard.rs` | Copying out: a clipboard command when there is one, OSC 52 when there isn't |

There is no shell anywhere: `git` and `gh` are always invoked with
argv-style `Command` arguments, never through `sh -c`.

## Color

Two sources of color meet in the diff, and they have to agree about the
background they are drawn on:

- **`theme.rs`** owns everything Loupe paints itself — diff backgrounds,
  gutters, buttons, borders, status text — as two `Palette` constants,
  `DARK` and `LIGHT`. `ui.rs`, `editor.rs`, and `wizard.rs` hold no color
  literals; they call `palette()`, which reads one atomic and hands back
  a `&'static Palette`, cheap enough to call per rendered row.
- **`highlight.rs`** owns the syntax colors, which come from the syntect
  theme. Themes classify themselves as light or dark by their own
  background color, and `for_appearance` maps one to its counterpart.

`main.rs` settles the appearance once, immediately after
`ratatui::init()` — raw mode is on, and nothing is reading events yet, so
that is the only safe window to write an OSC 11 query to `/dev/tty` and
read the reply without it being mistaken for a keystroke. A
device-attributes request is sent right behind the query as a sentinel:
its reply arriving first means the terminal will never answer OSC 11, so
detection gives up immediately instead of waiting out its 120 ms budget.

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

## Reverting

`FileDiff::section_at` groups rows into *sections* — maximal runs of
non-context rows, the same unit `{` / `}` jumps between — and
`revert_section` rebuilds the whole new side with one section taken from
the old text instead. Rebuilding from the row model rather than splicing
by line number is what keeps the bytes written in step with what is on
screen; the trailing newline follows whichever side supplied the last
line.

A section revert writes that text to the working tree and nothing else,
after checking the file on disk still matches what the diff was computed
from (otherwise the row model is stale and the write is refused). A
whole-file revert goes through `gitops::revert_path`:
`git checkout <rev> -- <path>`, which moves the index as well as the file
— so a reverted file stops showing as changed instead of lingering as a
staged edit with an empty diff — or, when the path does not exist at
`<rev>`, `git rm -f --ignore-unmatch` plus an unlink. Either way the file
is reloaded through the same `load_file_data` the silent refresh uses, so
the reader keeps their place.

`App::revert_gutter` is zero unless the working tree is what is under
review, and all the diff geometry measures from `App::diff_body` (the
pane minus that gutter), so a read-only review lays out exactly as it did
before the change bar existed.

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

Replacing the whole buffer (which is what formatting does) costs *two* of
`tui-textarea`'s undo steps — the delete and the insert — so a single
undo would leave the file looking empty. `Editor::pre_format` holds the
previous text and `undo_format` puts it back whole, which is what the
message on screen promises.

## Finding things

Three tiers, in rising order of cost and falling order of availability:

1. **Fuzzy path matching and the definition scanner** (`search.rs`) — in
   memory, no subprocess, always available. The scanner is a pattern
   matcher, not a parser: it is allowed to miss an exotic definition, but
   it must never invent one, because a wrong symbol sends the reader to
   the wrong line.
2. **`git grep`** — one subprocess per query, debounced by 140 ms so
   typing a word costs one process rather than one per keystroke. Git
   already knows what is tracked, ignored and binary, and can read a
   *commit* rather than the working tree — which matters, because in PR
   review the file on disk may belong to another branch.
3. **A language server** (`lsp.rs`) — whatever is on `PATH`, started on
   demand, one per language, kept for the session.

Each tier degrades into the one above it, and says so rather than going
quiet: no server for Kotlin means `@` still lists definitions, from
patterns.

## Language servers

Hand-rolled JSON-RPC over stdio (`Content-Length` framing, `serde_json`
for the bodies) rather than a protocol crate — it is about two hundred
lines and adds no dependency.

**Every call blocks**, so every call runs on a worker thread through the
job engine. The registry is cloneable and internally locked, one lock per
language, so a slow question about Rust doesn't hold up a question about
TypeScript.

Three things are easy to get wrong here, and all three are load-bearing:

- **The buffer, not the path.** Documents are opened with
  `textDocument/didOpen` carrying the text on screen. Pointing the server
  at the file would answer a question about code the reader isn't looking
  at.
- **Server-to-client requests must be answered.** `gopls` will not finish
  starting until `workspace/configuration` comes back, so the message
  pump replies to requests as well as reading responses.
- **`window.workDoneProgress` must be declared**, or the server never
  sends `$/progress` — and without progress there is no way to tell "no
  references" from "not indexed yet". A cold `rust-analyzer` answers
  `documentSymbol` in 25 ms and `references` with an empty list until its
  index is built; `request_when_ready` re-asks while progress is
  outstanding.

**Diagnostics are pushed, not requested**, which is why `handle_incoming`
exists: one place that reads every incoming message, absorbs
notifications and server-to-client requests, and hands back only
responses. `Lsp::poll` drains it from the main loop with `try_lock`
throughout — a redraw must never wait on a language server, and a skipped
tick costs nothing because the next one picks the messages up.

The editor pushes its buffer 220 ms after typing stops (`sync_open`, also
`try_lock`, and it never *starts* a server). Editor requests go through
`EditorJob`: non-modal like the finder's, and generation-guarded, because
an answer about text you have already typed past is worse than no answer.

`typescript-language-server` needs pointing at a `tsserver.js`
(`tsserver` itself speaks its own protocol, not LSP). Loupe prefers the
project's own `node_modules` copy so a pinned TypeScript is what does the
analysis.

## Copying, and why it needs code at all

Mouse reporting is what makes the file tree, the fold banners and the
diff lines clickable — and it takes the terminal's own click-drag
selection away in exchange. Most terminals hand it back while a modifier
is held (Option on macOS, Shift elsewhere), which covers "grab what's on
screen" but not "copy the lines this PR deleted": those are on the old
side of the diff, and the selection model already knows which side you
picked. So `y` copies from the *selected side*, which is the only way to
get deleted code out — it exists nowhere on disk.

One `Selection` serves both readers at different granularities:
character positions plus a `linewise` flag. A click or `V` sets it (whole
lines, which is the only thing GitHub can anchor a comment to); dragging
clears it and the selection becomes exactly the characters covered.
`Selection::cols_on` is what the renderer paints *and* what the clipboard
copies, so the highlight can never promise something different from what
you get.

`clipboard.rs` tries a clipboard command first (`pbcopy`, `wl-copy`,
`xclip`, `xsel`, `clip.exe`), then falls back to OSC 52, which asks the
terminal itself and is the only thing that works over SSH — where the
commands above would set the clipboard of the wrong machine. No
clipboard crate: the fallback needs base64 and nothing else.

## Files outside the changeset

A search result, or a jump to a definition, can land in a file the change
never touches. There is nothing to diff it against, so it opens in the
**editor** rather than the diff view — a file shown as a file. The review
state is not disturbed at all, which is what makes closing it free:
no reload, no restored scroll position, nothing to get wrong.

When the branch under review isn't checked out, the working tree belongs
to some other branch; the text then comes from the commit and the editor
is marked read-only, because saving would write over an unrelated
branch's file.

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

`cargo test` runs the full suite (no network, no GitHub — but two tests
*do* start a real language server when one is installed, and skip
themselves when it isn't; they are the only check that the handshake
still works against a real implementation): diff engine and
folding semantics, tree building, highlight (including incremental ==
fresh equivalence), `-z` porcelain parsers verified against real git
output, config parsing and merge precedence, CLI parsing, scroll and
resize clamps, fuzzy ranking, the definition scanner, LSP message
framing and every shape a `definition` answer can take — plus
`TestBackend` render tests that assert syntax colors and search
highlights actually reach the diff cells, and tests that drive a **real
temporary git repository** to keep the staging and `git grep` plumbing
honest. Clippy is
kept at zero warnings across all targets.

## Performance ground rules

Measured baselines live in the commit history; the standing rules:

- Nothing blocking on the UI thread — if it can take more than a frame,
  it's a job.
- No full-file highlight per keystroke, ever.
- Idle means idle: don't add periodic redraws; respect the dirty flag.
- Renderers coalesce same-style runs into single spans — no per-character
  `Span` allocation in hot paths.

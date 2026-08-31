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
| `diff.rs` | The diff engine: `FileDiff` computation, the three-version `stack`, folding, display entries, line widths |
| `conflict.rs` | Merge conflict markers: parsing them into hunks, building the two sides, writing a resolution back |
| — | Reviews live in `github.rs` (the request) and `app.rs` (the held batch and the composer) |
| `blame.rs` | `git blame --porcelain` parsing, the age heat ramp, the change set |
| `editor.rs` | The editor buffers: a custom renderer over `tui-textarea`, find and replace, the comment toggle |
| `markdown.rs` | Markdown → styled lines: the block/inline parser and the width-dependent layout |
| `preview.rs` | The preview pane: scrolling, the source-line map, reload, and the scrollbar |
| `pins.rs` | Pinned files: the tab list, the state file, and reading a dropped path out of a paste |
| `gitops.rs` | Everything that shells out to `git`: local scans, staging, stashes, commits, refs, file content |
| `github.rs` | Everything that shells out to `gh`: PR lists, details, comments, viewed sync, stacks |
| `highlight.rs` | Syntax highlighting via syntect + two-face: themes, caching, incremental editor highlighting |
| `theme.rs` | Light/dark appearance: terminal background detection and the two UI color palettes |
| `config.rs` | TOML config discovery, parsing, and merging (global + per-repo) |
| `search.rs` | Fuzzy path matching, the pattern-based definition scanner, and `git grep` |
| `lsp.rs` | Language servers: process lifecycle, JSON-RPC over stdio, symbols/definition/references/hover/completion |
| `linter.rs` | Linters: running one over the buffer on stdin, and turning its JSON into diagnostics |
| `explain.rs` | Splitting a diagnostic message into the claim, its reasons, and the names quoted inside them |
| `clipboard.rs` | Copying out: a clipboard command when there is one, OSC 52 when there isn't |
| `ctx.rs` | The context provider: the snapshot of what is on screen, and the unix socket that serves it |
| `hooks.rs` | Installing that context into a coding agent: the `UserPromptSubmit` merge for Claude Code and Codex |
| `wizard.rs` | The first-launch setup wizard: theme, default mode, and the agent hook |

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

A diff row's background comes from two questions, not one: which side of
the change it is on, and — with the layers on — which step of the change
wrote it. `Palette::layer` answers the second by handing back a `Shades`
of four colors, and `ui::diff_bg` picks one of the four by side and row
kind. `Layer::Local` reads its shades off the flat `added` / `removed`
fields, so an ordinary two-way diff paints exactly what it always did.

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

## The blame pane

`blame.rs` reads `git blame --porcelain` and hands back one commit per
file line. Three things about it are load-bearing:

- **It is not part of the file load.** A `git blame` on a long file costs
  about as much as the whole rest of the load, and folding it in would
  undo the open latency the diff pipeline is built around. It is its own
  background job, guarded by a generation counter the way `SearchJob` and
  `EditorJob` are, and the pane says `loading…` until it lands.
- **The porcelain format repeats a commit header only once.** Later lines
  of the same commit carry the hash alone, so the parser keeps a hash →
  `Commit` map. `--line-porcelain` would be simpler and several times
  larger on a file whose history is one commit deep.
- **The pane walks the same window `draw_diff` walks** —
  `display.iter().skip(diff_scroll).take(h)` — so the two cannot drift.
  Which side a row is blamed on follows what the row shows: an inline row
  names its side; a split row prefers the new side and falls back to the
  old one, which is the only side a purely removed row has.

The colors come from `theme.rs` like everything else Loupe paints, in one
absolute six-step age ramp plus two classes that outrank it: a line the
working tree owns, and a line from a commit in the change under review
(`base..head` for a pull request, `HEAD --not --remotes` locally). Two
properties of the ramp are load-bearing rather than decorative:

- **It is absolute**, not relative to the file, so a shade means the same
  age everywhere and the scale is learned once rather than per file.
- **It is one hue** — the palette's own neutral, lightness only. A ramp
  that spent color would compete with the two classes above it, and those
  are the whole point of the pane: they are what answers "is this related
  to what I am doing now?". `theme::tests` asserts the ramp stays grey
  and monotonic and that the two classes stay saturated, because a later
  palette tweak that quietly colored the ramp would cost the signal
  without breaking anything visible.

Pull request numbers come from the commit subject first — a squash or
merge commit names its own — and from one batched `gh api graphql` call
per file for the rest, cached by hash for the session. A hash already
asked about is never asked again, so a repository full of direct pushes
costs one call and then nothing. The number is **never truncated to fit**:
a shortened number is a link to a real but wrong pull request, so a number
too wide for the column renders as `#…`.

## The file panel

The panel shows one of three lists, chosen by `App::panel`: the files the
change touches, every file in the repository, or the commits this branch
has that the upstream does not. The first two are separate all the way
down — their own `TreeNodes`, their own collapse set — because a folder
closed in one is a folder the reader never touched in the other. The third
has no tree at all: a commit is a row, and its files are rows under it.

### Staged and unstaged

A local review divides the change into a `STAGED` and an `UNSTAGED`
section, headed by `FileEntry::StageHeading`. **Both sections are the one
cached tree, emitted twice under a filter** (`TreeNodes::emit_where`).
Nothing is rebuilt when a file moves across, which matters because the
index moves on every click of the staging column and again on every
background re-read.

Emitting under a filter has one consequence the unfiltered walk does not
have: a directory the filter empties would draw with nothing under it. So
`walk` takes a `prune` flag — it rolls the directory row back when the
recursion added nothing, and asks `node_keeps` directly for a collapsed
one, whose rows are never emitted to count. `prune` is off for the panel
that lists the repository, where a directory git would not walk into is
legitimately empty until somebody opens it.

`App::section_of` is the single answer to which half a file is in. A
partly staged file is in the index and in the working tree at once; it is
listed once, under `STAGED`, because two sections whose counts add up to
more than the change has files would be worse than either count alone.
`staged_count` — the panel title — asks the same function, so the title and
the heading can never disagree.

Because the sections reorder the rows, index order and row order are no
longer the same thing, and `step_file` walks `entries` rather than `files`.
It falls back to the list when the open file has no row: a folded section
holds it, or the panel is showing something else.

**The split that matters is build against emit.** `TreeNodes::build` walks
every path into a `BTreeMap` tree; `TreeNodes::emit` walks that tree and
produces the visible rows. Measured, the build is 99% of the cost:

| Paths | Build | Emit, collapsed | Emit, expanded |
| --- | --- | --- | --- |
| 16,761 | 3.95 ms | 500 ns | 724 µs |
| 400,000 | 48.25 ms | 1.29 µs | 9.72 ms |

**Invariant:** `rebuild_entries` emits and nothing else. It is on the click
path for every collapse and expand. `rebuild_files` is the one that builds,
and every path that assigns `App::files` has to call it — a debug assertion
on the file count fails a test rather than letting a stale tree draw the
previous change's rows. `app::tests::emitting_rows_does_not_rebuild_the_tree`
asserts ten emits cost less than one build, as a ratio rather than a
wall-clock bound so a slow machine does not fail the suite.

Files mode opens with every directory collapsed. That is not a nicety:
expanded, a 400,000-path tree allocates a 410,257-entry `Vec` on every
toggle, and collapsed it allocates seven. The collapse set comes from the
same walk that emits the rows, because single-child directory chains are
compressed into one row with a joined label and a set built from raw path
prefixes would match none of them.

### Two `git ls-files` calls

The listing runs both on a worker thread:

- `--cached --others --exclude-standard` — tracked and untracked files.
- `--others --ignored --exclude-standard --directory` — the ignored ones.

`--directory` is what makes the second affordable. Without it git walks
into every ignored directory: on one real repository that is 17,504 entries
and 147 ms, nearly all of it `node_modules`. With it git stops at a
directory whose whole contents are ignored and names the directory — 10
entries and 14 ms. Which is also the useful split: an ignored *file* like
`.env` is something a developer opens, and an ignored *directory* is noise
until asked for.

Git also reports a directory it would not walk into for a nested repository
or a linked worktree inside the clone. Both arrive as a path with a
trailing slash, which `search::list_files` returns separately from the
files — mixed in, the tree builder pops an empty last component and draws a
file row with no name.

Those become **stubs**: a `Node` with `stub: true` and nothing under it.
Expanding one reads that directory, one level, on a worker thread. A
subdirectory found inside becomes a stub of its own, and a symlinked
directory counts as a file, because following one that points at an
ancestor draws a tree with no bottom.

**Invariant:** paths are only ever appended to `App::repo_paths`. Every
`RowSrc::Path` on screen holds an index into it, so inserting or removing
in the middle repoints rows at the wrong file. A deleted file keeps its
slot; only the tree forgets it.

Whether a path is ignored is a flag per path, not a split point, because
everything inside `node_modules` is ignored wherever it lands in the list
after a read. Which of the two kinds a stub is matters for the same reason:
a nested worktree holds ordinary files, and inheriting "ignored" from every
stub would draw a whole worktree as if it were `node_modules`.

Above `MAX_EAGER_PATHS` the tree keeps the top level only and stubs every
directory. The build is not what forces that — 48 ms is affordable on a
worker — the memory is.

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

### The words that changed

A `RowKind::Modified` row is the only kind with a counterpart to differ
from, so it is the only kind that carries `old_words` / `new_words` —
character ranges the renderer paints a stronger shade of the row's own
colour. A whole added or removed line is all change, and a second shade
on it would mean nothing.

The tokenizer is loupe's own rather than `similar`'s. `TextDiff::from_words`
breaks on whitespace and nothing else, which makes
`Client::builder().timeout(timeout)` a single word — one renamed argument
inside it then reads as the whole expression changing, and the guard
below throws the highlight away entirely. Here a run of identifier
characters is one token, a run of whitespace is one token, and every
other character stands alone, so a changed bracket is a changed bracket.

Two guards keep it from saying nothing. A line longer than
`MAX_WORD_DIFF` is left alone: a minified bundle is one line of a hundred
thousand characters, and word-diffing two of those costs more than the
rest of the file. And past `WORD_DIFF_MAX_SHARE` of a line the ranges are
dropped — two lines that share almost nothing are a rewrite, and painting
nine tenths of both of them darker is louder than painting neither.

The renderer takes it as a third overlay in `hl_body`, below the search
hit and the selection: those are what the reader is doing, and this is a
standing property of the line that can afford to give way.

## Reviews

An inline comment has two exits, and they are different requests.
*Post now* is `POST /pulls/{n}/comments` — one comment, immediately.
*Add to review* touches no network at all: the comment goes into
`App::pending` and to disk, and reaches GitHub only when the review is
submitted as `POST /pulls/{n}/reviews`, carrying the summary, the
`event` (`COMMENT` / `APPROVE` / `REQUEST_CHANGES`) and every held comment
in one body.

That endpoint takes an array of comment objects, which `gh api -f k=v`
cannot express — so `run_gh_stdin` pipes JSON to `--input -` instead.
`review_payload` builds it and omits empty halves: GitHub reads a
present-but-blank `body` as a body, and a single-line comment carrying
`start_line` equal to `line` as a malformed range.

Held comments live in `.git/loupe/pending-review-<number>.json`, written
on every change. Under the git directory rather than the working tree:
per-clone state, never committed by accident, and it travels with the
checkout the comments were anchored against. `load_pending` runs when a PR
opens — after `pr` is set, since the file is keyed by number.

Two anchoring facts shape the rest:

- **Comments anchor to a commit, and the review carries one `commit_id`
  for all of them.** `pending_commit` records the head they were written
  against; a head that has since moved makes the confirm prompt warn,
  because GitHub refuses the *whole* review over one bad anchor.
- **The whole review is accepted or refused together.** A failure
  therefore leaves the batch untouched, so nothing written is ever lost to
  a rejection.

`pending_on_row` puts the `💬` in the change bar. It maps a display row to
a (side, line) and asks whether any held comment covers it — rather than
indexing by comment, because the same line can be reached from either view
mode and only the row model knows which side a row is showing.

## Reading one commit

The `Commits` panel lists `gitops::unpushed_commits` against
`unpushed_base` — the branch's own upstream where it has one, and
`origin/HEAD`'s default branch where it does not, because a branch that
tracks nothing has never been pushed at all. The list is capped at 200 and
re-read when the reader comes back to the panel after 30 seconds. A
commit's file list is read on the click that opens it and kept forever: a
commit's files never change, so nothing cached about one goes stale.

Opening one of those files sets `App::open_commit` and leaves `App::files`
alone. That is the whole trick. The change under review keeps its own file
list, its own cursor and its own staging state while the diff pane shows
something that is not in it, and `App::open_file` — not `files[file_cursor]`
— is what every part of the window that describes the diff asks.

`load_file_data` used to take five positional arguments describing where
the two sides come from. It now takes a `LoadCtx`, and the three reviews
say plainly which of the two they are:

| Review | Old side | New side | From disk | Anchors comments |
| --- | --- | --- | --- | --- |
| Pull request | merge base | head oid | when checked out | yes |
| Local changes | `HEAD` | the working tree | yes | no |
| One commit | its first parent | the commit | no | no |

A commit is read from git on **both** sides. The copy on disk belongs to
`HEAD`, which for every commit but the newest is a later version of the
same file. A root commit has no first parent, so its old ref is empty and
every file in it reads as added — which is what it is.

Everything that acts on the working tree asks `open_commit` first and
refuses: staging, reverting, editing, and the idle re-scan, which would
otherwise replace a commit's diff with a working-tree one under a reader
who did not ask to move.

## The PR ⇄ local swap

`toggle_workspace` (the `` ` `` key, and the `⇄` menu line) flips between
the two reviews without losing either. It is a stash, not a reload.

`Workspace` is every field of `App` that belongs to *one* side: which side
it is, the branch and its upstream drift, the merge in progress, the
conflict view, the `PrDetail`, the merge base, the file list, the viewed
and staged marks, the file cursor and scroll, the collapsed directories,
and the open `FileDiff`. `save_workspace` moves those out of `App` with
`take`/`mem::take`, leaving defaults behind for the caller to overwrite;
`restore_workspace` puts the other side's back.

Two things are deliberately *not* stored. `entries` and `display` are
cheap derivations of the diff and the fold state, so `restore_workspace`
rebuilds them rather than carrying them. And the restored diff cursor and
scroll are clamped to the rebuilt `display`: the view mode (split or
inline) is shared between the two sides, so switching it while a side sat
stashed changes how many rows that side's stored position was counted in.

A swap shows the stashed side immediately and then re-checks it on a
background job — `spawn_quiet_pr` or `spawn_quiet_local`, the same silent
refresh path the idle re-scan uses. So the flip costs no wall-clock and
still cannot show a stale diff. Any in-flight quiet refresh is dropped
first: it belonged to the side being left.

The stash holds at most one side, and only ever the *other* one. When
`toggle_workspace` finds it empty it loads that side the long way once
(`pr_for_current_branch`, or `spawn_open_local`). Opening local review on
top of a PR — or a PR from local review — stashes the side being left by
the same route, so arriving by menu and arriving by `` ` `` converge on
one state.

A swap is refused while the editor is open: the buffer belongs to the side
being left, and its unsaved text has nowhere to go.

## Merge conflicts

A conflicted file is fed through the *same* diff pipeline, with one
substitution at step 1: instead of `HEAD` against the working tree,
`load_conflict_data` diffs **our version against theirs**.

`conflict::Conflicted::parse` reads the marker lines into a list of
`Hunk` values — line ranges for our side, their side, and the common
ancestor where git wrote one (`diff3` / `zdiff3`) — and
`Conflicted::sides` rebuilds two plain files from them: agreed lines go
into both, a hunk's lines go into one each. The diff engine then aligns
those two, so an agreed line becomes a context row that folds away and a
conflict becomes a changed section. Everything the diff view already does
— folding, `{` / `}`, search, the cursor, syntax highlighting on real
file text — works with no special case in `ui.rs` beyond the marker drawn
in the change bar.

Mapping a row back to the conflict it came from is *not* done by counting
sections: a hunk whose two sides share lines splits into several sections,
and two adjacent hunks can merge into one. `sides` therefore also returns
a per-line owner vector for each side, which `ConflictView::owner` reads
by line number — exact by construction.

Writing is the inverse: `Conflicted::apply` walks the original lines and
substitutes the chosen side for the hunks named in the pick map, leaving
every other hunk's markers exactly as they were. So resolving one
conflict is a whole-file rewrite that is byte-identical everywhere else,
and the result re-parses with one fewer hunk.

Two facts drive the rest:

- **The index is the authority, not the file text.**
  `gitops::unmerged_paths` (`git diff --diff-filter=U`) decides what is
  conflicted, so a conflict git could not express with markers — one side
  deleted the file — is still marked. Those have no hunks to resolve;
  `gitops::take_side` reads merge stage 2 or 3 out of the index instead,
  and removes the file when the chosen side deleted it.
- **git treats a path as conflicted until it is added.** Resolving the
  last hunk in a file therefore stages it, or the file would keep warning
  after the reader had settled it.

`App::revert_gutter` is shared: the same two columns carry `↺` on a
normal diff and `⚑` on a conflict view, and reverting is refused on a
conflicted path because `git checkout HEAD -- <path>` mid-merge discards
the merge for it.

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

`App::editor` is the buffer on screen and `App::parked` holds the rest.
Opening a file parks the current one; closing pops the last one forward.
The field kept its meaning deliberately: 81 places read it, every one of
them means "the buffer the reader is looking at", and an index into a
vector would have changed all of them to say the same thing.

**Invariant:** `editor.is_none()` implies `parked.is_empty()`. Every "close
the editor first" guard depends on it.

`q` counts through `dirty_buffers()` rather than looking at the active
buffer. One buffer guarded itself — Esc armed, Esc again discarded — but
`q` reaches past that, and with several open it could take unsaved work in
files that were not on screen.

A `WorkspaceEdit` from a rename or a code action is applied to buffers,
never to disk. Files it touches that are not open are opened as unsaved
buffers; a file already open is edited where it stands, so unsaved work in
it is never read over. The reader lands back where they started.

### The renderer

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

### Find and replace

`Editor::find` is its own state, not a second user of `App::find`. That one
searches the diff and counts in display rows, which fold and pair two sides
together; this counts in buffer lines, and sharing would have meant one of
them lying about what a number means.

`tui-textarea` has a search of its own. It is behind a feature that pulls
in `regex`, it has no replace at all, and its match colors are painted by
its own widget render — which this editor replaces, so they would never
reach the screen. `search::find_ranges` does the matching instead, and
`render_row` paints it.

**Replace-all runs back to front.** Every match after the one being
replaced sits at a column the replacement is about to move, so going
forwards writes the second one into the wrong place as soon as the
replacement is a different length.

The comment toggle replaces the buffer whole, which costs two of
`tui-textarea`'s history entries — the delete and the insert — so it
borrows the `pre_format` trick that keeps one undo meaning one press.

## Finding things

`Ctrl+F` is one key over three different searches, because "this file"
means three different things depending on which pane is in front of the
reader. `app::is_find_key` is the single answer to "was that the find
key", so the three panes cannot drift apart on which modifiers count, and
`is_find_all_key` is the shifted half that reaches the repository grep.
Cmd is accepted alongside Ctrl and almost never arrives: terminals keep
Cmd+F for their own find bar, and none report Super at all without the
kitty keyboard protocol, which loupe does not ask for.

The preview's search is its own, in `preview::Find`, because the diff's
searches display rows of a `FileDiff` and the preview has none. It
matches the **rendered** text — `Line`s the markdown renderer already
produced — rather than the source, so the reader searches for what is on
the page. Highlighting that means walking the spans and the match ranges
together: a match can straddle two spans (bold in the middle of a
sentence) and one span can hold several, so `preview::highlight` splits
spans rather than replacing their style, and a hit inside a heading still
reads as a heading.


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

**Every write to a server goes through a thread that owns its pipe.**
`Client::send` frames the message and pushes it to a channel. Writing on
the caller's thread would block: a pipe holds 64 KB on macOS, a
`didChange` carrying a 536 KB buffer is 553 KB of escaped JSON, and
`sync_open` is called from the idle tick on the drawing thread. A channel
rather than a thread per message, because the protocol is ordered —
`didOpen` before the `didChange` that builds on it, version 2 before
version 3 — and two threads racing on one pipe corrupt the stream.

A broken pipe surfaces one message late: the writer thread ends, dropping
the receiver, which fails the next send.

`didClose` goes out when a buffer closes, and takes the document's
diagnostics with it. Without it every file opened in a session stays open
and the server keeps analysing files nobody is reading.

The `SERVERS` table is `BUILT_IN` plus whatever `[[server]]` tables the
config file holds, installed once at startup by `lsp::configure` and
leaked. A `ServerSpec` is `&'static` through `spec_for`, the registry and
the UI, and the table lives for the process.

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
analysis. Having the wrapper without a `typescript` package to drive is a
server that starts and then dies on every question, so `--lsp` checks for
both and says which half is missing.

### A loading server answers, and its answer is wrong

`request_when_ready` exists because there are three ways a server that is
still loading a project answers a question about it:

1. **Nothing.** `rust-analyzer` returns an empty list for references
   until its index is built.
2. **`ContentModified`** (-32801). The specification says to send the
   request again; `rust-analyzer` returns it for much of the time it
   spends loading.
3. **A plausible wrong answer.** `tsserver` answers go-to-definition out
   of a half-built program by pointing at the `import` line in the file
   the question came from, rather than the file the symbol is defined
   in. Not empty, not an error, and nothing in the answer itself gives
   it away.

The third is why an answer given while the server is still doing the work
it began at launch is treated as provisional whatever it contains, and
why the **first** question of a session waits out a short grace before it
is believed. That grace is a clock rather than a signal on purpose: ending
it as soon as the server reports it has finished something was measured
and made the failure more frequent, because `tsserver` announces smaller
pieces of work before it gets to the project.

The cost is about a second, once per server per session, behind the
spinner the status bar already shows. Every later question is unaffected.

## Suggestions

The completion popup opens on its own, so the decision about *when* is
its own small piece of logic (`App::maybe_suggest`). Two things earn a
request: a character the server named in
`completionProvider.triggerCharacters`, or one word character with a name
already behind it. Anything else — a space, a bracket — cancels whatever
was pending.

Loupe tells the server which of the two it was. `triggerKind: 2` with the
character is what makes `object.` in TypeScript answer with the object's
members; asked as though a human had invoked it, the same position
answers with everything in scope.

A popup that is already open is filtered locally rather than re-fetched:
the list in hand is the server's answer for this word. Filtering keeps
prefix matches above subsequence matches, so a typo narrows the list
instead of emptying it and closing the popup.

## Problems

Diagnostics arrive from two places on two clocks — the language server
pushes them, and the linter finishes a run — so `Editor` keeps the two
lists apart and merges them into the one `problems()` reads. The merged
list is private for that reason: its order (line, then column, then
severity) is an invariant that four readers depend on, and they all have
to agree on which problem a line's "worst" is.

`linter.rs` runs a subprocess with the buffer on stdin rather than
driving a second language server: no handshake, no workspace
negotiation, nothing kept alive between keystrokes, and it lints what is
on screen rather than what is on disk. It prefers the project's own copy
in `node_modules/.bin`, because that is the version whose rules the
repository agreed on.

ESLint numbers its severities the other way round from LSP — its `2` is
an error and its `1` a warning. That is turned round on the way in, and
it is the one thing in the parser that must not be guessed: a warning
painted red is a warning people stop believing.

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

## The markdown preview

A `.md` file has three views, and all three use the same pane:

- the **diff**, for what changed;
- the **editor** (`preview.rs` calls it the source view), for changing it;
- the **preview**, for reading it.

`markdown.rs` splits the work in two, because the halves have different
costs. `parse` reads the source once into a flat list of blocks, and
syntax-highlights every fenced code block while it is there — that is the
expensive part, and it does not depend on how wide the pane is. `lay_out`
turns those blocks into ratatui lines for one width, so dragging the
divider re-runs only the cheap half. Block structure is flat rather than a
tree: a block quote sets a depth on the blocks inside it, which keeps the
layout pass a single loop.

The layout also builds two maps. `src_of` gives the source line behind
every rendered row, and `heads` lists the rows that are headings. The
first is what makes `P` a *toggle* rather than two separate commands: it
carries the reader's place across in both directions, so the editor opens
on the line that was at the top of the preview and the preview comes back
scrolled to the line that was just edited. The second is what `}` and `{`
walk.

`App::preview` and `App::editor` are never both set — they are two ways of
holding the same file, and `toggle_preview` swaps one for the other.
Toggling from the editor renders the *buffer*, not the file, so unsaved
text is what the reader sees. The blame pane stands down while the
preview is open: one source line is any number of rendered rows there, or
none, and there is no honest way to line the column up against that.

The reload check rides the same idle tick as the local re-scan. It
compares the file's modification time, re-reads only when it moved, and
re-anchors by source line rather than by row, because a rewrite changes
the row count and staying on row 200 of a different document is not
staying put. A buffer with unsaved changes is never overwritten this way.

`loupe md <path>` sets `App::preview_only`, which gives the pane the whole
window and makes `q` the way out. It is the one launch mode that needs no
repository.

## Files outside the changeset

A search result, or a jump to a definition, can land in a file the change
never touches. There is nothing to diff it against, so it opens in the
**editor** rather than the diff view — a file shown as a file. A markdown
file opens in the **preview** instead: reaching for the finder to open a
plan file means wanting to read it, and `P` gets to its source. The review
state is not disturbed at all, which is what makes closing it free:
no reload, no restored scroll position, nothing to get wrong.

When the branch under review isn't checked out, the working tree belongs
to some other branch; the text then comes from the commit and the editor
is marked read-only, because saving would write over an unrelated
branch's file.

## Tabs

`tab_order` is the row, by path, and the only thing that is in row order.
Storage is elsewhere and neither list is ordered: `parked` holds buffers,
`parked_diffs` holds diffs, and the one on screen stays in the fields it
has always been in — `editor` for a buffer, `diff`/`old_content`/… for a
diff. That is what kept the change small: every one of the places that
reads `self.diff` still means "the diff the reader is looking at".

**A tab is a file, not a view of one.** The same path can have a parked
diff and a parked buffer at once, which is what `e` on a diff and `Esc`
back again produce, and the row draws it once either way. Closing a tab
closes both; closing the editor over a diff leaves the tab, because the
file is still open.

The row used to reconstruct its order from `buffer_slot` — where the
active buffer had been taken out of `parked`. That works while a tab is
only ever a buffer and cannot survive a second kind, because two storage
lists cannot agree on one order.

**The peek tab is what keeps the row short.** A review means opening a
great many files to read one, so a click reuses one tab and a double
click keeps it. The replacement takes the screen *before* the old tab
closes — otherwise the pane stands empty for the length of a read — so it
is already at the end of the row when the close happens and has to be
moved back into the tab it took over. `tab_hole` covers the other order,
where the close comes first.

## Pinned files, and files dropped on the window

`pins.rs` owns the tab row. A `Pin` is a path and nothing else: a bookmark,
never a copy. It is named relative to the repository root when it lives
inside one, and by its whole path when it does not — the `outside` flag
that the `↗` on the tab draws from.

The row also carries the open editor buffers, after the pins and a
divider. Drawn together and addressed apart: the pins are index-coupled
through `open_pin`, `PinTab`, `open_pin_number`, `active_pin` and
`Pins::step`, and folding the buffers into that numbering would renumber a
reader's pins every time a file was opened. A pin is a bookmark somebody
chose; a buffer is a file that happens to be open.

The state file lives under `git_dir`, which `rev-parse --absolute-git-dir`
resolves to `<main>/.git/worktrees/<name>` in a linked worktree. So the row
is per worktree, not per clone.

**Which tab is open is derived, not stored.** `App::active_pin` asks what
file is on screen — the preview's, the editor's, or the one under the file
panel cursor — and looks it up in the row. A reader leaves a document by a
dozen different doors (Esc, the file panel, a search result, a jump to a
definition), and a remembered index would have to be cleared in every one
of them. Derived, it cannot go stale. Both sides of the comparison are
canonicalized, because a dropped path arrives with its symlinks already
resolved and one built from the file panel does not — on macOS `/tmp`
alone is enough to give one file two tabs.

**A drop is a paste — or a burst of keystrokes.** Every terminal answers a
file dropped on its window by writing that file's path into the program as
if it had been typed, but not all of them mark it as a paste. Ghostty,
iTerm2 and Terminal.app wrap it in bracketed paste, which `main.rs` turns
on, so it arrives as one `Event::Paste`. Warp does not: the path arrives
as one key event per character.

That second case is why the event loop hands `App::handle_events` a
*batch* rather than dispatching events one at a time. Read as ordinary
keys, the leading `/` of a path opens the search prompt and the rest of
the path lands in the query box — the file never opens, and what the
reader sees is their own path spelled along the bottom of the window. So
the batch is read for a path first, by the same rule either way:
`pins::dropped_paths` accepts the text only when *every* token in it is an
absolute path to a file that exists. That rule is what
lets an ordinary paste stay an ordinary paste — a snippet of code, a URL,
a sentence — and it costs nothing, because a drop is always absolute. The
tokenizer handles the three spellings terminals use for a path with a
space in it: backslash-escaped, quoted, and percent-encoded in a `file://`
URL.

Anything that is *not* a drop goes wherever the keyboard already is — the
path box, a comment draft, the finder, the review box, or the editor —
which is how paste came to work properly in all of them.

**Where a tab goes.** A markdown file renders as a document wherever it
lives. A file the change touches goes through the ordinary
`spawn_load_file` door, so the diff, the blame pane and the staging state
all follow; `pin_wants_preview` is the one-shot flag that then opens the
document on top of it once the file lands. Anything else opens in the
editor with `standalone` set, which is what makes `Ctrl+S` write straight
to the path rather than try to refresh a diff that does not exist.

The row lives in `.git/loupe/pins.json`, beside the held comments and for
the same reasons: it is per-clone state, it must never be committed by
accident, and it belongs to this checkout. It is rewritten on every change,
so quitting never costs the reader their tabs, and a pin whose file has
since been deleted is dropped when it is read back.

## Nothing is locked down

Every mode used to answer its own clicks and its own keys, and every mode
answered a different subset. Right-clicking the PR badge to copy the link
did nothing at all while a document or the editor was open, because the
badge was only ever wired up in the diff.

Two functions are the whole of the fix, and both work the same way: the
mode's own handling comes first, and what is left falls through to one
shared definition.

- `chrome_mouse` — the panel divider, the toolbar, the ☰ menu, the badge,
  the file panel. Everything that belongs to the window rather than to
  whatever is in the middle of it. It runs above the mode split, so a new
  mode gets them for free and cannot forget one.
- `global_key` — quit, go somewhere, find something, stage, stash, and
  the settings. A mode's own keys are matched before it, which is how
  `Ctrl+B` still pages back in a document instead of reading as "back to
  the pull request list", and how `Esc` still closes the document instead
  of leaving the review. Bare keys only: a modifier means the key belongs
  to whoever bound it.

The editor is the one mode that cannot answer bare keys — they are text —
so `Alt+Enter` opens the ☰ menu there and the menu carries the lines that
go somewhere.

A foreground job is modal for the **pane**, not for the window. It used
to be modal for both, which is why the second click of a double click
went nowhere: the first click started the read and the read swallowed the
second. A load takes tens of milliseconds and a double click takes four
hundred, so every one of them landed mid-read.

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
  `gitops::show_file` takes the repository root and passes `-C <root>` for
  the same reason every pathspec call does: resolving against the
  process's own directory is the same repository today and silently the
  wrong one the moment the two differ.
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
resize clamps, fuzzy ranking, the definition scanner, the `git blame --porcelain`
parser and the age ramp, LSP message
framing and every shape a `definition` answer can take — plus
`TestBackend` render tests that assert syntax colors and search
highlights actually reach the diff cells, and tests that drive a **real
temporary git repository** to keep the staging, `git grep` and
`git blame` plumbing honest — the staging sections, the three stash
scopes, and the unpushed-commit list all run against one of those. Clippy
is kept at zero warnings across all targets.

## Performance ground rules

Measured baselines live in the commit history; the standing rules:

- Nothing blocking on the UI thread — if it can take more than a frame,
  it's a job.
- No full-file highlight per keystroke, ever.
- Idle means idle: don't add periodic redraws; respect the dirty flag.
- Renderers coalesce same-style runs into single spans — no per-character
  `Span` allocation in hot paths.
- Nothing that walks the whole repository runs on a toggle. Building the
  file tree costs 48 ms at 400,000 paths and emitting its rows costs about
  2 µs, so the build belongs to the job that loads the list and the emit
  belongs to the click. `app::tests::the_tree_stays_inside_its_budget`
  holds the bound.
- A timing test is a ratio or a loose absolute, never a tight wall-clock
  number. A shared CI runner is not a laptop, and a test that fails for
  being slow gets deleted rather than fixed.

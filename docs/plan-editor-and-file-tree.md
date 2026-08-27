# Plan — a full editor and a full file tree

Branch: `editor-and-file-tree`

**Speed is priority 1.** Every decision below is measured, not assumed.
The numbers come from this machine. Reproduce them before you trust them.

## Goal

1. **A full editor.** Open many files at once, find and replace inside a
   buffer, create and rename files, and get the language-server features
   an editor is expected to have.
2. **A full file tree.** See every file in the repository in the file
   panel, not only the files the change touches.

## On reading other editors

Lapce and Warp both solve parts of this, and both are worth reading when a
question comes up. Neither is worth relicensing loupe over. Loupe is
**MIT**.

| Project | Licence | What copying costs |
| --- | --- | --- |
| lapce | Apache-2.0 | Attribution and a NOTICE obligation |
| Warp — `warpui`, `warpui_core` | MIT | Nothing, but it is their UI toolkit and we use ratatui |
| Warp — **everything else** | **AGPL-3.0** | **Loupe becomes AGPL**, network use included |

Every Warp crate that touches our problems is on the AGPL side:
`editor`, `lsp`, `jsonrpc`, `fuzzy_match`, `sum_tree`, `syntax_tree`,
`virtual_fs`, `languages`, `vim`.

**The rule:**

- **Copy no code and no comments, from either project.** Read it,
  understand the technique, close the tab, then write ours against loupe's
  own types and job engine.
- **Treat AGPL more carefully than Apache-2.0.** Apache-2.0 copying is a
  paperwork mistake. AGPL copying relicenses the whole project. Where the
  same technique is documented somewhere friendlier, read that instead —
  we lose nothing.
- Cite the file when a decision came from reading it, as D12, D14 and D15
  do. A citation is a pointer for the next reader, not a licence to paste.
- A technique — a count per node, a thread that owns a pipe, a tree whose
  nodes cache a summary — is an idea. Ideas are not what a licence covers.
  The expression is.

## The speed budget

The event loop (`main.rs:740`) drains input, then draws. Everything on
that thread is frame cost. Three rules govern this whole plan.

| Rule | Budget |
| --- | --- |
| Work on the UI thread, per event | **2 ms** |
| Work before the first paint | **0 ms of new work** |
| A subprocess, a file read, or a whole-repository walk | **A background job. Never the UI thread.** |

Loupe already has the machinery: foreground jobs with a spinner, and
fire-and-forget jobs that apply an optimistic result. Use it.

## Measurements

### `git ls-files --cached --others --exclude-standard`

| Repository | Files | Time |
| --- | --- | --- |
| loupe | 54 | 5 ms |
| scalis-io | 16,761 | **93 ms** |

93 ms is far over the frame budget. The list is a background job.

### The tree builder, split in two

`build_tree_entries` (`app.rs:1214`) does two things: it builds a
`BTreeMap` node tree from every path, then it emits the visible rows.
Timed separately, with `rustc -O`:

| Paths | Build the node tree | Emit, collapsed | Emit, expanded |
| --- | --- | --- | --- |
| 16,761 | **3.95 ms** | 500 ns | 724 µs |
| 100,000 | **16.06 ms** | 417 ns | 3.24 ms |
| 400,000 | **48.25 ms** | 1.29 µs | 9.72 ms |

**The node tree is 99% of the cost. The emit is free.** This one
measurement drives D10 and D11.

`rebuild_entries` runs on the UI thread at 8 sites in `app.rs`, and on
every collapse and expand. Today it rebuilds the node tree every time.

### A subprocess per directory

| Operation | Time |
| --- | --- |
| `git check-ignore --stdin` (one call) | **10 ms** |
| One directory listed by `ls` | 8 ms |

A lazy per-directory design pays about 10 ms on every expand. The cached
node tree pays 500 ns. This settles D2.

### Listing the ignored files

Measured on a real Astro repository with `node_modules` on disk.

| Command | Time | Entries |
| --- | --- | --- |
| `--others --ignored --exclude-standard --directory` | **14 ms** | **10** |
| `--others --ignored --exclude-standard` (full recursion) | 147 ms | 17,504 |

`--directory` stops at a directory whose whole contents are ignored and
reports the directory itself. So an ignored **file** is listed, and an
ignored **directory** costs one row:

```
.env
debug.log
dist/
node_modules/
```

10x cheaper, and strictly more useful. This is D14.

### Full-document language-server sync

`src/app.rs` is 536,045 bytes.

| Step | Cost |
| --- | --- |
| Escape it into a JSON notification | 1.3 ms |
| Bytes written | 553,077 |
| macOS pipe buffer | 65,536 |

That is **9 blocking writes** into a pipe the server may not drain.
See D12.

---

## Decisions

### D1 — Build the tree on `FileEntry`, do not adopt a widget

`ratatui-explorer` and `tui-tree-widget` both exist and both work. Neither
fits. The file panel draws staging icons, viewed checkboxes, revert
markers, a conflict heading, a resize grip, and per-row hit-testing. A
foreign widget loses all of it, and neither one beats 500 ns.

### D2 — One `git ls-files`, not a read per directory

Measured above: a per-directory design costs about 10 ms of subprocess on
every expand. One `git ls-files` costs 93 ms **once**, on a background
thread, and every expand after that costs 500 ns.

`search::list_files` (`search.rs:172`) already runs that exact command for
`Ctrl+P`. Share the result between the two.

The `ignore` crate would also work, but it adds `globset`, `regex`, and
`crossbeam` to a 125-package lockfile and does not beat a cached tree.
Reject it.

Ignored files come from a second call. See D14.

**Threshold:** above about 200,000 files the 93 ms grows past 1 s. Add a
lazy fallback then, not now. STEP 13 covers it.

### D10 — Cache the node tree; `rebuild_entries` only emits

This is the most important change in the plan.

- Build the node tree **once**, in the background job that loads the path
  list. Store it on `App`.
- `rebuild_entries` walks the cached tree and emits rows. Nothing else.

At 16,761 files a collapse toggle goes from 3.95 ms to 500 ns. At 400,000
files it goes from 48 ms — a visible stutter on every click — to 1.29 µs.

Two smaller wins go in the same change:

- `emit` runs `node.files.clone()` per directory (`app.rs:1268`). Sort an
  index instead, or sort once at build time.
- The changeset panel gets the same win. It is small today, so this is
  not why you do it, but it costs nothing extra.

### D11 — Collapse every directory by default in Files mode

Emit at 400,000 files: 1.29 µs collapsed, 9.72 ms expanded. Expanded also
allocates a 410,257-entry `Vec`. Collapsed allocates 7.

The changeset panel keeps its current behavior. It is small, and a reader
wants to see the whole change at once.

Give each mode its own `collapsed_dirs` set. One shared set would collapse
a folder in a view where the reader never touched it.

### D12 — Give each language server a writer thread

`sync_editor_buffer` (`app.rs:2053`) runs on the UI thread. Its doc comment
says `sync_open` "never blocks". That is true of the locks — it uses
`try_lock` — and **false of the pipe write**. `Client::send` (`lsp.rs:822`)
calls `write_all` on the server's stdin. Measured above: a 536 KB buffer is
9 times the macOS pipe buffer. A server busy indexing does not drain it,
and the UI thread stops.

**Lapce reaches the same conclusion**, and its shape is better than the
obvious one. Read `lapce-proxy/src/plugin/lsp.rs` for the idea: one thread
per server owns the pipe, and every other thread reaches it through a
channel. A send becomes a channel push, so nothing that talks to a server
can block a caller.

Write our own. See the licence rule below.

**Take this design, not "spawn a worker per sync".** A worker per sync
would have introduced a bug: two syncs racing could deliver `didChange`
version 3 before version 2, and JSON-RPC has no way to recover from that.
One writer thread per server keeps the order the protocol requires and
makes every notification non-blocking, not only `didChange`.

### D13 — Budget the highlight cache across buffers

`EditorHighlight` (`highlight.rs:398`) is genuinely incremental: a
per-line cache, spliced on edit. It already refuses a file over
`MAX_EDITOR_HL_LINES` = 8,000 lines (`highlight.rs:36`). Loupe's own
`app.rs` is 13,601 lines, so it opens with no colors at all.

One buffer holds one cache. Ten buffers hold ten. So:

- Drop the highlight cache of a background buffer, and rebuild it when the
  reader comes back. The cache is derived state.
- Do **not** raise `MAX_EDITOR_HL_LINES` as part of this work. It is the
  guard that keeps a huge file cheap.

### D14 — Show ignored files, one directory stub at a time

`.env` is ignored by git and load-bearing for a developer. `node_modules`
is ignored by git and 17,494 rows of noise. The tree must separate them,
and `git ls-files --directory` already does exactly that.

The background job runs **two** commands, not one:

| Call | Gives |
| --- | --- |
| `--cached --others --exclude-standard` | Every tracked and untracked file |
| `--others --ignored --exclude-standard --directory --no-empty-directory` | Ignored **files**, and ignored **directories** as one stub each |

- An ignored file (`.env`, `.DS_Store`, `debug.log`) becomes a normal file
  row, drawn dim so it reads as outside the repository's tracked set.
- An ignored directory (`node_modules/`, `dist/`, `target/`) becomes a
  **collapsed directory row**. Its contents cost nothing until the reader
  expands it, and only then does loupe pay one `read_dir` for that one
  directory.

Total added cost at load: 14 ms on the background thread.

**Lapce does it differently, and worse for this requirement.** Its
explorer ignores git entirely and filters through a glob setting,
`editor.files_exclude` (`lapce-app/src/file_explorer/data.rs`). So a lapce
user hides `node_modules` by hand, and nothing tells them `.env` is
special. Git already knows both facts. Keep D14.

**This also solves D9's nested-worktree trap.** `wt/feat/` and
`node_modules/` are the same shape — a path with a trailing slash that
git refused to walk into. One mechanism handles both. STEP 0 changes
accordingly: do not drop those entries, render them as directories.

### D15 — Keep `children_open_count` in reserve

Lapce never flattens its tree, and the technique is worth knowing. Read
`lapce-rpc/src/file.rs` for it. Described rather than copied:

Every directory node carries a running count of the visible rows its whole
subtree contributes when open. To fetch the rows for a viewport, walk down
from the root carrying a position counter, and skip any subtree whose
count places it entirely before the window. The total row count is the
root's count, read in constant time.

The result is a window that costs depth plus window size, whatever the
repository holds, with no flattened list anywhere.

**The idea generalises, and Warp shows how far.** Its `crates/sum_tree` is
a B-tree whose every node caches a summary of its subtree, so one
structure answers "how many rows", "which row sits at position N", and
"what offset is line N" in logarithmic time. A per-node row count is the
one-summary case of that. If loupe ever outgrows D15, that is the shape to
grow into — read an MIT or Apache implementation of it, not Warp's.

That is strictly better than a flattened `Vec`. **Do not build it yet.**
Measured, loupe's emit costs 724 µs at 16,761 paths fully expanded and
9.72 ms at 400,000 — and D11 collapses everything by default, so the
expanded case is what a reader opts into. STEP 1 stays as written.

**The trigger is the STEP 5 timing test.** When it fails, this is the
design to move to, and `App::entries` stops being a `Vec`.

### D3 — Do not turn on the `search` feature of `tui-textarea`

`tui-textarea` has `set_search_pattern`, `search_forward`, and
`search_back`. Three facts rule them out:

1. The feature needs the `regex` crate. `regex` is **not** in
   `Cargo.lock` today, and speed work should not add a dependency it does
   not need.
2. Match colors come from `set_search_style`, which only the crate's own
   widget render paints. `editor.rs:478` renders its own rows through
   `render_row`, so those colors would never appear.
3. There is **no replace API** at all.

Use `search::find_ranges` (`search.rs:213`). It does smart-case literal
search and returns char ranges, which is what `render_row` needs. Search
the visible window first, then extend on demand.

### D4 — Poll `mtime`, do not add the `notify` crate

`preview.rs:49` already reloads a file when an agent rewrites it, by
comparing `mtime` on the idle tick. One `stat` per open buffer per tick is
cheaper than a watcher thread plus an FSEvents registration per directory.
Add no dependency.

Do not `stat` the whole tree on a tick. Refresh the path list on an
explicit action and on a long interval only.

### D5 — Many buffers means `Vec<Editor>` plus an active index

`App::editor` is `Option<Editor>` (`app.rs:1735`). About 40 sites touch
it. Add `fn editor(&self)` and `fn editor_mut(&mut self)` so most sites
keep their shape.

`pins.rs:155` (`Pins::labels`) already solves the tab-label problem.

Only the active buffer costs anything per frame. A background buffer must
not be highlighted (D13), must not be diffed, and must not be synced.

### D6 — The language server needs `didClose`

`lsp.rs` tracks open documents in `opened: uri -> (version, hash)`
(`lsp.rs:484`) and sends `didOpen` and `didChange`. It **never sends
`didClose`**. One buffer hides the leak. Many buffers turn it into a
server that re-analyzes files nobody reads, which costs the reader latency
on every later request.

### D7 — Rename symbol must come after many buffers

`textDocument/rename` returns a `WorkspaceEdit` that touches files which
are not open. With one buffer, loupe would have to write those files to
disk with no review.

### D8 — The editor stays a layer over the review

Do not add a third `Screen` variant. The editor draws over the review
today, and blame and preview both hang off that.

### D9 — Worktrees already work, with one trap

Verified against a real linked worktree.

| Question | Answer |
| --- | --- |
| `rev-parse --show-toplevel` in a linked worktree | The **worktree's own root** |
| `rev-parse --absolute-git-dir` there | `<main>/.git/worktrees/<name>` |
| `.git` in a linked worktree | A **file**, not a directory |
| `git ls-files` there | That worktree's own index |

`repo_root` (`gitops.rs:27`) and `git_dir` (`gitops.rs:382`) both do the
right thing. State under `git_dir` — pinned tabs, held comments, and the
new tree cache — is per worktree. That is correct, and it means the cache
never needs invalidation when the reader switches worktrees.

**Note:** `README.md:92` and `docs/keys-and-mouse.md:616` say pinned tabs
are "per clone". In a worktree they are per worktree. Fix the wording.

**The trap: a worktree nested inside the repository.** Git refuses to
recurse into a nested repository boundary and reports a directory stub
instead:

```
$ git ls-files --cached --others --exclude-standard   # from the main repo
wt/feat/
.gitignore
src/main.rs
```

`search::list_files` does not drop that stub. `build_tree_entries` splits
it on `/` and pops an **empty** last part, so it becomes a file row with
no name. A nested plain repository does the same. This is a live bug in
`Ctrl+P` today. STEP 0 fixes it.

## Settled

| Question | Answer |
| --- | --- |
| Mode toggle, or a second panel? | **Mode toggle.** `Changes` and `Files` swap in one panel. |
| Which key? | **`F`.** Every lowercase letter is taken; `F` is free. |
| Show ignored files? | **Yes**, per D14. `.env` matters; `node_modules` collapses. |
| Save a file the change does not touch? | **Stay in `Files`.** Mark the row. Never move the ground under a reader who is browsing. |
| Go to definition and find all references? | **Required.** See STEP 6 — references is missing from the editor today. |
| Which key for references in the editor? | **`Alt+R`**, matching the editor's existing `Alt+P`. |
| One tab strip or two? | **One.** Pinned files and dirty buffers are marked differently. |

**Root at the repository, not the shell's current directory.**
`safe_repo_path` (`gitops.rs:39`) rejects any path with `..` or a root
component, and every guard in loupe depends on that. In a linked worktree
the root is the worktree itself, per D9. The tree scrolls to the current
directory on open.

## Open questions

None. Every question is settled above.

---

# The steps

Do them in this order. Each step ends with a green `cargo test`.

Every step that touches the tree or a buffer ends with a **timing
assertion**, not a guess. See STEP 5.

## Phase A — the file tree

### STEP 0 — Treat a trailing slash as a directory, not a file

Per D9 and D14. Git emits `wt/feat/` for a nested worktree and
`node_modules/` for an ignored directory. Both are the same shape, and
`build_tree_entries` turns both into a file row with an empty name.

- In `search::list_files` (`search.rs:172`), return the trailing-slash
  entries separately rather than mixing them into the file list. `Ctrl+P`
  drops them; the tree keeps them.
- Add a test that no path ending in `/` reaches the file list.

The `Stub` **row** moves to STEP 3, where the tree first has something to
draw it with. Nothing produces a stub row before then.

**Done when:** `Ctrl+P` in a repository with a nested worktree shows no
blank row.
**Size:** half a day.

### STEP 1 — Cache the node tree

Do this **before** the tree feature. It is a speed fix to code that
already ships, and the feature depends on it.

- Split `build_tree_entries` into `build_nodes(paths) -> Nodes` and
  `emit(nodes, collapsed) -> Vec<FileEntry>`.
- Store `Nodes` on `App`. Rebuild it only when the path list changes.
- `rebuild_entries` calls `emit` and nothing else.
- Remove the `node.files.clone()` in `emit` (`app.rs:1268`).

**Done when:** a collapse toggle allocates no node tree.
**Measured target:** emit under 50 µs for 20,000 paths.
**Size:** half a day.

### STEP 2 — Break `FileEntry` free of the changeset index

Wide, but mechanical. Change nothing visible.

- Replace `FileEntry::File { idx, depth }` with
  `FileEntry::File { src: RowSrc, depth }`, where
  `RowSrc = Changed(usize) | Path(usize)`.
- Add `App::changed_of(&self, src) -> Option<&ChangedFile>`,
  `changed_idx`, and `row_path`.
- `ui.rs:934` must stop indexing `app.files` directly.

**Note on speed:** do **not** make `RowSrc::Path` hold a `String`. Rows
are emitted per toggle, so at 400,000 fully expanded rows that is 410,257
allocations. Hold an index into the cached path list, which already owns
the strings.

**Two corrections found while doing it:**

- `usize`, not `u32`. Measured, `FileEntry` is **56 bytes either way** —
  the `Dir` variant's two `String`s set the size, so a narrower index buys
  nothing and costs a cast at every site.
- It is **7 sites, not 32**. The earlier count was every mention of
  `FileEntry`; only 7 touch `FileEntry::File` outside the tests.

**Done when:** every test passes and the panel is pixel-identical.
**Size:** half a day.
**Risk:** low, but wide. Do not mix another change into this commit.

### STEP 3 — Add the panel mode

- Add `pub enum PanelMode { Changes, Files }` to `App`.
- Add a third toggle beside `Tree` and `Flat` (`ui.rs:896`), plus a key.
- Load the path list in a **background job**, and build the nodes there
  too. The 93 ms never touches the UI thread.
- Run **both** `git ls-files` calls in that job, per D14: the tracked and
  untracked list, then the ignored list with `--directory`. Draw ignored
  rows dim.
- The panel shows the changeset until that job lands. First paint waits
  for nothing.
- Collapse every directory by default, per D11.
- Give each mode its own `collapsed_dirs` set.

**Done when:** the toggle shows every repository file, and the first paint
is no slower than it is today.
**Size:** 1 day.

### STEP 4 — Open a row, and expand a stub

- A row with a `ChangedFile` opens the diff, exactly as now.
- A row without one goes to `apply_external` (`app.rs:2702`).
- Guard the path with `safe_repo_path` and the symlink check, the way
  `open_editor` (`app.rs:9276`) does.
- **Expanding a `Stub`** spawns a background `read_dir` of that one
  directory, then splices the result into the cached node tree. Never
  recurse. A second expand inside it reads one more level.
- Carry a "children fetched" flag on the node, false until the read lands.
  Lapce takes the same approach and never reads synchronously; see
  `lapce-app/src/file_explorer/data.rs` for the idea, then write ours
  against our own job engine.
- `node_modules/` must stay one row until the reader asks. That is the
  whole reason D14 is affordable.

**Size:** 1 day.

### STEP 5 — Mark the rows, refresh the list, and lock the speed in

- Show the staged, viewed, and conflict marks on any row that has a
  `ChangedFile`. Look that up through the cached list, not a scan.
- Refresh the path list on an explicit action and on a long interval, per
  D4. Never per tick.
- **Add the timing tests.** Generate 20,000 synthetic paths, then assert
  the bounds.

  **Correction found while doing it:** the 10 ms build bound named here is
  too tight. The build measures 8.06 ms on this machine, so a shared CI
  runner would fail it for being a shared CI runner. The tests use bounds
  about 20 times the measurement — 200 ms build, 5 ms emit — which still
  catch a change of *shape* (a walk gone quadratic, a subprocess on the
  emit path) and never catch a slow afternoon. The ratio test from STEP 1
  stays beside them: it is what catches both halves getting slowly worse
  together.

**Size:** 1 day.

**Phase A ships on its own.**

## Phase B — the editor

### STEP 6 — Find all references, from inside the editor

`gd` and `gr` both work in the diff view (`app.rs:5115`). In the editor,
`Ctrl+]` gives definition — and **references is missing**. `EditorRequest`
(`app.rs:1550`) has `Complete`, `Hover`, `Definition`, and `Format`, and
no `References`.

- Add `EditorRequest::References`. `Lsp::references` (`lsp.rs:1038`)
  already exists, and `FinderMode::Refs` (`app.rs:476`) already renders
  the result. This wires two things that are both already built.
- Bind a key. See open question 2.

**Done when:** `gr` in the diff and the editor key reach the same list.
**Size:** half a day.

### STEP 7 — Give each language server a writer thread

Per D12. A live bug today, so it ships whether or not Phase B does.

- Spawn one writer thread per server at startup, fed by an mpsc channel.
  `Client::send` pushes; the thread writes and flushes.
- Keep the debounce and the hash check. They are what make this cheap in
  the common case.
- Message order is the point. Do not spawn a worker per sync.

**Done when:** a busy server cannot stall the UI thread.
**Size:** half a day.

### STEP 8 — Send `didClose`

Per D6. Land it before the buffer list.

- Add `Lsp::close(&self, root, path)`. Send `textDocument/didClose` and
  drop the `opened` entry.
- Call it from `close_editor` (`app.rs:9339`).

**Size:** half a day.

### STEP 9 — `Vec<Editor>` plus an active index

- Replace `App::editor: Option<Editor>` with `editors: Vec<Editor>` and
  `active: usize`.
- Add `editor()` and `editor_mut()` so the 40 call sites keep their shape.
- Every "close the editor first" guard (`app.rs:5539`, `7230`, `7607`,
  `9363`, `9486`) must ask about **every** dirty buffer. So must quit.
- Per D13, drop the highlight cache of a background buffer and rebuild it
  on return. Only the active buffer costs per frame.

**Done when:** ten open buffers cost the same per frame as one.
**Size:** 2 days.
**Risk:** the highest in the plan. The dirty-buffer guards are where work
gets lost.

### STEP 10 — Buffer tabs

Reuse `draw_pin_tabs` (`ui.rs:89`) and `Pins::labels` (`pins.rs:155`).
Settle open question 2 first.
**Size:** 1 day.

## Phase C — editor depth

Independent. Order them by what you want first.

### STEP 11 — Find and replace inside a buffer

Build on `search::find_ranges` per D3. Paint matches in `render_row`
(`editor.rs:623`). Search the visible window first, then extend.
**Size:** 1 day.

### STEP 12 — New file, rename, delete, save-as

`Overlay::PathMenu` (`app.rs:138`, drawn at `ui.rs:2310`) and
`OpenPathBox` (`app.rs:160`) already exist. A file created or deleted here
must patch the cached node tree, not trigger a full reload.
**Size:** 1 day.

### STEP 13 — A lazy fallback above 200,000 files

Per the D2 threshold. Below it, do nothing — the cached tree is faster.
Above it, read one directory at a time on expand and accept the 10 ms.
**Size:** 1 day.

### STEP 14 — Rename symbol, code actions, signature help

Needs STEP 8 and STEP 9, per D7.

- `prepareRename`, then `rename` → `WorkspaceEdit`. Open every touched
  file as a dirty buffer.
- `codeAction` → a list in an overlay.
- `signatureHelp` → a popup beside the completion popup.
- Read the server's `initialize` reply for `renameProvider`,
  `codeActionProvider`, and `signatureHelpProvider` first.

`apply_text_edits` (`editor.rs:139`) already applies edits back to front.
**Size:** 3 days.

### STEP 15 — Language servers from the config file

`SERVERS` is a static table (`lsp.rs:70`). Move it into
`config.example.toml`. Keep the 3 built-in rows as the default.
**Size:** 1 day.

### STEP 16 — Editor comforts

Bracket match, comment toggle, auto-indent. All in `editor.rs`.
**Size:** 1 day.

---

## Totals

| Phase | Days |
| --- | --- |
| A — the file tree | 5 |
| B — the editor | 4.5 |
| C — editor depth | 8 |

Phase A grew by 2 days against the first draft: STEP 1, the stub handling
in STEP 0 and STEP 4, and the timing tests in STEP 5. Those are what keep
`node_modules` from costing anything and keep the panel under budget.

## Out of scope

- A third `Screen` variant, per D8.
- Multiple cursors and rectangular selection. `tui-textarea` supports
  neither.
- Split panes.
- A higher `MAX_EDITOR_HL_LINES`, per D13.
- A tree rooted outside the repository. Dropped files cover that through
  `pins.rs`.

## Sources

- [tui-textarea `TextArea` API](https://docs.rs/tui-textarea/latest/tui_textarea/struct.TextArea.html)
- [tui-textarea repository](https://github.com/rhysd/tui-textarea)
- [`ignore` crate `WalkBuilder`](https://docs.rs/ignore/latest/ignore/struct.WalkBuilder.html)
- [LSP 3.17 specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/)
- [lapce](https://github.com/lapce/lapce) (Apache-2.0, **reference only**)
  — `lapce-rpc/src/file.rs`, `lapce-app/src/file_explorer/data.rs`,
  `lapce-proxy/src/plugin/lsp.rs`
- [Warp](https://github.com/warpdotdev/warp) (**AGPL-3.0** outside
  `warpui`/`warpui_core`, **reference only, read with care**) —
  `crates/sum_tree`, `crates/editor`, `crates/lsp`, `crates/fuzzy_match`
- [ratatui-explorer](https://github.com/tatounee/ratatui-explorer)
- [tui-tree-widget](https://crates.io/crates/tui-tree-widget)

# Changelog

All notable changes to Loupe are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Reviews, not just comments.** Loupe could say something about a line;
  it had no way to say anything about the pull request, or to approve one.

  - **Comments can be held.** In an inline comment draft, `Ctrl+S` now
    *adds it to a review* instead of posting it, and `Ctrl+Enter` posts it
    on its own the way `Ctrl+S` used to. Holding is the default because
    ten comments posted one at a time are ten notifications to everyone
    watching the pull request, with nothing tying them together.
  - **A review box under the file panel.** A summary, and a split button
    carrying the verdict: **Comment**, **Approve**, or **Request
    changes**, picked from its `▾` or with `Tab`. `R` gives it the
    keyboard, `Ctrl+S` submits.
  - **One request.** The summary, the verdict, and every held comment go
    up as a single GitHub review — the same thing *Start a review* and
    *Submit review* do on the web.
  - **It asks first, and says what goes.** The confirm prompt lists the
    verdict, the summary, and where each held comment will land. A review
    notifies every watcher and cannot be taken back, so it is the second
    thing loupe confirms — reverting is the first.
  - **Held comments are visible and durable.** A `💬` in the change bar on
    the lines each one covers, a `💬N` beside the file name, and the count
    in the box. They are written to
    `.git/loupe/pending-review-<number>.json` as they are made, so quitting
    loupe does not lose a review in progress; reopening the pull request
    picks them back up. `✕ Discard` throws them away, and asks once first.
  - **Refusals arrive in GitHub's own words.** "Can not approve your own
    pull request", or which comment fell outside the diff — `gh api`
    writes the API's JSON to stdout and only a status line to stderr, so
    both are read. Loupe catches an empty review and a bodiless "request
    changes" before sending, and warns when the PR head has moved under
    the held comments.

- **Merge conflict resolution.** A merge, rebase, cherry-pick, or revert
  that stops on a conflict is now the thing the review is about, rather
  than a wall of marker lines in a working-tree diff.

  - **Impossible to miss.** Conflicted files sort to the top of the file
    panel — in the tree view as well as the flat one — under a red
    `⚠ N MERGE CONFLICTS` heading, each with a red `[!]` icon and a red
    `!` status letter. The top bar turns into an orange
    `⚠ MERGE` / `REBASE` / `CHERRY-PICK` / `REVERT` badge naming the
    operation and what finishes it. The `+`/`−` counts are left blank on
    those rows: they would describe the marker text git wrote, not a
    change anyone made.
  - **The diff shows the disagreement, not the markers.** A conflicted
    file opens as *our version on the left, their version on the right*,
    with the `<<<<<<<` / `=======` / `>>>>>>>` lines removed. Every line
    the two branches agree on is in both sides and reads as context; every
    conflict is a changed section. So the whole diff view already works on
    it — `}` and `{` walk the conflicts, `z` folds the agreed stretches,
    `/` searches, and syntax highlighting is correct on both sides because
    both sides are real file text. A `⚑` in the change bar marks each one.
  - **One key resolves one conflict.** `o` — or a click on the `⚑`, or on
    the `[!]` in the file panel — opens a menu offering **take ours**,
    **take theirs**, **take both**, the **common ancestor** where git
    wrote one (`diff3` / `zdiff3`), *ours/theirs everywhere* for the whole
    file, *edit it by hand*, and *mark it resolved*. Each choice rewrites
    only that conflict and leaves the others' markers alone.
  - **Resolving finishes the job.** The last conflict settled in a file
    stages it, because git treats a path as conflicted until it is added
    (`x` takes it back out). The file list re-scans itself, the file
    leaves the conflict group, and the status line says what is left and
    which command finishes the operation.
  - **Conflicts markers cannot describe.** A delete/modify conflict has
    no markers to read. It still warns in the panel, and the menu offers
    *take our whole file* / *take their whole file*, read from the index
    stages — removing the file where the chosen side deleted it.
  - **Reverting is refused mid-conflict.** `git checkout HEAD -- <path>`
    during a merge throws the merge away for that path, so `u` / `U` and
    the `↺` markers say so and offer the resolve menu instead. The blame
    pane stands down too: its line numbers would point at the
    working-tree file, which still has the markers in it.

- **Ahead and behind the upstream.** `↑3 ↓2 origin/main` beside the
  branch name in local review: commits you have not pushed, and commits
  waiting for you. `≡ origin/main` when you are level with it. The
  upstream is the branch git has configured, falling back to
  `origin/<branch>`; the counts come from a local `git rev-list`, so
  nothing is fetched.

- **A markdown preview** (`P`, the `📖 Preview` button, or
  `☰ → Actions → Preview the markdown`). A `.md` file now has a document
  view as well as a diff and a source view, in the pane the diff and the
  editor share.

  - **A full render.** Headings with rules under them, wrapped
    paragraphs, bold, italic, strikethrough, inline code, links that keep
    their address, nested and ordered lists, `[ ]` / `[x]` task boxes,
    block quotes at any depth, GitHub tables with column alignment,
    thematic rules, YAML front matter, and fenced code blocks colored by
    the same syntax themes the diff uses. Underscored identifiers such as
    `MAX_RETRY_COUNT` are left alone rather than read as emphasis, which
    is what the files this pane exists for are full of.
  - **The preview and the source are two views of one file.** `P` moves
    between them (`Alt+P` from inside the editor, where plain letters are
    text) and both keep their place: from the preview you land in the
    editor on the line you were reading, and from the editor you land in
    the preview at the line you just changed. Unsaved text renders as it
    stands, so a heading can be changed, looked at, and changed again
    without saving in between.
  - **It follows the file.** The idle tick watches the modification time
    and re-renders when something else rewrites it, holding your place by
    source line rather than by row — a plan file updating on screen as an
    agent writes it. Unsaved text is never overwritten this way.
  - **Reaching a file that is not in the change.** `Ctrl+P` opens any
    `.md` file in the repository as a document, and `loupe md <path>`
    reads one from anywhere on the machine with no review behind it.
  - `}` and `{` walk the headings, `r` re-reads the file now, and the
    blame pane stands down while the preview is open — one source line is
    any number of rendered rows there, or none.

- **A blame pane between the file panel and the diff** (`B`, or
  `☰ → View → Blame column`). Every visible diff row gets a blame row
  beside it: who last touched the line, how long ago, and the pull
  request the commit landed in.

  - **An age heat map.** One hue — the palette's own neutral grey, with
    lightness doing the work — on an absolute scale: under a day, a week,
    a month, three months, a year, older. A shade means the same age in
    every file, so it is learned once. Keeping the ramp colorless leaves
    the two classes above it as the only *colored* things in the column,
    and those are the whole point: lines your working tree owns, and
    lines a commit in the change under review moved. That is what answers *"is this related to what I am doing
    now?"* before you read a word. Your own commits are drawn apart from
    everyone else's, so "mine" and "recent" stay two signals.
  - **A link to the pull request.** The number comes from the commit
    subject where there is one — a squash or merge commit names its own —
    and from one batched GitHub lookup for the rest, cached by commit for
    the session. Click any blame row for the commit behind it: hash,
    date, author, subject, pull request title, and whether it is part of
    the change on screen. `o` opens the pull request in your browser, `y`
    copies its link, `c` copies the hash. Local review works too — the
    repository comes from the `origin` remote, offline.
  - **Everywhere the diff is.** Pull request review and local review;
    split and inline layouts (a removed line is blamed on the old side,
    the only side that can say what you are deleting); and beside the
    editor, where a dirty buffer dims the column and says `stale` until
    you save. Drag the second divider to resize it, double-click for 30
    columns. A terminal too narrow for three panes shows two.
  - **No slower to open a file.** Blame runs on its own background job
    after the diff is on screen, not as part of the load.

  Off by default. `blame = true` in your config makes it the default;
  `blame_width` and `blame_pr_lookup` tune it.

- **Right-click the `PR #123` badge to copy the link to the pull
  request**: the top-left badge is now a click target. The link goes to
  the clipboard through the same route as every other copy (`pbcopy` /
  `wl-copy` / `xclip` / `xsel`, or OSC 52 over SSH), and the status line
  says which route it used. The URL comes from `gh pr view --json url`,
  so a GitHub Enterprise host stays correct. Hand it to a coding agent
  without leaving the review. A local-changes review has no pull
  request, so the `⎇ LOCAL` badge says so instead.

- **The diff keeps up with an agent**: local review now re-scans the
  working tree by itself, about 2 seconds after the last key press or
  mouse move and at most once every 5 seconds. Files a coding agent (or
  a second terminal) rewrites under an open review show up without a key
  press. It never interrupts: the re-scan stands down while the editor
  is open, while any overlay or menu is open, while lines are selected,
  and during a drag or a panel resize. A re-scan that finds nothing says
  nothing and moves nothing — the file panel stays exactly where it was
  scrolled. Pull requests are never polled: a PR head lives on GitHub,
  and a timer would spend API calls on a commit that moves a few times a
  day. Turn it off with `auto_refresh = false`, or for one session from
  `☰ → Refresh while idle`.

- **A `⟳` refresh button, and `r` behind it**: re-scans the changed-file
  list and reloads the open file, without a loading screen and without
  losing your place — scroll position, cursor row and folds all survive.
  In a pull request it fetches from GitHub. `r` used to reload only the
  open file, modally; it now does the whole thing.

- **A `☰` menu, and a top bar that fits**: the review top bar carried
  eleven buttons and had begun to crowd out the PR title. It now shows
  only what fits what you are doing — `🔍 Find` `✎ Edit` while reading,
  `💬 Comment` `⧉ Copy` once lines are selected, `⇥ Format` `💾 Save`
  `✕ Close` in the editor — plus `⟳` and `☰`. Everything else moved into
  the `☰` menu (`m`), grouped under **View**, **Find**, **Actions**,
  **Go** and **Settings**. The menu is built from the state it opens in:
  no *Comment* line in local review, no *Refresh while idle* switch on a
  pull request, and lines that cannot act right now are drawn dim and
  skipped by the cursor. Every line names the key that does the same
  thing outside the menu. Menu lines and toolbar buttons run through one
  dispatch, so the two can never disagree.

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

### Changed

- **`Back to PRs`, `🎨 Theme`, `? Help`, `◫ Split`, `≡ Inline` and
  `⇕ Fold` left the top bar** for the `☰` menu. Their keys (`b`, `t`,
  `?`, `v`, `z`) are unchanged.

- **New config key `auto_refresh`** (default `true`) — see
  [docs/configuration.md](docs/configuration.md).

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

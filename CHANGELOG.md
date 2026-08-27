# Changelog

All notable changes to Loupe are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **The words that changed are painted darker than the line.** A modified
  line keeps its red or green; the characters the two versions do not
  share take a stronger shade of the same colour. A line painted whole
  says the line changed and nothing about where — on a long line with one
  renamed variable in it, that is two lines you have to read against each
  other character by character. Tokens rather than characters, so a
  rename reads as one mark instead of a scatter of letters. Rewrites and
  generated-length lines keep the plain colours.

- **Right-click a tab for its path.** `Copy relative path` and `Copy full
  path`, the same question the file panel answers about a row. It matters
  most for a file pinned from outside the repository: the tab shows it,
  nothing else in the window does, and handing one to a coding agent
  meant going and finding it in the filesystem again.
- **Diffs open in tabs.** A tab is a file, and the same tab holds its
  diff, its buffer and its rendered document — `e`, `P` and `Esc` move
  between them and the tab stays put. Coming back to one lands you where
  you left it: same scroll, same cursor, same folds, same selection, and
  nothing is read again to get there. One click walks the change in the
  peek tab; a double click keeps it, and the next file gets its own.

- **`Ctrl+F` finds in whatever you are reading.** The diff, the markdown
  preview and the editor all answer the same key, and `/` still opens the
  first two. `Ctrl+Shift+F` runs the repository-wide `git grep` that `#`
  has always run. `Cmd+F` works where the terminal forwards it — most keep
  it for their own find bar.
- **The markdown preview can be searched.** It had no search at all, which
  is the one pane where a long plan file is read end to end. It matches on
  the rendered document rather than on the markdown behind it, so you look
  for what is on the page; `n` and `N` walk the matches, and each one
  lights up keeping the colors around it.

- **Staged and unstaged, told apart.** The local file panel is now in two
  halves: `STAGED` at the top — what `git commit` would take — and
  `UNSTAGED` below it. Stage a file and its row moves up; unstage it and
  it moves back down. Either heading folds its section away, and carries
  the action for the whole of it: `✚ all` stages everything, `↩ all` takes
  it all back out. `X` does the same from the keyboard. A partly staged
  file is listed once, under `STAGED`, with the `[±]` icon that says the
  rest of it is not. Staging everything is refused while a merge conflict
  is open, because `git add` mid-merge means "resolved".
- **A stash menu.** `S`, or `📦 Stash…` in the ☰ menu, offers the three
  questions git asks: stash the tracked changes, take the untracked files
  with them, or take only what is in the index. Every one of them asks for
  a name first — press Enter on an empty box and git names it itself. The
  menu lists your stashes below those lines; click one to apply it, pop
  it, or drop it.
- **Commits you have not pushed.** A third panel beside `Change` and
  `Files`: `Commits` lists what this branch has that the upstream does
  not, newest first. Open a commit to see its files; open a file to read
  that commit's diff, against the commit's own first parent. The change
  pane only ever showed uncommitted work, so on a branch thirty commits
  ahead the other twenty-nine were invisible.
- **The whole repository in the file panel.** `F` swaps the panel between
  the files the change touches and every file in the repository, each with
  its own tree and its own collapse state. Ignored files are listed and
  drawn dim; an ignored directory costs one row until it is opened, and
  then is read one level at a time. A row there is a file and nothing
  else — no stage box, no `+`/`−` counts, no `↺`, and clicking one opens
  the file rather than its diff, even when the change touches it.
  Right-click to create, rename or delete a file — delete asks first.
- **One click peeks at a file.** Walking a tree means opening a great many
  files to look at one of them, so a click in the `Files` panel puts each
  file in the same **peek tab**, drawn in italics to say it is about to be
  replaced. Double-click the file or the tab to keep it. A buffer you have
  typed into is never replaced. (A *peek* is a tab you have not committed
  to; a *preview* is still the rendered markdown document behind `P`.)
- **The file panel keeps working with the editor open.** Clicking another
  file parks the one you were in rather than asking you to save or close
  it. It keeps its tab, its cursor, its scroll and anything unsaved.
- **More than one file open at once.** Opening a second file parks the
  first rather than closing it; every one shows in the tab row with a `●`
  on any that is unsaved. `Alt+]` and `Alt+[` step between them, and `q`
  counts the unsaved buffers before it takes them.
- **The tab row keeps the order you opened the files in.** Clicking a tab
  opens that file and moves nothing. Drag a tab along the row to put it
  where you want it, and click its `✕` (or middle-click it) to close one.
- **Double-click a word in the editor to select it.** The selection takes
  the whole identifier, wherever in it you clicked, and every other place
  that word appears in the file is marked while it holds. Triple-click
  takes the line.
- **Right-click the editor for a code menu.** The click takes the word
  under the pointer first, so the menu names what it is about: go to the
  definition, find every use, what is this, the signature, rename, fixes
  and refactors, and the edits that act on the selection. A line that
  needs a symbol is drawn dim rather than dropped when the pointer is on
  punctuation, so the menu keeps its shape.
- **The function keys.** `F12` goes to the definition and `F10` (or
  `Shift+F12`) finds every use, in the editor and in the diff view.
  `F2` renames, `F8` and `Shift+F8` walk the problems, `F3` and
  `Shift+F3` walk the find matches, and `F1` opens the help card.
- **Walk the problems in a file.** `F8` and `Shift+F8` put the cursor on
  the next and previous one, in line order and wrapping at the ends, and
  read the message out in the status bar. `Alt+E` lists them all: type to
  filter, `Enter` goes to the line. Both are in the `☰` menu and the
  right-click menu under `PROBLEMS`, and neither appears on a clean file.
- **Suggestions as you type.** The completion popup opens on its own
  after one character of a name, and after a character the server asks to
  be told about. Loupe now reads those trigger characters from the server
  rather than guessing at `.`, `:` and `>`, and tells the server *why* it
  is asking — which is what makes `object.` in TypeScript list the
  object's fields instead of everything in scope. `Tab` takes the
  highlighted one. Narrowing is forgiving: a name that starts with what
  you typed sorts first, and one that merely contains those letters in
  order still shows, so a typo no longer closes the list.
  `suggest_while_typing = false` goes back to `Ctrl+Space` only.
- **Problems you can actually read.** The offending span is underlined as
  well as colored, the gutter carries `✗` `▲` `ℹ` by severity, and the
  message itself is drawn in the margin past the end of the line. Errors
  are red and warnings are yellow, from their own palette entries rather
  than borrowed from the staging marks.
- **`Alt+X` explains one problem.** TypeScript folds its reasoning into a
  single sentence and the answer is the last clause. The panel puts each
  reason under the one it explains, picks the names and types out of the
  prose, and breaks a wide object type over lines at its semicolons.
- **Lint, beside the compiler.** Loupe runs `eslint` and `ruff` over the
  buffer as you edit and draws what they say alongside the language
  server, with the tool on every message — `eslint(no-undef)`,
  `typescript(2552)`. It prefers the project's own copy in
  `node_modules/.bin`, feeds it the buffer on standard input so the lint
  is for what is on screen, and never blocks the editor. ESLint's
  severities are turned the right way round on the way in. Add another
  with a `[[linter]]` table, or set `linters = false`.
- **Find and replace in the editor** (`Alt+F`), with the prompt in the
  border so nothing on screen moves while you search.
- **Find every use from the editor** (`Alt+R`) — the list `gr` has always
  given in the diff, which the editor could not reach.
- **Rename a symbol** (`Alt+M`), **fixes and refactors** (`Alt+.`) and
  **signature help** (`Alt+S`). A rename opens every file it touched as an
  unsaved buffer to read before saving; nothing is written behind you.
- **Comment toggle** (`Alt+C`), an `Enter` that keeps your indent, and the
  bracket under the cursor matched with its partner.
- **Language servers from the config file.** A `[[server]]` table adds a
  language or replaces a built-in one. `loupe --lsp` reports yours too.

### Changed

- `Ctrl+F` no longer pages forward in the diff and the preview — it opens
  the search. `PageDown` and `Ctrl+D` still page, and in the preview so
  does `Space`.
- `j` and `k` in the file panel now walk the rows on screen rather than
  the file list. The staging sections make row order and list order
  different things, and the cursor has to follow what you can see.
- The panel title's staged count now counts what is in the index, partly
  staged files included, so it agrees with the `STAGED` heading.

### Fixed

- **Right-clicking the PR badge did nothing while a document or the
  editor was open.** Each mode answered its own clicks and each answered
  a different subset of the window; the badge was only ever wired up in
  the diff. The window chrome is answered once now, above the mode split.
- **A reader in a document was locked out of the app.** Swapping to the
  pull request, going back to the list, the grep, the symbol list, the
  Commits panel, staging and the stash menu all did nothing there — none
  of which is about the pane in front of you.
- **Four commands refused while a file was open in the editor** — the
  finder, the PR ⇄ local swap, the refresh, and reverting. Buffers park
  rather than close, so none of them had anything to protect. Reverting
  now stops only for unsaved edits to the very file being reverted, which
  is the one case where it would quietly undo itself.
- **The second click of a double click went nowhere.** A file loading
  froze the whole window, and a read takes tens of milliseconds against a
  four-hundred-millisecond double click, so every one of them landed
  mid-read. A load is modal for the pane now, not for the window.
- `git show` ran in whatever directory loupe was started in rather than in
  the repository root, which every other git call uses. The same
  repository today, and silently the wrong one the moment the two differ.
- A nested worktree or repository made `Ctrl+P` show a blank row: `git
  ls-files` reports such a directory instead of its contents, and the entry
  was read as a file with no name.
- Editing a large file could freeze the window. `didChange` was written to
  the language server's pipe from the drawing thread, and a buffer bigger
  than the pipe waited there on a server busy indexing. Each server has a
  writer thread now.
- Loupe never sent `textDocument/didClose`, so every file opened in a
  session stayed open on the server for the rest of it.
- **Go to definition landed in the wrong file in a TypeScript project.**
  Asked before it has finished loading the project, tsserver answers a
  definition by pointing at the `import` line in the file you are already
  in, rather than the file the symbol is defined in. That answer is not
  empty and not an error, so neither of the "still indexing" rules caught
  it, and the first `F12` or `gd` after opening a TypeScript repository
  went one line up instead of to the other file. Find-every-use had the
  same shape: only the current file's uses came back. Rust never showed
  it, because rust-analyzer returns *nothing* while it indexes and the
  empty-answer rule already covered that.

  An answer given while a server is still doing the work it started at
  launch is now treated as provisional whatever it contains, and the very
  first question of a session waits out a short grace before it is
  believed. The obvious refinement — end that grace as soon as the server
  says it has finished something — was measured and made the failure
  *more* frequent, because tsserver announces smaller pieces of work
  before it gets to the project. The elapsed time is the only part a
  server cannot mislead us about, so that is what the wait is built on.
- **A cold language server failed the first question instead of waiting
  for its index.** rust-analyzer answers `ContentModified` for most of
  the time it spends loading a project, and loupe read that as a failure
  rather than as the "send it again" the specification says it is. So the
  first `gd`, `gr` or `F12` after launch — the one everybody presses —
  reported an error or nothing at all. Loupe now re-asks while the server
  reports progress, and the budget for that grew from 12 seconds to 45,
  which is what a real project actually costs.
- **`loupe --lsp` promised a TypeScript server that could not run.**
  `typescript-language-server` is a wrapper around the `typescript`
  package, and having the first without the second is a `✓` beside
  TypeScript and a server that dies on every question. The doctor now
  looks for the `tsserver` it would drive and says which half is missing.
- **The editor asked a language server in complete silence.** A lookup
  from the editor set nothing on screen: no spinner, no message, nothing
  until the answer landed up to half a minute later. There was no way to
  tell a key that did nothing from a server that was still indexing. The
  status bar now names what it is waiting for and spins until it lands.
  Typing still works throughout — the wait was never modal, only
  invisible.
- The pinned tab row is per worktree, not per clone. Both pages said clone.
- One test's doc comment held three copies of its own first line.
- The `●` on a tab with unsaved work was drawn in one color for both tab
  backgrounds, and the selected tab's is a saturated blue — so the mark
  vanished on the one tab it was most needed on, the file being typed
  into. The two backgrounds have their own colors now.
- Tabs ran together as one line of file names. Each has a seam beside it,
  a space between the `●` and the name, and its own `✕`.
- **Clicking through the file tree flashed the whole window.** Loupe
  closed the old buffer at the click and opened the new one when the read
  landed, so every frame in between drew a pane with nothing in it — which
  is the diff. Nothing on screen changes now until the file is in hand.
  The click itself costs 10-70 µs and the read lands in under 2 ms; the
  flash was never slowness, it was the order of the work.
- A file the reader clicked waited up to 80 ms to be picked up, because
  the main loop paces itself off the spinner. A read the reader is sitting
  and waiting for gets the next frame instead.
- **A pinned file opened twice.** Clicking one in the file tree gave it a
  second tab beside its pin — a peek on one click, a kept tab on two — so
  one file sat in the row twice and an edit went into whichever of them
  was on screen. A pinned file now opens through its pin, and a buffer
  whose file is pinned draws no tab of its own. Its unsaved `●` moved to
  the pin tab, where the file's tab actually is.
- **Every tab said "save or discard first".** Switching to a pinned tab
  refused outright while anything was unsaved. The buffer is parked
  instead: it keeps its text, its place in the row and its `●`, and the
  reader goes where they were going.

- **Loupe tells your coding agent what you are reading.** Loupe is the
  only process on the machine that knows which lines a human has on
  screen and is judging. An agent in the next pane has to guess, and it
  pays for the guess in grep calls. `loupe ctl context` publishes the
  file, the line range, the hunk, the comments you are holding, and the
  files you have not marked viewed. Wired into an agent's
  `UserPromptSubmit` hook, every instruction you type carries that block,
  so *"rename this"* means the lines under your cursor. Nothing is typed
  into your prompt and no pane ids are involved: the repository root is
  the address, so tmux, terminal splits, and separate windows all behave
  the same. `Y` copies the same block by hand, for an agent with no hooks
  or a review at the far end of an SSH connection. Unix only — the
  standard library has no unix sockets on Windows. See
  [docs/agent-context.md](docs/agent-context.md).

- **`loupe ctl install` sets that hook up for you**, and the first-launch
  wizard offers it as a last step when it finds a coding agent. Claude
  Code and Codex are both supported. The installer keeps every hook and
  setting already in the file, saves the old file beside the new one as
  `<name>.loupe.bak`, and marks its own hook so a second run replaces
  that hook rather than adding another. `loupe ctl uninstall` takes it
  back out.

  The two agents take the answer differently, and the installer writes the
  right command for each. Claude Code reads a hook's plain stdout as
  context. Codex ignores stdout and reads one JSON object, so its hook
  calls `loupe ctl context --json`. Codex also refuses to run a hook it has
  not been shown: approve loupe's once in an interactive `codex` session.

  The installer refuses to write a hook pointing inside a Cargo build
  directory. Such a path works until the next `cargo clean` and then fails
  the way hooks fail — quietly, in somebody else's process.

### Notes

- The hook gives up after about a second. A suspended loupe cannot answer
  (`SIGSTOP` stops the socket thread with the rest of it), and a prompt
  that waits on a stopped process is worse than a prompt with no context.
  Loupe serves each question on its own thread, so one client that
  connects and says nothing cannot hold up the next.

- The context block names at most 8 files in each list and then says how
  many it left out. Holding 40 review comments no longer puts 40 lines in
  every prompt you type.

- Loupe clears socket files left behind by loupes that were killed. The
  accept loop only ends when the process does, so nothing on the way out
  can do it; the sweep runs at startup instead and leaves any socket
  another loupe still answers.

## [0.2.1] - 2026-08-25

Two fixes for 0.2.0, both for bugs that only showed up on a machine the
release was not built on. The seven commits behind 0.2.0 were pushed
together at the tag, so the first time CI ran them on Windows and macOS
was after the binaries had shipped.

### Fixed

- **Dropping a file on the window did nothing on Windows.** A dropped
  path arrives written in as if typed, and loupe reads it the way a
  shell would: a backslash escapes the character after it. On Windows
  the backslash is the path separator, so `C:\Users\me\notes.md` was
  read as `C:Usersmenotes.md` — not an absolute path, not a file that
  exists, and the drop was refused. Every drop, every time; the tab row
  could only be filled with `Ctrl+O`. Backslash is now a separator
  rather than an escape there, and quoting — which is how a Windows
  terminal spells a path with a space in it — is unchanged.

- **rust-analyzer was *still* reported as installed when it was not.**
  0.2.0 fixed the half of this that reads the stand-in rustup keeps in
  `~/.cargo/bin` for every tool it could provide, but only recognised a
  *symlink* to rustup. A normal install **hard-links** them instead: the
  link is the same file, so resolving it gives back the stand-in's own
  name and the check said "a real tool". Loupe then offered `gd`, `gr`
  and `K` and failed at the first request with rustup's "Unknown binary"
  error — the exact bug 0.2.0's notes say was fixed. A hard link shares
  the original's inode, so that is what is compared now, against the
  `rustup` binary sitting in the same directory. The test is exact: a
  real tool that `cargo install` put in `~/.cargo/bin` is a different
  file and is still taken at face value.

  The other half of the same bug: `Client::start` asked `which` whether
  the server was installed — which resolves a stand-in to the real
  binary — and then started the *bare name*, which goes back through the
  stand-in. It starts the path `which` resolved now, so the answer to
  "is it installed?" and what actually runs are the same thing.

  The inode comparison is Unix-only. A Windows machine whose rustup
  stand-ins are hard links or copies is not covered by it.

## [0.2.0] - 2026-08-25

### Added

- **Pinned files, and a row of tabs to hold them.** A review sends you
  away from the file you were reading — to the plan file an agent is
  still writing, to the design note that says why the change looks like
  this. The file panel only lists what the change touches, so none of
  those were one key away, and half of them were not in the repository at
  all.

  - **A tab row under the top bar.** One tab per pinned file, numbered.
    `=` pins whatever is in front of you (a document, an editor buffer,
    or the file under the panel cursor), `1`–`9` open a tab, `,` and `.`
    step through them, and `-` unpins the one you are reading. Click a
    tab to open it, its `✕` to unpin it, or middle-click it. The same
    keys with `Alt` work from inside the editor, where a bare key is a
    letter. The row takes no height at all until something is pinned.
  - **Drop a file on the window to read it.** Drag a `.md` file onto
    loupe and it pins it and renders it, from anywhere on the machine —
    `~/Downloads`, another checkout, a scratch directory. Nothing is
    copied into the repository, so there is nothing to remember not to
    commit. Drop several at once and each gets a tab.
  - **Both kinds of terminal.** Terminals answer a drop in one of two
    ways, and loupe now reads both. Ghostty, iTerm2 and Terminal.app wrap
    the path in bracketed paste, which loupe turns on — so a paste into
    the editor, a comment, or the finder also arrives whole now instead
    of as a burst of keystrokes. Warp writes the path in as if it had
    been typed, one key at a time; loupe reads each batch of input for a
    path before dispatching any of it. Read as ordinary keys, the leading
    `/` of the path opened the search prompt and the rest of the path
    filled the query box along the bottom of the window — the file never
    opened.
  - **Files from outside are marked.** A `↗` on the tab, because "the
    plan file" means a different document depending on the answer.
  - **Any file, not only markdown.** A markdown tab renders as a
    document; a tab for a file the change touches opens its diff; any
    other opens in the editor, and `Ctrl+S` saves it — outside the
    repository or not.
  - **`Ctrl+O` opens a file by path.** Type or paste one, absolute, `~`,
    or relative to the repository root. It is the way in on a terminal
    that cannot report a drop, and the fast way to follow a path an agent
    just printed.
  - **The tabs come back.** They are written to `.git/loupe/pins.json` as
    they change, so quitting loupe does not cost you the row. A pin whose
    file has since been deleted drops out on read rather than becoming a
    tab that fails on every click.

- **rust-analyzer was reported as installed when it was not.** rustup
  keeps a stand-in in `~/.cargo/bin` for every tool it *could* provide,
  installed or not: on a machine that never added the component,
  `~/.cargo/bin/rust-analyzer` is still there as a link to `rustup`
  itself. Loupe looked for a file at that path, found one, and believed
  it — so `loupe --lsp` printed a ✓ beside Rust, the help overlay called
  it installed, and `gd` / `gr` / `K` on a `.rs` file failed with
  rustup's "Unknown binary" error instead of quietly falling back to
  pattern matching. Anything that resolves to rustup is now checked with
  `rustup which` before it is believed, and the answer is kept for the
  session. `loupe --lsp` also says "not installed" rather than "not found
  on PATH", which was the wrong place to send people looking.

- **The first thing you typed after launch was thrown away** (macOS).
  Loupe asks the terminal for its background color at startup, and asked
  down a second handle on `/dev/tty`. Opening and closing another
  descriptor on the same terminal disturbs the registration the event
  reader holds on stdin, and the next input to arrive — whenever it
  arrived — was swallowed re-arming it. A first key press is easy to miss
  and press again; a first dropped file simply vanished. The query now
  goes out on the terminal loupe already owns.

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

[Unreleased]: https://github.com/jjacoblee/Loupe/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/jjacoblee/Loupe/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/jjacoblee/Loupe/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jjacoblee/Loupe/releases/tag/v0.1.0

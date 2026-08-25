# Keyboard & mouse reference

The mouse is the primary interface, but everything is also drivable from
the keyboard — the diff view with vim motions. A cursor row (underlined,
the same mark the editor uses) moves with the motions; clicking a line
puts the cursor there, so the two never disagree. Press `?` inside Loupe
for the built-in help overlay.

## Global

| Key | Action |
| --- | --- |
| `q` | Quit |
| `?` | Help overlay |
| `Esc` | Clear the selection, then back / close overlay / close editor / cancel a load |
| `r` | Reload the current file (or the PR list) |
| `b` | Open the PR list (from an auto-opened PR) |
| `l` | Switch to local-changes review (from the PR list) |
| `t` or `🎨 Theme` | Open the theme picker — live preview; Enter keeps & saves, Esc reverts; `a` switches light ⇄ dark |
| `c` | Cancel a cancellable background load |

## File panel

| Key / mouse | Action |
| --- | --- |
| Click a file | Open its diff |
| Click a folder | Collapse / expand it |
| Right-click a file or folder | Copy its path (see [Copying a path](#copying-a-path)) |
| `]` / `[` | Next / previous file |
| `x` or click the icon column | PR: toggle *viewed* (syncs to GitHub) · Local: stage / unstage the file |
| Click `↺` at the end of a row | Revert every change in that file (asks first) |
| `Tree` / `Flat` buttons | Switch between tree and flat list |
| Drag the divider | Resize the panel |
| Double-click the divider | Reset the panel width (34 columns) |
| `<` / `>` | Narrow / widen the panel |

## Diff view — move

| Key | Action |
| --- | --- |
| `j` / `k`, `↑` / `↓` | Line down / up |
| `Ctrl+D` / `Ctrl+U` | Half page down / up |
| `Ctrl+F` / `Ctrl+B`, PgDn / PgUp | Full page down / up |
| `gg` / `G` | First / last line (also `Home` / `End`) |
| `}` / `{` | Next / previous run of changes |
| `H` / `M` / `L` | Top / middle / bottom of the screen |
| `Ctrl+E` / `Ctrl+Y` | Scroll a line, leaving the cursor put |
| `h` / `l`, `←` / `→` | Scroll sideways (gutters stay pinned) |
| `0` / `$` | First / last column |
| `]` / `[` | Next / previous file |

> `n` and `p` used to mean "next / previous file". They now step through
> search matches, the way they do in vim; `]` / `[` (which always did the
> same job) are the file keys.

Mouse: the wheel scrolls vertically; a horizontal trackpad swipe (or
tilt wheel) scrolls sideways, as does the wheel with a modifier held —
Shift is the convention, but terminals differ on which modifier they
pass through, so Alt and Ctrl work too. Clicking a line places the
cursor.

## Diff view — act

| Key / mouse | Action |
| --- | --- |
| `V` | Start a line selection — motions extend it, `Esc` cancels |
| Click a line | Select the whole line (what commenting anchors to) |
| Drag through text | Select exactly those characters, on the side you started in |
| `c` or `💬 Comment` | Comment on the selection, or on the cursor line (PR review) |
| `Enter` / `Space` | Expand or fold the run at the cursor |
| Click `··· N unchanged lines ···` | Expand a folded run |
| Click `⌃⌃⌃ … click to fold ⌃⌃⌃` | Re-fold that run |
| `z` or `⇕ Fold` | Fold / unfold every unchanged section |
| `v` or `◫ Split` / `≡ Inline` | Toggle side-by-side vs. inline layout |
| `e` / `i`, double-click a new-side line, or `✎ Edit` | Edit the file at the cursor line |
| `x` | Toggle *viewed* (stages the file in local review) |
| Click `↺` in the change bar | Put that section of the diff back (asks first) |
| `u` / `U` | Revert the change at the cursor / every change in the file |
| `y`, `Ctrl+C`, or `⧉ Copy` | Copy the selected lines — or the cursor line — to the clipboard |

### Putting changes back

Every changed section of the diff carries a `↺` in the change bar down
the left edge of the pane, and every row of the file panel carries one at
its right-hand end. Both ask before anything happens — reverting is the
only thing Loupe does that git cannot undo for you.

- **`↺` in the change bar** (or `u` at the cursor) puts one section back:
  the lines it covers return to the old side and every other change in
  the file stays exactly as it is. Only the working tree is written —
  anything you had staged stays staged. Clicking a marker acts on *that*
  section wherever the cursor happens to be.
- **`↺` in the file panel** (or `U`) reverts the whole file with
  `git checkout <old> -- <path>`, which puts the index back too, so the
  file stops showing as changed at all. A file the change *created* has
  nothing to go back to: the prompt says so, and reverting deletes it.
- The "old side" is whatever the diff is against — `HEAD` in local
  review, the merge base in a pull request.
- A PR opened **Review only** offers neither, and the change bar isn't
  drawn: there is no working tree of yours to put back.
- If the file changed on disk since the diff was loaded, the revert
  refuses rather than writing a stale copy over it — press `r` first.

### Copying

Loupe turns on mouse reporting so files, folds and diff lines are
clickable, and that takes away the terminal's own click-drag selection.
Two ways to get text out:

- **Drag through the text** to select exactly the characters you want —
  mid-word to mid-word, across as many lines as you like — then **`y`**
  (or `Ctrl+C`) copies precisely that. Dragging past the top or bottom
  scrolls and keeps selecting.
- A **click** (without dragging) selects the whole line instead, which is
  what commenting anchors to, and `V` starts a keyboard line selection.
  `y` copies whichever kind you have; with nothing selected it copies the
  cursor line.
- The selection is pinned to the side it started on. The old and new
  panes are two different documents, so a selection spanning both would
  copy nonsense — dragging across the divider keeps selecting the side
  you began in. That's what makes copying removed code work: the old
  text exists nowhere on disk, and only the diff still has it.
- **Hold a modifier and drag** to get the terminal's native selection
  back for anything else on screen: Option on macOS Terminal and iTerm2,
  Shift on most others.

Loupe copies through `pbcopy` / `wl-copy` / `xclip` / `xsel` when one is
installed, and falls back to asking the terminal itself (OSC 52), which
is what makes copying work over SSH. The status line says which was
used.

### Copying a path

The other thing worth copying is not in the diff at all: the path of the
file you are looking at, for a shell command or an agent prompt. The file
panel shows paths shortened and split across tree rows, so there is
nothing to drag through.

**Right-click any row of the file panel** — a file or a folder — and a
small menu opens at the pointer:

- **Copy relative path** (`r`) — the path as git spells it, relative to
  the repository root: `src/app.rs`.
- **Copy full path** (`f`) — the same path from the root of the disk:
  `/home/you/work/loupe/src/app.rs`.

Click a line, or press its letter. `↑` / `↓` move, `Enter` copies the
selected line, and `Esc` or a click anywhere else closes the menu. The
status line repeats the path that went to the clipboard.

The menu works while the editor is open, because copying a path does not
change which file is on screen. Right-clicking the *diff* still clears
the selection.

## Find

One overlay answers three questions; a prefix character in the input
picks which. Open it with `Ctrl+P` or the `🔍 Find` button.

| Key / mouse | Action |
| --- | --- |
| `/` | Search the open diff, incrementally — matches highlight as you type |
| `n` / `N` | Next / previous match (wraps, and says when it wrapped) |
| `Esc` | Cancel the search (restoring your place), then clear the highlight |
| `Ctrl+P` or `🔍 Find` | Fuzzy-match a file by path |
| `#` | Find text in files — one `git grep`, definitions sorted first |
| `@` | Definitions in the open file |
| `Tab` (in the finder) | Widen from the changed files to the whole repository |
| `Ctrl+R` (in the finder) | Treat the query as a regular expression |
| `Ctrl+U` / `Ctrl+W` | Clear the input / rub out the last word |
| `Enter`, or click a row | Go there |

A result in a file that isn't part of the change opens **in the editor** —
there is nothing to diff it against, so it is shown as what it is: a
file. Edit it and `Ctrl+S` if you want, or just read it; `Esc` closes and
the diff underneath is exactly as you left it. (If the branch under
review isn't checked out, the file comes from the commit instead of your
working tree and the editor says so and refuses to save — writing it
would edit whatever branch you happen to have out.)

Searches read what you are *reviewing*, not what happens to be on disk:
in PR review that is the PR's head commit, in local review the working
tree (including files git doesn't track yet).

## Language servers

`gd`, `gr` and `K` use a language server — one you already have
installed. Loupe starts it on demand, hands it the buffer on screen, and
falls back to pattern matching when there isn't one. Run `loupe --lsp`
to see what it found.

| Key | Action |
| --- | --- |
| `gd` | Go to the definition |
| `gr` | Find every reference, in the finder's result list |
| `K` | What is this? — the type and its documentation |

Which symbol on the line? Click one first and it is used; otherwise, if
the line holds several, loupe asks rather than guessing.

| Language | Server | Install |
| --- | --- | --- |
| TypeScript / JavaScript | `typescript-language-server` | `npm install -g typescript-language-server typescript` |
| Go | `gopls` | `go install golang.org/x/tools/gopls@latest` |
| Rust | `rust-analyzer` | `rustup component add rust-analyzer` |

`tsserver` on its own is not enough: it speaks its own protocol, not LSP.
`typescript-language-server` is the wrapper that does — loupe finds the
`tsserver.js` to pair it with, preferring the copy in your project's
`node_modules` so a pinned TypeScript version is the one doing the
analysis.

Set `language_servers = false` in your config to switch all of this off.

## Editor

| Key / mouse | Action |
| --- | --- |
| Click | Place the cursor |
| Drag | Select text |
| `Ctrl+S` | Save (the diff refreshes immediately) |
| `Ctrl+C` | Copy the selection, or the cursor line |
| `Ctrl+Z` or `Ctrl+U` | Undo (`Ctrl+U` is tui-textarea's own binding; `Ctrl+Z` is an alias) |
| `Ctrl+R` | Redo |
| `PgUp` / `PgDn` | Page through the file |
| `Esc` | Close (press twice to discard unsaved changes) |

### In the editor, with a language server

The diff view's `gd` / `gr` / `K` can't work in here — plain letters are
text — so these are on Ctrl keys that `tui-textarea` leaves free.

| Key | Action |
| --- | --- |
| `Ctrl+Space` | Suggest completions (they also appear on their own after `.`, `:` or `>`) |
| `Tab` / `Enter` | Accept the highlighted suggestion · `↑` `↓` to move · `Esc` to dismiss |
| `Ctrl+G` | What is this? — the type and docs for the symbol at the cursor |
| `Ctrl+]` | Go to the definition (vi's jump-to-tag key) |
| `Ctrl+T` | Format the file, or the `⇥ Format` button |

**Diagnostics** appear as you type, without saving: a `●` (error) or `▲`
(warning) in the gutter, the offending span colored, and the message in
the status bar when the cursor is on that line. When it isn't, the status
bar shows the count for the file, so a problem off screen isn't invisible.

The buffer is pushed to the server a fifth of a second after you stop
typing, so what it's checking is what you're looking at — not the file on
disk.

`format_on_save = true` in your config runs the formatter on every
`Ctrl+S`. It's off by default: reformatting a file mid-review would put
changes in the diff that nobody asked for. Either way `Ctrl+Z` undoes a
format in one keystroke.

## Theme picker

| Key / mouse | Action |
| --- | --- |
| `j` / `k`, wheel, or click a name | Preview that theme live |
| `PgUp` / `PgDn`, `Home` / `End` | Move through the list faster |
| `a` or the `☀ Light` / `🌙 Dark` button | Switch light ⇄ dark, carrying the selection to the counterpart theme |
| `Enter` or `Use … ` button | Keep the theme and save it to your config |
| `Esc` or `Cancel` | Put the previous theme (and appearance) back |

## Command line

| Flag | Effect |
| --- | --- |
| `--pr`, `-p` | Straight to pull requests |
| `--local`, `-l` | Review local changes, even when clean |
| `--auto` | Local changes if any, else PRs (overrides config) |
| `--theme <name>` | Use a theme for this session, unsaved |
| `--light` / `--dark` | Force light or dark colors for this session |
| `--themes` | List syntax themes and exit |
| `--lsp` | Report which language servers are installed, and how to add the rest |
| `--help`, `-h` | Usage and config reference |
| `set-theme [--light] <name>` | Save a theme to the global config, in the dark (or light) slot |
| `appearance` | Report what your terminal says its background is |
| `setup` | Re-run the first-launch setup wizard |

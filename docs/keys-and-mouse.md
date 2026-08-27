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
| `r` or `⟳` | Refresh — re-scan the changed files and reload the open one, keeping your place |
| `m` or `☰` | Open the menu (see [The top bar and the ☰ menu](#the-top-bar-and-the--menu)) |
| `b` | Open the PR list (from an auto-opened PR) |
| `l` | Switch to local-changes review (from the PR list) |
| `` ` `` or `☰ → Swap` | Swap between the pull request and your local changes, keeping your place in both (see [Swapping between the two reviews](#swapping-between-the-two-reviews)) |
| `B` or `☰ → Blame column` | Show / hide the blame pane (see [The blame pane](#the-blame-pane)) |
| `P` | Render the open markdown file as a document (see [Markdown preview](#markdown-preview)) |
| `=` | Pin the file in front of you to the tab row, or unpin it (see [Pinned files](#pinned-files)) |
| `1` … `9` | Open that tab |
| `Ctrl+O` | Open a file by path, from anywhere on the machine |
| `t` or `☰ → Theme` | Open the theme picker — live preview; Enter keeps & saves, Esc reverts; `a` switches light ⇄ dark |
| `c` | Cancel a cancellable background load |

## The top bar and the ☰ menu

The top bar shows only what fits what you are doing right now. Everything
else lives behind `☰`.

| What you are doing | What the bar offers |
| --- | --- |
| Reading a diff | `🔍 Find` · `📖 Preview` (markdown only) · `✎ Edit` · `⟳` · `☰` |
| Lines selected | `💬 Comment` (pull request only) · `⧉ Copy` · `⟳` · `☰` |
| Editor open | `⇥ Format` · `💾 Save` · `📖 Preview` (markdown only) · `✕ Close` · `☰` |
| Preview open | `✎ Source` · `⟳` · `✕ Close` · `☰` |
| Pull request list | `⎇ Local changes` · `⟳` · `☰` |

The badge at the left of the bar names what you review: `PR #123`, or
`⎇ LOCAL` for a local-changes review. **Right-click the `PR #123` badge
to copy the link to the pull request.** That is the URL a coding agent
needs, and the badge is the one place on screen that always carries it.
The status line repeats the link and names the clipboard route. The
`⎇ LOCAL` badge has no pull request behind it, so it says so instead.

`☰` (or `m`) opens the full menu, grouped under **View**, **Find**,
**Actions**, **Go** and **Settings**. It is built from the state you open
it in: no *Comment* line in local review, no *Refresh while idle* switch
on a pull request, and lines that cannot do anything right now (*Copy*
with nothing selected) are drawn dim and are skipped by the cursor.

| Key in the menu | Action |
| --- | --- |
| `j` / `k`, `↑` / `↓` | Move to the next live line |
| `Enter` / `Space` | Run the selected line |
| The key in the right-hand column | Run that line straight away |
| `Esc` | Put the menu away |
| Click a line | Run it · click anywhere else closes the menu |

Every line names the key that does the same thing outside the menu, so
the menu is a way to learn the keys rather than a second set of them.

## Refreshing

Loupe re-reads what it is showing you in two ways.

- **`r`, or the `⟳` button** — re-scans the changed-file list and reloads
  the open file. There is no loading screen and no jump: your scroll
  position, the cursor row and the folds all survive. Use it whenever you
  think the diff has gone stale.
- **While you sit idle** — in local review only, Loupe re-scans the
  working tree about 2 seconds after your last key press or mouse move,
  and at most once every 5 seconds. This is what keeps the diff current
  while an agent (or a second terminal) writes to your files.

The idle re-scan never interrupts you. It stands down while the editor is
open, while any overlay or menu is open, while lines are selected, and
during a drag or a panel resize. When it finds nothing it says nothing
and moves nothing — the file panel stays exactly where you scrolled it.
When it does find a change, the status line says so.

A pull request is never polled: its head lives on GitHub, and checking it
on a timer would spend API calls on a commit that moves a few times a
day. Press `r` (or click `⟳`) to fetch it.

Turn the idle re-scan off for one session from `☰ → Refresh while idle`,
or for good with `auto_refresh = false` in your config.

## Swapping between the two reviews

`` ` `` (backtick), or `☰ → Go → Swap`, moves between the pull request and
your own uncommitted changes. The menu line names the side you are going
to: `⇄ Swap to local changes` while you read a pull request, and
`⇄ Swap to the pull request` while you read your working tree.

You answer two different questions during one review — *what does this
branch do?* and *what have I changed in reply?* — and each one used to
cost a restart.

**Both sides keep their place.** Loupe holds the side you leave whole: the
file list, which file was open, the cursor row, the scroll position, the
folded directories, and the viewed or staged marks. Swap back and it is
the screen you left, not a fresh load of the same review.

**It is instant, and it is still current.** The side you swap to is drawn
from the stash straight away, and then re-checked in the background —
GitHub for the pull request, the working tree for your changes — so
there is no loading screen on the way back and no stale diff either. The
status line names what it is checking: `PR #123 — checking GitHub for
updates.`, or `⎇ Local changes — rescanning in the background.`

The very first swap has nothing stashed yet, so it loads that side the
long way once: it looks for a pull request for the current branch, or
scans the working tree.

Close the editor first. A swap with an editor open is refused, because the
buffer belongs to the side you are leaving.

## File panel

| Key / mouse | Action |
| --- | --- |
| Click a file | Open its diff |
| Click a folder | Collapse / expand it |
| Right-click a file or folder | Copy its path (see [Copying a path](#copying-a-path)) |
| `]` / `[` | Next / previous file |
| `x` or click the icon column | PR: toggle *viewed* (syncs to GitHub) · Local: stage / unstage the file |
| Click `[!]` on a conflicted row | Open its resolve menu (see [Merge conflicts](#merge-conflicts)) |
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
| `R` | The review box — summary, verdict, and everything held |
| `Enter` / `Space` | Expand or fold the run at the cursor |
| Click `··· N unchanged lines ···` | Expand a folded run |
| Click `⌃⌃⌃ … click to fold ⌃⌃⌃` | Re-fold that run |
| `z` or `☰ → Fold unchanged lines` | Fold / unfold every unchanged section |
| `v` or `☰ → Switch to inline` / `Switch to split` | Toggle side-by-side vs. inline layout |
| `e` / `i`, double-click a new-side line, or `✎ Edit` | Edit the file at the cursor line |
| `P` or `📖 Preview` | Render a markdown file as a document (see [Markdown preview](#markdown-preview)) |
| `x` | Toggle *viewed* (stages the file in local review) |
| Click `↺` in the change bar | Put that section of the diff back (asks first) |
| `u` / `U` | Revert the change at the cursor / every change in the file |
| `o`, or click `⚑` in the change bar | Resolve the merge conflict there (see [Merge conflicts](#merge-conflicts)) |
| `y`, `Ctrl+C`, or `⧉ Copy` | Copy the selected lines — or the cursor line — to the clipboard |
| `Y`, or `🤖 Copy the context for your agent` | Copy the block that says what you are looking at, for a coding agent (see [the context provider](agent-context.md)) |
| `B` | Show / hide the blame pane |

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

### Reviewing a pull request

A comment can go up on its own, or be held for one review:

| Key / mouse | Action |
| --- | --- |
| `Ctrl+S` or `✎ Add to review` | Hold the comment — nothing reaches GitHub yet |
| `Ctrl+Enter` or `Post now` | Post that one comment immediately |
| `Esc` or `Cancel` | Keep nothing |

The review box under the file panel is what actually sends them:

| Key / mouse | Action |
| --- | --- |
| `R`, or click the box | Give it the keyboard |
| `Tab` / `Shift+Tab`, or the `▾` | Comment ⇄ Approve ⇄ Request changes |
| `Ctrl+S`, or the button | Submit — asks first, listing what goes |
| `✕ Discard` (twice) | Throw the held comments away |
| `Esc` | Back to the diff |

Held comments show as `💬` in the change bar and `💬N` beside the file
name. They survive quitting loupe. Full detail is in
[Reviewing pull requests](reviewing-prs.md#the-review-box).

### Merge conflicts

A conflicted file opens as **our version against theirs**, with the
marker lines stripped out, so each conflict is an ordinary changed
section — `}` / `{` walk them, `z` folds the agreed lines, `/` searches.
A `⚑` marks the first row of each one.

| Key / mouse | Action |
| --- | --- |
| `o`, click `⚑`, or click `[!]` in the file panel | Open the resolve menu for that conflict |
| `u` on a conflicted file | The same menu — reverting is refused mid-merge |
| `☰ → Merge conflict → Resolve this whole file` | The menu with only the whole-file lines |

In the menu:

| Key | What it keeps |
| --- | --- |
| `o` | Ours — the version on the branch you are on |
| `t` | Theirs — the version being merged in |
| `b` | Both — our lines, then theirs |
| `a` | The common ancestor, where git wrote one |
| `O` / `T` | Ours / theirs for every conflict in the file |
| `e` | Edit the raw file, markers and all |
| `x` | Mark it resolved (`git add`) |
| `Esc` | Leave it alone |

Resolving the last conflict in a file stages it, because git treats a
path as conflicted until it is added. Full detail, including conflicts
git cannot write markers for, is in
[Reviewing local changes](local-changes.md#merge-conflicts).

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

## The blame pane

`B` (or `☰ → View → Blame column`) opens a third pane between the file
panel and the diff. It answers the question the diff cannot: is the code
around this change old and settled, or did it move recently — and was
that you?

```
┌ Files ────────┐┌ Blame ───────────────────────┐┌ registry.go ─────────
│▾ internal     ││░ tommaso-moro     4mo #13165 ││  32     DefaultAgentID
│ [ ] M reg.go  ││█ tommaso-moro     26m #14260 ││  34     claudeConfigDi
│ [ ] M reg_t.go││█                             ││
```

Each row carries four things about the line beside it:

| Column | What it says |
| --- | --- |
| The bar | How old the line is, on a heat ramp |
| The name | Who last touched it — **your own commits are drawn apart** |
| The age | `2h`, `3d`, `5mo`, `2y` |
| `#412` | The pull request the commit landed in |

A run of lines from one commit names it once, at the top of the run, so a
block of one change reads as one block. The bar keeps drawing on every
row, so the ramp stays continuous. A fold banner draws a rule instead:
it covers lines from many commits, and blaming one of them would be a
guess.

### The colors

The heat ramp is **one hue** — the same neutral grey the borders and line
numbers use, with lightness doing all the work. That is deliberate: it
leaves the two classes above it as the only *colored* things in the
column, so the question the pane exists to answer catches your eye before
anything else does.

It is also **absolute**, not relative to the file, so a shade means the
same age everywhere and you learn the scale once. The two classes above
the ramp mean something different from "recent":

| Color | Meaning |
| --- | --- |
| Uncommitted (green) | `git blame` claims nothing — your working tree, right now |
| In this change (blue) | The commit belongs to the change you are reviewing |
| The ramp (grey) | Committed history: under a day, a week, a month, three months, a year, older |

"In this change" is the commits a pull request adds (`base..head`). In
local review there is no pull request to bound it, so it is the commits
you have made and not pushed — the same question in the local shape.

### The commit behind a row

**Click a blame row** (either button) for the rest of the story: the
commit hash, the exact date, the author and their email, the subject, the
pull request title, and whether the commit is part of the change on
screen. Three keys act on it:

| Key | Action |
| --- | --- |
| `o` | Open the pull request in your browser |
| `y` | Copy the pull request link |
| `c` | Copy the commit hash |

The pull request comes from the commit subject where there is one — a
squash or merge commit names it — and from one batched GitHub lookup for
the rest, cached for the session. `blame_pr_lookup = false` in your
config skips the lookup and stays entirely offline.

### Where the pane works

- **Both review modes.** A pull request blames the head commit (or your
  working tree when the branch is checked out); local review blames the
  working tree, which is what marks your uncommitted lines.
- **Both diff layouts.** An inline row is blamed on the side it shows.
  A split row prefers the new side and falls back to the old one for a
  removed line — the only side that can say what you are deleting.
- **The editor.** The pane follows the buffer. Typing moves the text out
  from under the blame, so a dirty buffer draws the column dimmed with
  `stale` in its title; saving refreshes it.
- **Resizing.** Drag the second divider, or double-click it for 30
  columns. Below about 22 columns the pull request number drops out, then
  the age. A terminal too narrow for three panes shows two — the diff
  keeps its width.

Blame is read on its own background job, *after* the diff is on screen,
so opening a file is no slower than it was. The pane says `loading…`
while it waits.

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
| `Ctrl+C` | Copy the selection, or the cursor line, to the system clipboard |
| `Ctrl+X` / `Ctrl+Y` | Cut / paste, through the editor's own buffer |
| `Ctrl+Z` or `Ctrl+U` | Undo (`Ctrl+U` is tui-textarea's own binding; `Ctrl+Z` is an alias) |
| `Ctrl+R` | Redo |
| `Alt+P` | Preview the buffer as markdown, saved or not (`.md` files) |
| `PgUp` / `PgDn`, or `Ctrl+V` / `Alt+V` | Page through the file |
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

## Markdown preview

A `.md` file has a document view as well as a diff and a source view.
Press `P` on a markdown file and the pane renders it: headings, wrapped
paragraphs, bold and italic text, inline code, links, nested and task
lists, block quotes, tables, thematic rules, YAML front matter, and
fenced code blocks colored by your syntax theme.

It exists for the files agents write. A plan file, a build ladder, a
review write-up — all markdown, all previously readable only by leaving
Loupe for another app.

| Key / mouse | Action |
| --- | --- |
| `P` | Preview the markdown file, and from the preview open its source |
| `Alt+P` | The same, from inside the editor (plain `P` is text in there) |
| `j` / `k`, `↑` / `↓`, wheel | Scroll |
| `Ctrl+D` / `Ctrl+U`, `Ctrl+F` / `Ctrl+B`, `PgUp` / `PgDn` | Half page / full page |
| `gg` / `G`, `Home` / `End` | First / last row |
| `}` / `{`, `Tab` / `Shift+Tab` | Next / previous heading |
| `e` or `i` | Open the source in the editor |
| `r` | Re-read the file now |
| `]` / `[` | Next / previous file |
| `Esc` | Back to the diff |
| Click the file panel | Switch files, the way the diff does |

**The preview and the source are two views of one file.** `P` moves
between them and both keep their place: from the preview you land in the
editor on the line you were reading, and from the editor you land in the
preview at the line you just changed. Unsaved text renders as it stands,
so you can change a heading, look at it, and change it again without
saving in between. `Ctrl+S` in the source view writes the file.

**It follows the file.** Loupe checks the modification time while you sit
idle and re-renders when something else rewrites it, keeping your place
by source line rather than by row. That is a plan file updating on screen
as an agent writes it. Unsaved text in the source view is never
overwritten this way.

The blame pane stands down while the preview is open — one source line is
any number of rendered rows there, or none — and comes back with the
source view.

### Reading a file that is not in the change

Four ways in:

- **Drop it on the window.** Drag a `.md` file onto loupe from anywhere
  on the machine and it is pinned and rendered (see
  [Pinned files](#pinned-files)).
- `Ctrl+O` and type or paste a path — the same thing, for a terminal that
  cannot report a drop.
- `Ctrl+P` and pick any `.md` file in the repository. A markdown file
  opens as a document rather than in the editor.
- `loupe md <path>` from the shell, for a file anywhere on the machine —
  a review write-up in a central tree, a note in `/tmp`. There is no
  review behind it, so the document takes the whole window and `q` quits.

## Pinned files

A row of tabs under the top bar, holding the files you keep coming back
to. It takes no height at all until you pin something.

| Key / mouse | Action |
| --- | --- |
| **Drag a file onto the window** | Pin it and open it — from anywhere on the machine |
| `Ctrl+O` | Open a file by path: absolute, `~/…`, or relative to the repository root |
| `=` (or `+`) | Pin the file in front of you, or unpin it if it already has a tab |
| `-` | Unpin the file you are reading |
| `1` … `9` | Open that tab |
| `,` / `.` | Previous / next tab, wrapping at each end |
| Click a tab | Open it |
| Click its `✕`, or middle-click the tab | Unpin it |
| Wheel over the row | Step through the tabs |
| `Alt` + any of the keys above | The same, from inside the editor, where a bare key is a letter |
| `☰ → Pinned files` | All of it with the mouse, plus a line per tab |

**What a tab opens.** A markdown file renders as a document, wherever it
lives. A file the change touches opens as its diff — during a review that
is what coming back to it means. Anything else opens in the editor, and
`Ctrl+S` saves it, outside the repository or not.

**Files from outside the repository carry a `↗`.** "The plan file" means
a different document depending on the answer, and the row is the only
place that says so.

**Dropping.** Every terminal answers a drop by writing the file's path in
as if you had typed it, and they split into two camps about how.

- **Ghostty, iTerm2, Terminal.app** wrap the path in *bracketed paste*, so
  it arrives as a single event. Loupe turns bracketed paste on for this.
- **Warp** sends the path as plain keystrokes, one event per character.
  Loupe reads each batch of input for a path before dispatching any of it,
  so the leading `/` never reaches the search prompt.

Either way, the text is read as a drop only when *every* path in it is
absolute and names a file that exists. That is what lets an ordinary paste
stay an ordinary paste — a snippet of code, a URL, a sentence — and it
costs nothing, because a dropped path is always absolute. A path with a
space in it works in all three spellings terminals use: escaped, quoted,
and percent-encoded in a `file://` URL.

Drop several files at once and each gets a tab. Nothing is ever copied
into the repository.

If a drop does nothing at all in your terminal, `Ctrl+O` and paste the
path does the same job.

**They come back.** The tabs are written to `.git/loupe/pins.json` as they
change, so quitting loupe does not cost you the row. In a linked worktree
that is the worktree's own `.git` directory, so each one keeps its own row
— the branch you are on decides which tabs you see. Never committed. A pin whose file has since been deleted drops out when
loupe reads the file back.

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
| `ctl context [--json]` | Print what loupe has on screen, for an agent's `UserPromptSubmit` hook. `--json` wraps it for an agent that ignores plain stdout (Codex) |
| `ctl install` | Add that hook to every coding agent on this machine |
| `ctl uninstall` | Take the hook back out |
| `md <file.md>` | Read one markdown file in the preview, with no review beside it |

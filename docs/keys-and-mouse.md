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
| `?` or `F1` | Help overlay |
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
| Click a file | Open its diff, in the tab the last click used |
| Double-click a file | Open its diff and **keep** its tab |
| Click a folder | Collapse / expand it |
| Right-click a file or folder | Copy its path (see [Copying a path](#copying-a-path)) |
| `]` / `[` | Next / previous file |
| `x` or click the icon column | PR: toggle *viewed* (syncs to GitHub) · Local: stage / unstage the file |
| `X` | Local: stage the whole change, or unstage it when it is all staged |
| Click a `STAGED` / `UNSTAGED` heading | Fold that section away |
| Click `✚ all` / `↩ all` on a heading | Move that whole section across |
| `S` | Local: open the stash menu |
| Click `[!]` on a conflicted row | Open its resolve menu (see [Merge conflicts](#merge-conflicts)) |
| Click `↺` at the end of a row | Revert every change in that file (asks first) |
| `Tree` / `Flat` buttons | Switch between tree and flat list |
| `F` or the `Change` / `Files` / `Commits` buttons | Walk the three panels |
| Drag the divider | Resize the panel |
| Double-click the divider | Reset the panel width (34 columns) |
| `<` / `>` | Narrow / widen the panel |

### Commits not pushed yet

`F` again, or the `Commits` button, lists the commits this branch has
that the upstream does not — the work you have committed but not pushed.
The change pane shows only what is uncommitted, so without this the other
twenty-nine commits on a branch thirty ahead are invisible.

| Key / mouse | Action |
| --- | --- |
| Click a commit | Open it to its files, or close it again |
| Click a file under a commit | Read that commit's diff for that file |

Each row is the short id, the subject, and how long ago it was made. The
panel title names what the list is measured against, in the same shape
the top bar uses for the upstream drift: `Commits ↑4 origin/main`.

The upstream is the branch's own where it has one. A branch that tracks
nothing has never been pushed at all, so the question becomes "what is on
this branch that the default branch does not have", and `origin/HEAD`
answers it. With neither, the panel says so and offers `git branch -u`.

**A commit's diff is read against its own first parent**, both sides from
git. The copy on disk belongs to `HEAD`, which for every commit but the
newest is a later version of the same file. The diff title names the
commit it came from, because the same path can be in several of them:

```
 src/app.rs — +48 −6 · ◷ 4683983 Make the editor a real IDE surface
```

A commit already happened, so nothing in its diff can be staged,
reverted, or edited, and the idle rescan leaves it alone. Press `F` for
the change to do any of those.

The list is read again when you come back to the panel after thirty
seconds, and capped at 200 commits — a branch that forked long ago is a
panel nobody reads.

### Every file in the repository

`F` walks the panel through the change, every file in the repository, and
the unpushed commits. The change and the repository keep their own trees
and their own collapse state, so a folder you closed in one is a folder
you never touched in the other, and coming back lands where you left off.

The list comes from `git ls-files`, so it holds what git tracks plus what
it does not yet — and, deliberately, the files git is **ignoring**:

- An ignored **file** is listed and drawn dim. `.env` is worth opening,
  and worth knowing is not committed.
- An ignored **directory** costs one row. `node_modules` stays a single
  folder until you open it, and only then does loupe read what is inside —
  one level, never a walk.

**A row here is a file, and nothing else.** No stage box, no viewed box,
no `+`/`−` counts, no `↺` — every one of those belongs to the change, and
the `Change` panel one key away is where they live. A file the change
touches is drawn the same as one it does not, and clicking it opens the
file rather than its diff.

**One click peeks at a file.** Walking a tree means opening a great many
files to look at one of them, so a click puts the file in the **peek
tab** and the next click puts the next file in the same tab. The peek tab
is drawn in *italics* to say so, and there is only ever one of them.
**Double-click the file, or double-click the tab, to keep it** — it stops
being italic, and the next click peeks in a tab of its own beside it.

A peek is not a preview. In loupe a *preview* is the rendered markdown
document that `P` opens; a *peek* is a tab you have not committed to yet.

**A pinned file is never peeked.** It already has a tab — its pin — so
clicking it here opens it the way its own tab does, and the row gains
nothing. One file never sits in the row twice.

You never have to leave the editor, save, or close anything to do this.
Clicking another file parks the buffer you were in: it keeps its tab, its
cursor, its scroll and anything you had not saved, and a `●` on the tab
says the file on disk is not what you are looking at. A buffer you have
typed into is never the one a click replaces.

Markdown opens as source, like every other file, so it gets a tab. `P`
renders it and `P` puts the source back, and the tab stays put across
both.

Reading is refreshed when you come back to the panel after a minute, and
whenever you press `r`. Not on a timer: `git ls-files` costs about 90
milliseconds on a repository of seventeen thousand files.

**Right-click a row** for the file operations, in this panel only:

| Line | Action |
| --- | --- |
| `New file here…` | Create an empty file beside the row, or in the folder, and open it |
| `Rename…` | Give the file a new name |
| `Delete…` | Delete it — asks first, and this cannot be undone from loupe |

In the change under review the same row means "part of this diff", so
offering to delete it there would read as an offer to drop it from the
change. That is why these lines only appear in `Files`.

A very large repository — more than two hundred thousand files — is read
one directory at a time instead, so the panel is usable at once.

## Reading a diff

A removed line is painted red and an added line green, as everywhere
else. **The words that actually changed are painted a stronger shade of
the same colour.**

```
 12  −  let client = Client::builder().timeout(timeout).build()?;
 12  +  let client = Client::builder().timeout(deadline).build()?;
                                              ▔▔▔▔▔▔▔▔
```

A line painted whole says the line changed and nothing about where. On a
long line with one renamed variable in it, that is two lines you have to
read against each other character by character — which is how a changed
name, or an added space, gets missed.

The same hue rather than a new colour, because the line already says
added or removed and this only says *where*. It works in both views.

Two lines that share almost nothing are a rewrite, not an edit, so they
keep the plain colours: painting nine tenths of both of them darker says
less than painting neither. A whole added or removed line has no
counterpart to differ from, so every character of it is the change and it
keeps one shade too.

## Layers — what you already changed, and what is new

Press `s`.

A branch has two steps in it, not one: what the remote already has, and
what you have done since. Every diff tool draws the sum of the two and
calls it the change. That answers *"what does this branch do"* and cannot
answer *"am I rewriting what I already wrote"* — the question that
matters when an agent hands you a hundred lines and you have to tell new
work from a second attempt at old work.

`s` reads three versions of the file instead of two:

| Version | Where it comes from |
| --- | --- |
| **Base** | The merge base with the base branch (a PR) or with `origin/HEAD` (local review) |
| **Pushed** | The PR head on GitHub, or the branch's upstream. With nothing pushed yet, your last commit |
| **Working tree** | The file on disk |

The diff still reads base on the left and working tree on the right — it
is the same change a reviewer would see. What is new is the colour:

| Colour | Meaning |
| --- | --- |
| **Purple** | The pushed branch changed this line and you have not touched it since — already in the pull request |
| **Green / red** | Only the working tree changed it — new work, not pushed yet |
| **Amber** | Both changed it — this change has now written the line **twice** |

Amber is the one the setting exists for. A `‡` in the change bar marks
every section with amber in it, and the title counts them:

```
 src/app.rs — +48 −6 · the whole stack · ‡ 3 lines written twice
```

…or `nothing written twice`, which is the answer you want.

`s` again switches to **only what is new**: the pull request's own
changes drop off the screen and you are left with your newer edits alone,
with the amber still on the ones that land on lines the pull request had
already rewritten. `s` once more turns the layers off.

`▤  Layers` in the `☰` menu does the same, and is greyed out where there
is nothing to layer — a branch with no commits, or a remote with no
default branch.

Two notes:

- **A line you added since the push is green, not amber**, even inside a
  block the pull request rewrote: it has no counterpart in the pushed
  version for the pull request to have changed. The whole stack shows the
  purple around it.
- **Reverting is refused while the layers are on** (`u`, `U`, and the
  `↺` marks, which stand down). The left column is the base branch, not
  the file as it was, so putting a section back would write the pull
  request's own work out of the file along with the edit you meant. Press
  `s` to turn the layers off first.

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
| `s` or `☰ → Layers` | Walk the [layers](#layers--what-you-already-changed-and-what-is-new): off → the whole stack → only what is new |
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

### Find in this file

**`Ctrl+F`** searches whatever you are reading, wherever you are reading
it. It is the same search in all three panes, and `/` opens it too:

| Where | What it searches |
| --- | --- |
| The diff | Both sides of the open file, as you type |
| The markdown preview | The rendered document — what is on the page, not the `##` and `[]` behind it |
| The editor | The buffer, with a replacement field behind `Tab` |

| Key | Action |
| --- | --- |
| `Ctrl+F`, or `/` | Open the search |
| `n` / `N`, or `F3` / `Shift+F3` | Next / previous match (wraps, and says when it wrapped) |
| `Esc` (while typing) | Cancel, putting you back where you were reading |
| `Esc` (after) | Clear the highlight, leaving you where you are |

`Cmd+F` does the same where your terminal forwards it. Most do not:
Terminal.app and iTerm2 both keep `Cmd+F` for their own find bar and it
never reaches Loupe. `Ctrl+F` always arrives.

> `Ctrl+F` used to page forward, vim-style. `PageDown` and `Ctrl+D` still
> do, and in the preview so does `Space`.

### Find in every file

**`Ctrl+Shift+F`**, or `#`, runs one `git grep` across the repository with
definitions sorted first. Some terminals cannot tell `Ctrl+Shift+F` from
`Ctrl+F` and send the same thing for both; `#` always works.

### The finder

One overlay answers three questions; a prefix character in the input
picks which. Open it with `Ctrl+P` or the `🔍 Find` button.

| Key / mouse | Action |
| --- | --- |
| `Ctrl+P` or `🔍 Find` | Fuzzy-match a file by path |
| `#` or `Ctrl+Shift+F` | Find text in files — one `git grep`, definitions sorted first |
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
| `gd`, or `F12` | Go to the definition |
| `gr`, or `F10` (`Shift+F12`) | Find every reference, in the finder's result list |
| `K` | What is this? — the type and its documentation |

Which symbol on the line? Click one first and it is used; otherwise, if
the line holds several, loupe asks rather than guessing.

| Language | Server | Install |
| --- | --- | --- |
| TypeScript / JavaScript | `typescript-language-server` | `npm install -g typescript-language-server typescript` |
| Go | `gopls` | `go install golang.org/x/tools/gopls@latest` |
| Rust | `rust-analyzer` | `rustup component add rust-analyzer` |

Those three are built in. A `[[server]]` table in your config adds another
language, or replaces one of these with a server you prefer — see
[configuration](configuration.md#server--a-language-loupe-does-not-know).
`loupe --lsp` lists what it found, yours included.

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
| Double-click a word | Select the whole word — every other use of it lights up |
| Triple-click | Select the whole line |
| Right-click | The code menu for the word under the pointer |
| Drag | Select text |
| `Ctrl+S` | Save (the diff refreshes immediately) |
| `Ctrl+C` | Copy the selection, or the cursor line, to the system clipboard |
| `Ctrl+X` / `Ctrl+Y` | Cut / paste, through the editor's own buffer |
| `Ctrl+Z` or `Ctrl+U` | Undo (`Ctrl+U` is tui-textarea's own binding; `Ctrl+Z` is an alias) |
| `Ctrl+R` | Redo |
| `Ctrl+F`, or `Alt+F` | Find and replace in this file |
| `Alt+C` | Comment or uncomment the line, or the selection |
| `Alt+P` | Preview the buffer as markdown, saved or not (`.md` files) |
| `PgUp` / `PgDn`, or `Ctrl+V` / `Alt+V` | Page through the file |
| `Esc` | Close (press twice to discard unsaved changes) |

Every one of these is in the `☰` menu as well, which matters: some
terminals never send Alt, and the menu is the way in when yours does not.

**A double click selects the word, not the character you hit.** The
selection covers the whole identifier, and every other place that word
appears in the file is marked while it holds — the answer to "where else
is this?" before you ask the language server. `F12` and `F10` right after
a double click ask about the word that is lit up.

**Right-click the text and the menu is about that word.** The click takes
the word under the pointer first, so the title of the menu names what you
are about to ask about; a click inside a selection you made on purpose
keeps that selection instead. Every line of the menu is a key that works
without it, and a line that needs a symbol is drawn dim rather than
dropped when the pointer is on punctuation.

### More than one file open

Opening a second file parks the first rather than closing it. Every open
file shows in the tab row under the top bar, after any pinned files.

| Mark | What it says |
| --- | --- |
| `●` before the name | Unsaved work in it (a pinned file carries this on its pin tab) |
| *Italic* name | The peek tab — one click opened it, so the next click replaces it |
| **Bold** name | The file on screen |

| Key / mouse | Action |
| --- | --- |
| `Alt+]` / `Alt+[` | Next / previous open file |
| Click a tab | Go to that file |
| Double-click the peek tab | Keep it, so the next click peeks in its own tab |
| Drag a tab along the row | Put it where you want it |
| Click its `✕`, or middle-click the tab | Close that file |
| `Esc` | Close this one and go back to the last |
| `q` | Quit — asks once, and says how many files are unsaved |

A file you have pinned draws no tab here — its pin *is* its tab, and one
file in the row twice is one file too many. Unpin it and the tab comes
back.

**The row keeps the order you opened them in.** Clicking a tab opens that
file and moves nothing. A drag is the only thing that reorders the row,
and it changes the order without changing which file is on screen.

Closing a tab lands you on the tab beside it, not on the last file you
happened to open. A tab with unsaved work asks once before it goes, and
each tab asks for itself.

The file panel keeps working while the editor is up. Clicking another
file parks this one — you never have to save or close first, and nothing
you have not saved is thrown away. See
[Every file in the repository](#every-file-in-the-repository).

Coming back to a file keeps the buffer: the cursor, the scroll and any
unsaved edits are where you left them. Opening a file that is already open
switches to it rather than reading the file over the top of your edits.

### Find and replace

`Ctrl+F` — or `Alt+F`, which is the modifier the rest of the editor's
own commands use — puts the prompt in the editor's border, where the file
name goes, so nothing on screen moves while you search.

| Key | Action |
| --- | --- |
| `Tab` | Switch between what to find and what to put in its place |
| `Enter` | Next match |
| `Alt+N` / `Alt+B` | Next / previous match, without the prompt open |
| `Alt+R` | Replace this one |
| `Alt+A` | Replace all of them |
| `Esc` | Cancel — the cursor goes back where the search started |

`Alt+R` means two things, and which one depends on whether the prompt is
open: while you are typing in it, it replaces the match under the cursor;
the rest of the time it finds every use of the symbol you are on.

Matching follows the same rule as everywhere else in loupe: an
all-lowercase query ignores case, and a capital means that capital.

### In the editor, with a language server

The diff view's `gd` / `gr` / `K` can't work in here — plain letters are
text — so these are on function keys and on Ctrl keys that `tui-textarea`
leaves free.

| Key | Action |
| --- | --- |
| `F12` | Go to the definition |
| `F10`, or `Shift+F12` | Find every use — the same list `gr` gives in the diff |
| `F2` | Rename this symbol everywhere |
| `F8` / `Shift+F8` | Next / previous problem in this file |
| `F3` / `Shift+F3` | Next / previous match of the last find |
| `F1` | The help card |
| `Alt+E` | Every problem in this file, as a list |
| `Ctrl+Space` | Suggest completions now |
| `Tab` / `Enter` | Accept the highlighted suggestion · `↑` `↓` to move · `Esc` to dismiss |
| `Alt+X` | Explain the problem the cursor is on |
| `Ctrl+G` | What is this? — the type and docs for the symbol at the cursor |
| `Ctrl+]` | Go to the definition (vi's jump-to-tag key) |
| `Alt+R` | Find every use |
| `Alt+S` | The signature of the call the cursor is inside |
| `Alt+M` | Rename this symbol everywhere |
| `Alt+.` | Fixes and refactors on offer here |
| `Ctrl+T` | Format the file, or the `⇥ Format` button |

`F12` and `F10` work in the diff view too, where they mean the same two
things `gd` and `gr` mean. `F1` opens the help card from anywhere.

**The function keys do not always reach the terminal.**

- **On a Mac, `F10` and `F12` are the volume keys** unless you hold `fn`,
  or turn on *Use F1, F2, etc. keys as standard function keys* in System
  Settings → Keyboard. Until then the key never reaches loupe at all, and
  nothing happens — no message, because nothing arrived.
- **GNOME Terminal keeps `F10`** for its own menu bar.

Every one of these has another way in that no terminal intercepts:
`Ctrl+]` for the definition, `Alt+R` for every use, `Alt+M` to rename,
and the right-click menu or `☰` for all of them with the mouse.

**How to tell what happened.** Press the key and watch the status bar. A
request that arrived says what it is waiting for — `Finding the
definition of parse…` — and keeps saying it, with a spinner, until the
answer lands. If the bar stays silent, the key never got here: use
`Ctrl+]` or the right-click menu instead. If it names a problem
("no language server for CHANGELOG.md"), that is your answer.

**A rename does not save anything.** Every file it touches opens as an
unsaved buffer with a `●` in the tab row, so you read what changed before
it reaches disk, and `Ctrl+S` each one you accept. A file already open is
edited where it stands, so unsaved work in it is never read over, and you
land back in the buffer you started in. A rename reaching outside the
repository — into a dependency, or the standard library — is dropped.

**Fixes and refactors** open as a list, each line saying how many files it
touches: "extract function" over one file and over nine are different
decisions. An action a server wants to run itself rather than describe as
edits is not offered, because there would be nothing to show you first.

`format_on_save = true` in your config runs the formatter on every
`Ctrl+S`. It's off by default: reformatting a file mid-review would put
changes in the diff that nobody asked for. Either way `Ctrl+Z` undoes a
format in one keystroke.

### Suggestions as you type

The popup opens on its own. One character of a name is enough, and so is
a character the server asks to be told about — `.` in TypeScript, `.` and
`:` in Rust. That last one is what makes `object.` list the object's
fields rather than everything in scope: loupe tells the server *why* it
is asking, and a server answers a dot differently from a question asked
out of the blue.

| Key | Action |
| --- | --- |
| *(just type)* | The list appears after a moment |
| `Tab` or `Enter` | Take the highlighted one |
| `↑` / `↓` | Move through the list |
| `Ctrl+Space` | Ask now, without waiting |
| `Esc` | Put it away |

Typing narrows the list without asking the server again — it already
sent everything for this word. A name that starts with what you typed
sorts above one that merely contains those letters in order, so a typo
still finds `firstName` instead of closing the list.

Set `suggest_while_typing = false` in your config to go back to
`Ctrl+Space` only.

**TypeScript needs two packages, not one.** `typescript-language-server`
speaks LSP; the `typescript` package is what it drives. With only the
first one the server starts and then dies on every question. Loupe looks
in the project's `node_modules` first, then at a global install, and
`loupe --lsp` says which of the two is missing.

### Problems

**Diagnostics** appear as you type, without saving: a `●` (error) or `▲`
(warning) in the gutter, the offending span colored, and the message in
the status bar when the cursor is on that line. When it isn't, the status
bar shows the count for the file, so a problem off screen isn't invisible.

Four things say the same thing at once, so a problem is hard to miss and
easy to read:

| Where | What it shows |
| --- | --- |
| The gutter | `✗` error · `▲` warning · `ℹ` note, in that severity's color |
| Under the code | The offending span, colored **and** underlined |
| The margin | The message itself, past the end of the line |
| The status bar | The message again for the line the cursor is on, with the names picked out |

Errors are red and warnings are yellow, everywhere. The underline is
there as well as the color, because color alone is the one thing a
reader with a color-vision difference cannot use.

| Key | Action |
| --- | --- |
| `Alt+X` | Explain this one — the whole message, laid out |
| `F8` | The next problem — the cursor lands on it, and the status bar reads it out |
| `Shift+F8` | The previous one |
| `Alt+E` | All of them, as a list to pick from |

**`Alt+X` is for the messages that do not fit on one line.** TypeScript
folds its reasoning into a single sentence — *"Type 'A' is not assignable
to type 'B'. Types of property 'a' are incompatible. Type 'number' is not
assignable to type 'string'."* — and the answer is the last clause. The
panel puts each reason under the one it explains, picks the names and
types out of the prose, and breaks a wide object type over lines at its
semicolons. Any key closes it.

### Lint

A language server knows whether the code compiles. A linter knows whether
it is any good — an unused import, a `==` that should be `===`, a rule
the project agreed on. Loupe runs both and draws what they say together,
with the tool's name on every message: `eslint(no-undef)`,
`typescript(2552)`.

| Linter | Files | Loupe finds it |
| --- | --- | --- |
| `eslint` | `.js` `.jsx` `.mjs` `.cjs` `.ts` `.tsx` `.mts` `.cts` | The project's `node_modules/.bin` first, then your PATH |
| `ruff` | `.py` `.pyi` | Your PATH |

The project's own copy wins on purpose: a JavaScript project pins its
linter and its plugins, and that is the version whose rules the
repository agreed on.

The buffer goes to the linter on standard input, so what you see is the
lint for what is on screen — not for what is on disk. It runs when typing
pauses, and never blocks the editor: a linter that hangs costs a missing
underline, not a frozen window.

ESLint counts its severities the other way round from everything else
(its `2` is an error, its `1` a warning). Loupe turns them round, so an
ESLint error is red and an ESLint warning is yellow, the same as the
compiler's.

Add another with a `[[linter]]` table — see
[configuration](configuration.md) — or set `linters = false` to run none.
A linter you do not have installed costs nothing and is not an error;
`loupe --lsp` lists what it found.

`F8` walks the file in line order and wraps at the end, so holding it
takes you round every problem and back to the first. The list `Alt+E`
opens is the same overlay references use: type to filter it, `Enter` goes
to the line, `Esc` puts it away. Errors carry `✗` and warnings `▲`, and
the server's code (`E0425`, `ts(2552)`) comes with the message so you can
search for it.

Both are in the `☰` menu and in the right-click menu, under `PROBLEMS`,
and neither appears while the file is clean.

The buffer is pushed to the server a fifth of a second after you stop
typing, so what it's checking is what you're looking at — not the file on
disk.

### The first question after launch

Loupe starts a language server the first time you ask it something, and a
cold server has a project to read before it can answer. rust-analyzer runs
`cargo metadata` and builds proc macros first; on a real project that is
tens of seconds.

The wait is visible: the status bar names what it is waiting for and
spins until the answer arrives. You can keep typing through it — a
lookup never takes the keyboard.

Loupe keeps re-asking for as long as the server reports progress, up to
45 seconds. A server that answers "nothing" while it is still indexing is
not telling you there is no definition, so that answer is not passed on.
If the budget runs out you are told to ask again in a moment, rather than
told there are no references.

**The first question to a server takes about a second longer than the
rest.** A server that is still loading a project does not always answer
"nothing" — tsserver answers a `F12` out of a half-built program by
pointing at the `import` line in the file you are already in. That is not
an empty answer and not an error, so there is nothing in it to catch.
Loupe waits out a short grace on the first question of the session and
asks again, which is what makes go-to-definition reach the other file in
a TypeScript project. It is paid once per language, behind the spinner.

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
| `Ctrl+F`, or `/` | Find in the document |
| `n` / `N` | Next / previous match |
| `j` / `k`, `↑` / `↓`, wheel | Scroll |
| `Ctrl+D` / `Ctrl+U`, `Space` / `Ctrl+B`, `PgUp` / `PgDn` | Half page / full page |
| `gg` / `G`, `Home` / `End` | First / last row |
| `}` / `{`, `Tab` / `Shift+Tab` | Next / previous heading |
| `e` or `i` | Open the source in the editor |
| `r` | Re-read the file now |
| `]` / `[` | Next / previous file |
| `Esc` | Clear the search, then back to the diff |
| Click the file panel | Switch files, the way the diff does |

**Search reads the document, not the markdown.** A heading's `##` and a
link's brackets are not on the page, so they are not in what you search
either — you look for what you can see. Matches light up in place and
keep their own colors around them, so a hit inside a heading still reads
as a heading. `Esc` while typing puts you back where you were reading;
`Esc` after keeps your place and clears the highlight.

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

## Tabs

A row under the top bar, one tab per file you have open. It takes no
height at all while you are reading one file.

**A tab is a file, not a view of one.** The same tab holds the file's
diff, its buffer and its rendered document; `e`, `P` and `Esc` move
between them and the tab stays where it is. Come back to a tab and you
land where you left it — same scroll, same cursor, same folds, same
selection — because nothing was re-read to get there.

**One click walks, two keep.** A review means opening a great many files
to read one of them, so a click puts the file in the **peek tab** and the
next click puts the next file in the same tab. The peek tab is drawn in
*italics* to say so, and there is only ever one. **Double-click the file,
or double-click the tab, to keep it** — it stops being italic, and the
next click peeks in a tab of its own beside it.

A peek is not a preview. In Loupe a *preview* is the rendered markdown
document that `P` opens; a *peek* is a tab you have not committed to yet.

| Key / mouse | Action |
| --- | --- |
| Click a file in the panel | Open it in the peek tab |
| Double-click it | Keep its tab |
| Click a tab | Go to it |
| **Right-click a tab** | Copy its path — relative, or full |
| Click its `✕`, or middle-click the tab | Close it |
| Drag a tab | Put it where you want it in the row |
| `,` / `.` | Previous / next tab, wrapping at each end |
| `Alt+]` / `Alt+[` | The same, from inside the editor |

A tab with unsaved work carries a `●` and asks before it closes. A tab
holding only a diff has nothing to lose, so it closes without asking and
leaves you on the tab beside it.

### Getting a path out of a tab

**Right-click any tab** for the two ways to name the file it holds —
`Copy relative path` (`r`) and `Copy full path` (`f`) — the same question
the file panel answers about a row.

This is how you hand a file to a coding agent. It matters most for a file
pinned from outside the repository: the tab shows it, but nothing else in
the window does, and the alternative was going and finding it in the
filesystem again. Such a file gets one line rather than two — it is
already named by its absolute path, and there is nothing for a relative
one to be relative to.

The `✕` is part of the tab, so the right button asks there too instead of
closing the tab by surprise.

### Pinned files

A pin is a tab you have promised to keep: it survives quitting, it takes
a number, and it can hold a file from anywhere on the machine rather than
only from the change.

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

# Reviewing pull requests

## Opening a PR

Run `loupe` (or `loupe --pr` to skip the local-changes scan) inside a
clone of the repository.

- **The PR picker** lists the repository's open pull requests — number,
  title, author, and branch. Click one to open it, or press `r` to
  refresh the list.
- **Branch auto-open**: if the branch you have checked out already has an
  open PR, Loupe skips the picker and opens that PR directly in editable
  mode. This makes `loupe` a one-keystroke review tool in per-branch
  worktree workflows. Press `b` to get back to the full PR list at any
  time.
- **Fork / multi-org setups**: set the `org` config key to list and open
  PRs against an upstream organization instead of your clone's own owner
  — see [configuration](configuration.md).

Everything loads in the background: a spinner in the status bar shows
what's loading and for which PR, the UI stays responsive, and `c` or
`Esc` cancels a load you didn't want.

## Checkout & review vs. review only

When you open a PR from the picker, Loupe asks how you want to review it:

- **Checkout & review** — checks out the PR branch (via `gh pr checkout`)
  and enables editing. Your working tree switches to the PR's branch.
- **Review only** — no checkout: Loupe fetches the PR's head ref into
  `refs/prtui/*` without touching your working tree. Reading diffs and
  commenting work; editing is off.

If the PR's branch is already checked out (the auto-open case), the
redundant checkout is skipped and editing is on.

## The file panel

Changed files appear as a collapsible folder tree — single-child
directory chains are compressed (`src/app/jobs`) to keep the tree
shallow. Toggle to a flat list with the `Tree`/`Flat` buttons on the
panel border. Each file shows its status (`A`/`M`/`D`/`R`) and +/−
counts.

**Viewed checkboxes.** Every file has a checkbox (`[ ]` / `[✓]`); click
it or press `x` to mark the file viewed. Viewed state syncs to GitHub in
the background — the PR page shows the same files checked, and files you
already marked on the website arrive checked here. Syncs are optimistic:
if one fails, the checkbox reverts and the error shows in the status bar.

The panel is resizable: drag the divider, press `<` / `>`, or
double-click the divider to reset. A starting width can be set with the
`file_panel_width` config key.

## Stacked pull requests

A pull request in a [stack](https://docs.github.com/en/pull-requests/get-started/about-stacked-prs)
shows its rung in the badge — `PR #43 · 2/3` — and the file panel gains a
`Stack` button that draws the whole chain. Click a rung to review that
one. The chain is read from the GitHub API, so it works without the
`gh stack` extension.

A stacked pull request targets the branch below it rather than the
trunk, so its diff is its own work with everything under it already
applied. Loupe needed nothing new for that: it reads every diff between
the two refs the pull request names.

See [the stack panel](keys-and-mouse.md#the-stack) for the ladder, the
marks, and what Loupe does and does not do to a stack.

## The diff view

- **Layouts**: side-by-side or inline stacked. Press `v`, or pick
  **Switch to inline** / **Switch to split** from the `☰` menu.

  ![The inline layout (shown here reviewing local changes)](assets/inline-diff.png)
- **Syntax highlighting** is computed per file in the background using
  the same extended syntax set as [bat](https://github.com/sharkdp/bat),
  with green/red backgrounds layered underneath added/removed lines. Pick
  a theme with the `theme` config key (`loupe --themes` lists all 32).
- **Layers**: `s` reads three versions of the file instead of two — the
  base branch, the PR head on GitHub, and your working tree — and paints
  every row by the step that wrote it. Purple is already pushed and
  reviewed, green and red are your edits since, amber is a line this
  change has now written **twice**. The title counts the amber. See
  [Layers](keys-and-mouse.md#layers--what-you-already-changed-and-what-is-new).
- **Folding**: unchanged stretches collapse to a
  `··· N unchanged lines ···` row; click to expand. Expanded runs keep a
  `⌃⌃⌃ … click to fold ⌃⌃⌃` header to put them back. `☰ → Fold unchanged lines`
  (or `z`) re-folds all expanded runs first, then toggles the whole file.
- **Horizontal scrolling**: long lines scroll sideways with `h`/`l` or
  `←`/`→`, a horizontal trackpad swipe, or the wheel with a modifier
  held (Shift by convention; Alt and Ctrl also work since terminals
  differ in what they pass through). Line-number gutters stay pinned,
  the title shows the current column, and `0` snaps back to the first
  column.
- **Vim motions**: a cursor row (underlined) moves with `j`/`k`,
  `Ctrl+D`/`Ctrl+U`, `Ctrl+F`/`Ctrl+B`, `gg`/`G`, `{`/`}` (previous/next
  run of changes), and `H`/`M`/`L`; `Ctrl+E`/`Ctrl+Y` scroll without
  moving the cursor, and `Enter`/`Space` fold or expand the run at the
  cursor. Clicking a line places the same cursor, so mouse and keyboard
  never disagree.

## Review comments

1. Select lines on either the old or new side: click a diff line, drag
   across several — or press `V` to start a line selection and extend it
   with the motions (`Esc` cancels).
2. Click `💬 Comment` or press `c` (with no selection, `c` comments on
   the cursor line).
3. Write the comment, then choose how it leaves the box:

   | Key | Button | What happens |
   | --- | --- | --- |
   | `Ctrl+S` | `✎ Add to review` | It is **held** — nothing reaches GitHub yet |
   | `Ctrl+Enter` | `Post now` | It goes up on its own, immediately |
   | `Esc` | `Cancel` | Nothing is kept |

**Holding is the default, and it is what you want most of the time.**
Ten comments posted one at a time are ten notifications to everyone
watching the pull request, with nothing tying them together. Held
comments go up as **one review**, with a summary and a verdict — which is
what the review box is for.

Two GitHub-imposed rules to know:

- Comments anchor to the PR's **head commit**. After you edit and save a
  file locally, commenting on that file is blocked until you commit &
  push (or reload) — otherwise GitHub would pin the comment to the wrong
  lines. Loupe tracks this per file and tells you in the status bar.
- GitHub only accepts comments on lines inside the PR's diff hunks. A
  comment far outside a hunk is rejected by the API; the error is shown
  in the status bar.

## The review box

Under the file panel is the composer for the pull request as a whole: a
summary, and the verdict to send with it. Press `R` (or click in it) to
give it the keyboard.

```
┌ Review · 3 held ───────────────┐
│ 💬 3 comments held   ✕ Discard │
│ The retry loop needs a cap,    │
│ but the rest reads well.       │
│                                │
│ ✓ Approve  ▾  Ctrl+S           │
└────────────────────────────────┘
```

- **The button sends the review.** The `▾` beside it (or `Tab` while the
  box has the keyboard) chooses between **Comment**, **Approve**, and
  **Request changes**; the button's label and colour follow the choice.
- **`Ctrl+S` submits.** It asks first, listing the verdict, the summary,
  and where every held comment will land — a review notifies every
  watcher of the pull request and cannot be taken back.
- **Everything goes up in one request.** The summary, the verdict, and
  all the held comments become a single review, exactly as if you had
  clicked *Start a review* on github.com and then *Submit review*.
- `Esc` gives the keyboard back to the diff. `R` returns to the box.

### Held comments

While comments are held, they are visible without opening the box:

- A `💬` in the change bar, on the lines each one covers.
- A `💬N` beside the file name in the panel.
- The count in the box's title, and in the status bar.

They are written to `.git/loupe/pending-review-<number>.json` as you make
them, so quitting loupe does not lose a review in progress — reopening
the same pull request picks them back up and says so. Nothing in that
file has ever been sent to GitHub.

`✕ Discard` throws them away; it asks once first, and only a second press
on the same button confirms.

### What GitHub refuses

Loupe catches the two obvious cases before sending — a review with
nothing in it at all, and **Request changes** with no summary — and
reports the rest in the API's own words:

- You cannot approve your own pull request.
- Every held comment must fall on a line inside the diff of the commit it
  was written against. If the PR head moves while you have comments held,
  the confirm prompt warns you before you send.

The whole review is accepted or refused together, so a single bad anchor
takes the rest with it. Nothing is lost when that happens — the comments
stay held.

## Editing the new side

Double-click a line on the new side (or press `e` / `i` at the cursor
line, or click `✎ Edit`) to open a real editor over the working-tree
file — available when the PR branch is checked out:

- Click to place the cursor, drag to select, type, undo (`Ctrl+Z`, or
  `Ctrl+U`), redo (`Ctrl+R`), `Ctrl+S` to save, `Esc` to close (twice to
  discard unsaved changes).
- The editor uses the same syntax colors as the diff, re-highlighted
  incrementally as you type, so even large files stay responsive. (Files
  over 8,000 lines skip editor highlighting to keep opening instant.)
- Saving refreshes the diff immediately. Edits only touch your local
  working tree — **you** commit and push when ready; Loupe never commits
  for you.

## The rest of the review

These work the same way here as they do on your own changes, so they are
written up once in the reference:

| What | Where |
| --- | --- |
| Pin the files you keep returning to, and read files from outside the repository | [Pinned files](keys-and-mouse.md#pinned-files) |
| Read a plan or a write-up as a document instead of as markup | [Markdown preview](keys-and-mouse.md#markdown-preview) |
| See who last touched each line, and which PR it landed in | [The blame pane](keys-and-mouse.md#the-blame-pane) |
| Search this diff, a file by name, text in the repository, or definitions | [Find](keys-and-mouse.md#find) |
| Go to a definition, list references, ask what a symbol is | [Language servers](keys-and-mouse.md#language-servers) |
| Copy exact characters out of either side of the diff | [Copying](keys-and-mouse.md#copying) |
| Reload the PR after its head moves | [Refreshing](keys-and-mouse.md#refreshing) |

**Right-click the `PR #123` badge** to copy the link to the pull request.
That is the URL a coding agent needs, and the badge is the one place on
screen that always carries it.

**`` ` `` swaps to your own uncommitted changes** and back, keeping your
place in both — the fast way to answer "what did the review make me
change?" without losing the review. See
[swapping between the two reviews](keys-and-mouse.md#swapping-between-the-two-reviews).

## Notes & limits

- Old-side content is resolved from the merge base of the PR's base and
  head. In shallow clones some base content may be unavailable, in which
  case those files render as fully added.
- Loupe requires PRs to be **open**; the auto-open path ignores closed
  and merged PRs.
- With an `org` configured, branch auto-open matches upstream PRs by head
  branch name — exact for same-repo branches, best-effort for cross-fork
  heads.

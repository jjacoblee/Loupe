# Reviewing local changes

Loupe reviews your own uncommitted work with the same UI it uses for pull
requests — the same tree, diffs, folding, and editor — plus a staging
column, so you can review and stage a commit in one pass.

![Local review — staging icons in the file panel, side-by-side diff on the right](assets/local-review.png)

## When local mode opens

With no flags, `loupe` scans the working tree first and opens local
review whenever there is uncommitted work; only a clean tree falls
through to the pull-request flow. You can steer this:

- `loupe --local` (or `-l`) — review local changes even when clean.
- `loupe --pr` (or `-p`) — skip the scan, go straight to PRs.
- `mode = "local"` / `"pr"` / `"auto"` in the
  [config file](configuration.md) sets the default; command-line flags
  always win.
- From the PR list, press `l` or click `⎇ Local changes` to switch into
  local mode.

"Uncommitted" means everything different from `HEAD`: staged changes,
unstaged changes, and untracked files (which show as added). It is a
working-tree review, not a branch-vs-main review.

Local mode announces itself with a green `⎇ LOCAL` badge and the branch
name in the top bar.

## The staging column

In local mode, the file panel's icon column stages instead of marking
viewed. Click the icon (or press `x`) to toggle:

| Icon | Meaning |
| --- | --- |
| `[+]` | Not staged — click to `git add` the file |
| `[✓]` | Fully staged — click to take it back out of the index |
| `[±]` | Partially staged — some hunks in the index, some only in the working tree |

Details worth knowing:

- **Unstaging never touches your files.** It is `git reset -- <path>`:
  the index moves, the working tree doesn't.
- Staging state comes from `git status` itself, so partial staging
  (e.g. from `git add -p` elsewhere) is always shown faithfully.
- Renames stage and unstage as a unit — both the old and new path move
  together.
- The panel title shows progress: `Files 3/7 staged`.
- Works from any subdirectory of the repo, and in a brand-new repository
  with no commits yet.

Staging toggles are optimistic and re-read the real index in the
background; on failure the icon reverts and the error is shown.

## Putting changes back

Each changed section of the diff carries a `↺` in the change bar at the
left of the pane; each file row carries one at its right-hand end. The
section marker (or `u`) rewrites just those lines in the working tree and
leaves the index alone; the file marker (or `U`) runs
`git checkout HEAD -- <path>`, which puts the index back too and drops
the file out of the list. An untracked file has no `HEAD` version to
restore, so reverting it deletes it — the prompt says which of the two is
about to happen, and nothing moves until you confirm.

## What's different from PR review

- The diff compares your working tree against `HEAD`; the index moving
  doesn't change what's on screen.
- Editing is always on — these are your files. Double-click a line or
  press `e`, edit, `Ctrl+S`, and the diff refreshes.
- Commenting is disabled (there is no PR to comment on); the `💬 Comment`
  button is hidden and `c` explains why.
- Nothing syncs to GitHub — no viewed-state sync, no network calls beyond
  what git itself does.

## Keeping up with an agent

The working tree is not only yours. A coding agent in another pane, a
`cargo fix`, a rebase in a second terminal — any of them can rewrite the
files under an open review, and nothing in a terminal tells Loupe that
it happened.

So local review re-scans by itself. About 2 seconds after your last key
press or mouse move, and at most once every 5 seconds, Loupe re-reads
the changed-file list and reloads the open file. Your scroll position,
your cursor row and your folds all survive; the status line says
`⟳ <path> updated with the latest changes.` when something moved, and
says nothing at all when nothing did.

The re-scan waits for you. It stands down while the editor is open,
while any overlay or menu is open, while lines are selected, and during
a drag or a panel resize — a refresh that pulled a selection out from
under you would be worse than a stale diff.

Two ways to take over:

- **`r`, or the `⟳` button in the top bar** — re-scan now. Use it when
  the idle timer is too slow for you, or when you have been clicking
  around for a while and suspect the diff is behind.
- **`☰ → Refresh while idle`** — turn the polling off for this session.
  `auto_refresh = false` in your config turns it off for good.

## A typical flow

1. Hack until the feature works; run `loupe`.
2. Walk the file list top to bottom, reading each diff. Fix small things
   in place with the editor.
3. Stage each file as you finish reviewing it — `x`, or click the icon.
   Something you didn't mean to keep? `↺` beside it puts it back.
   Hand a fix to an agent and keep reading: its edits appear on their
   own, or press `r` to pull them in now.
4. Quit (`q`) and `git commit`: the index already contains exactly what
   you reviewed.

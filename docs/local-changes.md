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

## What's different from PR review

- The diff compares your working tree against `HEAD`; the index moving
  doesn't change what's on screen.
- Editing is always on — these are your files. Double-click a line or
  press `e`, edit, `Ctrl+S`, and the diff refreshes.
- Commenting is disabled (there is no PR to comment on); the `💬 Comment`
  button is hidden and `c` explains why.
- Nothing syncs to GitHub — no viewed-state sync, no network calls beyond
  what git itself does.

## A typical flow

1. Hack until the feature works; run `loupe`.
2. Walk the file list top to bottom, reading each diff. Fix small things
   in place with the editor.
3. Stage each file as you finish reviewing it — `x`, or click the icon.
4. Quit (`q`) and `git commit`: the index already contains exactly what
   you reviewed.

# Loupe documentation

Loupe is a mouse-first terminal UI for reviewing GitHub pull requests and
local uncommitted changes, built in Rust on [ratatui](https://ratatui.rs).
It brings the VS Code GitHub Pull Requests experience — clickable file
trees, syntax-highlighted diffs, inline comments, viewed checkboxes that
sync to GitHub, and in-place editing — to any modern terminal.

It also does the things a review needs but a diff cannot: it submits a
whole review with a verdict, resolves merge conflicts in the diff itself,
blames the code beside the change, renders the markdown an agent wrote,
and finds any file in the repository without leaving the review.

## Contents

| Page | What it covers |
| --- | --- |
| [Getting started](getting-started.md) | Requirements, installation, first run, a tour |
| [Reviewing pull requests](reviewing-prs.md) | The PR picker, checkout vs. review-only, diffs, comments, the review box, viewed sync, editing |
| [Reviewing local changes](local-changes.md) | Uncommitted changes, the staging column, merge conflicts, keeping up with an agent |
| [Configuration](configuration.md) | Config files, every key, CLI flags, syntax themes |
| [Keyboard & mouse reference](keys-and-mouse.md) | Every binding and clickable control, and how each feature works |
| [Architecture](architecture.md) | How Loupe is built — for contributors |

## By feature

The features below work in both review modes, so they live in the
reference rather than in one mode's page:

| Feature | What it is for |
| --- | --- |
| [Pinned files](keys-and-mouse.md#pinned-files) | A tab row for the files you keep coming back to. Drag one onto the window from anywhere on the machine, or `Ctrl+O` a path |
| [Markdown preview](keys-and-mouse.md#markdown-preview) | Read a `.md` file as a document, not as markup. It re-renders when an agent rewrites it |
| [The blame pane](keys-and-mouse.md#the-blame-pane) | Who last touched each line, how long ago, and the pull request it landed in |
| [Find](keys-and-mouse.md#find) | One overlay for four questions: this file, a file by name, text in files, definitions |
| [Language servers](keys-and-mouse.md#language-servers) | `gd`, `gr` and `K`, driven by whichever server is already on your PATH |
| [The editor](keys-and-mouse.md#editor) | Edit the new side in place, with completion, formatting and diagnostics |
| [Copying](keys-and-mouse.md#copying) | Character-level selection out of either side of the diff, over SSH too |
| [Swapping the two reviews](keys-and-mouse.md#swapping-between-the-two-reviews) | `` ` `` moves between the pull request and your working tree, keeping your place in both |
| [Refreshing](keys-and-mouse.md#refreshing) | `r` on demand, and an idle re-scan that keeps local review current under an agent |
| [The theme picker](keys-and-mouse.md#theme-picker) | 32 themes, previewed live, in a light and a dark slot |

## Quick orientation

Run `loupe` inside any clone of a GitHub repository:

- **Dirty working tree?** Loupe opens your uncommitted changes for review
  first — stage files from the file panel as you go.
- **Clean tree, and the current branch has an open PR?** That PR opens
  directly, ready for review.
- **Otherwise** you get a clickable list of the repository's open pull
  requests.
- **Mid-merge?** A merge, rebase, cherry-pick or revert that stopped on a
  conflict takes over the review: conflicted files sort to the top, and
  the diff shows the two sides rather than the marker lines. See
  [merge conflicts](local-changes.md#merge-conflicts).

You are never stuck on the side you landed on — `` ` `` swaps between the
pull request and your own changes and keeps your place in both.

Everything is driven by the mouse (click, drag, scroll, double-click) with
keyboard equivalents throughout — see the
[keyboard & mouse reference](keys-and-mouse.md). Press `?` inside Loupe
for the same reference on one screen.

## Getting help

- Found a bug or want a feature? [Open an issue](https://github.com/jjacoblee/Loupe/issues).
- Want to contribute? Start with the
  [contribution guide](../CONTRIBUTING.md) and the
  [architecture overview](architecture.md).
- Security concern? See the [security policy](../SECURITY.md).

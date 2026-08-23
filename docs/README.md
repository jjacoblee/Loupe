# Loupe documentation

Loupe is a mouse-first terminal UI for reviewing GitHub pull requests and
local uncommitted changes, built in Rust on [ratatui](https://ratatui.rs).
It brings the VS Code GitHub Pull Requests experience — clickable file
trees, syntax-highlighted diffs, inline comments, viewed checkboxes that
sync to GitHub, and in-place editing — to any modern terminal.

## Contents

| Page | What it covers |
| --- | --- |
| [Getting started](getting-started.md) | Requirements, installation, first run |
| [Reviewing pull requests](reviewing-prs.md) | The PR picker, checkout vs. review-only, diffs, comments, viewed sync, editing |
| [Reviewing local changes](local-changes.md) | The default uncommitted-changes mode and the staging column |
| [Configuration](configuration.md) | Config files, every key, CLI flags, syntax themes |
| [Keyboard & mouse reference](keys-and-mouse.md) | Every binding and clickable control in one place |
| [Architecture](architecture.md) | How Loupe is built — for contributors |

## Quick orientation

Run `loupe` inside any clone of a GitHub repository:

- **Dirty working tree?** Loupe opens your uncommitted changes for review
  first — stage files from the file panel as you go.
- **Clean tree, and the current branch has an open PR?** That PR opens
  directly, ready for review.
- **Otherwise** you get a clickable list of the repository's open pull
  requests.

Everything is driven by the mouse (click, drag, scroll, double-click) with
keyboard equivalents throughout — see the
[keyboard & mouse reference](keys-and-mouse.md).

## Getting help

- Found a bug or want a feature? [Open an issue](https://github.com/jjacoblee/Loupe/issues).
- Want to contribute? Start with the
  [contribution guide](../CONTRIBUTING.md) and the
  [architecture overview](architecture.md).
- Security concern? See the [security policy](../SECURITY.md).

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
| `t` or `🎨 Theme` | Open the theme picker — live preview; Enter keeps & saves, Esc reverts |
| `c` | Cancel a cancellable background load |

## File panel

| Key / mouse | Action |
| --- | --- |
| Click a file | Open its diff |
| Click a folder | Collapse / expand it |
| `n` / `p` | Next / previous file |
| `x` or click the icon column | PR: toggle *viewed* (syncs to GitHub) · Local: stage / unstage the file |
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
| `n` / `p` | Next / previous file |

Mouse: the wheel scrolls vertically; a horizontal trackpad swipe (or
tilt wheel) scrolls sideways, as does the wheel with a modifier held —
Shift is the convention, but terminals differ on which modifier they
pass through, so Alt and Ctrl work too. Clicking a line places the
cursor.

## Diff view — act

| Key / mouse | Action |
| --- | --- |
| `V` | Start a line selection — motions extend it, `Esc` cancels |
| Click a line / drag over lines | Select it / select a range |
| `c` or `💬 Comment` | Comment on the selection, or on the cursor line (PR review) |
| `Enter` / `Space` | Expand or fold the run at the cursor |
| Click `··· N unchanged lines ···` | Expand a folded run |
| Click `⌃⌃⌃ … click to fold ⌃⌃⌃` | Re-fold that run |
| `z` or `⇕ Fold` | Fold / unfold every unchanged section |
| `v` or `◫ Split` / `≡ Inline` | Toggle side-by-side vs. inline layout |
| `e` / `i`, double-click a new-side line, or `✎ Edit` | Edit the file at the cursor line |
| `x` | Toggle *viewed* (stages the file in local review) |

## Editor

| Key / mouse | Action |
| --- | --- |
| Click | Place the cursor |
| Drag | Select text |
| `Ctrl+S` | Save (the diff refreshes immediately) |
| `Ctrl+Z` / `Ctrl+Y` | Undo / redo |
| `PgUp` / `PgDn` | Page through the file |
| `Esc` | Close (press twice to discard unsaved changes) |

## Theme picker

| Key / mouse | Action |
| --- | --- |
| `j` / `k`, wheel, or click a name | Preview that theme live |
| `PgUp` / `PgDn`, `Home` / `End` | Move through the list faster |
| `Enter` or `Use … ` button | Keep the theme and save it to your config |
| `Esc` or `Cancel` | Put the previous theme back |

## Command line

| Flag | Effect |
| --- | --- |
| `--pr`, `-p` | Straight to pull requests |
| `--local`, `-l` | Review local changes, even when clean |
| `--auto` | Local changes if any, else PRs (overrides config) |
| `--theme <name>` | Use a theme for this session, unsaved |
| `--themes` | List syntax themes and exit |
| `--help`, `-h` | Usage and config reference |
| `set-theme <name>` | Save a theme to the global config |
| `setup` | Re-run the first-launch setup wizard |

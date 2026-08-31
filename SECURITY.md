# Security Policy

## Reporting a vulnerability

Please **do not report security vulnerabilities through public GitHub
issues.**

Instead, use GitHub's private vulnerability reporting: go to the
repository's [Security tab → Report a vulnerability](https://github.com/jjacoblee/Loupe/security/advisories/new)
and open a private advisory. You'll get an acknowledgement within a few
days, and a fix or a coordinated disclosure plan as quickly as the issue
warrants. Please include reproduction steps and the version (or commit)
of Loupe you tested.

## Supported versions

Loupe is pre-1.0: security fixes land on `main` and ship in the next
release. Only the [latest release](https://github.com/jjacoblee/Loupe/releases/latest)
is supported — please reproduce against it (or `main`) before reporting.

## Security model

Knowing what Loupe considers an attack helps aim reports:

- **PR content is untrusted.** File paths, branch names, PR titles, and
  file contents come from arbitrary pull requests. Loupe validates them
  before use: API-provided paths are rejected unless they resolve to
  normal components inside the repository root; commit oids must be
  40/64-character hex before they reach `git show`/`git merge-base`;
  fetched base refs are fully qualified (`refs/heads/…`) so a branch
  name cannot be parsed as a command-line option; and the editor refuses
  to open or save through symlinks, so a PR that adds a symlink cannot
  redirect a save over an arbitrary local file. Anything that lets a
  hostile PR escape those guards — write outside the repo, execute code,
  inject `git`/`gh` options, or inject terminal escape sequences — is a
  vulnerability we want to hear about.
- **No shell, no tokens.** `git` and `gh` are always invoked with
  argv-style arguments (never through a shell), and Loupe holds no
  GitHub credentials of its own — all API access goes through the `gh`
  CLI's existing authentication. Loupe never sends your code anywhere
  except to GitHub via `gh`/`git`.
- **Local mode stays local.** Reviewing uncommitted changes performs no
  GitHub calls; staging/unstaging only moves the git index.

Out of scope: vulnerabilities in `git`, `gh`, or your terminal emulator
themselves (report those upstream), and issues requiring a malicious
locally-installed config file (config is trusted local input, though
parse failures are treated as hard errors).

## Dependencies

The dependency tree is deliberately small and pure Rust (no C build
dependencies). `cargo audit` is run as part of release preparation;
known-unfixable advisories inherited via major upstream crates are
documented in the release notes when accepted.

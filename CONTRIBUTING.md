# Contributing to Loupe

Thanks for your interest in improving Loupe! Bug reports, documentation
fixes, and code contributions are all welcome.

## Reporting bugs & requesting features

- Search the [existing issues](https://github.com/jjacoblee/Loupe/issues)
  first.
- For bugs, use the bug-report template and include your OS, terminal
  emulator, and `git`/`gh` versions — terminal and mouse behavior varies
  a lot between emulators, and that context is usually the whole
  diagnosis.
- For security vulnerabilities, **do not open a public issue** — follow
  the [security policy](SECURITY.md) instead.

## Development setup

You need a recent **stable Rust** toolchain ([rustup](https://rustup.rs)),
`git`, and — to actually exercise the PR flows — an authenticated
[GitHub CLI](https://cli.github.com/) (`gh auth login`).

```sh
git clone https://github.com/jjacoblee/Loupe
cd Loupe
cargo build            # debug build
cargo run -- --help
```

The [architecture overview](docs/architecture.md) is the fastest way to
find your bearings in the code — it maps the modules, the background-job
model, and the invariants that are easy to break by accident (the dirty
flag, the editor's shadow viewport, tab-width agreement, the security
guards on git/gh arguments).

## Before you open a PR

All three of these must pass — CI enforces them:

```sh
cargo fmt --all              # rustfmt, default settings
cargo clippy --all-targets -- -D warnings   # zero-warning policy
cargo test                   # full suite; no network or GitHub needed
```

Notes on the test suite:

- Tests run offline. Nothing may call `gh` or the network in tests.
- Two staging tests create a **real temporary git repository** (under
  your temp dir) and shell out to `git` — git is a hard dependency of
  Loupe, and those tests are what catch a git invocation changing
  meaning. Keep them passing; extend them when you touch `gitops.rs`.
- UI changes should come with a `TestBackend` render test when they
  change what ends up in the cells (there are existing examples in
  `ui.rs` asserting syntax colors, badges, and layout survive to the
  rendered buffer).

## Pull request guidelines

- **Keep PRs focused** — one change per PR reviews quickly; a grab-bag
  doesn't.
- **Add tests** for behavior changes, and a regression test with any bug
  fix (the suite is full of them; follow the pattern).
- **Preserve the invariants** called out in
  [docs/architecture.md](docs/architecture.md) — in particular: no
  blocking work on the UI thread, no full-file re-highlight per
  keystroke, no periodic redraws, and never build `git`/`gh` invocations
  through a shell or with unvalidated PR-controlled arguments.
- **Treat PR content as untrusted input.** File paths, branch names,
  titles, and file contents all come from potentially hostile PRs; new
  code paths that touch them must go through the existing guards
  (`safe_repo_path`, oid validation, ref qualification) or add
  equivalent ones with tests.
- **Update the docs** (`README.md`, `docs/`) when behavior or keys
  change, and add a line to `CHANGELOG.md` under *Unreleased*.
- Write commit messages that explain *why*, not just *what*.

Manual testing tip: `loupe --local` in any dirty repo exercises most of
the UI without touching GitHub; the PR flows need a repo with open PRs
you can safely poke at.

## Code style

- `rustfmt` with default settings — no local style debates.
- Clippy stays at **zero warnings** on `--all-targets`.
- Prefer small, well-named functions over comments that narrate code;
  keep the comments that explain *why* (there's a deliberate culture of
  those in this codebase).
- New dependencies need a good reason — the dependency tree is small and
  pure-Rust on purpose (no C build dependencies).

## Code of conduct

This project follows a [code of conduct](CODE_OF_CONDUCT.md); by
participating you agree to uphold it.

## License

By contributing, you agree that your contributions are licensed under the
[Apache License 2.0](LICENSE). Unless you state otherwise, any
contribution you intentionally submit for inclusion in Loupe is licensed
under those terms, with no additional conditions.

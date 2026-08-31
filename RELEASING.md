# Releasing Loupe

Releases are cut from `main` by pushing a version tag; GitHub Actions
builds the binaries and publishes the release. Versions follow
[SemVer](https://semver.org) (pre-1.0: breaking changes bump the minor
version).

## Checklist

1. **Green main.** CI (fmt, clippy, tests on Linux/macOS/Windows) must be
   passing on the commit you're about to release.

2. **Audit dependencies.**

   ```sh
   cargo audit
   ```

   No new vulnerabilities; note any newly accepted advisory warnings in
   the release notes.

3. **Refresh third-party notices** if dependencies changed since the last
   release:

   ```sh
   cargo about generate --fail about.hbs > THIRD-PARTY-NOTICES.md
   ```

   CI fails when this file is stale, so it is usually already current —
   the archives ship it beside `LICENSE` and `NOTICE`, and `--fail` also
   catches a newly introduced non-permissive dependency. Use the same
   `cargo-about` version CI pins (`CARGO_ABOUT_VERSION` in
   `.github/workflows/ci.yml`); another version may render the file
   differently and fail the check.

4. **Bump the version** in `Cargo.toml`, then run `cargo build` (or
   `cargo test`) once so `Cargo.lock` picks up the new version — commit
   both files.

5. **Update `CHANGELOG.md`:** move the *Unreleased* items into a new
   `## [X.Y.Z] - YYYY-MM-DD` section, add the version compare links at
   the bottom, and leave *Unreleased* empty. The release workflow lifts
   this section verbatim into the GitHub release notes.

6. **Commit and tag:**

   ```sh
   git commit -am "Release vX.Y.Z"
   git tag -a vX.Y.Z -m "loupe vX.Y.Z"
   git push origin main vX.Y.Z
   ```

7. **Watch the release workflow** (Actions → Release). It builds:

   | Target | Runner |
   | --- | --- |
   | x86_64-unknown-linux-gnu | ubuntu-latest |
   | aarch64-unknown-linux-gnu | ubuntu-24.04-arm |
   | x86_64-apple-darwin | macos-13 |
   | aarch64-apple-darwin | macos-latest |
   | x86_64-pc-windows-msvc | windows-latest |

   and publishes a GitHub release with the archives, a `SHA256SUMS`
   file, and the changelog section as notes.

8. **Smoke-test an artifact:** download the archive for your platform,
   verify the checksum, run `loupe --help` and open a PR in a real repo.

9. **Announce** wherever makes sense.

## If something goes wrong

- **A build leg fails:** fix on `main`, delete the tag and the draft/
  partial release (`gh release delete vX.Y.Z`, `git push --delete origin
  vX.Y.Z`), and re-tag. Never reuse a tag that shipped working binaries —
  bump the patch version instead.
- **A bad release shipped:** publish a fixed `vX.Y.Z+1`; mark the bad
  release as such in its notes rather than deleting it.

## Notes

- The package sets `publish = false`: the crates.io name `loupe` belongs
  to an unrelated project, so nothing here publishes to a registry. If
  registry publishing is ever wanted, pick a unique crate name first and
  add a publish step to this checklist.
- Release binaries are built with the `release` profile as configured in
  `Cargo.toml` (LTO, stripped, single codegen unit).

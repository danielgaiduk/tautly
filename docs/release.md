# Releasing Tautly

Releases are built by [cargo-dist](https://axodotdev.github.io/cargo-dist/) (pinned in
`dist-workspace.toml`) from `.github/workflows/release.yml`, which `dist generate` writes. Never
edit that file by hand: change `dist-workspace.toml`, run `dist generate`, commit both.

## What a release contains

| Artifact | Notes |
|---|---|
| `tautly-<target>.tar.xz` + `.sha256` | targets: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl` (static), `aarch64-unknown-linux-musl` (static); the binary is `tautly` |
| `tautly-installer.sh` | `curl \| sh` installer; installs to `$CARGO_HOME/bin` |
| `tautly.rb` | Homebrew formula, pushed to the tap `<owner>/homebrew-tap` |
| `sha256.sum`, `source.tar.gz` | unified checksums, source archive |

The Cargo package is named `tautly` (its directory is `crates/tautly-cli`), and cargo-dist names
artifacts after the package. `cargo binstall tautly` finds the archives through
`[package.metadata.binstall]` in `crates/tautly-cli/Cargo.toml`. binstall's registry lookup needs the
crate on crates.io (publishing is not part of these steps); `cargo binstall --git
https://github.com/<owner>/tautly tautly` does not.

Runners: macOS targets build on `macos-14`, `x86_64-unknown-linux-musl` on `ubuntu-latest` with
`musl-tools`, `aarch64-unknown-linux-musl` on `ubuntu-24.04-arm` with `musl-tools`.

## Known properties

- **Tag trigger.** The workflow runs on the glob `**[0-9]+.[0-9]+.[0-9]+*`, which dist generates.
  It matches `v0.1.0` (use this form) and also any other tag ending in a version, such as
  `foo1.2.3`. Pushing tags needs write access to the repository.
- **Workflow permissions.** dist hardcodes `permissions: contents: write` for the whole release
  workflow, so the plan and build jobs hold it too (their checkouts use `persist-credentials:
  false`). Only the `publish-homebrew-formula` job sees `HOMEBREW_TAP_TOKEN`.
- **Installer checksums.** `tautly-installer.sh` verifies the archive's sha256 only when
  `sha256sum` exists (Linux). Stock macOS has none, so the installer prints that it skips the check
  and installs anyway. Point macOS users at Homebrew: the formula embeds the sha256.
- **Signing.** macOS binaries are not signed or notarised (post-v1); neither curl nor Homebrew sets
  the quarantine flag.
- **aarch64 musl.** CI builds and checks only `x86_64-unknown-linux-musl` (job `musl-static`). The
  `aarch64-unknown-linux-musl` target is first built by the release workflow itself.

## Checks

- `bash scripts/check-dist-plan.sh` (CI job `dist-plan`): the plan has exactly the four targets, the
  two installers and sha256 checksums; the runners, the `musl-tools` step, the tap and the publish
  job are as configured; `release.yml` is in sync; the binstall templates name the planned
  archives; the Linux targets pull in no libdbus or OpenSSL. It runs `cargo fetch --locked`, so it
  needs network access for the crates.
- `bash scripts/check-dist-plan.sh --release`: the same, and it fails while the `OWNER`
  placeholders remain. Run it before pushing the first tag.
- CI job `musl-static`: builds `x86_64-unknown-linux-musl`, asserts the binary is static (`file`
  and `ldd`) and runs `tautly --version`. Keyring's pure-Rust Secret Service backend (zbus) has to
  link there. If that ever fails, switch the two Linux targets in `dist-workspace.toml` to the
  `-gnu` triples, drop `[dist.dependencies.apt]`, run `dist generate`, update
  `scripts/check-dist-plan.sh` and say so in the release notes (spec 7.11.3).
- Locally: `dist build --artifacts=local --target=<host target>`, then run `tautly --version` from
  the archive in `target/distrib/`.

## Before the first release (steps only you can do)

1. **Set the repository URL.** Replace `OWNER` in `repository`/`homepage` in the root `Cargo.toml`
   (`[workspace.package]`) and in `tap` in `dist-workspace.toml`; run `dist generate` and commit.
   Then run `bash scripts/check-dist-plan.sh --release`, which must pass.
2. **Create the Homebrew tap.** Create an empty GitHub repository `<owner>/homebrew-tap`, a token
   with write access to it, and store the token as the repository secret `HOMEBREW_TAP_TOKEN`.
3. **Push the first tag.** Push `v0.1.0` (any `vX.Y.Z` matching the workspace version) from the
   commit to release. The workflow builds the four targets, creates the GitHub release, then
   pushes the formula to the tap. CI does not create tags.

Until step 1 is done the placeholder URLs in the plan are not downloadable, and a tag pushed
anyway creates a release whose installer and formula point at `github.com/OWNER/tautly`. Until
step 2 is done the `publish-homebrew-formula` job fails after the GitHub release has been created.

## TODO before the first release: pin third-party actions

The actions in `.github/workflows/ci.yml` are pinned to tags, because the commit SHAs could not be
resolved without contacting GitHub. Replace them with commit SHAs (keeping the tag in a comment):

- `dtolnay/rust-toolchain@stable` (three jobs)
- `Swatinem/rust-cache@v2` (two jobs)

The `dist-plan` job also downloads the cargo-dist installer by version (`v0.34.0`) with no hash
check, as the generated `release.yml` does. The actions in `release.yml` are all first-party.

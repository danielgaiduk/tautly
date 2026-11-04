# Releasing Tautly

Releases are built by [cargo-dist](https://axodotdev.github.io/cargo-dist/) (pinned in
`dist-workspace.toml`) from `.github/workflows/release.yml`, which `dist generate` writes. Never edit
that file by hand: change `dist-workspace.toml`, run `dist generate`, commit both.

## What a release contains

| Artifact | Notes |
|---|---|
| `tautly-<target>.tar.xz` + `.sha256` | targets: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl` (static), `aarch64-unknown-linux-musl` (static); the binary is `tautly` |
| `tautly-installer.sh` | `curl \| sh` installer; installs to `$CARGO_HOME/bin` |
| `tautly.rb` | Homebrew formula, pushed to the tap `<owner>/homebrew-tap` |
| `sha256.sum`, `source.tar.gz` | unified checksums, source archive |

The Cargo package is named `tautly` (its directory is `crates/tautly-cli`), and cargo-dist names
artifacts after the package. `cargo binstall tautly` finds the archives through
`[package.metadata.binstall]` in `crates/tautly-cli/Cargo.toml`; binstall's registry lookup needs the
crate on crates.io (publishing is not part of these steps), `cargo binstall --git
https://github.com/<owner>/tautly tautly` does not. macOS binaries are not signed or notarised (post-v1); neither curl nor Homebrew sets the
quarantine flag.

Runners: macOS targets build on `macos-14`, `x86_64-unknown-linux-musl` on `ubuntu-latest` with
`musl-tools`, `aarch64-unknown-linux-musl` on `ubuntu-24.04-arm` with `musl-tools`.

## Checks

- `bash scripts/check-dist-plan.sh` (CI job `dist-plan`): the plan has exactly the four targets, the
  two installers and sha256 checksums; `release.yml` is in sync; the binstall templates name the
  planned archives; the Linux targets pull in no libdbus or OpenSSL.
- CI job `musl-static`: builds `x86_64-unknown-linux-musl`, asserts the binary is static and runs
  `tautly --version`. Keyring's pure-Rust Secret Service backend (zbus) has to link there. If that
  ever fails, switch the two Linux targets in `dist-workspace.toml` to the `-gnu` triples, drop
  `[dist.dependencies.apt]`, run `dist generate`, update `scripts/check-dist-plan.sh` and say so in the
  release notes (spec 7.11.3).
- Locally: `dist build --artifacts=local --target=<host target>`, then run `tautly --version` from the
  archive in `target/distrib/`.

## Before the first release (three steps only you can do)

1. **Set the repository URL.** Replace `OWNER` in `repository`/`homepage` in the root `Cargo.toml`
   (`[workspace.package]`) and in `tap` in `dist-workspace.toml`; run `dist generate` and commit.
2. **Create the Homebrew tap.** Create an empty GitHub repository `<owner>/homebrew-tap`, a token with
   write access to it, and store the token as the repository secret `HOMEBREW_TAP_TOKEN`.
3. **Push the first tag.** Push `v0.1.0` (any `vX.Y.Z` matching the workspace version) from the commit
   to release. The workflow builds the four targets, creates the GitHub release, then pushes the
   formula to the tap. CI does not create tags.

Until step 1 is done the placeholder URLs in the plan are not downloadable, and until step 2 is done
the `publish-homebrew-formula` job fails after the GitHub release has been created.

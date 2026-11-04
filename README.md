# Tautly

A fast, token-frugal AI coding harness for the terminal. Apache-2.0.

## Install

Nothing is published yet. From the first `v*` release on, binaries will be available for macOS and
Linux on arm64 and x86_64; the Linux ones will be static (musl). Replace `OWNER` with the account
that hosts the repository (see [docs/release.md](docs/release.md)).

```sh
# Homebrew (recommended on macOS: the formula verifies the download's sha256)
brew install OWNER/tap/tautly

# Shell installer (installs to $CARGO_HOME/bin)
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/OWNER/tautly/releases/latest/download/tautly-installer.sh | sh

# cargo-binstall
cargo binstall --git https://github.com/OWNER/tautly tautly

# From source
cargo install --path crates/tautly-cli  # the package is `tautly`
```

Each release will also carry `tautly-<target>.tar.xz` archives with `.sha256` checksums. The shell
installer verifies the checksum only where `sha256sum` exists (Linux); stock macOS lacks it and the
installer then skips the check, so macOS users should prefer Homebrew. macOS binaries are not yet
signed or notarised.

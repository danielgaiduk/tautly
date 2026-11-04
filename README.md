# Tautly

A fast, token-frugal AI coding harness for the terminal. Apache-2.0.

## Install

Release binaries are static on Linux (musl) and exist for macOS and Linux on arm64 and x86_64.
Replace `OWNER` with the account that hosts the repository (see [docs/release.md](docs/release.md);
nothing is published until the first `v*` tag).

```sh
# Homebrew
brew install OWNER/tap/tautly

# Shell installer (installs to $CARGO_HOME/bin)
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/OWNER/tautly/releases/latest/download/tautly-cli-installer.sh | sh

# cargo-binstall
cargo binstall --git https://github.com/OWNER/tautly tautly-cli

# From source
cargo install --path crates/tautly-cli
```

Each release also carries `tautly-cli-<target>.tar.xz` archives with `.sha256` checksums. macOS
binaries are not yet signed or notarised.

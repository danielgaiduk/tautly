#!/usr/bin/env bash
# Checks the release plan cargo-dist derives from dist-workspace.toml (spec 7.11.3):
#   - `dist plan` succeeds, which includes "release.yml is up to date with the config";
#   - the cargo-dist version is pinned;
#   - exactly the four release targets;
#   - exactly the `shell` and `homebrew` installers;
#   - a sha256 checksum for every archive;
#   - the `cargo binstall` templates in crates/tautly-cli/Cargo.toml name the planned archives;
#   - the Linux targets pull in no libdbus or OpenSSL (keyring's pure-Rust Secret Service).
# Needs `dist`, `jq` and `cargo`; contacts nothing (the dependency tree comes from Cargo.lock).
set -euo pipefail

cd "$(dirname "$0")/.."

fail() {
    echo "check-dist-plan: FAIL: $*" >&2
    exit 1
}

command -v dist >/dev/null || fail "dist (cargo-dist) is not on PATH"
command -v jq >/dev/null || fail "jq is not on PATH"
[ -f dist-workspace.toml ] || fail "dist-workspace.toml is missing"

grep -Eq '^cargo-dist-version = "[0-9]+\.[0-9]+\.[0-9]+"' dist-workspace.toml ||
    fail "cargo-dist-version is not pinned in dist-workspace.toml"

plan=$(dist plan --output-format=json) || fail "dist plan failed (release.yml out of date? run: dist generate)"

# Targets: the archives (kind executable-zip) cover exactly the four release targets.
want_targets='["aarch64-apple-darwin","aarch64-unknown-linux-musl","x86_64-apple-darwin","x86_64-unknown-linux-musl"]'
got_targets=$(jq -c '[.artifacts[] | select(.kind == "executable-zip") | .target_triples[]] | unique' <<<"$plan")
[ "$got_targets" = "$want_targets" ] || fail "targets: want $want_targets, got $got_targets"

# Installers: the shell script and the Homebrew formula, nothing else.
want_installers='["tautly-installer.sh","tautly.rb"]'
got_installers=$(jq -c '[.artifacts[] | select(.kind == "installer") | .name] | sort' <<<"$plan")
[ "$got_installers" = "$want_installers" ] || fail "installers: want $want_installers, got $got_installers"

# Checksums: every archive names a `<archive>.sha256` that is itself in the plan.
jq -e '
    . as $p
    | [.artifacts[] | select(.kind == "executable-zip")]
    | length == 4
      and all(. as $a | $a.checksum == ($a.name + ".sha256") and ($p.artifacts | has($a.checksum)))
' <<<"$plan" >/dev/null || fail "an archive has no .sha256 checksum in the plan"

# binstall: expand the pkg-url template for each target and compare it with the planned download URL.
meta=$(cargo metadata --no-deps --format-version 1 --offline) || fail "cargo metadata failed"
binstall=$(jq -c '.packages[] | select(.name == "tautly") | {name, version, repository, binstall: .metadata.binstall}' <<<"$meta")
[ -n "$binstall" ] || fail "package tautly not found"
pkg_name=$(jq -r .name <<<"$binstall")
version=$(jq -r .version <<<"$binstall")
repo=$(jq -r '.repository | rtrimstr("/")' <<<"$binstall")
pkg_url=$(jq -r '.binstall["pkg-url"] // empty' <<<"$binstall")
bin_dir=$(jq -r '.binstall["bin-dir"] // empty' <<<"$binstall")
pkg_fmt=$(jq -r '.binstall["pkg-fmt"] // empty' <<<"$binstall")
[ -n "$pkg_url" ] || fail "[package.metadata.binstall] pkg-url is missing"
[ "$pkg_fmt" = "txz" ] || fail "binstall pkg-fmt must be txz (dist archives are .tar.xz), got '$pkg_fmt'"

base=$(jq -r '.releases[0].hosting.github | .artifact_base_url + .artifact_download_path' <<<"$plan")
for target in $(jq -r '.[]' <<<"$want_targets"); do
    expand() {
        local t=$1
        t=${t//'{ repo }'/$repo}
        t=${t//'{ name }'/$pkg_name}
        t=${t//'{ version }'/$version}
        t=${t//'{ target }'/$target}
        t=${t//'{ archive-suffix }'/.tar.xz}
        t=${t//'{ archive-format }'/tar.xz}
        t=${t//'{ bin }'/tautly}
        t=${t//'{ binary-ext }'/}
        printf '%s' "$t"
    }
    url=$(expand "$pkg_url")
    want="$base/$pkg_name-$target.tar.xz"
    [ "$url" = "$want" ] || fail "binstall pkg-url expands to '$url' for $target, the plan has '$want'"
    jq -e --arg n "$pkg_name-$target.tar.xz" '.artifacts | has($n)' <<<"$plan" >/dev/null ||
        fail "$pkg_name-$target.tar.xz is not a planned artifact"
    # dist archives hold one directory named like the archive, with the binary in it.
    dir=$(expand "$bin_dir")
    [ "$dir" = "$pkg_name-$target/tautly" ] || fail "binstall bin-dir expands to '$dir' for $target, the archive has '$pkg_name-$target/tautly'"
done

# Static Linux: keyring must use the pure-Rust Secret Service (zbus), not libdbus or OpenSSL.
for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
    tree=$(cargo tree -p tautly --target "$target" -e normal --prefix none --offline 2>/dev/null) ||
        fail "cargo tree failed for $target"
    if grep -Eq '^(dbus-secret-service|libdbus-sys|openssl-sys|openssl) ' <<<"$tree"; then
        fail "$target links libdbus or OpenSSL; keyring needs async-secret-service + crypto-rust"
    fi
    grep -q '^secret-service ' <<<"$tree" || fail "$target has no pure-Rust secret-service in its tree"
done

echo "check-dist-plan: OK"

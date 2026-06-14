#!/usr/bin/env sh
# Builds the static musl rbuildd daemon inside a container, so nothing (not a
# C cross-toolchain, not musl) has to be installed on your machine. The result
# is a fully static binary that runs in any minimal image on the remote.
#
# Usage:  scripts/build-daemon.sh [output-dir]
# Output: <output-dir>/rbuildd  (default: target/daemon/)
set -eu

out_dir="${1:-target/daemon}"
mkdir -p "$out_dir"
repo="$(cd "$(dirname "$0")/.." && pwd)"

docker run --rm \
    -v "$repo":/src \
    -v rbuilddev-cargo-registry:/usr/local/cargo/registry \
    -w /src \
    rust:alpine sh -ec '
        apk add --no-cache musl-dev >/dev/null
        cargo build --release -p rbuildd \
            --target x86_64-unknown-linux-musl \
            --target-dir /src/target/daemon-build
    '

cp "$repo/target/daemon-build/x86_64-unknown-linux-musl/release/rbuildd" "$out_dir/rbuildd"
echo "Static daemon: $out_dir/rbuildd"

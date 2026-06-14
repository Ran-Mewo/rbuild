#!/usr/bin/env bash
# Stamp the workspace version from a release tag, so the compiled binaries
# report the released version (CARGO_PKG_VERSION is baked from Cargo.toml at
# build time — it does NOT track the git tag on its own). Run in CI before
# building. Usage: stamp-version.sh <tag>   e.g. stamp-version.sh v0.2.0
set -eu

tag="${1:?usage: stamp-version.sh <tag>}"
# Accept "v0.2.0" or "0.2.0"; the crate version must be bare semver.
version="${tag#v}"

if ! printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-+].*)?$'; then
    echo "stamp-version: tag '$tag' is not a valid x.y.z version" >&2
    exit 1
fi

# Rewrite the first `version = "..."` line (the [workspace.package] one).
# Portable across GNU/BSD sed by writing to a temp file rather than -i.
awk -v v="$version" '
    !done && /^version = "/ { sub(/"[^"]*"/, "\"" v "\""); done=1 }
    { print }
' Cargo.toml > Cargo.toml.tmp
mv Cargo.toml.tmp Cargo.toml

echo "stamp-version: set workspace version to $version"
grep -m1 '^version = ' Cargo.toml

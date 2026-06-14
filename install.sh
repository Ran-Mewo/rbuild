#!/usr/bin/env sh
# rbuild installer.
#
#   curl -fsSL https://raw.githubusercontent.com/Ran-Mewo/rbuild/main/install.sh | sh
#
# Downloads the latest release archive for this OS/arch and installs `rbuild`
# (and the static `rbuildd` daemon it pushes to the remote) into ~/.local/bin.
# Override the install dir with RBUILD_INSTALL_DIR, or the version with
# RBUILD_VERSION (defaults to the latest release).
set -eu

REPO="${RBUILD_REPO:-Ran-Mewo/rbuild}"
INSTALL_DIR="${RBUILD_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${RBUILD_VERSION:-latest}"

say()  { printf 'rbuild-install: %s\n' "$1" >&2; }
die()  { printf 'rbuild-install: error: %s\n' "$1" >&2; exit 1; }

# --- detect platform, mapped to the release archive's target triple ---------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Linux)  os_part="unknown-linux-gnu" ;;
    Darwin) os_part="apple-darwin" ;;
    *) die "unsupported OS '$os'. On Windows, download the release zip manually." ;;
esac
case "$arch" in
    x86_64|amd64)  arch_part="x86_64" ;;
    aarch64|arm64) arch_part="aarch64" ;;
    *) die "unsupported architecture '$arch'." ;;
esac
triple="${arch_part}-${os_part}"

# --- resolve the download URL ----------------------------------------------
if [ "$VERSION" = "latest" ]; then
    base="https://github.com/$REPO/releases/latest/download"
else
    base="https://github.com/$REPO/releases/download/$VERSION"
fi
archive="rbuild-${triple}.tar.gz"
url="$base/$archive"

command -v curl >/dev/null 2>&1 || die "curl is required."
command -v tar  >/dev/null 2>&1 || die "tar is required."

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "downloading $url"
curl -fSL "$url" -o "$tmp/$archive" || die "download failed (is there a release for $triple?)"

# Archive layout: rbuild, daemon/rbuildd
tar -xzf "$tmp/$archive" -C "$tmp"
[ -f "$tmp/rbuild" ] || die "archive missing rbuild binary"
[ -f "$tmp/daemon/rbuildd" ] || die "archive missing daemon/rbuildd"

mkdir -p "$INSTALL_DIR/daemon"
install -m 0755 "$tmp/rbuild" "$INSTALL_DIR/rbuild"
install -m 0755 "$tmp/daemon/rbuildd" "$INSTALL_DIR/daemon/rbuildd"

say "installed rbuild to $INSTALL_DIR/rbuild"

# If the live-sync agent is already running (this is an update), restart it so
# it picks up the new binary.
if command -v systemctl >/dev/null 2>&1 && systemctl --user is-active --quiet rbuild-agent 2>/dev/null; then
    systemctl --user restart rbuild-agent >/dev/null 2>&1 || true
    say "restarted the live-sync agent"
fi

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) say "note: $INSTALL_DIR is not on your PATH — add it to use \`rbuild\` directly." ;;
esac
say "next: rbuild init <ssh-host> && rbuild add <code-dir> && rbuild init-shell <shell>"

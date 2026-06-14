#!/usr/bin/env bash
# Common setup sourced by every Wine command wrapper.
#
# The daemon points WINEPREFIX at a persistent, mounted directory so toolchain
# state survives across builds. On its first use that prefix is empty, so we
# seed it from the image's pre-initialized prefix (/opt/wineprefix) once.
set -e

readonly SEED_PREFIX=/opt/wineprefix
: "${WINEPREFIX:=$SEED_PREFIX}"
export WINEPREFIX WINEDEBUG=-all

# Seed an empty mounted prefix from the image's prepared one (toolchains, rustup).
if [ ! -e "$WINEPREFIX/system.reg" ]; then
    mkdir -p "$WINEPREFIX"
    cp -a "$SEED_PREFIX/." "$WINEPREFIX/" 2>/dev/null || true
    wineboot --init >/dev/null 2>&1 || true
fi

# Locate a Windows executable installed somewhere under the prefix (rustup puts
# things under the invoking user's profile, whose name varies).
find_win_exe() {
    local name="$1"
    find "$WINEPREFIX/drive_c/users" -iname "$name" -print -quit 2>/dev/null
}

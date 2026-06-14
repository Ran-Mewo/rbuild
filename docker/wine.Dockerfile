# Windows build image for rbuild — runs builds through Wine, no VM, no KVM.
#
# The remote here is a KVM-less Linux host, so a Windows VM (e.g. dockur/windows)
# isn't an option. Instead this image carries genuine Windows toolchains and runs
# them under Wine: the real MSVC cl.exe/link.exe via msvc-wine, the Windows Rust
# toolchain, and Windows Python. The intercepted command names (cargo, cmake,
# python, …) are PATH wrappers that exec the Windows tool under Wine, so the
# daemon passes the user's command through unchanged and it behaves as on Windows,
# emitting real .exe/.dll artifacts.
#
# Build is intentionally heavy (it downloads the MSVC toolchain, which is large
# and not redistributable — you accept Microsoft's license by building it). Build
# it once on the remote, where disk is plentiful:
#   docker build -f docker/wine.Dockerfile -t rbuild/wine:latest docker/
FROM debian:trixie-slim

ENV DEBIAN_FRONTEND=noninteractive

# Wine (64-bit), plus tooling msvc-wine needs to unpack the MSVC MSIs.
RUN dpkg --add-architecture i386 \
    && apt-get update && apt-get install -y --no-install-recommends \
        wine wine64 \
        python3 python3-pip \
        msitools ca-certificates winbind \
        curl git \
    && rm -rf /var/lib/apt/lists/*

# --- MSVC toolchain under Wine (msvc-wine) --------------------------------
# Downloads cl.exe/link.exe and the Windows SDK using the VS installer manifests,
# then installs the Wine wrappers so they run transparently from Unix build tools.
ARG MSVC_DEST=/opt/msvc
RUN git clone --depth 1 https://github.com/mstorsjo/msvc-wine /tmp/msvc-wine \
    && python3 /tmp/msvc-wine/vsdownload.py --accept-license --dest ${MSVC_DEST} \
    && /tmp/msvc-wine/install.sh ${MSVC_DEST} \
    && rm -rf /tmp/msvc-wine

# --- Windows Rust toolchain (runs under Wine) -----------------------------
# Fetch the Windows rustup-init and install the x86_64-pc-windows-msvc toolchain
# into a Wine prefix at build time, so `cargo`/`rustc` execute as Windows binaries.
ENV WINEPREFIX=/opt/wineprefix \
    WINEDEBUG=-all
RUN wineboot --init \
    && curl -L -o /tmp/rustup-init.exe https://win.rustup.rs/x86_64 \
    && wine /tmp/rustup-init.exe -y --default-host x86_64-pc-windows-msvc --profile minimal \
    && rm -f /tmp/rustup-init.exe

# --- Command wrappers ------------------------------------------------------
# Rust's Windows toolchain runs under Wine via these wrappers (rustup installs
# under a Wine user profile whose path we resolve at run time). The MSVC tools
# (cl.exe/link.exe) are already installed as run-under-Wine wrappers by
# msvc-wine in ${MSVC_DEST}/bin/x64, so we put that on PATH rather than re-wrap
# them; CMake/Ninja in the image then drive them as on Windows.
ENV PATH=${MSVC_DEST}/bin/x64:/usr/local/bin:$PATH
COPY wine-wrappers/_common.sh wine-wrappers/cargo wine-wrappers/rustc /usr/local/bin/
RUN chmod +x /usr/local/bin/_common.sh /usr/local/bin/cargo /usr/local/bin/rustc

# The daemon overrides WINEPREFIX to a mounted, persistent prefix at run time so
# first-run state is reused across builds; this build-time prefix seeds it.
WORKDIR /work

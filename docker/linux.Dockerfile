# Default Linux build image for rbuild.
#
# Carries the toolchains a typical project needs so the host never has them
# installed. Extend or replace per project via `linux_image` in .rbuild/config.toml.
FROM debian:trixie-slim

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        cmake \
        ninja-build \
        pkg-config \
        git \
        curl \
        ca-certificates \
        python3 \
        python3-pip \
    && rm -rf /var/lib/apt/lists/*

# Rust via rustup, installed to a shared location on PATH for any build user.
ENV RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    PATH=/opt/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --profile minimal \
    && chmod -R a+rwX /opt/rustup /opt/cargo

# The daemon runs builds as the invoking host user (via `docker run --user`),
# pointing HOME/CARGO_HOME at a mounted cache, so no in-image user is needed.
WORKDIR /work

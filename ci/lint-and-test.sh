#!/usr/bin/env bash
# The `lint-and-test` check: the steps of ci.yml's `check` job ("fmt, clippy, test"), in the same order.
# yggdrasil.yml runs it in the pinned stable Rust image from the repository's root.
#
# The workflow installed the ffmpeg development libraries with apt and the toolchain (with rustfmt and clippy)
# with dtolnay/rust-toolchain; here the image is the toolchain (1.99.1 is what `stable` was), the libraries come
# from ci/apt-packages.sh (as root, which is how the agents run checks), and the two components are added with
# rustup. Swatinem/rust-cache has no counterpart: every run builds from scratch.
set -euo pipefail

# The repository's root, wherever this is run from.
cd "$(dirname "${BASH_SOURCE[0]}")/.."

# shellcheck source=ci/apt-packages.sh
. ci/apt-packages.sh

# Fail the build on any warning rather than letting drift accumulate: the tree is warning-free, so a new warning
# is a new regression.
export CARGO_TERM_COLOR=always
export RUSTFLAGS="-D warnings"

# ffmpeg-next's underlying ffmpeg-sys-next crate needs the ffmpeg C development libraries at build time (and clang
# for bindgen): the first system dependency this workspace has needed.
ensure_packages \
  libavformat-dev libavcodec-dev libavutil-dev \
  libavfilter-dev libavdevice-dev libswscale-dev \
  libswresample-dev pkg-config clang

rustup component add clippy rustfmt

# Formatting first: it is the cheapest gate and its failures are the least interesting to read past.
cargo fmt --all -- --check

# `--all-targets` so the lint pass covers tests and benches, not just the libraries: most of the workspace's
# code lives in its tests.
cargo clippy --workspace --all-targets -- -D warnings

cargo test --workspace

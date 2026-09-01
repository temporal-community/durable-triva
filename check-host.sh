#!/bin/sh
# Run the host-side checks for web, shared and badge-screen.
#
# The workspace .cargo/config.toml pins build.target to xtensa-esp32s3-espidf
# for the firmware, which means a bare `cargo test` or `cargo clippy` from the
# repository root tries to build the host crates for the badge and fails. This
# script supplies the host target and the stable toolchain explicitly.
#
# Firmware build and hardware verification live in ./build-firmware.sh.
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$project_dir"

host_target=$(rustc -vV | awk '/^host: /{print $2}')
: "${RUSTUP_TOOLCHAIN:=stable}"
export RUSTUP_TOOLCHAIN

packages="-p temporal-trivia-web -p temporal-trivia-shared -p badge-screen -p badge-input"

echo "==> fmt"
cargo fmt --all -- --check

echo "==> clippy ($host_target)"
# shellcheck disable=SC2086
cargo clippy --no-deps $packages --all-targets --target "$host_target" -- -D warnings

echo "==> test ($host_target)"
# shellcheck disable=SC2086
cargo test --no-fail-fast $packages --target "$host_target" "$@"

#!/usr/bin/env bash
# Builds the headless daemon and stages it for the Debian bundle. Tauri runs
# this before `cargo tauri build`; the CLI is also usable without a frontend
# toolchain because the shell serves a static page.
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
root="$(cd "$here/../.." && pwd)"
cd "$root"
cargo build -p cd-daemon --release
mkdir -p "$here/dist"
cp target/release/crossdev "$here/dist/crossdev"

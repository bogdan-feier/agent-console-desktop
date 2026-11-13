#!/usr/bin/env bash
# One-time setup on macOS: installs what is missing, runs the tests, builds the app.
# Safe to run again; every step is skipped when it is already done.
#
#     bash scripts/setup.sh
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n==> %s\n' "$1"; }

step "Xcode command line tools (compiler and linker)"
if xcode-select -p >/dev/null 2>&1; then
    echo "already installed"
else
    xcode-select --install || true
    echo "A window opened to install them. When it finishes, run this script again."
    exit 1
fi

step "Rust"
if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
    source "$HOME/.cargo/env"
fi
if command -v cargo >/dev/null 2>&1; then
    echo "already installed: $(cargo --version)"
else
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    source "$HOME/.cargo/env"
fi
# rust-toolchain.toml makes cargo fetch the pinned toolchain with rustfmt and clippy.
cargo --version

step "Tauri command line tool (compiles from source the first time: several minutes)"
if cargo tauri --version >/dev/null 2>&1; then
    echo "already installed: $(cargo tauri --version)"
else
    cargo install tauri-cli --version "^2" --locked
fi

step "Claude Code"
if command -v claude >/dev/null 2>&1; then
    echo "found: $(command -v claude)"
else
    echo "WARNING: 'claude' is not on your PATH. The app needs Claude Code installed and"
    echo "logged in (run 'claude', then /login)."
fi

step "Tests"
cargo test --workspace

step "Building the app (release build: a few minutes the first time)"
cargo tauri build

APP="target/release/bundle/macos/Agent Console.app"
printf '\nDone. The app is at:\n    %s\n' "$APP"
echo "Open it with:      open \"$APP\""
echo "Or install it:     cp -R \"$APP\" /Applications/"

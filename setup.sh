#!/usr/bin/env bash
# Phase 0 dev environment bootstrap.
#
# Clones the Limine binary branch to ./limine, builds the `limine` host tool,
# and verifies the other host tools that make(1) needs. Never rewrites any
# project files (DESIGN §0.5).

set -euo pipefail

LIMINE_TAG="${LIMINE_TAG:-v9.6.7-binary}"
LIMINE_COMMIT="${LIMINE_COMMIT:-ee5d29cd0a8034612dcd1df3f00052480db785c5}"
LIMINE_REPO="${LIMINE_REPO:-https://github.com/limine-bootloader/limine.git}"
LIMINE_DIR="${LIMINE_DIR:-./limine}"

ROOT=$(cd "$(dirname "$0")" && pwd)
TOOLCHAIN_FILE="$ROOT/rust-toolchain.toml"

need() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "setup: missing required tool: $1" >&2
        return 1
    fi
    echo "setup: found $1"
}

echo "setup: checking host tools"
need git
need make
need cc
need qemu-system-x86_64
need xorriso
need nasm
need python3
need cargo

pyver=$(python3 -c 'import sys; print("%d.%d" % sys.version_info[:2])')
python3 -c 'import sys; raise SystemExit(0 if sys.version_info >= (3, 11) else 1)' \
    || { echo "setup: python3 >= 3.11 required (found $pyver)" >&2; exit 1; }
echo "setup: python3 $pyver"

if command -v ruff >/dev/null 2>&1; then
    echo "setup: found ruff ($(ruff --version))"
else
    echo "setup: ruff not installed (optional; pip install ruff)"
fi
if command -v mypy >/dev/null 2>&1; then
    echo "setup: found mypy ($(mypy --version | head -1))"
else
    echo "setup: mypy not installed (optional; pip install mypy)"
fi

if [ -f "$TOOLCHAIN_FILE" ]; then
    PINNED=$(sed -n 's/^channel = "\(.*\)"/\1/p' "$TOOLCHAIN_FILE" | head -n1)
    if [ -z "$PINNED" ]; then
        echo "setup: could not read channel from $TOOLCHAIN_FILE" >&2
        exit 1
    fi
    echo "setup: rust toolchain $PINNED"
    if command -v rustup >/dev/null 2>&1; then
        if rustup toolchain list | grep -qF "$PINNED"; then
            echo "setup: $PINNED already installed"
        else
            echo "setup: installing $PINNED"
            rustup toolchain install "$PINNED" --component rust-src --component llvm-tools
        fi
    else
        echo "setup: rustup not found; install $PINNED with rust-src and llvm-tools" >&2
    fi
fi

if [ ! -d "$LIMINE_DIR/.git" ]; then
    echo "setup: cloning $LIMINE_REPO@$LIMINE_TAG -> $LIMINE_DIR"
    git clone --depth 1 --branch "$LIMINE_TAG" "$LIMINE_REPO" "$LIMINE_DIR"
else
    echo "setup: $LIMINE_DIR already present, skipping clone"
fi

got=$(git -C "$LIMINE_DIR" rev-parse HEAD)
if [ "$got" != "$LIMINE_COMMIT" ]; then
    echo "setup: limine HEAD $got != pinned $LIMINE_COMMIT (tag $LIMINE_TAG)" >&2
    echo "setup: rm -rf $LIMINE_DIR and re-run" >&2
    exit 1
fi
echo "setup: limine $LIMINE_TAG @ $LIMINE_COMMIT"

if [ ! -x "$LIMINE_DIR/limine" ]; then
    echo "setup: building limine host tool"
    make -C "$LIMINE_DIR"
else
    echo "setup: limine host tool already built"
fi

echo "setup: ok"

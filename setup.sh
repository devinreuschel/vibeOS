#!/usr/bin/env bash
# Phase 0 dev environment bootstrap.
#
# Clones the Limine binary branch to ./limine, builds the `limine` host tool,
# and verifies the other host tools that make(1) needs. Never rewrites any
# project files (DESIGN §0.5).

set -euo pipefail

LIMINE_TAG="${LIMINE_TAG:-v9.6.7-binary}"
LIMINE_REPO="${LIMINE_REPO:-https://github.com/limine-bootloader/limine.git}"
LIMINE_DIR="${LIMINE_DIR:-./limine}"

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

if [ ! -d "$LIMINE_DIR/.git" ]; then
    echo "setup: cloning $LIMINE_REPO@$LIMINE_TAG -> $LIMINE_DIR"
    git clone --depth 1 --branch "$LIMINE_TAG" "$LIMINE_REPO" "$LIMINE_DIR"
else
    echo "setup: $LIMINE_DIR already present, skipping clone"
fi

if [ ! -x "$LIMINE_DIR/limine" ]; then
    echo "setup: building limine host tool"
    make -C "$LIMINE_DIR"
else
    echo "setup: limine host tool already built"
fi

echo "setup: ok"

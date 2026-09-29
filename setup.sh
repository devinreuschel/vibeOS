#!/usr/bin/env bash
# Phase 0 dev environment bootstrap.
#
# Clones the Limine binary branch to ./limine, builds the `limine` host tool,
# and verifies the other host tools that make(1) needs. Never rewrites any
# project files (ROADMAP §0.5). Installs scripts/hooks/commit-msg into the
# clone's git hooks unless a hook of another origin is already there.

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

# `make check` and the FAT host tests need these (ROADMAP §10.1). Reported,
# not required here: the ladder, smp-stress and release jobs run this script
# without them.
lint_tool() {
    local tool=$1 version
    if command -v "$tool" >/dev/null 2>&1; then
        version=$("$tool" --version 2>&1 | head -n1)
    elif python3 -m "$tool" --version >/dev/null 2>&1; then
        version=$(python3 -m "$tool" --version 2>&1 | head -n1)
    else
        echo "setup: missing required tool: $tool (make check fails without it unless VIBEOS_ALLOW_MISSING_TOOLS=1; pip install the version the check job in .github/workflows/ci.yml pins)" >&2
        return 0
    fi
    echo "setup: found $tool ($version)"
}
lint_tool ruff
lint_tool mypy
# cargo-deny, for `make check`'s `cargo deny check licenses bans sources`
# (ROADMAP §10.9), against the version the check job pins. Reported, not
# required: only the check job installs it.
deny_pin=$(sed -n 's/^ *CARGO_DENY_VERSION: *//p' "$ROOT/.github/workflows/ci.yml" | head -n1)
deny_install="cargo install cargo-deny --locked --version $deny_pin"
if command -v cargo-deny >/dev/null 2>&1; then
    deny_version=$(cargo-deny --version 2>&1 | sed -n 1p)
    if [ "$deny_version" = "cargo-deny $deny_pin" ]; then
        echo "setup: found cargo-deny ($deny_version)"
    else
        echo "setup: found $deny_version, not the pinned $deny_pin; $deny_install" >&2
    fi
else
    echo "setup: missing required tool: cargo-deny (make check fails without it unless VIBEOS_ALLOW_MISSING_TOOLS=1; $deny_install)" >&2
fi
if command -v fsck.fat >/dev/null 2>&1; then
    echo "setup: found fsck.fat ($(fsck.fat --help 2>&1 | head -n1))"
else
    echo "setup: missing required tool: fsck.fat (make check fails without it unless VIBEOS_ALLOW_MISSING_TOOLS=1; install dosfstools)" >&2
fi

# UEFI firmware for `make test-e2e-uefi` (ROADMAP §10.2): the harness's probe,
# reported and never required here.
for arch in x86_64 aarch64; do
    case $arch in
        x86_64) hint="apt install ovmf, or brew install qemu" ;;
        *) hint="apt install qemu-efi-aarch64, or brew install qemu" ;;
    esac
    if fw=$(cd "$ROOT" && PYTHONPATH="$ROOT" python3 tests/harness/run_interactive.py firmware "$arch" 2>&1); then
        echo "setup: $fw"
    else
        echo "setup: note: $fw ($hint)"
    fi
done

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
            rustup toolchain install "$PINNED" --component rust-src --component llvm-tools --no-self-update
        fi
        echo "setup: adding target x86_64-unknown-none"
        rustup target add x86_64-unknown-none --toolchain "$PINNED"
        # The user runtime's triple (ROADMAP §10.5), linked by rust-lld.
        echo "setup: adding target x86_64-unknown-linux-musl"
        rustup target add x86_64-unknown-linux-musl --toolchain "$PINNED"
        # vibeos-core's MSRV, which `make check` builds it with (ROADMAP §10.1).
        MSRV=$(sed -n 's/^rust-version = "\(.*\)"$/\1/p' "$ROOT/crates/core/Cargo.toml")
        if [ -z "$MSRV" ]; then
            echo "setup: could not read rust-version from crates/core/Cargo.toml" >&2
            exit 1
        fi
        if rustup toolchain list | cut -d' ' -f1 | grep -q "^$MSRV-"; then
            echo "setup: MSRV $MSRV already installed"
        else
            echo "setup: installing MSRV $MSRV"
            rustup toolchain install "$MSRV" --profile minimal --no-self-update
        fi
        rustup target add x86_64-unknown-none --toolchain "$MSRV"
    else
        echo "setup: rustup not found; install $PINNED with rust-src, llvm-tools, and targets x86_64-unknown-none and x86_64-unknown-linux-musl" >&2
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
# A tracked file that differs from HEAD (a restored cache or a local edit)
# would ship a Limine binary the HEAD check passed (ROADMAP §10.1). Untracked
# files stay: the macOS build leaves limine.dSYM/.
changed=$(git -C "$LIMINE_DIR" status --porcelain --untracked-files=no)
if [ -n "$changed" ]; then
    echo "setup: limine has changed tracked files (a restored cache or a local edit):" >&2
    echo "$changed" >&2
    echo "setup: rm -rf $LIMINE_DIR and re-run" >&2
    exit 1
fi

if [ ! -x "$LIMINE_DIR/limine" ]; then
    echo "setup: building limine host tool"
    make -C "$LIMINE_DIR"
else
    echo "setup: limine host tool already built"
fi

hooks_path=$(git -C "$ROOT" config --get core.hooksPath || true)
hook_dir=$(git -C "$ROOT" rev-parse --git-path hooks)
case "$hook_dir" in /*) ;; *) hook_dir="$ROOT/$hook_dir" ;; esac
if [ -n "$hooks_path" ]; then
    echo "setup: core.hooksPath is $hooks_path; add scripts/hooks/commit-msg there by hand"
elif [ -e "$hook_dir/commit-msg" ] && ! grep -q 'vibeOS commit-msg hook' "$hook_dir/commit-msg"; then
    echo "setup: $hook_dir/commit-msg is not vibeOS's; add scripts/hooks/commit-msg to it by hand"
else
    mkdir -p "$hook_dir"
    cp "$ROOT/scripts/hooks/commit-msg" "$hook_dir/commit-msg"
    chmod +x "$hook_dir/commit-msg"
    echo "setup: installed the commit-msg hook (check_ticks.py --commit-msg)"
fi

echo "setup: ok"

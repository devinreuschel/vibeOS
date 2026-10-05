#!/usr/bin/env bash
# Phase 0 dev environment bootstrap.
#
# Unpacks the pinned Limine release's binary archive to ./limine, builds the
# `limine` host tool, and verifies the other host tools that make(1) needs.
# Never rewrites any project files (ROADMAP §0.5). Installs
# scripts/hooks/commit-msg into the clone's git hooks unless a hook of another
# origin is already there.

set -euo pipefail

# Limine 12 publishes its binaries only as a release asset, so the pin is the
# tag, the commit it must still name, and the archive's SHA-256.
LIMINE_TAG="${LIMINE_TAG:-v12.9.1}"
LIMINE_COMMIT="${LIMINE_COMMIT:-c82c3708b3304be806b2492dc2ce34e219c6f989}"
LIMINE_SHA256="${LIMINE_SHA256:-5cdebc518daa3af30b22c2322ba0dba2e0e2046fa8b087b9e13071b8dbcdcff4}"
LIMINE_REPO="${LIMINE_REPO:-https://github.com/limine-bootloader/limine}"
LIMINE_URL="${LIMINE_URL:-$LIMINE_REPO/releases/download/$LIMINE_TAG/limine-binary.tar.gz}"
LIMINE_DIR="${LIMINE_DIR:-./limine}"

ROOT=$(cd "$(dirname "$0")" && pwd)
TOOLCHAIN_FILE="$ROOT/rust-toolchain.toml"

# kani-verifier for `make models` (ROADMAP §10.8); it brings its own nightly.
KANI_VERSION=0.68.0
if [ "${1:-}" = "--kani" ]; then
    # Host triple: .cargo/config.toml sets build.target to the kernel's.
    host=$(rustc -vV | sed -n 's/^host: //p')
    echo "setup: installing kani-verifier $KANI_VERSION for $host"
    cargo install --locked kani-verifier --version "$KANI_VERSION" --target "$host"
    cargo kani setup
    exit 0
fi

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
# ROADMAP §11.7: the qemu-system the job's ARCH boots. Default x86_64.
arch="${ARCH:-x86_64}"
case "$arch" in
    x86_64) need qemu-system-x86_64 ;;
    aarch64) need qemu-system-aarch64 ;;
    *)
        echo "setup: ARCH=$arch: not x86_64 or aarch64" >&2
        exit 1
        ;;
esac
need xorriso
need python3
need cargo
need curl
# The harness compresses guest cores with zstd (ROADMAP §10.7).
need zstd

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
# Reported, not required: only `make models` needs it.
kani_found=$(cd "$ROOT" && cargo kani --version 2>/dev/null | sed -n 's/^Kani Rust Verifier \([^ ]*\).*/\1/p') || true
if [ "$kani_found" = "$KANI_VERSION" ]; then
    echo "setup: found kani-verifier ($kani_found)"
else
    echo "setup: kani-verifier $KANI_VERSION not installed (optional; make models needs it: ./setup.sh --kani)"
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
        echo "setup: adding target aarch64-unknown-none-softfloat"
        rustup target add aarch64-unknown-none-softfloat --toolchain "$PINNED"
        echo "setup: adding target aarch64-unknown-linux-musl"
        rustup target add aarch64-unknown-linux-musl --toolchain "$PINNED"
        # vibeos-core's MSRV, which `make check` builds it with (ROADMAP §10.1).
        # VIBEOS_SKIP_MSRV=1 leaves it out: `make repro`'s two builds run only
        # `make isos`, each in a RUSTUP_HOME of its own (scripts/repro_build.py).
        MSRV=$(sed -n 's/^rust-version = "\(.*\)"$/\1/p' "$ROOT/crates/core/Cargo.toml")
        if [ -z "$MSRV" ]; then
            echo "setup: could not read rust-version from crates/core/Cargo.toml" >&2
            exit 1
        fi
        if [ "${VIBEOS_SKIP_MSRV:-}" = 1 ]; then
            echo "setup: MSRV $MSRV skipped (VIBEOS_SKIP_MSRV=1; make check needs it)"
        else
            if rustup toolchain list | cut -d' ' -f1 | grep -q "^$MSRV-"; then
                echo "setup: MSRV $MSRV already installed"
            else
                echo "setup: installing MSRV $MSRV"
                rustup toolchain install "$MSRV" --profile minimal --no-self-update
            fi
            rustup target add x86_64-unknown-none --toolchain "$MSRV"
        fi
    else
        echo "setup: rustup not found; install $PINNED with rust-src, llvm-tools, and targets x86_64-unknown-none and x86_64-unknown-linux-musl" >&2
    fi
fi

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}
limine_stale() {
    echo "setup: $*" >&2
    echo "setup: rm -rf $LIMINE_DIR and re-run" >&2
    exit 1
}

# $LIMINE_DIR/.pin keeps the pin and the archive the directory came from, so
# a restored cache is checked against both without the network.
pin="$LIMINE_TAG $LIMINE_COMMIT $LIMINE_SHA256"
if [ ! -e "$LIMINE_DIR" ]; then
    # The tag's peeled line comes last; a tag moved off the pinned commit
    # is a release nobody checked.
    tagged=$(git ls-remote "$LIMINE_REPO" "refs/tags/$LIMINE_TAG" "refs/tags/$LIMINE_TAG^{}" | awk 'END { print $1 }')
    if [ "$tagged" != "$LIMINE_COMMIT" ]; then
        echo "setup: limine tag $LIMINE_TAG names ${tagged:-no commit}, not the pinned $LIMINE_COMMIT" >&2
        exit 1
    fi
    echo "setup: fetching $LIMINE_URL -> $LIMINE_DIR"
    rm -rf "$LIMINE_DIR.part"
    mkdir -p "$LIMINE_DIR.part/.pin"
    curl -fsSL --retry 3 -o "$LIMINE_DIR.part/.pin/limine-binary.tar.gz" "$LIMINE_URL"
    got=$(sha256 "$LIMINE_DIR.part/.pin/limine-binary.tar.gz")
    if [ "$got" != "$LIMINE_SHA256" ]; then
        echo "setup: $LIMINE_URL has SHA-256 $got, not the pinned $LIMINE_SHA256" >&2
        exit 1
    fi
    tar -xzf "$LIMINE_DIR.part/.pin/limine-binary.tar.gz" -C "$LIMINE_DIR.part" --strip-components=1
    echo "$pin" > "$LIMINE_DIR.part/.pin/pin"
    mv "$LIMINE_DIR.part" "$LIMINE_DIR"
fi

[ -f "$LIMINE_DIR/.pin/pin" ] || limine_stale "$LIMINE_DIR is not an unpacked $LIMINE_TAG (an older clone?)"
[ "$(cat "$LIMINE_DIR/.pin/pin")" = "$pin" ] || limine_stale "$LIMINE_DIR is pinned at $(cat "$LIMINE_DIR/.pin/pin"), not $pin"
got=$(sha256 "$LIMINE_DIR/.pin/limine-binary.tar.gz")
[ "$got" = "$LIMINE_SHA256" ] || limine_stale "$LIMINE_DIR/.pin/limine-binary.tar.gz has SHA-256 $got"
# A file that differs from the archive (a restored cache or a local edit)
# would ship a Limine binary the pin check passed (ROADMAP §10.1). Files the
# archive lacks stay: the host tool, and the macOS build's limine.dSYM/.
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
tar -xzf "$LIMINE_DIR/.pin/limine-binary.tar.gz" -C "$scratch" --strip-components=1
unpacked=$(cd "$LIMINE_DIR" && pwd)
changed=$(cd "$scratch" && find . -type f | sort | while read -r f; do
    cmp -s "$f" "$unpacked/$f" || echo "$f"
done)
if [ -n "$changed" ]; then
    limine_stale "limine has files that differ from its archive (a restored cache or a local edit):
$changed"
fi
echo "setup: limine $LIMINE_TAG @ $LIMINE_COMMIT"

if [ ! -x "$LIMINE_DIR/limine" ]; then
    echo "setup: building limine host tool"
    make -C "$LIMINE_DIR"
else
    echo "setup: limine host tool already built"
fi
tool=$("$LIMINE_DIR/limine" version --version-only)
[ "v$tool" = "$LIMINE_TAG" ] || limine_stale "the limine host tool is $tool, not $LIMINE_TAG"

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

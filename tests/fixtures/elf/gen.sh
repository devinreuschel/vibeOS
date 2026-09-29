#!/usr/bin/env bash
# Regenerate static-lld and static-gnuld from start.S (ROADMAP §10.5, the
# `elf::parse` host tests over checked-in static binaries).
#
#   bash tests/fixtures/elf/gen.sh                         # Linux
#   GNU_LD=x86_64-elf-ld bash tests/fixtures/elf/gen.sh    # macOS (Homebrew x86_64-elf-binutils)
#
# The outputs are checked in, so a build never runs this. Intermediates go
# under build/fixtures/elf. After a regeneration, update the expected values
# in crates/core/src/proc/elf.rs (`elf_parse_linked_static_binaries`) from
#   llvm-readobj --elf-output-style=GNU -h -l tests/fixtures/elf/static-*
#
# Last generated with:
#   clang: Ubuntu clang version 18.1.3 (1ubuntu1)
#   ld.lld: LLD 23.1.1 (rust-lld of nightly-2026-09-22)
#   GNU ld: GNU ld (GNU Binutils for Ubuntu) 2.42
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
out="$root/build/fixtures/elf"
mkdir -p "$out"

H=$(rustc -vV | sed -n 's/^host: //p')
LLD="$(rustc --print sysroot)/lib/rustlib/$H/bin/rust-lld"
GNU_LD=${GNU_LD:-ld}
CLANG=${CLANG:-clang}

gnu_version=$("$GNU_LD" --version | sed -n 1p)
case $gnu_version in
    "GNU ld"*) ;;
    *)
        echo "gen.sh: $GNU_LD is not GNU ld ($gnu_version); set GNU_LD" >&2
        exit 1
        ;;
esac

FLAGS=(-static -nostdlib --build-id=none -z noexecstack -z max-page-size=0x1000 -e _start)

"$CLANG" --target=x86_64-unknown-linux-gnu -c "$here/start.S" -o "$out/start.o"
"$LLD" -flavor gnu "${FLAGS[@]}" "$out/start.o" -o "$here/static-lld"
"$GNU_LD" "${FLAGS[@]}" "$out/start.o" -o "$here/static-gnuld"

echo "gen.sh: clang: $("$CLANG" --version | sed -n 1p)"
echo "gen.sh: ld.lld: $("$LLD" -flavor gnu --version | sed -n 1p)"
echo "gen.sh: GNU ld: $gnu_version"

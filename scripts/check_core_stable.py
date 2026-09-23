#!/usr/bin/env python3
"""The portable crate builds on stable Rust (DESIGN §1.1).

`vibeos-core` enables no language or library feature: Verus (ROADMAP Phase 38),
Kani, loom, and Miri each pin their own toolchain, and they must keep building
the crate. Feature attributes are crate-level, so the crate root is the only
file to check. Nightly features stay in the kernel binary (`src/main.rs`).
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CORE_ROOT = ROOT / "src" / "lib.rs"

# `#![feature(..)]` or `#![cfg_attr(<cond>, feature(..))]`; `feature = "std"`
# inside a cfg predicate is a Cargo feature, not a language feature.
FEATURE_ATTR = re.compile(r"^\s*#!\[\s*(?:cfg_attr\s*\(.*,\s*)?feature\s*\(")


def find_features(text: str) -> list[int]:
    """1-based line numbers of crate-level feature attributes."""
    return [n for n, line in enumerate(text.splitlines(), start=1) if FEATURE_ATTR.match(line)]


def main() -> int:
    lines = find_features(CORE_ROOT.read_text(encoding="utf-8"))
    if lines:
        rel = CORE_ROOT.relative_to(ROOT)
        for n in lines:
            msg = f"{rel}:{n}: vibeos-core enables a nightly feature (DESIGN §1.1)"
            print(msg, file=sys.stderr)
        return 1
    print("check_core_stable: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

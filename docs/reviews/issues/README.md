# Review issues (2026-09-22)

One document per recommendation in [ARCHITECTURE_REVIEW.md](../ARCHITECTURE_REVIEW.md). Each has the observation with evidence, the recommended fix, a step-by-step implementation plan, acceptance criteria, tests, and risks. Tiers: **I** quick wins, **II** near term, **III** strategic. They are the review's priority buckets, which ARCHITECTURE_REVIEW §6 and each plan's header call Phase I to III; they are not ROADMAP phases, and ROADMAP Phase 10's waves set the order. Effort: S ≤ 1 day, M a few days, L 1–2 weeks.

**Phase numbers (2026-09-22).** The roadmap was restructured after this review: phases from 10 up were renumbered (old 10 → 12, 11 → 13, 12 → 14, 13 → 15, 14 → 16, 15 → 17, 16 → 18, 17 → 19, 18 → 20, 19 → 21, 20 → 22), and these items are tracked as [ROADMAP Phase 10: Consolidation](../../ROADMAP.md#phase-10-consolidation). Section numbers quoted in the issue documents were updated where an item is still open; the review itself keeps the old numbers.

**Precedence.** A plan is advice on how to do the work its ROADMAP boxes state; where they differ, the boxes decide (ROADMAP, How to read this). Each plan whose code is `proposed` or `in progress` opens with a `ROADMAP:` line that names its boxes and the steps they supersede.

| ID | Title | Impact | Effort | Tier | Depends on | Status |
|---|---|---|---|---|---|---|
| [A1](A1-directory-per-subsystem.md) | Directory per subsystem, matching a corrected module map | High | L | III | Q1, A2 | `proposed` |
| [A2](A2-portable-half-host-tests.md) | Make the portable half portable; host tests on any host | High | M | II | C1 | `in progress (#88)` |
| [A3](A3-vfs-single-dispatch.md) | Make the VFS the only file-operation dispatch point | High | L | III | Q3 | `proposed` |
| [A4](A4-break-module-cycles.md) | Break the mutual dependencies between kernel modules | Medium | M | III | A3, A1 | `proposed` |
| [Q1](Q1-fmt-clippy-warnings-gates.md) | Land the promised gates: rustfmt, clippy, `-D warnings` | High | S | I | — | `implemented (#80, #194)` |
| [Q2](Q2-isolate-test-hooks.md) | Isolate `kernel_tests` scaffolding from production modules | Medium | M | II | Q1 | `proposed` |
| [Q3](Q3-boot-cell-primitive.md) | One boot-cell primitive; retire `static mut` and `&'static mut` accessors | Medium | S+M | II | — | `implemented (#87)` |
| [Q4](Q4-naming-consistency.md) | Naming and feature-flag consistency | Low | S | II | — | `implemented (#83)` |
| [Q5](Q5-split-monolithic-files.md) | Split monolithic files; add a size guard | Medium | M | III | A1, T1 | `proposed` |
| [D1](D1-heap-backed-tables.md) | Heap-allocate the growable fixed-capacity tables (Phase 10) | High | L | III | Q3, A3 | `in progress (#194)` |
| [D2](D2-driver-instances.md) | Driver and volume instances instead of module singletons | Medium | L | III | A1, Q3 | `proposed` |
| [D3](D3-bootinfo.md) | Capture boot information once (`BootInfo`) | Low | S | I | Q3 | `implemented (#90, #91)` |
| [E1](E1-parser-robustness-lints.md) | Lock in parser robustness with restriction lints | Low | S | II | Q1 | `in progress (#86)` |
| [E2](E2-kerror-errno.md) | One `KError` (errno-shaped) ahead of the syscall boundary | Medium | M | III | A3 | `proposed` |
| [E3](E3-emit-paths.md) | Make the marker-vs-log rule explicit; one macro per intent | Low | S | II | — | `implemented (#83)` |
| [C1](C1-pin-toolchain-and-inputs.md) | Pin every external input: nightly date, action SHAs, Limine commit | High | S | I | — | `implemented (#77)` |
| [C2](C2-centralize-env-config.md) | Centralize `VIBEOS_*` environment handling in the harness | Low | S | I | T2 | `implemented (#79)` |
| [T1](T1-split-ktest-registry.md) | Split the in-guest registry; make failures self-diagnosing | Medium | M | II | Q2 | `proposed` |
| [T2](T2-single-qemu-launcher.md) | One QEMU launcher for all drivers | Medium | S | I | — | `implemented (#79)` |
| [T3](T3-fast-check-ci-job.md) | A fast `check` CI job ahead of the QEMU ladder; coverage floor | Medium | S | II | Q1, T2 | `implemented (#82)` |
| [T4](T4-fuzz-parsers.md) | Fuzz the pure parsers now | Medium | S | II | A2 | `proposed` |
| [P1](P1-build-multiplicity.md) | Cut the build multiplicity | Low | M | II | B1, B2 | `proposed` |
| [P2](P2-lock-hotspots-note.md) | Record the known single-lock hotspots as Phase 19 items | Low | S | II | — | `implemented (#83)` |
| [S1](S1-pre-ring3-hardening.md) | Finish the pre-ring-3 hardening checklist | Medium | S | II | — | `implemented (#85)` |
| [B1](B1-parametrize-makefile.md) | Parametrize the Makefile's ISO recipes; add `make check` | Medium | S | I | — | `implemented (#76)` |
| [B2](B2-builtin-target-spike.md) | Replace the custom target JSON with built-in `x86_64-unknown-none` | Medium | M | II | C1 | `implemented (#84)` |
| [B3](B3-tag-and-release.md) | Version, tag phases, and publish the ISO | Low | S | I | — | `implemented (#81)` |
| [B4](B4-build-rs-inputs.md) | Simplify `build.rs` inputs and the initrd path | Low | S–M | II | B1 | `implemented (#89)` |
| [I1](I1-macos-job-ovmf.md) | macOS CI job; find OVMF wherever it lives | Low | S | II | A2 | `proposed` |
| [DOC1](DOC1-top-of-funnel-docs.md) | README, DESIGN header, module map, MIT LICENSE | High | S | I | — | `implemented (#78)` |
| [DOC2](DOC2-split-design-doc.md) | Split `DESIGN.md`; as-built in tables, not prose | Medium | M | III | A1 (helpful) | `proposed` |
| [DOC3](DOC3-agents-md.md) | Version the agent instructions in the repo (`AGENTS.md`) | High | S | I | — | `implemented (#78)` |
| [DOC4](DOC4-changelog-style.md) | Keep `CHANGELOG.md` short and user-facing | Low | S | I | B3 | `in progress (#81)` |
| [DX1](DX1-make-check-and-lint-config.md) | A fast local gate (`make check`) and formatter/linter configuration | Medium | S | I | Q1, B1, A2 | `in progress (#76)` |
| [R1](R1-repo-tidy.md) | Tidy the root, `tests/`, and remote branches | Low | S | I | B1, T2 | `in progress (#78, #79)` |
| [O1](O1-phase9-resume-note.md) | Note the Phase 9 pause and resume; research notes deferred | Low | S | II | — | `superseded: Phase 9 did not pause, so there is no resume to note; research notes stay deferred` |

Suggested order for tier I (all independent unless noted): DOC1, DOC3, C1, Q1, B1, T2 → C2, DX1, B3 → DOC4, D3, R1.

**Status** in the table is the only status of a code; each plan's own status line points here. `proposed`: none of its work has landed. `in progress (#PR, …)`: some has, in those PRs, and an open ROADMAP box in Phases 0 to 10 still cites the code. `implemented (#PR, …)`: its work has landed and no open box there cites it. `declined: <reason>` and `superseded: <what replaced it>` close a code without its work. A box cites a code that opens an item of a parenthesized list on its line, as in `(Q1, F147)`. The status notes in ARCHITECTURE_REVIEW.md §6 are dated history. `scripts/check_issues.py` (ROADMAP Phase 10 exit gate) checks each cell against the boxes.

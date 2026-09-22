# Review issues (2026-09-22)

One document per recommendation in [ARCHITECTURE_REVIEW.md](../ARCHITECTURE_REVIEW.md). Each has the observation with evidence, the recommended fix, a step-by-step implementation plan, acceptance criteria, tests, and risks. Phases: **I** quick wins, **II** near term, **III** strategic (alongside Phase 9–10). Effort: S ≤ 1 day, M a few days, L 1–2 weeks.

| ID | Title | Impact | Effort | Phase | Depends on |
|---|---|---|---|---|---|
| [A1](A1-directory-per-subsystem.md) | Directory per subsystem, matching a corrected module map | High | L | III | Q1, A2 |
| [A2](A2-portable-half-host-tests.md) | Make the portable half portable; host tests on any host | High | M | II | C1 |
| [A3](A3-vfs-single-dispatch.md) | Make the VFS the only file-operation dispatch point | High | L | III | Q3 |
| [A4](A4-break-module-cycles.md) | Break the mutual dependencies between kernel modules | Medium | M | III | A3, A1 |
| [Q1](Q1-fmt-clippy-warnings-gates.md) | Land the promised gates: rustfmt, clippy, `-D warnings` | High | S | I | — |
| [Q2](Q2-isolate-test-hooks.md) | Isolate `kernel_tests` scaffolding from production modules | Medium | M | II | Q1 |
| [Q3](Q3-boot-cell-primitive.md) | One boot-cell primitive; retire `static mut` and `&'static mut` accessors | Medium | S+M | II | — |
| [Q4](Q4-naming-consistency.md) | Naming and feature-flag consistency | Low | S | II | — |
| [Q5](Q5-split-monolithic-files.md) | Split monolithic files; add a size guard | Medium | M | III | A1, T1 |
| [D1](D1-heap-backed-tables.md) | Heap-allocate the growable fixed-capacity tables before Phase 9 | High | L | III | Q3, A3 |
| [D2](D2-driver-instances.md) | Driver and volume instances instead of module singletons | Medium | L | III | A1, Q3 |
| [D3](D3-bootinfo.md) | Capture boot information once (`BootInfo`) | Low | S | I | Q3 |
| [E1](E1-parser-robustness-lints.md) | Lock in parser robustness with restriction lints | Low | S | II | Q1 |
| [E2](E2-kerror-errno.md) | One `KError` (errno-shaped) ahead of the syscall boundary | Medium | M | III | A3 |
| [E3](E3-emit-paths.md) | Make the marker-vs-log rule explicit; one macro per intent | Low | S | II | — |
| [C1](C1-pin-toolchain-and-inputs.md) | Pin every external input: nightly date, action SHAs, Limine commit | High | S | I | — |
| [C2](C2-centralize-env-config.md) | Centralize `VIBEOS_*` environment handling in the harness | Low | S | I | T2 |
| [T1](T1-split-ktest-registry.md) | Split the in-guest registry; make failures self-diagnosing | Medium | M | II | Q2 |
| [T2](T2-single-qemu-launcher.md) | One QEMU launcher for all drivers | Medium | S | I | — |
| [T3](T3-fast-check-ci-job.md) | A fast `check` CI job ahead of the QEMU ladder; coverage floor | Medium | S | II | Q1, T2 |
| [T4](T4-fuzz-parsers.md) | Fuzz the pure parsers now | Medium | S | II | A2 |
| [P1](P1-build-multiplicity.md) | Cut the build multiplicity | Low | M | II | B1, B2 |
| [P2](P2-lock-hotspots-note.md) | Record the known single-lock hotspots as Phase 17 items | Low | S | II | — |
| [S1](S1-pre-ring3-hardening.md) | Finish the pre-ring-3 hardening checklist | Medium | S | II | — |
| [B1](B1-parametrize-makefile.md) | Parametrize the Makefile's ISO recipes; add `make check` | Medium | S | I | — |
| [B2](B2-builtin-target-spike.md) | Replace the custom target JSON with built-in `x86_64-unknown-none` | Medium | M | II | C1 |
| [B3](B3-tag-and-release.md) | Version, tag phases, and publish the ISO | Low | S | I | — |
| [B4](B4-build-rs-inputs.md) | Simplify `build.rs` inputs and the initrd path | Low | S–M | II | B1 |
| [I1](I1-macos-job-ovmf.md) | macOS CI job; find OVMF wherever it lives | Low | S | II | A2 |
| [DOC1](DOC1-top-of-funnel-docs.md) | README, DESIGN header, module map, MIT LICENSE | High | S | I | — |
| [DOC2](DOC2-split-design-doc.md) | Split `DESIGN.md`; as-built in tables, not prose | Medium | M | III | A1 (helpful) |
| [DOC3](DOC3-agents-md.md) | Version the agent instructions in the repo (`AGENTS.md`) | High | S | I | — |
| [DOC4](DOC4-changelog-style.md) | Keep `CHANGELOG.md` short and user-facing | Low | S | I | B3 |
| [DX1](DX1-make-check-and-lint-config.md) | A fast local gate (`make check`) and formatter/linter configuration | Medium | S | I | Q1, B1, A2 |
| [R1](R1-repo-tidy.md) | Tidy the root, `tests/`, and remote branches | Low | S | I | B1, T2 |
| [O1](O1-phase9-resume-note.md) | Note the Phase 9 pause and resume; research notes deferred | Low | S | II | — |

Suggested order for Phase I (all independent unless noted): DOC1, DOC3, C1, Q1, B1, T2 → C2, DX1, B3 → DOC4, D3, R1.

**Status (2026-09-22).** DOC1 and DOC3 landed in the docs/meta PR. R1: merged `feature/*` branches pruned; ISO→`build/` waits on B1; Python driver moves wait on T2 (`tests/e2e/` already gone). **O1 superseded** — Phase 9 exit closed 2026-09-22; do not add a "Phase 9 resumed" note. Everything else still proposed.

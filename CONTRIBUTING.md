# Contributing

Code is written by agents. Humans review, file bugs, and run the kernel. Read [AGENTS.md](AGENTS.md) and [README.md](README.md) first.

PR checklist: `make test` green (or CI); new serial markers registered in the harness in the same commit; host tests for portable logic and a ktest for hardware; `CHANGELOG.md` entry (≤ 2 lines) for anything visible when running the kernel; DESIGN/ROADMAP updated if an invariant, constant, or checkbox changed; no `TODO` for a correctness gap.

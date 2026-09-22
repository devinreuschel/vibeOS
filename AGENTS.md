# Agent notes

Standing instructions for coding agents. Keep this short; details live in `docs/`.

## Toolchain bump (C1)

Bump the date in `rust-toolchain.toml` and the matching `toolchain:` inputs in
`.github/workflows/ci.yml` and `.github/workflows/smp-stress.yml` in one PR.
`make test` must be green. Do not re-introduce an undated nightly except the
weekly canary job in `smp-stress.yml`.

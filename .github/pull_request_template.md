## Summary

## Checklist

- [ ] CHANGELOG entry (≤ 2 lines) if user-visible
- [ ] every ROADMAP box this PR ticks has a `Proves: <test> -- <the box's first words>` line in the commit that ticks it, and a test the per-push tiers skip names its `[workflow]` (ROADMAP, How to read this)
- [ ] no box this PR ticks needs a box that is still open (`tests/gates/phase-<N>-needs.toml`)
- [ ] a `Gate-change:` trailer for each gate input this PR edits or removes and each expected-failure or skip entry it adds (ROADMAP §10.9)

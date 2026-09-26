//! Portable trap kinds and their ring-3 actions: each port decodes a trap into
//! a `TrapKind`, and one table gives each kind its signal and `si_code`, or
//! marks it as not a ring-3 fault (AGENTS.md rule 3, DESIGN §5.2 and §11.5).
//! Contents land with ROADMAP §10.6 (F005).

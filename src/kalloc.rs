//! Fallible heap types for paths reachable from untrusted input (AGENTS.md
//! rule 4, DESIGN §4.4): a syscall, a device, a disk image, a packet, or a
//! firmware table's bounds. `alloc`'s growing calls panic on failure; these
//! return an error instead. Contents land with ROADMAP §10.4 (F010).

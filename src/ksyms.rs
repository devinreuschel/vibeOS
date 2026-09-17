//! In-image symbol table. Filled by `scripts/gen_ksyms.py` via `build.rs`.

include!(concat!(env!("OUT_DIR"), "/ksyms.rs"));

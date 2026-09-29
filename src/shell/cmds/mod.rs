//! Kernel-shell commands, one file per subsystem. Each exports its
//! `COMMANDS`, which `shell_init::register_builtins` registers in `help`
//! order. The subsystems they report on do not know the shell.
#![cfg_attr(feature = "vibefs_crash", allow(dead_code))]

pub(crate) mod blk;
pub(crate) mod dev;
pub(crate) mod fs;
pub(crate) mod sys;

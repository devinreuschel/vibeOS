//! The initial process stack: argc, argv, envp and the auxiliary vector
//! (ROADMAP §10.5).
//!
//! At entry the stack pointer points at `argc`, then `argc` argument pointers
//! and a NULL, then the environment pointers and a NULL, then `(key, value)`
//! auxv pairs ending in `AT_NULL` (the psABI's initial process stack). The
//! strings are NUL-terminated and live for the whole process.

use core::ffi::{CStr, c_char};

// Auxiliary vector keys, from Linux `include/uapi/linux/auxvec.h`.
/// End of the auxiliary vector.
pub const AT_NULL: usize = 0;
/// Address of the program headers.
pub const AT_PHDR: usize = 3;
/// Size of one program header.
pub const AT_PHENT: usize = 4;
/// Number of program headers.
pub const AT_PHNUM: usize = 5;
/// The page size.
pub const AT_PAGESZ: usize = 6;
/// The interpreter's base address.
pub const AT_BASE: usize = 7;
/// Flags.
pub const AT_FLAGS: usize = 8;
/// The program's entry point.
pub const AT_ENTRY: usize = 9;
/// Real user id.
pub const AT_UID: usize = 11;
/// Effective user id.
pub const AT_EUID: usize = 12;
/// Real group id.
pub const AT_GID: usize = 13;
/// Effective group id.
pub const AT_EGID: usize = 14;
/// Clock ticks per second.
pub const AT_CLKTCK: usize = 17;
/// Secure mode.
pub const AT_SECURE: usize = 23;
/// Address of 16 random bytes.
pub const AT_RANDOM: usize = 25;
/// The executed file's name.
pub const AT_EXECFN: usize = 31;

/// The program's arguments, environment and auxiliary vector.
#[derive(Clone, Copy, Debug)]
pub struct Env {
    argc: usize,
    argv: *const *const c_char,
    envp: *const *const c_char,
    auxv: *const usize,
}

/// The bytes of the NUL-terminated string at `p`, without the NUL.
///
/// # Safety
///
/// `p` points to a NUL-terminated string that lives for the whole process.
unsafe fn cstr(p: *const c_char) -> &'static [u8] {
    // SAFETY: the contract of this function, stated in its `# Safety`
    // section, established here by its callers in `Env`.
    unsafe { CStr::from_ptr(p) }.to_bytes()
}

impl Env {
    /// Parse the initial stack at `sp`.
    ///
    /// # Safety
    ///
    /// `sp` points at `argc` of an initial process stack laid out as the
    /// module docs say, whose memory stays valid and unchanged for the rest
    /// of the process.
    pub unsafe fn from_stack(sp: *const usize) -> Env {
        // SAFETY: the layout this function's `# Safety` section states,
        // established here by its caller.
        unsafe {
            let argc = *sp;
            let argv = sp.add(1) as *const *const c_char;
            let envp = argv.add(argc + 1);
            let mut e = envp;
            while !(*e).is_null() {
                e = e.add(1);
            }
            let auxv = e.add(1) as *const usize;
            Env {
                argc,
                argv,
                envp,
                auxv,
            }
        }
    }

    /// The number of arguments.
    pub fn argc(&self) -> usize {
        self.argc
    }

    /// Argument `i`, or `None` past the last.
    pub fn arg(&self, i: usize) -> Option<&'static [u8]> {
        if i >= self.argc {
            return None;
        }
        // SAFETY: `i < argc`, so `argv[i]` is an argument string of the
        // initial stack, whose layout `from_stack` checked in, established
        // here by the `Env` it built.
        Some(unsafe { cstr(*self.argv.add(i)) })
    }

    /// The arguments, in order.
    pub fn args(&self) -> impl Iterator<Item = &'static [u8]> + '_ {
        (0..self.argc).filter_map(|i| self.arg(i))
    }

    /// The environment strings (`NAME=value`), in order.
    pub fn vars(&self) -> impl Iterator<Item = &'static [u8]> + '_ {
        let mut p = self.envp;
        core::iter::from_fn(move || {
            // SAFETY: the environment array ends in a NULL, which stops the
            // walk: the initial stack's layout, established here by the `Env`
            // `from_stack` built.
            unsafe {
                let s = *p;
                if s.is_null() {
                    return None;
                }
                p = p.add(1);
                Some(cstr(s))
            }
        })
    }

    /// The value of the first environment variable called `name`.
    pub fn var(&self, name: &[u8]) -> Option<&'static [u8]> {
        self.vars().find_map(|v| {
            let rest = v.strip_prefix(name)?;
            rest.strip_prefix(b"=")
        })
    }

    /// The value of auxv entry `key`, or `None` when the vector has none.
    pub fn aux(&self, key: usize) -> Option<usize> {
        let mut p = self.auxv;
        loop {
            // SAFETY: the auxiliary vector is `(key, value)` pairs ending in
            // `AT_NULL`, which stops the walk: the initial stack's layout,
            // established here by the `Env` `from_stack` built.
            let (k, v) = unsafe { (*p, *p.add(1)) };
            if k == AT_NULL {
                return None;
            }
            if k == key {
                return Some(v);
            }
            // SAFETY: the pair was not `AT_NULL`, so another follows, by the
            // layout established here as above.
            p = unsafe { p.add(2) };
        }
    }
}

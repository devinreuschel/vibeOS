//! One `execve`'s arguments and the initial stack they go on (ROADMAP
//! §10.5): the argument block, with Linux's limits as execve(2) states
//! them, and the stack's layout. The kernel half copies the strings in
//! and writes the result to the new image.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use crate::kalloc::TryVec;
use crate::limits::{ARG_SPACE_MAX, ARG_SPACE_MIN, MAX_ARG_STRLEN};

pub const AT_NULL: u64 = 0;
pub const AT_PHDR: u64 = 3;
pub const AT_PHENT: u64 = 4;
pub const AT_PHNUM: u64 = 5;
pub const AT_PAGESZ: u64 = 6;
pub const AT_BASE: u64 = 7;
pub const AT_FLAGS: u64 = 8;
pub const AT_ENTRY: u64 = 9;
pub const AT_UID: u64 = 11;
pub const AT_EUID: u64 = 12;
pub const AT_GID: u64 = 13;
pub const AT_EGID: u64 = 14;
pub const AT_CLKTCK: u64 = 17;
pub const AT_SECURE: u64 = 23;
pub const AT_RANDOM: u64 = 25;
pub const AT_EXECFN: u64 = 31;

/// One auxiliary-vector entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Auxv {
    pub tag: u64,
    pub val: u64,
}

/// Why an `execve`'s arguments were refused: `TooBig` is `E2BIG`, `NoMem`
/// is `ENOMEM` (SYSCALL.md §3.1).
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgError {
    /// A string of [`MAX_ARG_STRLEN`] bytes or more before its NUL, or
    /// strings and pointers together over the block's limit.
    TooBig,
    /// The block's buffer could not grow.
    NoMem,
}

/// An over-limit argument is `E2BIG`; a failed allocation, `ENOMEM`.
impl From<ArgError> for crate::kerror::KError {
    fn from(e: ArgError) -> Self {
        match e {
            ArgError::TooBig => Self::TooBig,
            ArgError::NoMem => Self::NoMem,
        }
    }
}

/// The bytes `argv` and `envp` may take together, strings with their NULs
/// and pointers: max(128 KiB, min(`rlimit_stack` / 4, 6 MiB)), as
/// execve(2) states it; 2 MiB at the default 8 MiB `RLIMIT_STACK`.
pub fn arg_space_limit(rlimit_stack: u64) -> usize {
    let quarter = usize::try_from(rlimit_stack / 4).unwrap_or(usize::MAX);
    quarter.clamp(ARG_SPACE_MIN, ARG_SPACE_MAX)
}

/// One `execve`'s arguments: `argv`'s strings, then `envp`'s, each with
/// its NUL, in one [`TryVec`] with no count cap. Each string costs its
/// bytes, its NUL and an 8-byte pointer against the limit, as execve(2)
/// counts them. A failed call leaves the block as it was, apart from a
/// string it began, and the caller drops the block on any error.
pub struct ExecArgs {
    strings: TryVec<u8>,
    argc: usize,
    envc: usize,
    /// Bytes counted so far: strings, NULs and pointers.
    used: usize,
    limit: usize,
    /// Bytes of the string being built, `None` between strings.
    cur: Option<usize>,
    /// Whether the string being built is an environment string.
    cur_env: bool,
    /// [`ExecArgs::finish_argv`] has run: only `envp` strings follow.
    argv_done: bool,
}

impl ExecArgs {
    /// An empty block whose strings and pointers may take `limit` bytes.
    pub fn new(limit: usize) -> ExecArgs {
        ExecArgs {
            strings: TryVec::new(),
            argc: 0,
            envc: 0,
            used: 0,
            limit,
            cur: None,
            cur_env: false,
            argv_done: false,
        }
    }

    /// Count `n` more bytes against the limit.
    fn charge(&self, n: usize) -> Result<usize, ArgError> {
        self.used
            .checked_add(n)
            .filter(|&u| u <= self.limit)
            .ok_or(ArgError::TooBig)
    }

    /// Start a string: its 8-byte pointer counts now.
    pub fn begin(&mut self, env: bool) -> Result<(), ArgError> {
        debug_assert!(self.cur.is_none(), "ExecArgs::begin inside a string");
        debug_assert!(env || !self.argv_done, "an argv string after finish_argv");
        self.used = self.charge(8)?;
        self.cur = Some(0);
        self.cur_env = env;
        Ok(())
    }

    /// Append `part` to the string begun last. `TooBig` once the string
    /// reaches [`MAX_ARG_STRLEN`] bytes without its NUL, or the block its
    /// limit.
    pub fn extend(&mut self, part: &[u8]) -> Result<(), ArgError> {
        debug_assert!(self.cur.is_some(), "ExecArgs::extend outside a string");
        let cur = self.cur.unwrap_or(0);
        let len = cur
            .checked_add(part.len())
            .filter(|&l| l < MAX_ARG_STRLEN)
            .ok_or(ArgError::TooBig)?;
        let used = self.charge(part.len())?;
        self.strings
            .try_extend_from_slice(part)
            .map_err(|_| ArgError::NoMem)?;
        self.used = used;
        self.cur = Some(len);
        Ok(())
    }

    /// End the string begun last with its NUL.
    pub fn end(&mut self) -> Result<(), ArgError> {
        let used = self.charge(1)?;
        self.strings.try_push(0).map_err(|_| ArgError::NoMem)?;
        self.used = used;
        self.cur = None;
        if self.cur_env {
            self.envc = self.envc.saturating_add(1);
        } else {
            self.argc = self.argc.saturating_add(1);
        }
        Ok(())
    }

    fn push(&mut self, env: bool, s: &[u8]) -> Result<(), ArgError> {
        self.begin(env)?;
        self.extend(s)?;
        self.end()
    }

    /// Append the `argv` string `s`.
    pub fn push_arg(&mut self, s: &[u8]) -> Result<(), ArgError> {
        self.push(false, s)
    }

    /// Append the `envp` string `s`.
    pub fn push_env(&mut self, s: &[u8]) -> Result<(), ArgError> {
        self.push(true, s)
    }

    /// End `argv`: an empty one becomes `[""]`, `argc` 1 with an empty
    /// `argv[0]`, as on Linux. Only `envp` strings follow.
    pub fn finish_argv(&mut self) -> Result<(), ArgError> {
        if self.argc == 0 {
            self.push_arg(b"")?;
        }
        self.argv_done = true;
        Ok(())
    }

    pub fn argc(&self) -> usize {
        self.argc
    }

    pub fn envc(&self) -> usize {
        self.envc
    }

    /// Every string with its NUL: `argv`'s, then `envp`'s.
    pub fn strings(&self) -> &[u8] {
        &self.strings
    }
}

/// Where [`build_initial_stack`] put things: the user RSP, which starts
/// the table, and the address of [`ExecArgs::strings`], where the table
/// ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StackImage {
    pub rsp: u64,
    pub strings_va: u64,
}

/// Bytes of the table [`build_initial_stack`] writes at RSP: `argc`, the
/// `argv` pointers and NULL, the `envp` pointers and NULL, `naux` auxv
/// pairs, `AT_RANDOM` and `AT_NULL`.
fn table_words(args: &ExecArgs, naux: usize) -> Option<usize> {
    let ptrs = args.argc.checked_add(args.envc)?.checked_add(3)?;
    naux.checked_add(2)?.checked_mul(2)?.checked_add(ptrs)
}

fn align16(x: usize) -> Option<usize> {
    Some(x.checked_add(15)? & !15)
}

/// The initial stack's layout below a 16-aligned top, as offsets down
/// from it: where the strings start, the random bytes, and RSP.
fn stack_offsets(args: &ExecArgs, naux: usize) -> Option<(usize, usize, usize)> {
    let strings = args.strings.len().checked_add(8)?;
    let random = align16(strings.checked_add(16)?)?;
    let table = table_words(args, naux)?.checked_mul(8)?;
    let rsp = align16(random.checked_add(table)?)?;
    Some((strings, random, rsp))
}

/// Bytes from the initial RSP to the stack's top for `args` and `naux`
/// auxv pairs: what the stack must hold before any headroom. `None` on
/// overflow.
pub fn initial_stack_len(args: &ExecArgs, naux: usize) -> Option<usize> {
    stack_offsets(args, naux).map(|(_, _, rsp)| rsp)
}

/// `elf::build_initial_stack`'s layout; `None` where it fails with
/// `ElfError::Stack`.
pub(super) fn build(
    stack_top: u64,
    args: &ExecArgs,
    aux: &[Auxv],
    random: &[u8; 16],
    table: &mut [u8],
) -> Option<StackImage> {
    debug_assert!(args.cur.is_none(), "an unfinished ExecArgs string");
    if stack_top & 15 != 0 {
        return None;
    }
    let (strings_off, random_off, rsp_off) = stack_offsets(args, aux.len())?;
    let at = |off: usize| {
        u64::try_from(off)
            .ok()
            .and_then(|o| stack_top.checked_sub(o))
    };
    let strings_va = at(strings_off)?;
    let random_va = at(random_off)?;
    let rsp = at(rsp_off)?;
    let table_len = rsp_off.checked_sub(strings_off)?;
    if table.len() != table_len {
        return None;
    }
    table.fill(0);

    fn poke_u64(mem: &mut [u8], off: &mut usize, v: u64) -> Option<()> {
        let e = off.checked_add(8)?;
        mem.get_mut(*off..e)?.copy_from_slice(&v.to_le_bytes());
        *off = e;
        Some(())
    }

    // The random bytes, at their offset from RSP.
    let r = rsp_off.checked_sub(random_off)?;
    let r_end = r.checked_add(random.len())?;
    table.get_mut(r..r_end)?.copy_from_slice(random);

    let mut o = 0usize;
    poke_u64(table, &mut o, args.argc as u64)?;
    // Each string's address, in order: `argc` of `argv`'s, a NULL, then
    // `envp`'s and a NULL.
    if args.argc == 0 {
        // Every `ExecArgs` past `finish_argv` has an `argv[0]`; the table
        // keeps its shape without one.
        poke_u64(table, &mut o, 0)?;
    }
    let mut start = 0usize;
    let mut n = 0usize;
    for (i, &b) in args.strings().iter().enumerate() {
        if b != 0 {
            continue;
        }
        let va = strings_va.checked_add(start as u64)?;
        poke_u64(table, &mut o, va)?;
        n = n.checked_add(1)?;
        if n == args.argc {
            poke_u64(table, &mut o, 0)?;
        }
        start = i.checked_add(1)?;
    }
    if n != args.argc.checked_add(args.envc)? {
        return None;
    }
    poke_u64(table, &mut o, 0)?;
    for a in aux {
        poke_u64(table, &mut o, a.tag)?;
        poke_u64(table, &mut o, a.val)?;
    }
    poke_u64(table, &mut o, AT_RANDOM)?;
    poke_u64(table, &mut o, random_va)?;
    poke_u64(table, &mut o, AT_NULL)?;
    poke_u64(table, &mut o, 0)?;
    Some(StackImage { rsp, strings_va })
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "host tests: the module deny overrides the crate root's cfg(test) allow, and a failing index ends the test"
)]
mod tests {
    use super::*;
    use crate::elf::{ElfError, build_initial_stack};

    /// `args` with `argv` then `envp`, at the 2 MiB default limit.
    fn exec_args(argv: &[&[u8]], envp: &[&[u8]]) -> ExecArgs {
        let mut a = ExecArgs::new(arg_space_limit(crate::limits::RLIMIT_STACK_DEFAULT));
        for s in argv {
            a.push_arg(s).unwrap();
        }
        a.finish_argv().unwrap();
        for s in envp {
            a.push_env(s).unwrap();
        }
        a
    }

    /// The stack `build_initial_stack` lays out below `top`, as the
    /// kernel writes it: `[rsp, top)` with the table at RSP and the
    /// strings at `strings_va`.
    fn lay_out(top: u64, args: &ExecArgs, aux: &[Auxv]) -> (StackImage, Vec<u8>) {
        let len = initial_stack_len(args, aux.len()).unwrap();
        let mut table = vec![0xAAu8; len - 8 - args.strings().len()];
        let st = build_initial_stack(top, args, aux, &[0x11; 16], &mut table).unwrap();
        assert_eq!(top - st.rsp, len as u64);
        assert_eq!(st.strings_va - st.rsp, table.len() as u64);
        let mut mem = table;
        mem.extend_from_slice(args.strings());
        mem.extend_from_slice(&[0; 8]);
        (st, mem)
    }

    fn word(mem: &[u8], off: usize) -> u64 {
        u64::from_le_bytes(mem[off..off + 8].try_into().unwrap())
    }

    /// The NUL-terminated string at user address `va` of a stack laid
    /// out from `rsp`.
    fn cstr(mem: &[u8], rsp: u64, va: u64) -> &[u8] {
        let o = (va - rsp) as usize;
        let n = mem[o..].iter().position(|&b| b == 0).unwrap();
        &mem[o..o + n]
    }

    /// `argc`, then `argv`, then `envp`, read back through the table's
    /// pointers.
    fn read_back(mem: &[u8], rsp: u64) -> (Vec<Vec<u8>>, Vec<Vec<u8>>, usize) {
        let argc = word(mem, 0) as usize;
        let mut o = 8;
        let mut argv = Vec::new();
        while word(mem, o) != 0 {
            argv.push(cstr(mem, rsp, word(mem, o)).to_vec());
            o += 8;
        }
        o += 8;
        let mut envp = Vec::new();
        while word(mem, o) != 0 {
            envp.push(cstr(mem, rsp, word(mem, o)).to_vec());
            o += 8;
        }
        (argv, envp, argc)
    }

    #[test]
    fn stack_argv_auxv() {
        let top = 0x0000_0000_8000_0000u64;
        let aux = [
            Auxv {
                tag: AT_PAGESZ,
                val: 4096,
            },
            Auxv {
                tag: AT_ENTRY,
                val: 0x4000_0000,
            },
        ];
        let args = exec_args(&[b"/hello"], &[]);
        let (st, mem) = lay_out(top, &args, &aux);
        assert_eq!(st.rsp & 0xf, 0);
        let (argv, envp, argc) = read_back(&mem, st.rsp);
        assert_eq!(argc, 1);
        assert_eq!(argv, [b"/hello".to_vec()]);
        assert!(envp.is_empty());
        // argc, argv[0], NULL, NULL, then the auxv pairs.
        assert_eq!(word(&mem, 32), AT_PAGESZ);
        assert_eq!(word(&mem, 40), 4096);
        assert_eq!(word(&mem, 48), AT_ENTRY);
        assert_eq!(word(&mem, 64), AT_RANDOM);
        let rnd = (word(&mem, 72) - st.rsp) as usize;
        assert_eq!(rnd & 0xf, 0);
        assert_eq!(&mem[rnd..rnd + 16], &[0x11; 16]);
        assert_eq!(word(&mem, 80), AT_NULL);
        assert_eq!(&mem[mem.len() - 8..], &[0; 8]);
    }

    #[test]
    fn build_initial_stack_envp() {
        let top = 0x0000_0000_8000_0000u64;
        let args = exec_args(&[b"/bin/envcheck", b"x"], &[b"K=v", b"PATH=/bin", b""]);
        assert_eq!((args.argc(), args.envc()), (2, 3));
        let (st, mem) = lay_out(top, &args, &[]);
        let (argv, envp, argc) = read_back(&mem, st.rsp);
        assert_eq!(argc, 2);
        assert_eq!(argv, [b"/bin/envcheck".to_vec(), b"x".to_vec()]);
        assert_eq!(envp, [b"K=v".to_vec(), b"PATH=/bin".to_vec(), Vec::new()]);
        // Every pointer lands in the strings, above the table.
        for o in (8..8 * 8).step_by(8) {
            let p = word(&mem, o);
            assert!(p == 0 || (p >= st.strings_va && p < top - 8));
        }
    }

    #[test]
    fn build_initial_stack_many_args() {
        let one: [&[u8]; 1] = [b"a"];
        let argv: Vec<&[u8]> = one.iter().copied().cycle().take(10_000).collect();
        let args = exec_args(&argv, &[b"K=v"]);
        let top = 0x0000_0000_8000_0000u64;
        let (st, mem) = lay_out(top, &args, &[]);
        let (argv, envp, argc) = read_back(&mem, st.rsp);
        assert_eq!(argc, 10_000);
        assert_eq!(argv.len(), 10_000);
        assert!(argv.iter().all(|a| a == b"a"));
        assert_eq!(envp, [b"K=v".to_vec()]);
    }

    #[test]
    fn build_initial_stack_empty_argv() {
        let args = exec_args(&[], &[b"ARGCHECK=empty"]);
        assert_eq!(args.argc(), 1);
        let (st, mem) = lay_out(0x7000_0000, &args, &[]);
        let (argv, envp, argc) = read_back(&mem, st.rsp);
        assert_eq!(argc, 1);
        assert_eq!(argv, [Vec::<u8>::new()]);
        assert_eq!(envp, [b"ARGCHECK=empty".to_vec()]);
        // The empty `argv[0]` counts in the limit: a pointer and a NUL.
        assert_eq!(args.used, 9 + 8 + 15);
    }

    #[test]
    fn execve_arg_limits() {
        assert_eq!(arg_space_limit(8 << 20), 2 << 20);
        assert_eq!(arg_space_limit(256 << 10), 128 << 10);
        assert_eq!(arg_space_limit(0), 128 << 10);
        assert_eq!(arg_space_limit(64 << 20), 6 << 20);
        assert_eq!(arg_space_limit(u64::MAX), 6 << 20);
        let limit = arg_space_limit(crate::limits::RLIMIT_STACK_DEFAULT);
        let big = vec![b'x'; MAX_ARG_STRLEN];
        // 131,071 bytes and a NUL fit; 131,072 bytes do not, in one part
        // or in several.
        let mut a = ExecArgs::new(limit);
        a.push_arg(&big[..MAX_ARG_STRLEN - 1]).unwrap();
        assert_eq!(a.push_arg(&big), Err(ArgError::TooBig));
        let mut a = ExecArgs::new(limit);
        a.begin(true).unwrap();
        a.extend(&big[..MAX_ARG_STRLEN - 1]).unwrap();
        assert_eq!(a.extend(b"y"), Err(ArgError::TooBig));
        // Exactly at the limit, then one byte over: 15 strings of
        // 131,071 bytes (each 131,080 with its NUL and pointer), then one
        // that takes what is left.
        let each = MAX_ARG_STRLEN + 8;
        let last = limit - 15 * each - 9;
        let mut a = ExecArgs::new(limit);
        for _ in 0..15 {
            a.push_arg(&big[..MAX_ARG_STRLEN - 1]).unwrap();
        }
        let mut b = ExecArgs::new(limit);
        for _ in 0..15 {
            b.push_arg(&big[..MAX_ARG_STRLEN - 1]).unwrap();
        }
        a.push_arg(&big[..last]).unwrap();
        assert_eq!(a.used, limit);
        assert_eq!(a.push_env(b""), Err(ArgError::TooBig));
        assert_eq!(b.push_arg(&big[..last + 1]), Err(ArgError::TooBig));
        // Pointers count: 128 KiB of empty strings is 14,563 of them.
        let mut a = ExecArgs::new(128 << 10);
        let mut n = 0usize;
        while a.push_env(b"").is_ok() {
            n += 1;
        }
        assert_eq!(n, (128 << 10) / 9);
    }

    #[test]
    fn exec_args_nomem() {
        let mut a = exec_args(&[b"/bin/argcheck", b"one"], &[]);
        let before = (a.strings().to_vec(), a.argc(), a.envc(), a.used);
        // A string longer than any capacity reserved so far must grow the
        // buffer; the counting allocator fails that growth.
        let long = vec![b'q'; 4096];
        a.begin(true).unwrap();
        let used = a.used;
        crate::kalloc::tests::fail_in(0);
        let r = a.extend(&long);
        crate::kalloc::tests::disarm();
        assert_eq!(r, Err(ArgError::NoMem));
        assert_eq!(a.strings(), &before.0[..]);
        assert_eq!((a.argc(), a.envc(), a.used), (before.1, before.2, used));
        crate::kalloc::tests::fail_in(0);
        let r = ExecArgs::new(1 << 20).push_arg(b"x");
        crate::kalloc::tests::disarm();
        assert_eq!(r, Err(ArgError::NoMem));
        assert_eq!(
            crate::kerror::KError::from(ArgError::NoMem),
            crate::kerror::KError::NoMem
        );
        assert_eq!(
            crate::kerror::KError::from(ArgError::TooBig),
            crate::kerror::KError::TooBig
        );
    }

    /// A stack whose top is below its own length has no user VA for its
    /// base; `base + off` used to overflow and panic (ROADMAP §10.1, E1).
    #[test]
    fn elf_rejects_stack_top_below_len() {
        let args = exec_args(&[b"/hello"], &[]);
        let len = initial_stack_len(&args, 0).unwrap();
        let mut table = vec![0u8; len - 8 - args.strings().len()];
        let got = build_initial_stack(0x10, &args, &[], &[0; 16], &mut table);
        assert_eq!(got, Err(ElfError::Stack));
        let top = 0x7000_0000u64;
        assert!(build_initial_stack(top, &args, &[], &[0; 16], &mut table).is_ok());
        // A top that is not 16-aligned is refused.
        let got = build_initial_stack(top - 8, &args, &[], &[0; 16], &mut table);
        assert_eq!(got, Err(ElfError::Stack));
    }

    /// A table of any length but the layout's is refused, never indexed
    /// out of bounds.
    #[test]
    fn elf_rejects_stack_too_small() {
        let top = 0x7000_0000u64;
        let args = exec_args(&[b"/hello"], &[b"A=B"]);
        let want = initial_stack_len(&args, 0).unwrap() - 8 - args.strings().len();
        for len in [0usize, 1, 7, 8, 16, 24, 64, want - 1, want + 1] {
            let mut table = std::vec![0u8; len];
            let got = build_initial_stack(top, &args, &[], &[0; 16], &mut table);
            assert_eq!(got, Err(ElfError::Stack), "len {len}");
        }
    }
}

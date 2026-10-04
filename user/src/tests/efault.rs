//! `efault_matrix` (ROADMAP §10.5, §P9, F077): every pointer argument the
//! generated `sys::CALLS` declares, given each bad form that applies, with
//! every other argument valid, returns `-EFAULT`, and the run going on
//! shows the kernel stayed up.
//!
//! The forms: a kernel-half address; a page that was mapped and is not; a
//! range crossing `USER_MAP_END`, the page below it mapped; one crossing
//! `USER_END`; a read-only page, for a pointer the kernel writes; a length
//! of 2^63, for a buffer whose length is 64 bits wide; and NULL, where the
//! row does not allow it. A C string runs into the boundary, and a vector's
//! first element is valid with the next one past it; a vector also gets a
//! kernel-half element and an unmapped one. `nanosleep`'s `rmtp` is not
//! declared (Linux writes it only on `EINTR`; SYSCALL.md §3.1), so it has
//! no case.
//!
//! `read` reads `/hello` from offset 0, so a copy happens. Each `wait4`
//! case reaps its own zombie child, which is gone after the `EFAULT`. Each
//! `execve` case runs in a child that exits `100 + errno`.

use vibeos_user::sys::{self, CALLS, Call, Errno, Ptr, PtrKind, Sys, USER_END, USER_MAP_END};
use vibeos_user::utest::{self, Got, Outcome, Runner};

use super::errno::{
    Failures, HELLO, KERNEL_PTR, PAGE, PROT_READ, PROT_WRITE, SCRATCH, child_result, close, map,
    map_at, open, poll, raw, unmap, unmapped_page,
};

const NAME: &str = "efault_matrix";

/// The pointer arguments ROADMAP §P9 and §10.5 name: the table must
/// declare each.
const REQUIRED: &[(Sys, &str)] = &[
    (Sys::Read, "buf"),
    (Sys::Write, "buf"),
    (Sys::Open, "pathname"),
    (Sys::Execve, "pathname"),
    (Sys::Execve, "argv"),
    (Sys::Execve, "envp"),
    (Sys::Wait4, "wstatus"),
    (Sys::Psinfo, "buf"),
    (Sys::Getdents64, "dirent"),
    (Sys::Fstat, "statbuf"),
    (Sys::Nanosleep, "rqtp"),
];

/// The calls whose buffer length is a 32-bit `unsigned int`: 2^63 does
/// not reach the kernel, so they get no huge-length form.
const NARROW_LEN: &[Sys] = &[Sys::Getdents64];

// From reboot(2).
const REBOOT_MAGIC1: u64 = 0xfee1_dead;
const REBOOT_MAGIC2: u64 = 0x2812_1969;
const REBOOT_CMD_RESTART2: u64 = 0xa1b2_c3d4;

/// The case's deadline: about 3x its run under TCG at `-smp 2` (under a
/// second), rounded up to 5 s, and at least 10 s.
const DEADLINE_MS: u32 = 10_000;

pub fn run(t: &mut Runner) {
    t.case_ms(NAME, DEADLINE_MS, matrix);
}

/// A bad form of one pointer argument.
#[derive(Clone, Copy, Debug)]
enum Form {
    Kernel,
    Unmapped,
    MapEnd,
    UserEnd,
    ReadOnly,
    HugeLen,
    Null,
    /// A vector whose first element is kernel-half.
    ElemKernel,
    /// A vector whose first element is unmapped.
    ElemUnmapped,
}

const FORMS: [Form; 9] = [
    Form::Kernel,
    Form::Unmapped,
    Form::MapEnd,
    Form::UserEnd,
    Form::ReadOnly,
    Form::HugeLen,
    Form::Null,
    Form::ElemKernel,
    Form::ElemUnmapped,
];

/// The memory every case borrows: a scratch page, the page below
/// `USER_MAP_END`, a read-only page and an unmapped one.
struct Pages {
    scratch: u64,
    top: u64,
    ro: u64,
    gone: u64,
}

// Where the scratch page holds what the valid arguments point at.
const PATH_AT: u64 = 0;
const ARGV_AT: u64 = 64;
const TS_AT: u64 = 128;
const BAD_ARGV_AT: u64 = 192;
const BUF_AT: u64 = 1024;

impl Pages {
    fn new() -> Result<Pages, &'static str> {
        let scratch = map(PAGE, PROT_READ | PROT_WRITE)?;
        let top = map_at(USER_MAP_END - PAGE, PAGE, PROT_READ | PROT_WRITE)?;
        let ro = map(PAGE, PROT_READ)?;
        let gone = unmapped_page()?;
        let p = Pages {
            scratch,
            top,
            ro,
            gone,
        };
        p.put(scratch + PATH_AT, b"/hello\0");
        p.put_u64(scratch + ARGV_AT, scratch + PATH_AT);
        p.put_u64(scratch + ARGV_AT + 8, 0);
        p.put_u64(scratch + TS_AT, 0);
        p.put_u64(scratch + TS_AT + 8, 1000);
        // The read-only page must be present, so its fault comes from W.
        // SAFETY: `ro` is a page this fn mapped readable, which nothing
        // references; established here.
        let _byte =
            unsafe { core::ptr::with_exposed_provenance::<u8>(ro as usize).read_volatile() };
        Ok(p)
    }

    /// Write `bytes` at `at`, inside the scratch or the top page.
    fn put(&self, at: u64, bytes: &[u8]) {
        let p = core::ptr::with_exposed_provenance_mut::<u8>(at as usize);
        // SAFETY: every caller passes an address inside `scratch` or `top`
        // with `bytes` fitting before the page's end; both are pages
        // `Pages::new` mapped writable that no reference covers;
        // established here.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len()) };
    }

    fn put_u64(&self, at: u64, v: u64) {
        self.put(at, &v.to_le_bytes());
    }

    fn drop_all(&self) {
        unmap(self.scratch, PAGE);
        unmap(self.top, PAGE);
        unmap(self.ro, PAGE);
    }
}

fn matrix() -> Outcome {
    let mut f = Failures::new(NAME);
    for &(sys, arg) in REQUIRED {
        let declared = CALLS
            .iter()
            .any(|c| c.sys == sys && c.ptrs.iter().any(|p| p.name == arg));
        if !declared {
            f.add(format_args!(
                "{}.{arg}: the table declares no pointer",
                sys.name()
            ));
        }
    }
    let pages = match Pages::new() {
        Ok(p) => p,
        Err(why) => return utest::fail(format_args!("setup: {why}")),
    };
    let mut cases = 0u32;
    for call in CALLS {
        for ptr in call.ptrs {
            for form in FORMS {
                if !applies(call, ptr, form) {
                    continue;
                }
                cases += 1;
                let name = call.sys.name();
                match case(&pages, call, ptr, form) {
                    Ok(Err(Errno::EFAULT)) => {}
                    Ok(r) => f.add(format_args!("{name}.{} {form:?}: got {}", ptr.name, Got(r))),
                    Err(why) => f.add(format_args!("{name}.{} {form:?}: {why}", ptr.name)),
                }
            }
        }
    }
    pages.drop_all();
    utest::info(NAME, format_args!("{cases} cases"));
    f.outcome()
}

/// Whether `form` is a case for `ptr`.
fn applies(call: &Call, ptr: &Ptr, form: Form) -> bool {
    match form {
        Form::Kernel | Form::Unmapped | Form::MapEnd | Form::UserEnd => true,
        Form::ReadOnly => ptr.out,
        Form::HugeLen => ptr.kind == PtrKind::Buf && !NARROW_LEN.contains(&call.sys),
        Form::Null => !ptr.nullable,
        Form::ElemKernel | Form::ElemUnmapped => ptr.kind == PtrKind::Strvec,
    }
}

/// A crossing buffer's length: 8 bytes before the boundary and the rest
/// past it, room enough for `getdents64`'s first record, which it checks
/// before the copy.
const CROSS_LEN: u64 = 512;

/// How many bytes before a boundary a crossing range starts: half a fixed
/// value, else 8.
fn before_end(ptr: &Ptr) -> u64 {
    if ptr.kind == PtrKind::Fixed {
        (ptr.size as u64 / 2).clamp(1, 8)
    } else {
        8
    }
}

/// The bad value of `ptr` for `form`, and its length when the form sets
/// one. Fills the top page or the scratch page as the form needs.
fn bad_value(p: &Pages, ptr: &Ptr, form: Form) -> (u64, Option<u64>) {
    let k = before_end(ptr);
    match form {
        Form::Kernel => (KERNEL_PTR, None),
        Form::Unmapped => (p.gone, None),
        Form::MapEnd => {
            let at = USER_MAP_END - k;
            match ptr.kind {
                // A string with no NUL before the boundary.
                PtrKind::Cstr => p.put(at, &[b'a'; 8]),
                // A valid first element, the next one past the boundary.
                PtrKind::Strvec => p.put_u64(at, p.scratch + PATH_AT),
                PtrKind::Buf | PtrKind::Fixed => {}
            }
            (at, Some(CROSS_LEN))
        }
        Form::UserEnd => (USER_END - k, Some(CROSS_LEN)),
        Form::ReadOnly => (p.ro, None),
        Form::HugeLen => (p.scratch + BUF_AT, Some(1 << 63)),
        Form::Null => (0, None),
        Form::ElemKernel | Form::ElemUnmapped => {
            let elem = if matches!(form, Form::ElemKernel) {
                KERNEL_PTR
            } else {
                p.gone
            };
            p.put_u64(p.scratch + BAD_ARGV_AT, elem);
            p.put_u64(p.scratch + BAD_ARGV_AT + 8, 0);
            (p.scratch + BAD_ARGV_AT, None)
        }
    }
}

/// The call's valid arguments, and the descriptor they opened, if any.
fn valid_args(p: &Pages, sys: Sys) -> Result<([u64; 6], Option<u32>), &'static str> {
    let buf = p.scratch + BUF_AT;
    let path = p.scratch + PATH_AT;
    Ok(match sys {
        Sys::Read => {
            let fd = open(HELLO, sys::O_RDONLY).map_err(|_| "open /hello")?;
            ([u64::from(fd), buf, 16, 0, 0, 0], Some(fd))
        }
        Sys::Write => {
            let flags = sys::O_CREAT | sys::O_TRUNC | sys::O_WRONLY;
            let fd = open(SCRATCH, flags).map_err(|_| "create")?;
            ([u64::from(fd), buf, 16, 0, 0, 0], Some(fd))
        }
        Sys::Open => ([path, 0, 0, 0, 0, 0], None),
        Sys::Fstat => ([1, buf, 0, 0, 0, 0], None),
        Sys::Nanosleep => ([p.scratch + TS_AT, 0, 0, 0, 0, 0], None),
        Sys::Execve => ([path, p.scratch + ARGV_AT, 0, 0, 0, 0], None),
        Sys::Wait4 => ([0, buf, 0, 0, 0, 0], None),
        Sys::Reboot => (
            [
                REBOOT_MAGIC1,
                REBOOT_MAGIC2,
                REBOOT_CMD_RESTART2,
                path,
                0,
                0,
            ],
            None,
        ),
        Sys::Getdents64 => {
            let fd = open(c"/", sys::O_RDONLY).map_err(|_| "open /")?;
            ([u64::from(fd), buf, 512, 0, 0, 0], Some(fd))
        }
        Sys::Psinfo => ([buf, 64, 0, 0, 0, 0], None),
        Sys::Openat => ([(-100i64) as u64, path, 0, 0, 0, 0], None),
        _ => return Err("no valid arguments for this call"),
    })
}

/// One case: what the call returned.
fn case(
    p: &Pages,
    call: &Call,
    ptr: &Ptr,
    form: Form,
) -> Result<Result<usize, Errno>, &'static str> {
    let (mut args, fd) = valid_args(p, call.sys)?;
    let (value, len) = bad_value(p, ptr, form);
    args[ptr.arg] = value;
    if let (Some(len), Some(i)) = (len, ptr.len_from) {
        args[i] = len;
    }
    let nr = call.sys.nr();
    let r = match call.sys {
        Sys::Execve => child_result(|| {
            // SAFETY: `execve` writes no user memory, as its table row
            // declares; established here.
            unsafe { raw(nr, args) }
        }),
        Sys::Wait4 => wait4_case(nr, args),
        // SAFETY: every pointer in `args` is a bad form or memory of the
        // scratch page, which no reference covers; established here.
        _ => Ok(unsafe { raw(nr, args) }),
    };
    if let Some(fd) = fd {
        close(fd);
    }
    r
}

/// A fresh zombie child reaped with `args`' status pointer: then the child
/// must be gone (`ECHILD`), reaped before the copy (SYSCALL.md §3.1).
fn wait4_case(nr: usize, mut args: [u64; 6]) -> Result<Result<usize, Errno>, &'static str> {
    let pid = utest::fork_child(|| 0).map_err(|_| "fork")?;
    if !poll(20_000, || utest::zombie(pid)) {
        return Err("the child did not exit");
    }
    args[0] = pid as u64;
    // SAFETY: the status pointer is a bad form or the scratch page, which
    // no reference covers; established here.
    let r = unsafe { raw(nr, args) };
    // SAFETY: a null status and rusage, so the kernel writes nothing; established here.
    let again = unsafe { sys::wait4(pid as i32, core::ptr::null_mut(), 0, core::ptr::null_mut()) };
    if again != Err(Errno::ECHILD) {
        return Err("the child outlived its EFAULT");
    }
    Ok(r)
}

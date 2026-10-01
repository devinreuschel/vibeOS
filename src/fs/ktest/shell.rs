//! The kernel shell's file commands on FAT and vibefs (kernel_tests
//! only), re-exported from `fs::ktest` (ROADMAP §10.4, F126): each test
//! calls a command's function with a buffer sink, several from a thread
//! on `spawn`'s 16 KiB stack ([`run_on_spawn_stack`]).

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::fs::{FsError, MAX_PATH};
use vibeos::kalloc::TryVec;
use vibeos::lock::RANK_DEVICE;
use vibeos::sched::stack_depth;

use crate::fat_init;
use crate::file_init;
use crate::ktest::{Outcome, sleep_until};
use crate::shell::cmds::fs::{self as sh, Out};
use crate::sync_init::SpinMutex;
use crate::thread_init;

/// What a command wrote, up to [`BUF_CAP`] bytes; more sets `over`.
pub(crate) struct BufOut {
    pub buf: TryVec<u8>,
    pub over: bool,
}

const BUF_CAP: usize = 4096;

impl BufOut {
    fn new() -> Result<Self, Outcome> {
        let buf = TryVec::try_with_capacity(BUF_CAP).map_err(|_| Outcome::Fail("out buffer"))?;
        Ok(Self { buf, over: false })
    }
}

impl Out for BufOut {
    fn put(&mut self, bytes: &[u8]) {
        if self.buf.len().saturating_add(bytes.len()) > BUF_CAP
            || self.buf.try_extend_from_slice(bytes).is_err()
        {
            self.over = true;
        }
    }
}

/// The body [`run_on_spawn_stack`]'s worker runs, and what it returned.
static BODY: SpinMutex<Option<fn() -> Outcome>> = SpinMutex::with_rank(None, RANK_DEVICE);
static RESULT: SpinMutex<Option<Outcome>> = SpinMutex::with_rank(None, RANK_DEVICE);
static DONE: AtomicBool = AtomicBool::new(false);

fn spawn_worker() {
    let body = BODY.lock().take();
    let out = body.map_or(Outcome::Fail("no body"), |f| f());
    *RESULT.lock() = Some(out);
    // Release: the worker's last store; `run_on_spawn_stack` reads it
    // with Acquire before it takes the outcome.
    DONE.store(true, Ordering::Release);
}

/// Run `body` on a thread `spawn` starts with its default 16 KiB stack,
/// and hand its outcome back; a failure too when the thread's recorded
/// depth is over that stack's budget. `name` names the thread.
pub(crate) fn run_on_spawn_stack(name: &'static str, body: fn() -> Outcome) -> Outcome {
    *RESULT.lock() = None;
    *BODY.lock() = Some(body);
    DONE.store(false, Ordering::Relaxed);
    let h = match thread_init::spawn(name, spawn_worker) {
        Ok(h) => h,
        Err(_) => {
            BODY.lock().take();
            return Outcome::Fail("spawn");
        }
    };
    if !sleep_until(|| DONE.load(Ordering::Acquire), 9_000) {
        return Outcome::Fail("the 16 KiB-stack worker did not finish in 9 s");
    }
    let out = RESULT.lock().take().unwrap_or(Outcome::Fail("no outcome"));
    let depth = crate::sched::ktest::wait_exit_depth(h.id().0);
    crate::ktest_info!("{}: 16 KiB stack depth {:?}", name, depth);
    match (out, depth) {
        (Outcome::Ok, Some(d)) if d > stack_depth::budget(16 * 1024) => {
            crate::fail_fmt!("{name} used {d} bytes of its 16 KiB stack, over budget")
        }
        (out, _) => out,
    }
}

/// A path assembled in a fixed buffer, cut at `MAX_PATH`.
struct PathBuf {
    buf: [u8; MAX_PATH],
    len: usize,
}

impl PathBuf {
    fn of(parts: &[&[u8]]) -> Self {
        let mut p = Self {
            buf: [0; MAX_PATH],
            len: 0,
        };
        for part in parts {
            p.push(part);
        }
        p
    }

    fn push(&mut self, part: &[u8]) {
        let end = self.len.saturating_add(part.len()).min(MAX_PATH);
        let n = end - self.len;
        self.buf[self.len..end].copy_from_slice(&part[..n]);
        self.len = end;
    }

    fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/// A failed step: what, and the error.
type Step<T> = Result<T, Outcome>;

fn step<T>(what: &'static str, r: Result<T, FsError>) -> Step<T> {
    r.map_err(|e| crate::fail_fmt!("{what}: {}", e.as_str()))
}

/// Free bytes on the FAT initrd.
fn initrd_free() -> Step<u64> {
    let v = step("initrd volume", fat_init::root_volume())?;
    let fat = v
        .downcast_ref::<fat_init::FatVolume>()
        .ok_or(Outcome::Fail("initrd volume is not FAT"))?;
    step("df", fat_init::df(fat)).map(|(_, _, free, _)| free)
}

/// Create `path` holding `data`.
fn put_file(path: &[u8], data: &[u8]) -> Result<(), FsError> {
    use vibeos::fs::{O_CREAT, O_TRUNC, O_WRONLY, OpenFlags};
    let f = file_init::open(
        path,
        OpenFlags::from_bits(O_WRONLY | O_CREAT | O_TRUNC),
        0o644,
    )?;
    let w = file_init::write(&f, data);
    let c = file_init::close(f);
    match w? {
        n if n == data.len() => c,
        _ => Err(FsError::Io),
    }
}

fn gone(path: &[u8]) -> bool {
    file_init::stat_path(path) == Err(FsError::NotFound)
}

// ---- shell_rm_r_tree ----

const RM_TOP: &str = "/kt61rm";
/// The vibefs tree's top directory, and the 56-character name it is
/// renamed to, which puts the fourth level below it past `MAX_PATH`.
const NL_TOP: &str = "/vibe/kt61nl";
const NL_LONG: &str = "/vibe/kt61nl-0123456789012345678901234567890123456789012345678";
const _: () = {
    let long = NL_LONG.len() + 4 * 51;
    let short = NL_TOP.len() + 4 * 51 + 2;
    assert!(NL_LONG.len() == 6 + 56 && long > MAX_PATH && short <= MAX_PATH);
};

/// `rm -r` on FAT removes a 20-entry, 8-deep tree and frees every cluster
/// it used; on vibefs, a tree whose deepest path passes `MAX_PATH` is
/// `NameTooLong` and left whole, and removed once it fits.
pub(crate) fn test_shell_rm_r_tree() -> Outcome {
    if !fat_init::live() {
        return Outcome::Skip("no FAT initrd");
    }
    run_on_spawn_stack("kt61rm", rm_r_tree)
}

fn rm_r_tree() -> Outcome {
    let r = rm_r_fat().and_then(|()| rm_r_too_long());
    // Whatever failed, leave nothing behind.
    if let Ok(mut o) = BufOut::new() {
        let _ = sh::rm(&["rm", "-r", RM_TOP, NL_TOP, NL_LONG], &mut o);
    }
    match r {
        Ok(()) => Outcome::Ok,
        Err(o) => o,
    }
}

fn rm_r_fat() -> Step<()> {
    let f0 = initrd_free()?;
    fat_tree()?;
    let mut out = BufOut::new()?;
    step("rm -r /kt61rm", sh::rm(&["rm", "-r", RM_TOP], &mut out))?;
    if !gone(RM_TOP.as_bytes()) {
        return Err(Outcome::Fail("/kt61rm is still there after rm -r"));
    }
    let f1 = initrd_free()?;
    if f1 != f0 {
        return Err(crate::fail_fmt!(
            "initrd free {f1} after rm -r, {f0} before"
        ));
    }
    Ok(())
}

/// `/kt61rm`: files `F00` to `F18` and directory `D1`, 20 entries, and
/// below it `D2` to `D8`, eight levels under `/kt61rm`, `D8` holding three
/// files. Uppercase 8.3 names take no LFN slots.
#[inline(never)]
fn fat_tree() -> Step<()> {
    step("mkdir /kt61rm", file_init::mkdir(RM_TOP.as_bytes(), 0o755))?;
    for i in 0..19u8 {
        let name = [b'F', b'0' + i / 10, b'0' + i % 10];
        let p = PathBuf::of(&[RM_TOP.as_bytes(), b"/", &name]);
        let data: &[u8] = if i % 9 == 0 { b"kt61" } else { b"" };
        step("create /kt61rm/Fnn", put_file(p.bytes(), data))?;
    }
    let mut dir = PathBuf::of(&[RM_TOP.as_bytes()]);
    for i in 1..=8u8 {
        dir.push(&[b'/', b'D', b'0' + i]);
        step("mkdir /kt61rm/D1/...", file_init::mkdir(dir.bytes(), 0o755))?;
    }
    for name in [&b"/X1"[..], b"/X2", b"/X3"] {
        let p = PathBuf::of(&[dir.bytes(), name]);
        step("create D8/Xn", put_file(p.bytes(), b"deep"))?;
    }
    Ok(())
}

fn rm_r_too_long() -> Step<()> {
    if !crate::vibefs_init::live() {
        return Err(Outcome::Fail("vibefs is not mounted on /vibe"));
    }
    long_tree()?;
    let mut out = BufOut::new()?;
    step(
        "mv to the long name",
        sh::mv(&["mv", NL_TOP, NL_LONG], &mut out),
    )?;
    match sh::rm(&["rm", "-r", NL_LONG], &mut out) {
        Err(FsError::NameTooLong) => {}
        Ok(()) => return Err(Outcome::Fail("rm -r of a path past MAX_PATH succeeded")),
        Err(e) => {
            return Err(crate::fail_fmt!(
                "rm -r past MAX_PATH: {}, not NameTooLong",
                e.as_str()
            ));
        }
    }
    step("mv back", sh::mv(&["mv", NL_LONG, NL_TOP], &mut out))?;
    deepest_there()?;
    step(
        "rm -r /vibe/kt61nl",
        sh::rm(&["rm", "-r", NL_TOP], &mut out),
    )?;
    if !gone(NL_TOP.as_bytes()) {
        return Err(Outcome::Fail("/vibe/kt61nl is still there after rm -r"));
    }
    Ok(())
}

/// `/vibe/kt61nl/<50×A>/<50×B>/<50×C>/<50×D>/f`, a 218-byte path.
#[inline(never)]
fn long_tree() -> Step<()> {
    let mut p = PathBuf::of(&[NL_TOP.as_bytes()]);
    step("mkdir /vibe/kt61nl", file_init::mkdir(p.bytes(), 0o755))?;
    for c in *b"ABCD" {
        p.push(b"/");
        p.push(&[c; 50]);
        step("mkdir a 50-byte level", file_init::mkdir(p.bytes(), 0o755))?;
    }
    p.push(b"/f");
    if p.len != 218 {
        return Err(crate::fail_fmt!(
            "the deepest path is {} bytes, not 218",
            p.len
        ));
    }
    step("create the deepest file", put_file(p.bytes(), b"x"))
}

/// The deepest file of [`long_tree`] is still there.
#[inline(never)]
fn deepest_there() -> Step<()> {
    let mut p = PathBuf::of(&[NL_TOP.as_bytes()]);
    for c in *b"ABCD" {
        p.push(b"/");
        p.push(&[c; 50]);
    }
    p.push(b"/f");
    step("stat the deepest file", file_init::stat_path(p.bytes())).map(|_| ())
}

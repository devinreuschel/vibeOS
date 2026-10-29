//! The kernel shell's file commands on FAT and vibefs (kernel_tests
//! only), re-exported from `fs::ktest` (ROADMAP §10.4, F126): each test
//! calls a command's function with a buffer sink, several from a thread
//! on `spawn`'s 16 KiB stack ([`run_on_spawn_stack`]).

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::fmt_util::StackBuf;
use vibeos::fs::{FileRef, FsError, InodeKind, MAX_PATH, O_CREAT, O_RDWR, OpenFlags, SeekFrom};
use vibeos::kalloc::TryVec;
use vibeos::lock::RANK_DEVICE;
use vibeos::sched::stack_depth;

use crate::fat_init;
use crate::file_init;
use crate::fs_init;
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

    /// Whether some line of the output is exactly `line`.
    fn has_line(&self, line: &[u8]) -> bool {
        self.buf.split(|&b| b == b'\n').any(|l| l == line)
    }

    /// How many lines of the output are exactly `line`.
    fn count_line(&self, line: &[u8]) -> usize {
        self.buf
            .split(|&b| b == b'\n')
            .filter(|&l| l == line)
            .count()
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.over = false;
    }

    /// Run command `f` with `args` into a cleared buffer; its result, with
    /// output past the buffer a failure.
    fn run(
        &mut self,
        what: &'static str,
        f: fn(&[&str], &mut dyn Out) -> Result<(), FsError>,
        args: &[&str],
    ) -> Step<()> {
        self.clear();
        step(what, f(args, self))?;
        if self.over {
            return Err(crate::fail_fmt!("{what}: output past {BUF_CAP} bytes"));
        }
        Ok(())
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
    use vibeos::fs::{O_TRUNC, O_WRONLY};
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

// ---- shell_ls_subdir ----

const LS_FAT: &str = "/etc/kt61ls";
const LS_VIBE: &str = "/vibe/kt61ls";
/// Files `/vibe/kt61ls` holds besides `f`: with `f`, more than one
/// `readdir_from` batch.
const LS_MORE: u8 = 11;

/// `ls` lists a FAT subdirectory (`/etc`) and a vibefs one through the
/// File API's `readdir`, in batches that resume where the last stopped;
/// `ls -l` gives each entry's kind and size; `ls /dev` (kernfs) still
/// lists `null`.
pub(crate) fn test_shell_ls_subdir() -> Outcome {
    if !fat_init::live() {
        return Outcome::Skip("no FAT initrd");
    }
    if !crate::vibefs_init::live() {
        return Outcome::Fail("vibefs is not mounted on /vibe");
    }
    let r = ls_subdir();
    let _ = file_init::unlink(LS_FAT.as_bytes());
    if let Ok(mut o) = BufOut::new() {
        let _ = sh::rm(&["rm", "-r", LS_VIBE], &mut o);
    }
    match r {
        Ok(()) => Outcome::Ok,
        Err(o) => o,
    }
}

fn ls_subdir() -> Step<()> {
    step("create /etc/kt61ls", put_file(LS_FAT.as_bytes(), b""))?;
    step(
        "mkdir /vibe/kt61ls",
        file_init::mkdir(LS_VIBE.as_bytes(), 0o755),
    )?;
    step(
        "create /vibe/kt61ls/f",
        put_file(b"/vibe/kt61ls/f", b"kt61-ls\n"),
    )?;
    for i in 0..LS_MORE {
        let name = [b'/', b'g', b'0' + i / 10, b'0' + i % 10];
        let p = PathBuf::of(&[LS_VIBE.as_bytes(), &name]);
        step("create /vibe/kt61ls/gnn", put_file(p.bytes(), b""))?;
    }
    let mut out = BufOut::new()?;
    out.run("ls /etc", sh::ls, &["ls", "/etc"])?;
    if !out.has_line(b"kt61ls") {
        return Err(Outcome::Fail("ls /etc does not list kt61ls"));
    }
    out.run("ls /vibe/kt61ls", sh::ls, &["ls", LS_VIBE])?;
    if out.count_line(b"f") != 1 {
        return Err(Outcome::Fail("ls /vibe/kt61ls does not list f once"));
    }
    for i in 0..LS_MORE {
        if out.count_line(&[b'g', b'0' + i / 10, b'0' + i % 10]) != 1 {
            return Err(crate::fail_fmt!(
                "ls /vibe/kt61ls does not list g{:02} once",
                i
            ));
        }
    }
    let lines = out
        .buf
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .count();
    if lines != usize::from(LS_MORE) + 1 {
        return Err(crate::fail_fmt!("ls /vibe/kt61ls printed {lines} entries"));
    }
    out.run("ls -l /vibe/kt61ls", sh::ls, &["ls", "-l", LS_VIBE])?;
    if !out.has_line(b"reg        8 f") {
        return Err(Outcome::Fail(
            "ls -l /vibe/kt61ls does not show `reg        8 f`",
        ));
    }
    out.run("ls /dev", sh::ls, &["ls", "/dev"])?;
    if !out.has_line(b"null") {
        return Err(Outcome::Fail("ls /dev does not list null"));
    }
    Ok(())
}

// ---- shell_mount_same_path_64 ----

const MNT_TOP: &str = "/kt61m";
const MNT_AT: &str = "/kt61m/m";

/// Dentries the VFS cache cannot evict (`Vfs::dentries_held`).
fn held() -> usize {
    fs_init::with(|v| v.dentries_held())
}

/// 64 `mount ramfs` of one path, from a thread on `spawn`'s 16 KiB stack:
/// each mount stacks on the last until the mount table is full, the rest
/// are `NoSpace`, and the dentries held grow by what the stacked mounts
/// hold by design and no more; `k` unmounts give them all back.
pub(crate) fn test_shell_mount_same_path_64() -> Outcome {
    run_on_spawn_stack("kt61mnt", mount_same_path)
}

fn mount_same_path() -> Outcome {
    let mut k = 0usize;
    let r = mount_64(&mut k);
    let mut left = 0usize;
    while left < k && file_init::umount(MNT_AT.as_bytes()).is_ok() {
        left += 1;
    }
    if let Ok(mut o) = BufOut::new() {
        let _ = sh::rm(&["rm", "-r", MNT_TOP], &mut o);
    }
    match r {
        Ok(()) => Outcome::Ok,
        Err(o) => o,
    }
}

/// The 64 mounts and their checks; `k` counts the mounts that succeeded
/// and are still mounted.
fn mount_64(k: &mut usize) -> Step<()> {
    let mut out = BufOut::new()?;
    out.run("mkdir -p /kt61m/m", sh::mkdir, &["mkdir", "-p", MNT_AT])?;
    step("resolve /kt61m/m", file_init::stat_path(MNT_AT.as_bytes()))?;
    let h0 = held();
    for _ in 0..64 {
        out.clear();
        match sh::mount(&["mount", "ramfs", MNT_AT], &mut out) {
            Ok(()) => *k = k.saturating_add(1),
            Err(FsError::NoSpace) => {}
            Err(e) => {
                return Err(crate::fail_fmt!(
                    "mount ramfs /kt61m/m: {}, not NoSpace",
                    e.as_str()
                ));
            }
        }
    }
    if *k == 0 {
        return Err(Outcome::Fail("no mount ramfs /kt61m/m succeeded"));
    }
    // By design, each stacked mount holds its root dentry (the superblock's
    // count), and the first one's mountpoint, /kt61m/m, becomes held once;
    // every later mountpoint is the root of the mount below, held already.
    let h1 = held();
    let want = h0.saturating_add(*k).saturating_add(1);
    crate::ktest_info!("{} of 64 mounts: dentries held {} -> {}", *k, h0, h1);
    if h1 != want {
        return Err(crate::fail_fmt!(
            "{} mounts: {h1} dentries held, want {want} (h0 {h0})",
            *k
        ));
    }
    let null = step(
        "open /dev/null",
        file_init::open(b"/dev/null", OpenFlags::from_bits(vibeos::fs::O_RDONLY), 0),
    )?;
    step("close /dev/null", file_init::close(null))?;
    while *k > 0 {
        out.run("umount /kt61m/m", sh::umount, &["umount", MNT_AT])?;
        *k -= 1;
    }
    let h2 = held();
    if h2 != h0 {
        return Err(crate::fail_fmt!(
            "{h2} dentries held after the unmounts, {h0} before"
        ));
    }
    Ok(())
}

// ---- shell_mount_umount ----

/// The partition the FAT32 tests format: P10-S60's `umount_consistent`'s,
/// which keeps the GPT and the persist sector intact.
const FAT_DEV: &str = "vdap2";
const U_RAM: &str = "/kt61u/r";
const U_FAT: &str = "/kt61u/f";
const U_TOP: &str = "/kt61u";

/// Format [`FAT_DEV`] with a FAT32 image, after checking nothing mounts
/// it.
fn fresh_fat_dev() -> Step<()> {
    let dev =
        crate::block::blockdev_init::lookup(FAT_DEV.as_bytes()).ok_or(Outcome::Fail("no vdap2"))?;
    if fs_init::with(|v| v.super_of_dev(dev.id())).is_some() {
        return Err(Outcome::Fail(
            "vdap2 is already mounted: another test left it",
        ));
    }
    super::stack16k::fat_image_to(FAT_DEV.as_bytes()).map_err(Outcome::Fail)
}

/// `mount` and `umount` from a thread on `spawn`'s 16 KiB stack: a ramfs
/// mount's file is gone after its `umount`; a FAT32 mount on `vdap2` with
/// a file open is `Busy` to `umount` and stays readable, unmounts once
/// the file is closed, and mounts and unmounts again.
pub(crate) fn test_shell_mount_umount() -> Outcome {
    run_on_spawn_stack("kt61umnt", mount_umount)
}

fn mount_umount() -> Outcome {
    let r = mount_umount_steps();
    for _ in 0..2 {
        let _ = file_init::umount(U_RAM.as_bytes());
        let _ = file_init::umount(U_FAT.as_bytes());
    }
    if let Ok(mut o) = BufOut::new() {
        let _ = sh::rm(&["rm", "-r", U_TOP], &mut o);
    }
    match r {
        Ok(()) => Outcome::Ok,
        Err(o) => o,
    }
}

fn mount_umount_steps() -> Step<()> {
    let mut out = BufOut::new()?;
    out.run(
        "mount ramfs /kt61u/r",
        sh::mount,
        &["mount", "ramfs", U_RAM],
    )?;
    step("create /kt61u/r/x", put_file(b"/kt61u/r/x", b"ram"))?;
    out.run("umount /kt61u/r", sh::umount, &["umount", U_RAM])?;
    if !gone(b"/kt61u/r/x") {
        return Err(Outcome::Fail("/kt61u/r/x is still there after umount"));
    }
    fresh_fat_dev()?;
    out.run(
        "mount fat32 vdap2",
        sh::mount,
        &["mount", "fat32", FAT_DEV, U_FAT],
    )?;
    let held = step(
        "open /kt61u/f/h",
        file_init::open(b"/kt61u/f/h", OpenFlags::from_bits(O_RDWR | O_CREAT), 0o644),
    )?;
    let r = busy_umount(&held, &mut out);
    let c = file_init::close(held);
    r?;
    step("close /kt61u/f/h", c)?;
    out.run("umount /kt61u/f", sh::umount, &["umount", U_FAT])?;
    out.run(
        "mount fat32 vdap2 again",
        sh::mount,
        &["mount", "fat32", FAT_DEV, U_FAT],
    )?;
    if !out_has_file(b"/kt61u/f/h") {
        return Err(Outcome::Fail("/kt61u/f/h is gone after the remount"));
    }
    out.run("umount /kt61u/f again", sh::umount, &["umount", U_FAT])
}

/// Whether `path` is there.
fn out_has_file(path: &[u8]) -> bool {
    file_init::stat_path(path).is_ok()
}

/// With `held` open on the FAT mount: `umount` is `Busy`, and the mount
/// still writes, reads and lists.
fn busy_umount(held: &FileRef, out: &mut BufOut) -> Step<()> {
    match file_init::write(held, b"kt61") {
        Ok(4) => {}
        _ => return Err(Outcome::Fail("write /kt61u/f/h")),
    }
    match sh::umount(&["umount", U_FAT], out) {
        Err(FsError::Busy) => {}
        Ok(()) => return Err(Outcome::Fail("umount with a file open succeeded")),
        Err(e) => {
            return Err(crate::fail_fmt!(
                "umount with a file open: {}, not Busy",
                e.as_str()
            ));
        }
    }
    step("seek /kt61u/f/h", file_init::seek(held, SeekFrom::Start(0)))?;
    let mut buf = [0u8; 8];
    match file_init::read(held, &mut buf) {
        Ok(4) if buf.get(..4) == Some(b"kt61") => {}
        _ => return Err(Outcome::Fail("read back /kt61u/f/h on the busy mount")),
    }
    out.run("ls /kt61u/f", sh::ls, &["ls", U_FAT])?;
    if !out.has_line(b"h") {
        return Err(Outcome::Fail("ls of the busy mount does not list h"));
    }
    Ok(())
}

// ---- shell_fs_commands ----

const CAT_DATA: &[u8] = b"kt61-cat\n";

/// `cat path`'s output into `out`, which must be exactly `want`.
fn cat_is(out: &mut BufOut, what: &'static str, path: &str, want: &[u8]) -> Step<()> {
    out.run(what, sh::cat, &["cat", path])?;
    if &out.buf[..] != want {
        return Err(crate::fail_fmt!(
            "{what}: {} bytes, not the {} written",
            out.buf.len(),
            want.len()
        ));
    }
    Ok(())
}

/// Some line of `out` is `ls -l`'s `<kind> <size, 8 wide> <name>`; any
/// size when `size` is `None`.
fn has_long(out: &BufOut, kind: &str, size: Option<u64>, name: &[u8]) -> bool {
    let mut b = [0u8; 32];
    let mut w = StackBuf::new(&mut b);
    let _ = match size {
        Some(s) => write!(w, "{kind} {s:>8} "),
        None => write!(w, "{kind} "),
    };
    let head = w.as_bytes();
    out.buf.split(|&b| b == b'\n').any(|l| {
        let Some(rest) = l.strip_prefix(head) else {
            return false;
        };
        match size {
            Some(_) => rest == name,
            None => {
                rest.len() == 9 + name.len()
                    && rest.get(9..) == Some(name)
                    && rest.get(8) == Some(&b' ')
            }
        }
    })
}

/// The numbers after `total ` and `free ` on `df`'s `fat32` line.
fn df_fat(out: &BufOut) -> Option<(u64, u64)> {
    let line = out
        .buf
        .split(|&b| b == b'\n')
        .find(|l| l.starts_with(b"vibeOS: df: fat32 total "))?;
    let mut words = line.split(|&b| b == b' ');
    let mut num_after = |key: &[u8]| -> Option<u64> {
        words.by_ref().find(|&w| w == key)?;
        core::str::from_utf8(words.next()?).ok()?.parse().ok()
    };
    let total = num_after(b"total")?;
    let free = num_after(b"free")?;
    Some((total, free))
}

/// Each file command through its function with a buffer sink, on the
/// FAT initrd (`/kt61c`), then `mkdir -p`, `cp`, `mv` and `rm -r` on
/// vibefs (`/vibe/kt61c`).
pub(crate) fn test_shell_fs_commands() -> Outcome {
    if !fat_init::live() {
        return Outcome::Skip("no FAT initrd");
    }
    if !crate::vibefs_init::live() {
        return Outcome::Fail("vibefs is not mounted on /vibe");
    }
    // aarch64's registry stack is 16 KiB (ROADMAP §11.3). This path plus
    // `registry_main` crosses DESIGN §4.5's margin; the worker's stack is
    // the same size without that frame. x86_64's registry is 64 KiB.
    #[cfg(target_arch = "aarch64")]
    {
        run_on_spawn_stack("kt61c", shell_fs_commands)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        shell_fs_commands()
    }
}

fn shell_fs_commands() -> Outcome {
    let r = fs_commands_fat().and_then(|()| fs_commands_vibe());
    if let Ok(mut o) = BufOut::new() {
        let _ = sh::rm(&["rm", "-r", "/kt61c", "/vibe/kt61c"], &mut o);
    }
    match r {
        Ok(()) => Outcome::Ok,
        Err(o) => o,
    }
}

fn fs_commands_fat() -> Step<()> {
    let f0 = initrd_free()?;
    let mut out = BufOut::new()?;
    // Each step is its own frame. Inlined into one function, opt-level 1
    // keeps every step's buffers live across `cat`, and that depth crosses
    // the 16 KiB worker's budget (DESIGN §4.5).
    fat_dirs(&mut out)?;
    fat_touch(&mut out)?;
    fat_cp_write(&mut out)?;
    fat_cp_copy(&mut out)?;
    fat_mv(&mut out)?;
    fat_case_mv(&mut out)?;
    fat_slot_files(&mut out)?;
    fat_slot_stat(&mut out)?;
    fat_list(&mut out)?;
    fat_finish(&mut out, f0)?;
    Ok(())
}

#[inline(never)]
fn fat_dirs(out: &mut BufOut) -> Step<()> {
    out.run(
        "mkdir -p /kt61c/a/b/c",
        sh::mkdir,
        &["mkdir", "-p", "/kt61c/a/b/c"],
    )?;
    for d in [&b"/kt61c/a"[..], b"/kt61c/a/b", b"/kt61c/a/b/c"] {
        let st = step("stat a made directory", file_init::stat_path(d))?;
        if st.kind != InodeKind::Dir {
            return Err(Outcome::Fail("mkdir -p made a non-directory"));
        }
    }
    Ok(())
}

#[inline(never)]
fn fat_touch(out: &mut BufOut) -> Step<()> {
    out.run("touch /kt61c/t", sh::touch, &["touch", "/kt61c/t"])?;
    let t = step("stat /kt61c/t", file_init::stat_path(b"/kt61c/t"))?;
    if t.kind != InodeKind::Reg || t.size != 0 {
        return Err(Outcome::Fail("touch did not make an empty file"));
    }
    Ok(())
}

#[inline(never)]
fn fat_cp_write(out: &mut BufOut) -> Step<()> {
    step("write /kt61c/s", put_file(b"/kt61c/s", CAT_DATA))?;
    cat_is(out, "cat /kt61c/s", "/kt61c/s", CAT_DATA)
}

#[inline(never)]
fn fat_cp_copy(out: &mut BufOut) -> Step<()> {
    out.run(
        "cp /kt61c/s /kt61c/a/s2",
        sh::cp,
        &["cp", "/kt61c/s", "/kt61c/a/s2"],
    )?;
    cat_is(out, "cat /kt61c/a/s2", "/kt61c/a/s2", CAT_DATA)
}

#[inline(never)]
fn fat_mv(out: &mut BufOut) -> Step<()> {
    out.run(
        "mv /kt61c/a/s2 /kt61c/m",
        sh::mv,
        &["mv", "/kt61c/a/s2", "/kt61c/m"],
    )?;
    if !gone(b"/kt61c/a/s2") {
        return Err(Outcome::Fail("mv left /kt61c/a/s2"));
    }
    cat_is(out, "cat /kt61c/m", "/kt61c/m", CAT_DATA)?;
    Ok(())
}

/// A case-only FAT rename (F059).
#[inline(never)]
fn fat_case_mv(out: &mut BufOut) -> Step<()> {
    out.run(
        "mv /kt61c/m /kt61c/M",
        sh::mv,
        &["mv", "/kt61c/m", "/kt61c/M"],
    )?;
    cat_is(out, "cat /kt61c/M", "/kt61c/M", CAT_DATA)?;
    Ok(())
}

/// The rename gave `M` a new entry and freed `m`'s: a file made in that
/// slot is its own, and `M` keeps its data, where the inode left keyed
/// by the freed entry once took the new file's name onto `M`'s chain.
#[inline(never)]
fn fat_slot_files(out: &mut BufOut) -> Step<()> {
    step("write /kt61c/n", put_file(b"/kt61c/n", b"NN"))?;
    cat_is(out, "cat /kt61c/n", "/kt61c/n", b"NN")?;
    cat_is(out, "cat /kt61c/M", "/kt61c/M", CAT_DATA)?;
    step("rm /kt61c/n", file_init::unlink(b"/kt61c/n"))
}

#[inline(never)]
fn fat_slot_stat(out: &mut BufOut) -> Step<()> {
    out.run("stat /kt61c/M", sh::stat, &["stat", "/kt61c/M"])?;
    if !out.buf.windows(7).any(|w| w == b"size 9 ") {
        return Err(Outcome::Fail("stat /kt61c/M does not print `size 9`"));
    }
    Ok(())
}

#[inline(never)]
fn fat_list(out: &mut BufOut) -> Step<()> {
    out.run("ls -l /kt61c", sh::ls, &["ls", "-l", "/kt61c"])?;
    if !has_long(out, "dir", None, b"a")
        || !has_long(out, "reg", Some(0), b"t")
        || !has_long(out, "reg", Some(9), b"s")
        || !has_long(out, "reg", Some(9), b"M")
    {
        return Err(Outcome::Fail(
            "ls -l /kt61c does not list a, t, s and M with kinds and sizes",
        ));
    }
    if has_long(out, "reg", None, b"m") {
        return Err(Outcome::Fail(
            "ls -l /kt61c still lists m after the case-only mv",
        ));
    }
    Ok(())
}

#[inline(never)]
fn fat_finish(out: &mut BufOut, f0: u64) -> Step<()> {
    out.run("df", sh::df, &["df"])?;
    match df_fat(out) {
        Some((total, free)) if free <= total => {}
        Some(_) => return Err(Outcome::Fail("df: fat32 free over total")),
        None => return Err(Outcome::Fail("df prints no fat32 line")),
    }
    out.run("sync", sh::sync, &["sync"])?;
    out.run("rm -r /kt61c", sh::rm, &["rm", "-r", "/kt61c"])?;
    if !gone(b"/kt61c") {
        return Err(Outcome::Fail("/kt61c is still there after rm -r"));
    }
    let f1 = initrd_free()?;
    if f1 != f0 {
        return Err(crate::fail_fmt!(
            "initrd free {f1} after rm -r, {f0} before"
        ));
    }
    Ok(())
}

fn fs_commands_vibe() -> Step<()> {
    let mut out = BufOut::new()?;
    vibe_setup(&mut out)?;
    vibe_cp(&mut out)?;
    vibe_mv(&mut out)?;
    vibe_rm(&mut out)?;
    Ok(())
}

#[inline(never)]
fn vibe_setup(out: &mut BufOut) -> Step<()> {
    out.run(
        "mkdir -p /vibe/kt61c/a/b",
        sh::mkdir,
        &["mkdir", "-p", "/vibe/kt61c/a/b"],
    )?;
    step(
        "stat /vibe/kt61c/a/b",
        file_init::stat_path(b"/vibe/kt61c/a/b"),
    )?;
    step("write /vibe/kt61c/s", put_file(b"/vibe/kt61c/s", CAT_DATA))?;
    Ok(())
}

#[inline(never)]
fn vibe_cp(out: &mut BufOut) -> Step<()> {
    out.run(
        "cp on vibefs",
        sh::cp,
        &["cp", "/vibe/kt61c/s", "/vibe/kt61c/a/s2"],
    )?;
    cat_is(out, "cat /vibe/kt61c/a/s2", "/vibe/kt61c/a/s2", CAT_DATA)?;
    Ok(())
}

#[inline(never)]
fn vibe_mv(out: &mut BufOut) -> Step<()> {
    out.run(
        "mv on vibefs",
        sh::mv,
        &["mv", "/vibe/kt61c/a/s2", "/vibe/kt61c/a/b/m"],
    )?;
    if !gone(b"/vibe/kt61c/a/s2") {
        return Err(Outcome::Fail("mv left /vibe/kt61c/a/s2"));
    }
    cat_is(out, "cat /vibe/kt61c/a/b/m", "/vibe/kt61c/a/b/m", CAT_DATA)?;
    Ok(())
}

#[inline(never)]
fn vibe_rm(out: &mut BufOut) -> Step<()> {
    out.run("rm -r /vibe/kt61c", sh::rm, &["rm", "-r", "/vibe/kt61c"])?;
    if !gone(b"/vibe/kt61c") {
        return Err(Outcome::Fail("/vibe/kt61c is still there after rm -r"));
    }
    Ok(())
}

// ---- shell_fat32_image ----

const V_AT: &str = "/kt61v";
/// The file the test writes on the initrd and copies to the image.
const V_SRC: &str = "/KT61V.TXT";
const V_DATA: &[u8] = b"kt61 fat32 image\n";

/// A FAT32 image on `vdap2` through the shell, from a thread on `spawn`'s
/// 16 KiB stack: `mount`, `mkdir`, `cp` from the initrd, `cat`, `ls`,
/// `rm`, `mkdir -p`, `touch`, `rm -r` and `umount`. Self-contained: it
/// formats the partition first.
pub(crate) fn test_shell_fat32_image() -> Outcome {
    if !fat_init::live() {
        return Outcome::Skip("no FAT initrd");
    }
    run_on_spawn_stack("kt61img", fat32_image)
}

fn fat32_image() -> Outcome {
    let r = fat32_image_steps();
    let _ = file_init::umount(V_AT.as_bytes());
    let _ = file_init::rmdir(V_AT.as_bytes());
    let _ = file_init::unlink(V_SRC.as_bytes());
    match r {
        Ok(()) => Outcome::Ok,
        Err(o) => o,
    }
}

fn fat32_image_steps() -> Step<()> {
    fresh_fat_dev()?;
    step("write /KT61V.TXT", put_file(V_SRC.as_bytes(), V_DATA))?;
    let mut out = BufOut::new()?;
    out.run(
        "mount fat32 vdap2 /kt61v",
        sh::mount,
        &["mount", "fat32", FAT_DEV, V_AT],
    )?;
    out.run("mkdir /kt61v/d", sh::mkdir, &["mkdir", "/kt61v/d"])?;
    out.run("cp to /kt61v/d/h", sh::cp, &["cp", V_SRC, "/kt61v/d/h"])?;
    cat_is(&mut out, "cat /kt61v/d/h", "/kt61v/d/h", V_DATA)?;
    out.run("ls /kt61v/d", sh::ls, &["ls", "/kt61v/d"])?;
    if !out.has_line(b"h") {
        return Err(Outcome::Fail("ls /kt61v/d does not list h"));
    }
    out.run("rm /kt61v/d/h", sh::rm, &["rm", "/kt61v/d/h"])?;
    if !gone(b"/kt61v/d/h") {
        return Err(Outcome::Fail("/kt61v/d/h is still there after rm"));
    }
    out.run(
        "mkdir -p /kt61v/t/u",
        sh::mkdir,
        &["mkdir", "-p", "/kt61v/t/u"],
    )?;
    out.run("touch /kt61v/t/u/x", sh::touch, &["touch", "/kt61v/t/u/x"])?;
    step("stat /kt61v/t/u/x", file_init::stat_path(b"/kt61v/t/u/x"))?;
    out.run("rm -r /kt61v/t", sh::rm, &["rm", "-r", "/kt61v/t"])?;
    if !gone(b"/kt61v/t") {
        return Err(Outcome::Fail("/kt61v/t is still there after rm -r"));
    }
    out.run("umount /kt61v", sh::umount, &["umount", V_AT])
}

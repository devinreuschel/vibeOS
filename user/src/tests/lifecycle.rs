//! The process lifecycle (ROADMAP §10.5, §9.6, §9.7, F077): the fork limit,
//! orphan reparenting, an exec chain, wait ordering, and `SIGKILL`,
//! `SIGSTOP` and `SIGCONT`. This suite runs last, so the fork bomb counts
//! no stray child. Every fork goes through `utest::fork` (F069).

use core::ffi::CStr;

use vibeos_user::sys::{self, Errno, MAX_PROCS};
use vibeos_user::utest::{self, Outcome, Runner};

use super::errno::{
    HELLO_EXIT, SCRATCH, SEEK_SET, SIGKILL, SIGSTOP, WNOHANG, close, discard, open, parse_dec,
    poll, ps_state, psinfo, put_file, sleep_ms, state_is,
};

/// The one file this suite writes (`errno::SCRATCH`), truncated after
/// each case.
const LIFE_FILE: &CStr = SCRATCH;

/// Each case's deadline: about 3x its run under TCG at `-smp 2`, rounded
/// up to 5 s, and at least 10 s (each takes under a second there).
const DEADLINE_MS: u32 = 10_000;

/// A case: its name and its body.
type Case = (&'static str, fn() -> Outcome);

/// The cases, in run order.
const CASES: [Case; 5] = [
    ("fork_bomb_eagain_at_limit", fork_bomb_eagain_at_limit),
    ("orphan_grandchild_getppid_1", orphan_grandchild_getppid_1),
    (
        "exec_chain_three_steps_status",
        exec_chain_three_steps_status,
    ),
    (
        "wait4_specific_pid_blocks_then_zombie_sibling",
        wait4_specific_pid_blocks_then_zombie_sibling,
    ),
    ("signals_kill_stop_cont", signals_kill_stop_cont),
];

pub fn run(t: &mut Runner) {
    for (name, case) in CASES {
        t.case_ms(name, DEADLINE_MS, || {
            let r = case();
            discard(LIFE_FILE);
            r
        });
    }
}

/// `wait4(-1, NULL, 0)` until `ECHILD`: how many it reaped.
fn reap_all() -> usize {
    let mut n = 0;
    // SAFETY: a null status and rusage, so the kernel writes nothing;
    // established here.
    while unsafe { sys::wait4(-1, core::ptr::null_mut(), 0, core::ptr::null_mut()) }.is_ok() {
        n += 1;
    }
    n
}

/// The processes `psinfo` lists, zombies included, or `None` when its
/// 512 bytes may have cut the list short.
fn live_processes() -> Option<usize> {
    let mut buf = [0u8; 512];
    let n = psinfo(&mut buf).ok()?;
    let text = buf.get(..n)?;
    if n > 400 || text.last() != Some(&b'\n') {
        return None;
    }
    Some(text.iter().filter(|&&b| b == b'\n').count())
}

/// Fork children that exit at once, each a zombie before the next fork,
/// so at most one copy of this process lives: forks 1 to `MAX_PROCS -
/// live` succeed, and the next one is `-EAGAIN`. Then every child is
/// reaped, and one more fork succeeds.
fn fork_bomb_eagain_at_limit() -> Outcome {
    let Some(live) = live_processes() else {
        return Outcome::Fail("psinfo looks truncated");
    };
    let Some(want) = MAX_PROCS.checked_sub(live) else {
        return Outcome::Fail("more processes than MAX_PROCS");
    };
    let mut made = 0;
    let mut why = None;
    while made < want {
        match utest::fork_child(|| 0) {
            Ok(pid) => {
                made += 1;
                if !poll(20_000, || utest::zombie(pid)) {
                    why = Some(utest::fail(format_args!("child {made} did not exit")));
                    break;
                }
            }
            Err(e) => {
                why = Some(utest::fail(format_args!(
                    "fork {} of {want}: -{}",
                    made + 1,
                    e.0
                )));
                break;
            }
        }
    }
    if why.is_none() {
        why = match utest::fork_child(|| 0) {
            Err(Errno::EAGAIN) => None,
            Err(e) => Some(utest::fail(format_args!(
                "fork {}: -{}, not EAGAIN",
                want + 1,
                e.0
            ))),
            Ok(_) => {
                made += 1;
                Some(utest::fail(format_args!(
                    "fork {} succeeded past the limit",
                    want + 1
                )))
            }
        };
    }
    let reaped = reap_all();
    if let Some(why) = why {
        return why;
    }
    if reaped != made {
        return utest::fail(format_args!("reaped {reaped} of {made} children"));
    }
    utest::info(
        "fork_bomb_eagain_at_limit",
        format_args!("{live} live, {made} forks, then EAGAIN"),
    );
    match utest::fork_child(|| 0).map(utest::wait_status) {
        Ok(Ok(0)) => Outcome::Ok,
        _ => Outcome::Fail("no fork after the children were reaped"),
    }
}

/// The file the grandchild reports through.
const ORPHAN_FILE: &CStr = LIFE_FILE;

/// C forks G and exits; G yields until `getppid()` is 1, then writes `<its
/// pid> 1`. The parent reads that, then waits for G to leave `psinfo`:
/// init reaped it.
fn orphan_grandchild_getppid_1() -> Outcome {
    if sys::getppid() != Ok(1) {
        return Outcome::Skip("no init");
    }
    if put_file(ORPHAN_FILE, b"").is_err() {
        return Outcome::Fail("truncate the report file");
    }
    let c = utest::fork_child(|| match utest::fork_child(grandchild) {
        Ok(_) => 0,
        Err(_) => 1,
    });
    let Ok(c) = c else {
        return Outcome::Fail("fork");
    };
    if utest::wait_status(c) != Ok(0) {
        return Outcome::Fail("the middle child did not exit 0");
    }
    let mut gpid = None;
    let mut bad = false;
    for _ in 0..500 {
        let mut buf = [0u8; 32];
        let n = read_file(ORPHAN_FILE, &mut buf);
        let text = buf.get(..n).unwrap_or(&[]);
        if text.starts_with(b"bad") {
            bad = true;
            break;
        }
        if let Some(sp) = text.iter().position(|&b| b == b' ')
            && text.get(sp + 1..) == Some(b"1\n")
        {
            gpid = text.get(..sp).and_then(parse_dec);
            break;
        }
        sleep_ms(10);
    }
    if bad {
        return Outcome::Fail("the grandchild saw a parent other than its own or 1");
    }
    let Some(gpid) = gpid else {
        return Outcome::Fail("the grandchild never saw getppid() == 1");
    };
    for _ in 0..500 {
        if ps_state(gpid).is_none() {
            utest::info(
                "orphan_grandchild_getppid_1",
                format_args!("pid {gpid} saw ppid 1, and init reaped it"),
            );
            return Outcome::Ok;
        }
        sleep_ms(10);
    }
    Outcome::Fail("init did not reap the orphan")
}

/// G: yield until reparented to init, then report.
fn grandchild() -> i32 {
    let first = sys::getppid().unwrap_or(0);
    loop {
        match sys::getppid() {
            Ok(1) => break,
            Ok(p) if p == first => {}
            _ => {
                report(b"bad\n");
                return 1;
            }
        }
        #[expect(
            clippy::let_underscore_must_use,
            reason = "the loop yields only to wait"
        )]
        let _ = sys::sched_yield();
    }
    let me = sys::getpid().unwrap_or(0);
    let mut line = [0u8; 24];
    let n = dec(me, &mut line);
    line[n..n + 3].copy_from_slice(b" 1\n");
    report(&line[..n + 3]);
    0
}

/// `v` in decimal at the start of `out`: its length.
fn dec(mut v: usize, out: &mut [u8; 24]) -> usize {
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let d = &digits[i..];
    out[..d.len()].copy_from_slice(d);
    d.len()
}

/// Write `line` to the report file in one write.
fn report(line: &[u8]) {
    if let Ok(fd) = open(ORPHAN_FILE, sys::O_WRONLY) {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a lost report fails the parent's wait for it"
        )]
        let _ = sys::write(fd, line.as_ptr(), line.len());
        close(fd);
    }
}

/// The first bytes of `path` into `buf`: how many.
fn read_file(path: &CStr, buf: &mut [u8]) -> usize {
    let Ok(fd) = open(path, sys::O_RDONLY) else {
        return 0;
    };
    // SAFETY: the kernel writes at most `buf.len()` bytes into `buf`, which
    // the `&mut` borrow lends to this call alone; established here.
    let n = unsafe { sys::read(fd, buf.as_mut_ptr(), buf.len()) }.unwrap_or(0);
    close(fd);
    n
}

/// A child `execve`s `/bin/tests --exec-step /hello`, which `execve`s
/// `/hello`: the parent's `wait4` gets `/hello`'s status. 126 means a step
/// failed.
fn exec_chain_three_steps_status() -> Outcome {
    let pid = utest::fork_child(|| {
        let prog = c"/bin/tests".as_ptr().cast::<u8>();
        let argv = [
            prog,
            c"--exec-step".as_ptr().cast(),
            c"/hello".as_ptr().cast(),
            core::ptr::null(),
        ];
        #[expect(
            clippy::let_underscore_must_use,
            reason = "an execve that returns failed; status 126 reports it"
        )]
        let _ = sys::execve(prog, argv.as_ptr(), core::ptr::null());
        126
    });
    let Ok(pid) = pid else {
        return Outcome::Fail("fork");
    };
    match utest::wait_status(pid).map(utest::exited) {
        Ok(Some(c)) if c == HELLO_EXIT => Outcome::Ok,
        Ok(Some(126)) => Outcome::Fail("an exec step failed (status 126)"),
        Ok(Some(c)) => utest::fail(format_args!("exit {c}, not /hello's {HELLO_EXIT}")),
        Ok(None) => Outcome::Fail("the chain was killed"),
        Err(_) => Outcome::Fail("wait4"),
    }
}

/// The file B waits on.
const WAIT_FILE: &CStr = LIFE_FILE;

/// A exits 3 at once; B exits 4 100 ms after the file says go. `wait4(B)`
/// returns B although zombie A exited first, then `wait4(A, WNOHANG)`
/// returns A's status at once, and no child is left.
fn wait4_specific_pid_blocks_then_zombie_sibling() -> Outcome {
    if put_file(WAIT_FILE, b"").is_err() {
        return Outcome::Fail("truncate the go file");
    }
    let Ok(a) = utest::fork_child(|| 3) else {
        return Outcome::Fail("fork A");
    };
    if !poll(20_000, || utest::zombie(a)) {
        return Outcome::Fail("A did not exit");
    }
    let b = utest::fork_child(|| {
        let mut buf = [0u8; 4];
        while read_file(WAIT_FILE, &mut buf) == 0 {
            sleep_ms(10);
        }
        sleep_ms(100);
        4
    });
    let Ok(b) = b else {
        return Outcome::Fail("fork B");
    };
    if utest::zombie(b) {
        return Outcome::Fail("B exited before the go");
    }
    if put_file(WAIT_FILE, b"go").is_err() {
        return Outcome::Fail("write the go file");
    }
    let mut st = 0i32;
    // SAFETY: `wait4` writes 4 bytes through `&raw mut st`, a local no
    // reference covers, and nothing through the null rusage; established
    // here.
    let rb = unsafe { sys::wait4(b as i32, &raw mut st, 0, core::ptr::null_mut()) };
    if rb != Ok(b) || utest::exited(st as u32) != Some(4) {
        return Outcome::Fail("wait4(B) did not return B's exit 4");
    }
    // SAFETY: as above; established here.
    let ra = unsafe { sys::wait4(a as i32, &raw mut st, WNOHANG, core::ptr::null_mut()) };
    if ra != Ok(a) || utest::exited(st as u32) != Some(3) {
        return Outcome::Fail("wait4(A, WNOHANG) did not return zombie A's exit 3");
    }
    // SAFETY: a null status and rusage, so the kernel writes nothing; established here.
    let none = unsafe { sys::wait4(-1, core::ptr::null_mut(), WNOHANG, core::ptr::null_mut()) };
    if none != Err(Errno::ECHILD) {
        return Outcome::Fail("a child was left");
    }
    Outcome::Ok
}

/// The file the counting child writes.
const SIG_FILE: &CStr = LIFE_FILE;

/// (a) `SIGKILL` ends a child that yields in a loop; (b) a child counting
/// into a file stops counting under `SIGSTOP` (and `psinfo` says `stop`),
/// counts again after `SIGCONT`, and `SIGKILL` ends it.
fn signals_kill_stop_cont() -> Outcome {
    let Ok(y) = utest::fork_child(|| {
        loop {
            #[expect(clippy::let_underscore_must_use, reason = "the parent kills it")]
            let _ = sys::sched_yield();
        }
    }) else {
        return Outcome::Fail("fork the yielding child");
    };
    if sys::kill(y as i32, SIGKILL) != Ok(0) {
        return Outcome::Fail("kill(SIGKILL) did not return 0");
    }
    if utest::wait_status(y).map(utest::signaled) != Ok(Some(SIGKILL as u8)) {
        return Outcome::Fail("the yielding child did not die of SIGKILL");
    }
    if put_file(SIG_FILE, &0u64.to_le_bytes()).is_err() {
        return Outcome::Fail("create the counter file");
    }
    let Ok(c) = utest::fork_child(count_forever) else {
        return Outcome::Fail("fork the counting child");
    };
    let r = stop_cont(c);
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a child already dead is reaped below all the same"
    )]
    let _ = sys::kill(c as i32, SIGKILL);
    let dead = utest::wait_status(c).map(utest::signaled);
    match r {
        Err(why) => Outcome::Fail(why),
        Ok(()) if dead == Ok(Some(SIGKILL as u8)) => Outcome::Ok,
        Ok(()) => Outcome::Fail("the counting child did not die of SIGKILL"),
    }
}

/// The counting child: an 8-byte counter at offset 0 of its own open file
/// description, then a yield, forever.
fn count_forever() -> i32 {
    let Ok(fd) = open(SIG_FILE, sys::O_RDWR) else {
        return 1;
    };
    let mut n = 0u64;
    loop {
        n += 1;
        let b = n.to_le_bytes();
        if sys::lseek(fd, 0, SEEK_SET).is_err() || sys::write(fd, b.as_ptr(), 8) != Ok(8) {
            return 2;
        }
        #[expect(
            clippy::let_underscore_must_use,
            reason = "the loop yields only to share"
        )]
        let _ = sys::sched_yield();
    }
}

/// The counter the child last wrote.
fn counter() -> u64 {
    let mut b = [0u8; 8];
    read_file(SIG_FILE, &mut b);
    u64::from_le_bytes(b)
}

fn stop_cont(c: usize) -> Result<(), &'static str> {
    if !poll(20_000, || counter() > 0) {
        return Err("the child never counted");
    }
    if sys::kill(c as i32, SIGSTOP) != Ok(0) {
        return Err("kill(SIGSTOP) did not return 0");
    }
    sleep_ms(50);
    let c1 = counter();
    sleep_ms(100);
    let c2 = counter();
    if c1 != c2 {
        return Err("the child counted while stopped");
    }
    if !ps_state(c).is_some_and(|(_, s)| state_is(&s, b"stop")) {
        return Err("psinfo does not show the child stopped");
    }
    if sys::kill(c as i32, utest::SIGCONT) != Ok(0) {
        return Err("kill(SIGCONT) did not return 0");
    }
    if !poll(20_000, || counter() > c2) {
        return Err("the child did not count again after SIGCONT");
    }
    Ok(())
}

//! In-guest test of a large tmpfs directory under the IF-off tracer
//! (`irqoff` and kernel_tests only). Rows: the parent `ktest.rs`'s `TESTS`.

use vibeos::fs::{FsError, O_CREAT, O_RDONLY, O_RDWR, OpenFlags};

use crate::file_init;
use crate::ktest::Outcome;
use crate::sched::irqoff::{BOUND_NS, testing};

/// Files the directory holds. Under a spinlock, a create in a tmpfs
/// directory of this many children held IF off for up to about 190,000
/// instructions (`-icount shift=0`), past DESIGN §2.9 rule 2's bound.
const FILES: usize = 3_000;

const DIR: &[u8] = b"/tmp/irqoff_dir";

/// `DIR/f<i>` in `buf`.
fn path(buf: &mut [u8; 32], i: usize) -> &[u8] {
    let pre = b"/tmp/irqoff_dir/f";
    buf[..pre.len()].copy_from_slice(pre);
    let mut digits = [0u8; 8];
    let mut n = 0;
    let mut v = i;
    loop {
        digits[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    for k in 0..n {
        buf[pre.len() + k] = digits[n - 1 - k];
    }
    &buf[..pre.len() + n]
}

/// The last create in a tmpfs directory of [`FILES`] children, and a
/// lookup of a name it lacks, hold IF off for no stretch past the bound:
/// the kernfs store's lock is a sleeping one (`fs::StoreLock`), where a
/// spinlock held IF off while each walked the children.
pub(crate) fn irqoff_big_tmp_dir() -> Outcome {
    if file_init::mkdir(DIR, 0o755).is_err() {
        return Outcome::Fail("mkdir");
    }
    let mut name = [0u8; 32];
    let mut made = 0usize;
    let mut fail = None;
    let mut walked = None;
    while made < FILES && fail.is_none() {
        let flags = OpenFlags::from_bits(O_CREAT | O_RDWR);
        // The last create, then the lookup, under the capture.
        let cap = (made == FILES - 1).then(testing::capture);
        match file_init::open(path(&mut name, made), flags, 0o644) {
            Ok(f) => {
                made += 1;
                if file_init::close(f).is_err() {
                    fail = Some(crate::fail_fmt!("close {made}"));
                }
            }
            Err(e) => fail = Some(crate::fail_fmt!("create {made}: {}", e.as_str())),
        }
        if cap.is_some() && fail.is_none() {
            let missing = OpenFlags::from_bits(O_RDONLY);
            let r = file_init::open(b"/tmp/irqoff_dir/missing", missing, 0);
            walked = Some((r.map(|_| ()).err(), testing::last()));
        }
        drop(cap);
    }
    for i in 0..made {
        if file_init::unlink(path(&mut name, i)).is_err() && fail.is_none() {
            fail = Some(crate::fail_fmt!("unlink {i}"));
        }
    }
    if file_init::rmdir(DIR).is_err() && fail.is_none() {
        fail = Some(Outcome::Fail("rmdir"));
    }
    if let Some(o) = fail {
        return o;
    }
    match walked {
        Some((Some(FsError::NotFound), Some((ns, site, _)))) if ns > BOUND_NS => {
            crate::fail_fmt!("{ns} ns IF off at {site}, over {BOUND_NS}")
        }
        Some((Some(FsError::NotFound), _)) => Outcome::Ok,
        _ => Outcome::Fail("the missing name's lookup did not fail NotFound"),
    }
}

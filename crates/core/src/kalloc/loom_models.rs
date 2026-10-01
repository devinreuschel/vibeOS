//! Loom models of `TryArc`'s count and deferred release (C-LOOM).

extern crate std;

use super::*;
use crate::sync::variant::{self, Site};
use loom::cell::UnsafeCell;
use loom::sync::Arc;
use loom::thread;

loom::lazy_static! {
    /// The sink's target: one list for the model's threads.
    static ref LIST: DeferList = DeferList::new();
}

/// Every put is in simulated atomic context, so every last put defers
/// to `LIST`; the model's threads release it themselves.
fn install() {
    set_release_context(|| false);
    set_deferral(|d| {
        LIST.push(d);
    });
}

/// A cell one holder writes and the release reads, and a release count.
struct Val {
    cell: UnsafeCell<u64>,
    released: Arc<AtomicUsize>,
}

// SAFETY: `cell` is written by one holder before it puts its
// reference, and read by the release after the last put; the models
// check, through loom, that the count orders the two. Established here.
unsafe impl Sync for Val {}

impl Drop for Val {
    fn drop(&mut self) {
        // SAFETY: the release runs after every put, so no holder still
        // writes `cell`; loom reports it if the count fails to order the
        // write before this read. Established by `kalloc::put_raw`.
        let v = self.cell.with(|p| unsafe { *p });
        assert_eq!(v, 7);
        self.released.fetch_add(1, Ordering::Relaxed);
    }
}

/// T1 writes the value and puts its reference with `put_deferred`, the
/// main thread drops the other plainly, and W and then main release
/// the list: exactly one release, which sees T1's write.
fn tryarc_count_model() {
    loom::model(|| {
        install();
        let released = Arc::new(AtomicUsize::new(0));
        let a = TryArc::try_new(Val {
            cell: UnsafeCell::new(0),
            released: released.clone(),
        })
        .unwrap();
        let b = a.clone();
        let t1 = thread::spawn(move || {
            // SAFETY: this holder is the only writer, and the value is
            // live while `b` is held; established here.
            b.cell.with_mut(|p| unsafe { *p = 7 });
            b.put_deferred();
        });
        let w = thread::spawn(|| {
            LIST.release_all();
        });
        drop(a);
        t1.join().unwrap();
        w.join().unwrap();
        LIST.release_all();
        assert_eq!(released.load(Ordering::Relaxed), 1);
        assert!(LIST.is_empty());
    });
}

#[test]
fn loom_tryarc_count() {
    tryarc_count_model();
}

#[test]
#[should_panic(expected = "Causality violation")]
fn loom_tryarc_count_relaxed_dec_fails() {
    let _w = variant::weaken(Site::TryArcDecrement);
    tryarc_count_model();
}

/// `UsersArc`'s `users` count: two holders put racing a pin.
mod users {
    use super::super::*;
    use crate::sync::variant::{Bound, Site, check};
    use loom::cell::UnsafeCell;
    use loom::sync::Arc;
    use loom::thread;

    /// Two fields the two `users` holders each write, and a teardown count.
    struct Val {
        a: UnsafeCell<u64>,
        b: UnsafeCell<u64>,
        torn: Arc<AtomicUsize>,
    }

    // SAFETY: `a` and `b` are each written by one holder before it puts its
    // reference, and read only by the teardown after the last put; the
    // models check, through loom, that the count orders the two.
    // Established here.
    unsafe impl Sync for Val {}

    impl Teardown for Val {
        fn teardown(&self) {
            assert_eq!(
                self.torn.fetch_add(1, Ordering::Relaxed),
                0,
                "users: teardown ran twice"
            );
            // SAFETY: the teardown runs after every put, so no holder still
            // writes `a`; loom reports it if the count fails to order the
            // write before this read. Established by
            // `kalloc::UsersArc`'s drop.
            let a = self.a.with(|p| unsafe { *p });
            // SAFETY: as for `a` just above, for `b`; established by
            // `kalloc::UsersArc`'s drop.
            let b = self.b.with(|p| unsafe { *p });
            assert_eq!((a, b), (1, 2));
        }
    }

    /// Main and T1 each hold a `users` reference, write their own field
    /// and put; T2 races a pin and puts it if it got one. The teardown runs
    /// exactly once, sees both writes, and no pin succeeds after it.
    fn model() {
        let torn = Arc::new(AtomicUsize::new(0));
        let u1 = UsersArc::try_new(Val {
            a: UnsafeCell::new(0),
            b: UnsafeCell::new(0),
            torn: torn.clone(),
        })
        .unwrap();
        let core = u1.core();
        let u2 = core.pin().unwrap();
        let t1 = thread::spawn(move || {
            // SAFETY: this holder is the only writer of `a`, and the value
            // lives while `u1` is held; established here.
            u1.a.with_mut(|p| unsafe { *p = 1 });
            drop(u1);
        });
        let t2 = thread::spawn(move || {
            if let Some(p) = core.pin() {
                drop(p);
            }
        });
        // SAFETY: as above, for `b` and `u2`; established here.
        u2.b.with_mut(|p| unsafe { *p = 2 });
        drop(u2);
        t1.join().unwrap();
        t2.join().unwrap();
        assert_eq!(torn.load(Ordering::Relaxed), 1);
    }

    const BOUND: Bound = Bound {
        threads: 3,
        preemptions: 3,
    };

    fn install() {
        // Every last put may release in place here, as in process context.
        set_release_context(|| true);
    }

    #[test]
    fn loom_users_arc() {
        check(None, BOUND, || {
            install();
            model();
        });
    }

    #[test]
    #[should_panic(expected = "Causality violation")]
    fn loom_users_arc_relaxed_dec_fails() {
        check(Some(Site::UsersDecrement), BOUND, || {
            install();
            model();
        });
    }

    #[test]
    #[should_panic(expected = "users: teardown ran twice")]
    fn loom_users_arc_blind_pin_fails() {
        check(Some(Site::UsersBlindPin), BOUND, || {
            install();
            model();
        });
    }
}

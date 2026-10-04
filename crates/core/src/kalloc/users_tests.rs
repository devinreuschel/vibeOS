//! Host tests of `UsersArc` and `CoreArc`, the two-count object.

extern crate std;

use super::tests::{disarm, fail_in, live};
use super::*;
use std::sync::Arc as StdArc;
use std::sync::Mutex as StdMutex;
use std::vec::Vec as StdVec;

/// Records each event on a shared log: `T` for a teardown, `D` for the
/// value's drop.
struct Probe(StdArc<StdMutex<StdVec<char>>>);

impl Teardown for Probe {
    fn teardown(&self) {
        self.0.lock().unwrap().push('T');
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.0.lock().unwrap().push('D');
    }
}

fn probe() -> (Probe, StdArc<StdMutex<StdVec<char>>>) {
    let log = StdArc::new(StdMutex::new(StdVec::with_capacity(8)));
    // On Apple targets std's Mutex allocates its pthread mutex on the first
    // lock; take it here so a test's `live()` window does not count it.
    drop(log.lock());
    (Probe(log.clone()), log)
}

fn events(log: &StdMutex<StdVec<char>>) -> StdVec<char> {
    log.lock().unwrap().clone()
}

#[test]
fn users_arc_pin_fails_after_last_put() {
    let (p, log) = probe();
    let u = UsersArc::try_new(p).unwrap();
    let core = u.core();
    let pin = core.pin().expect("pin while a users reference lives");
    assert_eq!(core.users(), 2);
    drop(u);
    assert!(events(&log).is_empty(), "a pin keeps the teardown off");
    let again = core.pin().expect("pin while the first pin lives");
    drop(pin);
    drop(again);
    assert_eq!(events(&log), ['T']);
    assert_eq!(core.users(), 0);
    assert!(core.pin().is_none(), "a pin after the last put fails");
    assert_eq!(core.users(), 0, "a failed pin leaves zero");
}

#[test]
fn users_arc_teardown_once_before_drop() {
    let (p, log) = probe();
    let u = UsersArc::try_new(p).unwrap();
    let pins: StdVec<_> = (0..4).map(|_| u.core().pin().unwrap()).collect();
    drop(u);
    for p in pins {
        assert!(events(&log).is_empty());
        drop(p);
    }
    assert_eq!(events(&log), ['T', 'D'], "one teardown, then the drop");
}

#[test]
fn users_arc_core_outlives_users() {
    let (p, log) = probe();
    let u = UsersArc::try_new(p).unwrap();
    let a = u.core();
    let b = a.clone();
    drop(u);
    assert_eq!(
        events(&log),
        ['T'],
        "the teardown runs at the last users put"
    );
    drop(a);
    assert_eq!(
        events(&log),
        ['T'],
        "the value lives while a core reference does"
    );
    drop(b);
    assert_eq!(events(&log), ['T', 'D']);
}

#[test]
fn users_arc_try_new_fails_cleanly() {
    let (p, log) = probe();
    let base = live();
    fail_in(0);
    let r = UsersArc::try_new(p);
    disarm();
    assert!(matches!(r, Err(AllocError)));
    assert_eq!(events(&log), ['D'], "the value drops; no teardown runs");
    assert_eq!(live(), base, "nothing stays allocated");
}

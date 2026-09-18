//! Kernel log: levels, filter, fixed ring. ROADMAP §5.5, DESIGN §1.3.
//!
//! Portable half. Overflow **drops oldest** (overwrite on wrap). The
//! kernel wraps this in an IRQ-off TAS so the fast path never allocates
//! and never takes SCHED. Runtime filter is an `AtomicU8` checked on
//! emit and again on `dmesg`.

use core::sync::atomic::{AtomicU8, Ordering};

/// Compile-time ceiling. Records above this are not formatted or stored.
#[cfg(debug_assertions)]
pub const COMPILE_MAX: Level = Level::Trace;
#[cfg(not(debug_assertions))]
pub const COMPILE_MAX: Level = Level::Debug;

/// Default runtime max. Boot markers are `Info`, so they land in the ring.
pub const DEFAULT_RUNTIME_MAX: Level = Level::Info;

/// Kernel ring size. Host tests use smaller consts.
pub const RING_CAP: usize = 256;
/// Bytes of message text kept per record. Longer lines truncate.
pub const MSG_CAP: usize = 96;
/// Panic dump prints this many newest records.
pub const DUMP_LAST: usize = 16;

/// Severity. Smaller is more severe. Emit if `level <= max`.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl Level {
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Error),
            1 => Some(Self::Warn),
            2 => Some(Self::Info),
            3 => Some(Self::Debug),
            4 => Some(Self::Trace),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "error" | "0" => Some(Self::Error),
            "warn" | "1" => Some(Self::Warn),
            "info" | "2" => Some(Self::Info),
            "debug" | "3" => Some(Self::Debug),
            "trace" | "4" => Some(Self::Trace),
            _ => None,
        }
    }
}

/// True when `level` should be stored / shown given compile and runtime caps.
pub const fn allowed(level: Level, runtime_max: Level, compile_max: Level) -> bool {
    (level as u8) <= (compile_max as u8) && (level as u8) <= (runtime_max as u8)
}

/// Runtime max level. Store is `Release`; emit/dmesg load `Acquire`.
pub struct Filter {
    max: AtomicU8,
}

impl Filter {
    pub const fn new(max: Level) -> Self {
        Self {
            max: AtomicU8::new(max as u8),
        }
    }

    pub fn set(&self, max: Level) {
        self.max.store(max as u8, Ordering::Release);
    }

    pub fn get(&self) -> Level {
        Level::from_u8(self.max.load(Ordering::Acquire)).unwrap_or(DEFAULT_RUNTIME_MAX)
    }

    pub fn allows(&self, level: Level) -> bool {
        allowed(level, self.get(), COMPILE_MAX)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Record<const M: usize> {
    /// Monotonic milliseconds from the seqlock clock, or raw TSC before
    /// time init (still ordered by ring position).
    pub timestamp: u64,
    pub cpu_id: u8,
    pub level: Level,
    pub len: u8,
    pub msg: [u8; M],
}

impl<const M: usize> Record<M> {
    pub const fn empty() -> Self {
        Self {
            timestamp: 0,
            cpu_id: 0,
            level: Level::Info,
            len: 0,
            msg: [0; M],
        }
    }

    pub fn from_msg(timestamp: u64, cpu_id: u8, level: Level, msg: &[u8]) -> Self {
        let mut rec = Self::empty();
        rec.timestamp = timestamp;
        rec.cpu_id = cpu_id;
        rec.level = level;
        let n = msg.len().min(M).min(255);
        rec.len = n as u8;
        rec.msg[..n].copy_from_slice(&msg[..n]);
        rec
    }

    pub fn msg(&self) -> &[u8] {
        &self.msg[..self.len as usize]
    }

    pub fn msg_str(&self) -> &str {
        core::str::from_utf8(self.msg()).unwrap_or("<bin>")
    }
}

/// Fixed ring. Wrap overwrites the oldest record.
pub struct Ring<const N: usize, const M: usize> {
    recs: [Record<M>; N],
    /// Next write index.
    head: usize,
    /// Occupied slots, `0..=N`.
    len: usize,
    /// Records overwritten by wrap.
    dropped: u64,
    /// Total successful pushes (including those later overwritten).
    written: u64,
}

impl<const N: usize, const M: usize> Ring<N, M> {
    pub const fn new() -> Self {
        Self {
            recs: [Record::empty(); N],
            head: 0,
            len: 0,
            dropped: 0,
            written: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        N
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    /// Push. If full, the oldest record is dropped.
    pub fn push(&mut self, rec: Record<M>) {
        if N == 0 {
            return;
        }
        if self.len == N {
            self.dropped = self.dropped.saturating_add(1);
        } else {
            self.len += 1;
        }
        self.recs[self.head] = rec;
        self.head = (self.head + 1) % N;
        self.written = self.written.saturating_add(1);
    }

    /// Oldest-first walk.
    pub fn get(&self, i: usize) -> Option<&Record<M>> {
        if i >= self.len {
            return None;
        }
        let idx = if self.len < N {
            i
        } else {
            (self.head + i) % N
        };
        Some(&self.recs[idx])
    }

    pub fn iter(&self) -> Iter<'_, N, M> {
        Iter { ring: self, i: 0 }
    }

    /// Newest-first, at most `n` records.
    pub fn last_n(&self, n: usize) -> LastN<'_, N, M> {
        let take = n.min(self.len);
        LastN {
            ring: self,
            remaining: take,
            // first index in oldest-first order of the newest `take`
            next: self.len - take,
        }
    }
}

pub struct Iter<'a, const N: usize, const M: usize> {
    ring: &'a Ring<N, M>,
    i: usize,
}

impl<'a, const N: usize, const M: usize> Iterator for Iter<'a, N, M> {
    type Item = &'a Record<M>;

    fn next(&mut self) -> Option<Self::Item> {
        let r = self.ring.get(self.i)?;
        self.i += 1;
        Some(r)
    }
}

pub struct LastN<'a, const N: usize, const M: usize> {
    ring: &'a Ring<N, M>,
    remaining: usize,
    next: usize,
}

impl<'a, const N: usize, const M: usize> Iterator for LastN<'a, N, M> {
    type Item = &'a Record<M>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let r = self.ring.get(self.next)?;
        self.next += 1;
        self.remaining -= 1;
        Some(r)
    }
}

/// Ring + filter used by host tests and (by value) the kernel cell.
pub struct Logger<const N: usize, const M: usize> {
    pub ring: Ring<N, M>,
    pub filter: Filter,
}

impl<const N: usize, const M: usize> Logger<N, M> {
    pub const fn new() -> Self {
        Self {
            ring: Ring::new(),
            filter: Filter::new(DEFAULT_RUNTIME_MAX),
        }
    }

    /// Store if compile-time and runtime filters allow. Returns whether stored.
    pub fn emit(&mut self, rec: Record<M>) -> bool {
        if !self.filter.allows(rec.level) {
            return false;
        }
        self.ring.push(rec);
        true
    }

    /// `dmesg` view: records that pass `view` (and compile max).
    pub fn visible<'a>(&'a self, view: Level) -> impl Iterator<Item = &'a Record<M>> + 'a {
        self.ring
            .iter()
            .filter(move |r| allowed(r.level, view, COMPILE_MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(ts: u64, lvl: Level, msg: &str) -> Record<8> {
        Record::from_msg(ts, 0, lvl, msg.as_bytes())
    }

    #[test]
    fn levels_are_ordered_by_severity() {
        assert!(Level::Error < Level::Warn);
        assert!(Level::Warn < Level::Info);
        assert!(Level::Info < Level::Debug);
        assert!(Level::Debug < Level::Trace);
        assert_eq!(Level::from_u8(2), Some(Level::Info));
        assert_eq!(Level::from_u8(9), None);
        assert_eq!(Level::Error.as_str(), "error");
        assert_eq!(Level::Trace.as_str(), "trace");
        assert_eq!(Level::from_name("debug"), Some(Level::Debug));
        assert_eq!(Level::from_name("3"), Some(Level::Debug));
        assert_eq!(Level::from_name("nope"), None);
    }

    #[test]
    fn filter_hides_above_runtime_max() {
        let f = Filter::new(Level::Info);
        assert!(f.allows(Level::Error));
        assert!(f.allows(Level::Info));
        assert!(!f.allows(Level::Debug));
        f.set(Level::Trace);
        assert!(f.allows(Level::Debug));
        f.set(Level::Error);
        assert!(f.allows(Level::Error));
        assert!(!f.allows(Level::Warn));
    }

    #[test]
    fn wrap_drops_oldest() {
        let mut r: Ring<4, 8> = Ring::new();
        r.push(rec(1, Level::Info, "a"));
        r.push(rec(2, Level::Info, "b"));
        r.push(rec(3, Level::Info, "c"));
        r.push(rec(4, Level::Info, "d"));
        assert_eq!(r.len(), 4);
        assert_eq!(r.dropped(), 0);
        r.push(rec(5, Level::Info, "e"));
        assert_eq!(r.len(), 4);
        assert_eq!(r.dropped(), 1);
        let msgs: Vec<&str> = r.iter().map(|x| x.msg_str()).collect();
        assert_eq!(msgs, ["b", "c", "d", "e"]);
        r.push(rec(6, Level::Info, "f"));
        let msgs: Vec<&str> = r.iter().map(|x| x.msg_str()).collect();
        assert_eq!(msgs, ["c", "d", "e", "f"]);
        assert_eq!(r.dropped(), 2);
        assert_eq!(r.written(), 6);
    }

    #[test]
    fn last_n_is_newest() {
        let mut r: Ring<4, 8> = Ring::new();
        for (i, m) in ["a", "b", "c", "d", "e"].iter().enumerate() {
            r.push(rec(i as u64, Level::Info, m));
        }
        let msgs: Vec<&str> = r.last_n(3).map(|x| x.msg_str()).collect();
        assert_eq!(msgs, ["c", "d", "e"]);
        let all: Vec<&str> = r.last_n(99).map(|x| x.msg_str()).collect();
        assert_eq!(all, ["b", "c", "d", "e"]);
    }

    #[test]
    fn emit_and_dmesg_honor_runtime_filter() {
        let mut log: Logger<8, 8> = Logger::new();
        assert!(log.emit(rec(1, Level::Info, "boot")));
        assert!(!log.emit(rec(2, Level::Debug, "dbg")));
        assert_eq!(log.ring.len(), 1);
        log.filter.set(Level::Debug);
        assert!(log.emit(rec(3, Level::Debug, "dbg")));
        assert_eq!(log.ring.len(), 2);
        // dmesg at Info hides the debug record still in the ring
        let info: Vec<&str> = log.visible(Level::Info).map(|r| r.msg_str()).collect();
        assert_eq!(info, ["boot"]);
        let dbg: Vec<&str> = log.visible(Level::Debug).map(|r| r.msg_str()).collect();
        assert_eq!(dbg, ["boot", "dbg"]);
        log.filter.set(Level::Error);
        assert!(!log.emit(rec(4, Level::Warn, "w")));
        assert_eq!(log.ring.len(), 2);
    }

    #[test]
    fn truncates_long_messages() {
        let r = Record::<4>::from_msg(0, 1, Level::Warn, b"hello");
        assert_eq!(r.msg(), b"hell");
        assert_eq!(r.cpu_id, 1);
        assert_eq!(r.level, Level::Warn);
    }

    #[test]
    fn empty_ring_iterates_nothing() {
        let r: Ring<4, 8> = Ring::new();
        assert_eq!(r.iter().count(), 0);
        assert_eq!(r.last_n(4).count(), 0);
        assert!(r.get(0).is_none());
    }

    #[test]
    fn compile_max_is_at_least_info() {
        assert!(COMPILE_MAX >= Level::Info);
        assert!(allowed(Level::Info, Level::Info, COMPILE_MAX));
    }
}

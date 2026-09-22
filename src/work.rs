//! Work item ring. ROADMAP §6.6.
//!
//! Kernel threads consume these. Enqueue is allocation-free so a hard IRQ
//! can raise the softirq-equivalent (high-prio ring) without blocking.

#[derive(Clone, Copy, Debug)]
pub struct WorkItem {
    pub func: fn(usize),
    pub arg: usize,
}

impl WorkItem {
    pub const EMPTY: Self = Self { func: nop, arg: 0 };

    pub fn new(func: fn(usize), arg: usize) -> Self {
        Self { func, arg }
    }

    pub fn run(self) {
        (self.func)(self.arg);
    }
}

fn nop(_: usize) {}

pub const RING_CAP: usize = 32;

/// Power-of-two ring. `head` is next pop, `tail` is next push.
#[derive(Clone, Copy)]
pub struct WorkRing {
    slots: [WorkItem; RING_CAP],
    head: u32,
    tail: u32,
}

impl WorkRing {
    pub const fn new() -> Self {
        Self {
            slots: [WorkItem::EMPTY; RING_CAP],
            head: 0,
            tail: 0,
        }
    }

    pub fn len(self) -> usize {
        self.tail.wrapping_sub(self.head) as usize
    }

    pub fn is_empty(self) -> bool {
        self.head == self.tail
    }

    pub fn is_full(self) -> bool {
        self.len() >= RING_CAP
    }

    pub fn push(&mut self, item: WorkItem) -> bool {
        if self.is_full() {
            return false;
        }
        self.slots[(self.tail as usize) % RING_CAP] = item;
        self.tail = self.tail.wrapping_add(1);
        true
    }

    pub fn pop(&mut self) -> Option<WorkItem> {
        if self.is_empty() {
            return None;
        }
        let item = self.slots[(self.head as usize) % RING_CAP];
        self.head = self.head.wrapping_add(1);
        Some(item)
    }
}

impl Default for WorkRing {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy)]
pub struct WorkQueues {
    pub hi: WorkRing,
    pub norm: WorkRing,
}

impl WorkQueues {
    pub const fn new() -> Self {
        Self {
            hi: WorkRing::new(),
            norm: WorkRing::new(),
        }
    }

    pub fn push_hi(&mut self, item: WorkItem) -> bool {
        self.hi.push(item)
    }

    pub fn push(&mut self, item: WorkItem) -> bool {
        self.norm.push(item)
    }

    pub fn pop(&mut self) -> Option<WorkItem> {
        self.hi.pop().or_else(|| self.norm.pop())
    }

    pub fn is_empty(self) -> bool {
        self.hi.is_empty() && self.norm.is_empty()
    }
}

impl Default for WorkQueues {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};

    static HITS: AtomicUsize = AtomicUsize::new(0);

    fn mark(arg: usize) {
        HITS.fetch_add(arg, Ordering::SeqCst);
    }

    #[test]
    fn fifo_and_full() {
        let mut r = WorkRing::new();
        assert!(r.is_empty());
        assert!(r.push(WorkItem::new(mark, 1)));
        assert!(r.push(WorkItem::new(mark, 2)));
        assert_eq!(r.len(), 2);
        assert_eq!(r.pop().unwrap().arg, 1);
        assert_eq!(r.pop().unwrap().arg, 2);
        assert!(r.pop().is_none());
        let mut i = 0usize;
        while i < RING_CAP {
            assert!(r.push(WorkItem::new(mark, i)));
            i += 1;
        }
        assert!(r.is_full());
        assert!(!r.push(WorkItem::new(mark, 99)));
        assert_eq!(r.pop().unwrap().arg, 0);
        assert!(r.push(WorkItem::new(mark, 100)));
    }

    #[test]
    fn hi_before_norm() {
        HITS.store(0, Ordering::SeqCst);
        let mut q = WorkQueues::new();
        assert!(q.push(WorkItem::new(mark, 1)));
        assert!(q.push_hi(WorkItem::new(mark, 10)));
        q.pop().unwrap().run();
        q.pop().unwrap().run();
        assert_eq!(HITS.load(Ordering::SeqCst), 11);
        assert!(q.is_empty());
        WorkItem::EMPTY.run();
    }

    #[test]
    fn wrap_index() {
        let mut r = WorkRing::new();
        let mut i = 0u32;
        while i < RING_CAP as u32 * 3 {
            assert!(r.push(WorkItem::new(mark, i as usize)));
            assert_eq!(r.pop().unwrap().arg, i as usize);
            i += 1;
        }
        assert!(r.is_empty());
    }
}

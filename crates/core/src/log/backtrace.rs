//! The panic backtrace's frame-pointer walk (DESIGN §2.5 step 4, ROADMAP
//! §10.7, F139).
//!
//! The walk follows `rbp` only into a known stack: each frame record's 16
//! bytes lie inside one [`StackRange`] the kernel names (the current
//! thread's stack, the boot stack, this CPU's IST and RSP0 stacks), so it
//! never reads an MMIO window or a user page. It reads through a closure
//! only after that check, and ends on a null `rbp` (the syscall entry
//! zeroes it before the Rust body), an unknown stack, a return address
//! outside the kernel image, a frame that does not rise, or [`DEPTH_CAP`].
//! Arithmetic is checked: a walker panic would re-enter the dump.

/// Frames the walk prints at most.
pub const DEPTH_CAP: usize = 24;

/// A stack's bytes `[lo, hi)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StackRange {
    pub lo: u64,
    pub hi: u64,
}

impl StackRange {
    /// An empty range, which holds no frame.
    pub const EMPTY: StackRange = StackRange { lo: 0, hi: 0 };

    pub const fn new(lo: u64, hi: u64) -> Self {
        Self { lo, hi }
    }

    /// The `pages` 4 KiB pages below `top`, or [`StackRange::EMPTY`] when
    /// they would wrap.
    pub const fn below(top: u64, pages: usize) -> Self {
        let bytes = (pages as u64).saturating_mul(4096);
        match top.checked_sub(bytes) {
            Some(lo) => Self { lo, hi: top },
            None => Self::EMPTY,
        }
    }

    /// Whether `[addr, addr + len)` lies inside this range.
    pub const fn holds(&self, addr: u64, len: u64) -> bool {
        match addr.checked_add(len) {
            Some(end) => self.lo <= addr && end <= self.hi,
            None => false,
        }
    }
}

/// Bytes of one frame record: the saved `rbp`, then the return address.
const RECORD: u64 = 16;

/// The range among `stacks` that holds `rbp`'s whole frame record.
fn range_of(rbp: u64, stacks: &[StackRange]) -> Option<usize> {
    if rbp == 0 || rbp & 7 != 0 {
        return None;
    }
    stacks.iter().position(|s| s.holds(rbp, RECORD))
}

/// Whether `rbp` is 8-aligned and its frame record `[rbp, rbp + 16)` lies
/// inside one of `stacks`.
pub fn on_known_stack(rbp: u64, stacks: &[StackRange]) -> bool {
    range_of(rbp, stacks).is_some()
}

/// Why a walk ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkEnd {
    /// A null `rbp`: the bottom of a chain, the syscall boundary included.
    NullRbp,
    /// An `rbp` whose frame record is on no known stack.
    UnknownStack,
    /// A return address (or the first `rip`) outside the kernel image.
    OutsideImage,
    /// A saved `rbp` at or below its frame on the same stack: a loop.
    NotRising,
    /// [`DEPTH_CAP`] frames printed.
    DepthCap,
}

/// Walk from `rip` and its frame pointer `rbp`: `out` gets each frame's
/// return address, `rip` first, and the result is the count and why the
/// walk ended. `read` loads one word, and is called only for an address
/// inside `stacks` (both words of a record [`on_known_stack`] accepted).
/// A first `rip` outside the image (`in_image`) is still printed once
/// when nonzero, since it names where the fault was.
pub fn walk(
    rip: u64,
    rbp: u64,
    stacks: &[StackRange],
    in_image: impl Fn(u64) -> bool,
    mut read: impl FnMut(u64) -> u64,
    mut out: impl FnMut(u64),
) -> (usize, WalkEnd) {
    if !in_image(rip) {
        if rip == 0 {
            return (0, WalkEnd::OutsideImage);
        }
        out(rip);
        return (1, WalkEnd::OutsideImage);
    }
    let mut rip = rip;
    let mut rbp = rbp;
    let mut n = 0usize;
    loop {
        out(rip);
        n = n.saturating_add(1);
        if n >= DEPTH_CAP {
            return (n, WalkEnd::DepthCap);
        }
        if rbp == 0 {
            return (n, WalkEnd::NullRbp);
        }
        let Some(here) = range_of(rbp, stacks) else {
            return (n, WalkEnd::UnknownStack);
        };
        let prev = read(rbp);
        let Some(ret_at) = rbp.checked_add(8) else {
            return (n, WalkEnd::UnknownStack);
        };
        let ret = read(ret_at);
        if !in_image(ret) {
            return (n, WalkEnd::OutsideImage);
        }
        // A caller's frame is higher on the same stack; one on another
        // known stack (an IST frame's interrupted thread) may be anywhere.
        let same_stack = stacks.get(here).is_some_and(|s| s.holds(prev, RECORD));
        if prev != 0 && same_stack && prev <= rbp {
            return (n, WalkEnd::NotRising);
        }
        rip = ret;
        rbp = prev;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;
    use std::vec::Vec;

    const IMAGE: core::ops::Range<u64> = 0xFFFF_FFFF_8000_0000..0xFFFF_FFFF_8100_0000;
    const STACK: StackRange = StackRange::new(0xFFFF_E000_0010_0000, 0xFFFF_E000_0010_4000);

    fn in_image(a: u64) -> bool {
        IMAGE.contains(&a)
    }

    /// A fake stack: word `(addr - STACK.lo) / 8`.
    struct Mem(Vec<u64>);

    impl Mem {
        fn new() -> Self {
            Mem(std::vec![0; ((STACK.hi - STACK.lo) / 8) as usize])
        }
        fn set(&mut self, addr: u64, v: u64) {
            let i = ((addr - STACK.lo) / 8) as usize;
            self.0[i] = v;
        }
        /// A frame record at `rbp`: saved `prev`, return `ret`.
        fn frame(&mut self, rbp: u64, prev: u64, ret: u64) {
            self.set(rbp, prev);
            self.set(rbp + 8, ret);
        }
        fn read(&self, stacks: &[StackRange], addr: u64) -> u64 {
            assert!(
                stacks.iter().any(|s| s.holds(addr, 8)),
                "read {addr:#x} outside every known stack"
            );
            self.0[((addr - STACK.lo) / 8) as usize]
        }
    }

    fn run(mem: &Mem, stacks: &[StackRange], rip: u64, rbp: u64) -> (Vec<u64>, WalkEnd) {
        let mut frames = Vec::new();
        let (n, end) = walk(
            rip,
            rbp,
            stacks,
            in_image,
            |a| mem.read(stacks, a),
            |r| frames.push(r),
        );
        assert_eq!(n, frames.len());
        (frames, end)
    }

    #[test]
    fn known_stack_accepts_inside() {
        let s = [STACK];
        assert!(on_known_stack(STACK.lo, &s));
        assert!(on_known_stack(STACK.lo + 0x100, &s));
        assert!(on_known_stack(STACK.hi - 16, &s));
        let two = [StackRange::EMPTY, STACK];
        assert!(on_known_stack(STACK.lo + 8, &two));
        assert_eq!(StackRange::below(STACK.hi, 4), STACK);
        assert_eq!(StackRange::below(0x1000, 2), StackRange::EMPTY);
    }

    #[test]
    fn known_stack_rejects_ioremap_and_low_user() {
        let s = [STACK];
        // What the old range check took: the ioremap window, low identity memory,
        // any user address.
        assert!(!on_known_stack(0xFFFF_E000_0000_0000, &s));
        assert!(!on_known_stack(0xFFFF_C000_0000_1000, &s));
        assert!(!on_known_stack(0x1000, &s));
        assert!(!on_known_stack(0x1FFF_FFF0, &s));
        assert!(!on_known_stack(0x4000_0800, &s));
        assert!(!on_known_stack(0, &s));
        assert!(!on_known_stack(STACK.lo + 4, &s), "unaligned");
        assert!(!on_known_stack(STACK.lo, &[]));
        assert!(!on_known_stack(0, &[StackRange::new(0, 64)]));
        assert!(!on_known_stack(8, &[StackRange::EMPTY]));
    }

    #[test]
    fn known_stack_rejects_straddle() {
        let s = [STACK];
        assert!(!on_known_stack(STACK.hi - 8, &s));
        assert!(!on_known_stack(STACK.hi, &s));
        assert!(!on_known_stack(STACK.lo - 8, &s));
        // Two adjacent ranges: a record across their seam is in neither.
        let a = StackRange::new(0x10_0000, 0x10_1000);
        let b = StackRange::new(0x10_1000, 0x10_2000);
        assert!(!on_known_stack(0x10_0FF8, &[a, b]));
        assert!(on_known_stack(0x10_0FF0, &[a, b]));
        // Near the top of the address space the end does not wrap.
        let top = StackRange::new(u64::MAX - 0xFFF, u64::MAX);
        assert!(!on_known_stack(u64::MAX - 7, &[top]));
        assert!(!top.holds(u64::MAX, 16));
    }

    #[test]
    fn walk_ends_at_null_rbp() {
        let mut m = Mem::new();
        let f0 = STACK.lo + 0x100;
        let f1 = STACK.lo + 0x200;
        let f2 = STACK.lo + 0x300;
        m.frame(f0, f1, IMAGE.start + 0x10);
        m.frame(f1, f2, IMAGE.start + 0x20);
        // The syscall entry zeroed rbp before its call: the last record
        // saves 0 and returns into the entry.
        m.frame(f2, 0, IMAGE.start + 0x30);
        let (frames, end) = run(&m, &[STACK], IMAGE.start + 1, f0);
        assert_eq!(end, WalkEnd::NullRbp);
        assert_eq!(
            frames,
            [
                IMAGE.start + 1,
                IMAGE.start + 0x10,
                IMAGE.start + 0x20,
                IMAGE.start + 0x30
            ]
        );
        let (frames, end) = run(&m, &[STACK], IMAGE.start + 1, 0);
        assert_eq!((frames.len(), end), (1, WalkEnd::NullRbp));
    }

    #[test]
    fn walk_never_reads_outside_known_stacks() {
        let mut m = Mem::new();
        let f0 = STACK.lo + 0x100;
        // A chain that leaves for the ioremap window, a user page, and a
        // straddling record: each ends the walk before any read there.
        for bad in [
            0xFFFF_E000_0000_0000,
            0x4000_0800,
            STACK.hi - 8,
            STACK.lo + 0x104,
        ] {
            m.frame(f0, bad, IMAGE.start + 0x10);
            let (frames, end) = run(&m, &[STACK], IMAGE.start + 1, f0);
            assert_eq!(end, WalkEnd::UnknownStack, "{bad:#x}");
            assert_eq!(frames, [IMAGE.start + 1, IMAGE.start + 0x10]);
        }
        // A first rbp off every stack reads nothing.
        let (frames, end) = run(&m, &[STACK], IMAGE.start + 1, 0x2000);
        assert_eq!((frames.len(), end), (1, WalkEnd::UnknownStack));
        let (frames, end) = run(&m, &[], IMAGE.start + 1, f0);
        assert_eq!((frames.len(), end), (1, WalkEnd::UnknownStack));
        // A return address outside the image ends it unprinted.
        m.frame(f0, STACK.lo + 0x200, 0x1234);
        let (frames, end) = run(&m, &[STACK], IMAGE.start + 1, f0);
        assert_eq!(
            (frames, end),
            (std::vec![IMAGE.start + 1], WalkEnd::OutsideImage)
        );
        // A first rip outside the image prints once and reads nothing.
        let (frames, end) = run(&m, &[STACK], 0x40_0000, f0);
        assert_eq!((frames, end), (std::vec![0x40_0000], WalkEnd::OutsideImage));
        let (frames, end) = run(&m, &[STACK], 0, f0);
        assert_eq!((frames.len(), end), (0, WalkEnd::OutsideImage));
    }

    #[test]
    fn walk_rejects_loop_and_caps_depth() {
        let mut m = Mem::new();
        let f0 = STACK.lo + 0x100;
        m.frame(f0, f0, IMAGE.start + 0x10);
        let (frames, end) = run(&m, &[STACK], IMAGE.start + 1, f0);
        assert_eq!((frames.len(), end), (1, WalkEnd::NotRising));
        let f1 = STACK.lo + 0x200;
        m.frame(f1, f0, IMAGE.start + 0x20);
        let (_, end) = run(&m, &[STACK], IMAGE.start + 1, f1);
        assert_eq!(end, WalkEnd::NotRising);
        // A rising chain longer than the cap stops at it.
        for i in 0..40u64 {
            let at = STACK.lo + 0x100 + i * 0x80;
            m.frame(at, at + 0x80, IMAGE.start + 0x100 + i);
        }
        let (frames, end) = run(&m, &[STACK], IMAGE.start + 1, STACK.lo + 0x100);
        assert_eq!((frames.len(), end), (DEPTH_CAP, WalkEnd::DepthCap));
        // A caller's frame on another known stack may be lower.
        let low = StackRange::new(0x10_0000, 0x10_1000);
        let mut words = std::collections::BTreeMap::new();
        words.insert(STACK.lo + 0x100, 0x10_0800u64);
        words.insert(STACK.lo + 0x108, IMAGE.start + 0x10);
        words.insert(0x10_0800, 0);
        words.insert(0x10_0808, IMAGE.start + 0x20);
        let stacks = [STACK, low];
        let mut frames = Vec::new();
        let (_, end) = walk(
            IMAGE.start + 1,
            STACK.lo + 0x100,
            &stacks,
            in_image,
            |a| {
                assert!(stacks.iter().any(|s| s.holds(a, 8)));
                words.get(&a).copied().unwrap_or(0)
            },
            |r| frames.push(r),
        );
        assert_eq!(end, WalkEnd::NullRbp);
        assert_eq!(
            frames,
            [IMAGE.start + 1, IMAGE.start + 0x10, IMAGE.start + 0x20]
        );
    }
}

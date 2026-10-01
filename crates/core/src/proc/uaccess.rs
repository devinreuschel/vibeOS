//! User-memory access, portable half (INTERRUPTS §5.1, ROADMAP §10.6).
//!
//! Every copy between kernel and user memory runs [`user_range_ok`] first,
//! because inside the port's user-access window (`stac`/`clac`, PAN) the MMU
//! still lets the kernel reach its own pages. The port's [`UserAccess`]
//! methods then dereference the user address, so the page tables' present
//! and writable bits apply, and a fault there resumes at the exception-table
//! fixup ([`ExEntry`], [`search`]) with the count of bytes left uncopied.

use core::mem::size_of;

pub use zerocopy::{Immutable, IntoBytes};

use crate::arch::UserAccess;
use crate::paging::{NULL_GUARD_LEN, PAGE_SIZE_4K, USER_MAP_END};

/// A user copy that the range check refused or that faulted before its end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault;

impl From<Fault> for crate::kerror::KError {
    fn from(_: Fault) -> Self {
        Self::Fault
    }
}

/// Whether `addr..addr + len` may be handed to the port's user copy: a
/// non-empty range from `NULL_GUARD_LEN` up to at most `USER_MAP_END`
/// without overflow, or an empty range below `USER_MAP_END` (address 0
/// included, as Linux's `access_ok`). Canonical and user-half follow from
/// `USER_MAP_END`. Pure: no page-table walk.
pub const fn user_range_ok(addr: u64, len: u64) -> bool {
    if len == 0 {
        return addr < USER_MAP_END;
    }
    match addr.checked_add(len) {
        Some(end) => addr >= NULL_GUARD_LEN && end <= USER_MAP_END,
        None => false,
    }
}

/// One exception-table record, as the port's accessors emit it into
/// `__ex_table`: `insn` and `fixup` are offsets from each field's own
/// address to the faulting instruction and to where it resumes, and bit 0
/// of `data` is the [`ExKind`].
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExEntry {
    pub insn: i32,
    pub fixup: i32,
    pub data: u32,
}

const _: () = assert!(size_of::<ExEntry>() == 12);

/// What a fault at an entry's instruction means (INTERRUPTS §5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExKind {
    /// A copy that may fault: resume at the fixup with the bytes left.
    /// Bit 0 clear; every entry today.
    Faulting,
    /// ROADMAP §12.5's non-faulting accessor. Bit 0 set.
    NonFaulting,
}

impl ExKind {
    /// The kind in bit 0 of an entry's `data`.
    pub const fn from_data(data: u32) -> Self {
        if data & 1 == 0 {
            Self::Faulting
        } else {
            Self::NonFaulting
        }
    }
}

impl ExEntry {
    /// The faulting instruction's address. `self` is the record in the
    /// table, since the offset is from `insn`'s own address.
    pub fn insn_addr(&self) -> u64 {
        (core::ptr::addr_of!(self.insn) as usize as u64).wrapping_add_signed(i64::from(self.insn))
    }

    /// The fixup's address, from `fixup`'s own address.
    pub fn fixup_addr(&self) -> u64 {
        (core::ptr::addr_of!(self.fixup) as usize as u64).wrapping_add_signed(i64::from(self.fixup))
    }
}

/// The fixup and kind of the entry whose instruction is at `rip`. A linear
/// scan that takes no lock, so a fault handler may run it on any CPU.
pub fn search(table: &[ExEntry], rip: u64) -> Option<(u64, ExKind)> {
    table
        .iter()
        .find(|e| e.insn_addr() == rip)
        .map(|e| (e.fixup_addr(), ExKind::from_data(e.data)))
}

/// Bytes of `n` that a port reports copied when it left `left` behind.
const fn copied(n: usize, left: usize) -> usize {
    n.saturating_sub(left)
}

/// Copy `dst.len()` bytes from user address `src`, all or nothing: the
/// range check, then the port's copy inside its user-access window.
pub fn copy_from_user<A: UserAccess>(dst: &mut [u8], src: u64) -> Result<(), Fault> {
    if user_range_ok(src, dst.len() as u64) && copy_from_user_partial::<A>(dst, src) == dst.len() {
        Ok(())
    } else {
        Err(Fault)
    }
}

/// Copy `src` to user address `dst`, all or nothing.
pub fn copy_to_user<A: UserAccess>(dst: u64, src: &[u8]) -> Result<(), Fault> {
    if user_range_ok(dst, src.len() as u64) && copy_to_user_partial::<A>(dst, src) == src.len() {
        Ok(())
    } else {
        Err(Fault)
    }
}

/// Copy `v` to user address `dst`, all or nothing: the typed form of
/// [`copy_to_user`]. `IntoBytes` refuses at compile time a type with
/// padding or uninitialized bytes, so no kernel byte leaks through a hole
/// (INVARIANTS §2.4). A uapi struct whose Linux layout has an implicit
/// hole declares it as an explicit field the kernel zeroes.
///
/// A `#[repr(C)]` struct with an implicit hole does not build:
///
/// ```compile_fail
/// use vibeos::proc::uaccess::copy_to_user_val;
/// use zerocopy::{Immutable, IntoBytes};
///
/// #[derive(IntoBytes, Immutable)]
/// #[repr(C)]
/// struct Hole {
///     a: u8,
///     b: u32,
/// }
///
/// let _ = copy_to_user_val::<vibeos::arch::stub::Arch, _>(0x4000_0000, &Hole { a: 1, b: 2 });
/// ```
///
/// The same struct with the hole as an explicit field does:
///
/// ```no_run
/// use vibeos::proc::uaccess::copy_to_user_val;
/// use zerocopy::{Immutable, IntoBytes};
///
/// #[derive(IntoBytes, Immutable)]
/// #[repr(C)]
/// struct Hole {
///     a: u8,
///     _pad: [u8; 3],
///     b: u32,
/// }
///
/// let _ = copy_to_user_val::<vibeos::arch::stub::Arch, _>(
///     0x4000_0000,
///     &Hole { a: 1, _pad: [0; 3], b: 2 },
/// );
/// ```
pub fn copy_to_user_val<A: UserAccess, T: IntoBytes + Immutable>(
    dst: u64,
    v: &T,
) -> Result<(), Fault> {
    copy_to_user::<A>(dst, v.as_bytes())
}

/// Copy from user address `src` into `dst` and return the bytes copied
/// before the first fault: 0 for a refused range.
pub fn copy_from_user_partial<A: UserAccess>(dst: &mut [u8], src: u64) -> usize {
    let n = dst.len();
    if !user_range_ok(src, n as u64) {
        return 0;
    }
    if n == 0 {
        return 0;
    }
    // SAFETY: `dst` is `n` bytes of kernel memory this fn borrows mutably,
    // the `# Safety` contract of `vibeos::arch::UserAccess::copy_in`;
    // established here.
    let left = unsafe { A::copy_in(dst.as_mut_ptr(), src, n) };
    copied(n, left)
}

/// Copy `src` to user address `dst` and return the bytes copied before the
/// first fault: 0 for a refused range.
pub fn copy_to_user_partial<A: UserAccess>(dst: u64, src: &[u8]) -> usize {
    let n = src.len();
    if !user_range_ok(dst, n as u64) {
        return 0;
    }
    if n == 0 {
        return 0;
    }
    // SAFETY: `src` is `n` bytes of kernel memory this fn borrows, the
    // `# Safety` contract of `vibeos::arch::UserAccess::copy_out`;
    // established here.
    let left = unsafe { A::copy_out(dst, src.as_ptr(), n) };
    copied(n, left)
}

/// Copy the NUL-terminated string at user address `src` into `out` and
/// return its length without the NUL. `Ok(out.len())` means `out` filled
/// before a NUL. Reads in chunks that stop at each page boundary and at
/// `USER_MAP_END`, so a string that ends before an unmapped page copies
/// without touching it.
pub fn strncpy_from_user<A: UserAccess>(out: &mut [u8], src: u64) -> Result<usize, Fault> {
    let mut pos = 0usize;
    while pos < out.len() {
        let va = src.checked_add(pos as u64).ok_or(Fault)?;
        if va >= USER_MAP_END {
            return Err(Fault);
        }
        let page_left = PAGE_SIZE_4K - (va & (PAGE_SIZE_4K - 1));
        let room = (out.len() - pos) as u64;
        let chunk = room.min(page_left).min(USER_MAP_END - va) as usize;
        let Some(buf) = out.get_mut(pos..pos + chunk) else {
            return Err(Fault);
        };
        copy_from_user::<A>(buf, va)?;
        if let Some(i) = buf.iter().position(|&b| b == 0) {
            return Ok(pos + i);
        }
        pos += chunk;
    }
    Ok(out.len())
}

#[cfg(test)]
mod tests {
    extern crate std;

    use core::cell::RefCell;
    use std::vec;
    use std::vec::Vec;

    use super::*;

    /// A test port over one window of fake user memory: `base..base +
    /// mem.len()` is mapped, and every byte from `fault_at` on faults.
    struct Fake;

    struct Mem {
        base: u64,
        mem: Vec<u8>,
        fault_at: u64,
        calls: usize,
    }

    std::thread_local! {
        static MEM: RefCell<Mem> = const {
            RefCell::new(Mem { base: 0, mem: Vec::new(), fault_at: 0, calls: 0 })
        };
    }

    fn setup(base: u64, len: usize, fault_at: u64) {
        MEM.with(|m| {
            *m.borrow_mut() = Mem {
                base,
                mem: vec![0; len],
                fault_at,
                calls: 0,
            }
        });
    }

    fn poke(va: u64, bytes: &[u8]) {
        MEM.with(|m| {
            let mut m = m.borrow_mut();
            let off = (va - m.base) as usize;
            m.mem[off..off + bytes.len()].copy_from_slice(bytes);
        });
    }

    fn peek(va: u64, len: usize) -> Vec<u8> {
        MEM.with(|m| {
            let m = m.borrow();
            let off = (va - m.base) as usize;
            m.mem[off..off + len].to_vec()
        })
    }

    fn calls() -> usize {
        MEM.with(|m| m.borrow().calls)
    }

    /// Bytes from `va` the fake can copy before a fault, at most `len`.
    fn reach(m: &Mem, va: u64, len: usize) -> usize {
        let end = m.fault_at.min(m.base + m.mem.len() as u64);
        if va < m.base || va >= end {
            return 0;
        }
        ((end - va) as usize).min(len)
    }

    impl UserAccess for Fake {
        unsafe fn copy_in(dst: *mut u8, src: u64, len: usize) -> usize {
            MEM.with(|m| {
                let mut m = m.borrow_mut();
                m.calls += 1;
                let n = reach(&m, src, len);
                let off = (src.wrapping_sub(m.base)) as usize;
                for i in 0..n {
                    // SAFETY: `dst` has `len` writable bytes and `i < n <=
                    // len`, the `# Safety` contract of
                    // `vibeos::arch::UserAccess::copy_in`; established here by
                    // the caller's unsafe call.
                    unsafe { *dst.add(i) = m.mem[off + i] };
                }
                len - n
            })
        }
        unsafe fn copy_out(dst: u64, src: *const u8, len: usize) -> usize {
            MEM.with(|m| {
                let mut m = m.borrow_mut();
                m.calls += 1;
                let n = reach(&m, dst, len);
                let off = (dst.wrapping_sub(m.base)) as usize;
                for i in 0..n {
                    // SAFETY: `src` has `len` readable bytes and `i < n <=
                    // len`, the `# Safety` contract of
                    // `vibeos::arch::UserAccess::copy_out`; established here by
                    // the caller's unsafe call.
                    m.mem[off + i] = unsafe { *src.add(i) };
                }
                len - n
            })
        }
    }

    const BASE: u64 = 0x4000_0000;
    const P: u64 = PAGE_SIZE_4K;

    #[test]
    fn user_range_ok_bounds() {
        assert!(user_range_ok(0, 0));
        assert!(user_range_ok(USER_MAP_END - 1, 0));
        assert!(!user_range_ok(USER_MAP_END, 0));
        assert!(!user_range_ok(NULL_GUARD_LEN - 1, 1));
        assert!(user_range_ok(NULL_GUARD_LEN, 1));
        assert!(!user_range_ok(NULL_GUARD_LEN - 1, 2));
        assert!(user_range_ok(NULL_GUARD_LEN, 2));
        assert!(user_range_ok(USER_MAP_END - 1, 1));
        assert!(!user_range_ok(USER_MAP_END, 1));
        assert!(user_range_ok(USER_MAP_END - 2, 2));
        assert!(!user_range_ok(USER_MAP_END - 1, 2));
        assert!(!user_range_ok(u64::MAX, 2));
        assert!(!user_range_ok(2, u64::MAX));
        assert!(!user_range_ok(0xFFFF_8000_0000_1000, 8));
        assert!(!user_range_ok(0xFFFF_8000_0000_1000, 0));
        assert!(!user_range_ok(0x0000_8000_0000_0000, 8));
        assert!(!user_range_ok(0x0000_8000_0000_0000, 0));
    }

    /// Entries whose offsets point `insn_off` and `fixup_off` bytes past
    /// each field's own address.
    fn entry_to(table: &mut [ExEntry], i: usize, insn: u64, fixup: u64, data: u32) {
        let e = &mut table[i];
        let ia = core::ptr::addr_of!(e.insn) as usize as u64;
        let fa = core::ptr::addr_of!(e.fixup) as usize as u64;
        e.insn = (insn.wrapping_sub(ia)) as i64 as i32;
        e.fixup = (fixup.wrapping_sub(fa)) as i64 as i32;
        e.data = data;
    }

    #[test]
    fn extable_search_hits_and_misses() {
        let mut t = [ExEntry {
            insn: 0,
            fixup: 0,
            data: 0,
        }; 3];
        let here = t.as_ptr() as usize as u64;
        entry_to(&mut t, 0, here + 0x100, here + 0x110, 0);
        entry_to(&mut t, 1, here - 0x200, here - 0x1F0, 0);
        entry_to(&mut t, 2, here + 0x300, here + 0x310, 0);
        assert_eq!(
            search(&t, here + 0x100),
            Some((here + 0x110, ExKind::Faulting))
        );
        assert_eq!(
            search(&t, here - 0x200),
            Some((here - 0x1F0, ExKind::Faulting))
        );
        assert_eq!(
            search(&t, here + 0x300),
            Some((here + 0x310, ExKind::Faulting))
        );
        assert_eq!(search(&t, here + 0x101), None);
        assert_eq!(search(&t, here + 0x110), None);
        assert_eq!(search(&[], here + 0x100), None);
        // The same offsets in another entry name other addresses.
        assert_eq!(t[0].insn_addr(), here + 0x100);
        assert_eq!(t[2].fixup_addr(), here + 0x310);
    }

    #[test]
    fn extable_kind_bit() {
        assert_eq!(ExKind::from_data(0), ExKind::Faulting);
        assert_eq!(ExKind::from_data(1), ExKind::NonFaulting);
        assert_eq!(ExKind::from_data(2), ExKind::Faulting);
        assert_eq!(ExKind::from_data(0xFFFF_FFFF), ExKind::NonFaulting);
        let mut t = [ExEntry {
            insn: 0,
            fixup: 0,
            data: 0,
        }; 1];
        let here = t.as_ptr() as usize as u64;
        entry_to(&mut t, 0, here + 64, here + 96, 1);
        assert_eq!(
            search(&t, here + 64),
            Some((here + 96, ExKind::NonFaulting))
        );
    }

    #[test]
    fn copy_range_checked_before_port() {
        setup(0, 0x2000, u64::MAX);
        let mut b = [0u8; 8];
        assert_eq!(copy_from_user::<Fake>(&mut b, 0), Err(Fault));
        assert_eq!(copy_to_user::<Fake>(0x10, &b), Err(Fault));
        assert_eq!(copy_from_user_partial::<Fake>(&mut b, u64::MAX - 2), 0);
        assert_eq!(copy_to_user_partial::<Fake>(0xFFFF_8000_0000_0000, &b), 0);
        assert_eq!(copy_to_user_partial::<Fake>(USER_MAP_END - 4, &b), 0);
        assert_eq!(strncpy_from_user::<Fake>(&mut b, 0), Err(Fault));
        assert_eq!(calls(), 0);
        // Empty copies below USER_MAP_END succeed without the port.
        assert_eq!(copy_from_user::<Fake>(&mut [], 0), Ok(()));
        assert_eq!(copy_to_user::<Fake>(USER_MAP_END, &[]), Err(Fault));
        assert_eq!(calls(), 0);
        assert_eq!(crate::kerror::KError::from(Fault).errno(), 14);
    }

    #[test]
    fn copy_partial_counts() {
        setup(BASE, 2 * P as usize, BASE + P);
        let src: Vec<u8> = (0..=255u8).collect();
        assert_eq!(copy_to_user_partial::<Fake>(BASE + P - 100, &src), 100);
        assert_eq!(peek(BASE + P - 100, 100), &src[..100]);
        assert_eq!(copy_to_user_partial::<Fake>(BASE + P, &src), 0);
        assert_eq!(copy_to_user_partial::<Fake>(BASE, &src), 256);
        let mut dst = [0u8; 256];
        assert_eq!(copy_from_user_partial::<Fake>(&mut dst, BASE + P - 10), 10);
        assert_eq!(&dst[..10], &src[90..100]);
        assert_eq!(copy_from_user_partial::<Fake>(&mut dst, BASE + P), 0);
    }

    #[test]
    fn copy_all_or_nothing() {
        setup(BASE, 2 * P as usize, BASE + P);
        assert_eq!(copy_to_user::<Fake>(BASE + P - 4, b"abcdefgh"), Err(Fault));
        assert_eq!(copy_to_user::<Fake>(BASE, b"abcdefgh"), Ok(()));
        let mut got = [0u8; 8];
        assert_eq!(copy_from_user::<Fake>(&mut got, BASE), Ok(()));
        assert_eq!(&got, b"abcdefgh");
        assert_eq!(copy_from_user::<Fake>(&mut got, BASE + P - 4), Err(Fault));
    }

    #[test]
    fn copy_to_user_val_bytes() {
        setup(BASE, P as usize, u64::MAX);
        assert_eq!(
            copy_to_user_val::<Fake, u32>(BASE + 4, &0x1122_3344),
            Ok(())
        );
        assert_eq!(peek(BASE + 4, 4), [0x44, 0x33, 0x22, 0x11]);
        assert_eq!(copy_to_user_val::<Fake, u32>(BASE + P - 2, &1), Err(Fault));
    }

    #[test]
    fn strncpy_nul_in_first_page() {
        setup(BASE, 2 * P as usize, u64::MAX);
        poke(BASE + 8, b"/bin/sh\0junk");
        let mut out = [0xAAu8; 64];
        assert_eq!(strncpy_from_user::<Fake>(&mut out, BASE + 8), Ok(7));
        assert_eq!(&out[..7], b"/bin/sh");
        poke(BASE + 100, b"\0");
        assert_eq!(strncpy_from_user::<Fake>(&mut out, BASE + 100), Ok(0));
        assert_eq!(strncpy_from_user::<Fake>(&mut [], BASE + 100), Ok(0));
    }

    #[test]
    fn strncpy_no_nul_fills_buffer() {
        setup(BASE, 2 * P as usize, u64::MAX);
        poke(BASE + P - 3, b"abcdefgh");
        let mut out = [0u8; 6];
        assert_eq!(strncpy_from_user::<Fake>(&mut out, BASE + P - 3), Ok(6));
        assert_eq!(&out, b"abcdef");
    }

    #[test]
    fn strncpy_fault_before_nul() {
        setup(BASE, 2 * P as usize, BASE + P);
        poke(BASE + P - 3, b"abc");
        let mut out = [0u8; 64];
        assert_eq!(
            strncpy_from_user::<Fake>(&mut out, BASE + P - 3),
            Err(Fault)
        );
    }

    #[test]
    fn strncpy_stops_before_unmapped_page_after_nul() {
        setup(BASE, 2 * P as usize, BASE + P);
        poke(BASE + P - 3, b"ab\0");
        let mut out = [0u8; 64];
        let before = calls();
        assert_eq!(strncpy_from_user::<Fake>(&mut out, BASE + P - 3), Ok(2));
        assert_eq!(calls() - before, 1);
        assert_eq!(&out[..2], b"ab");
    }

    #[test]
    fn strncpy_clamped_at_user_map_end() {
        let base = USER_MAP_END - P;
        setup(base, P as usize, u64::MAX);
        poke(USER_MAP_END - 10, b"0123456789");
        let mut out = [0u8; 64];
        assert_eq!(
            strncpy_from_user::<Fake>(&mut out, USER_MAP_END - 10),
            Err(Fault)
        );
        poke(USER_MAP_END - 1, b"\0");
        assert_eq!(
            strncpy_from_user::<Fake>(&mut out, USER_MAP_END - 10),
            Ok(9)
        );
        assert_eq!(&out[..9], b"012345678");
        assert_eq!(
            strncpy_from_user::<Fake>(&mut out, USER_MAP_END),
            Err(Fault)
        );
    }
}

// ------------------ Kani proofs (ROADMAP §10.8) ------------------

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// The §10.6 range check accepts exactly: an empty range at any
    /// address below `USER_MAP_END`, the null page included (Linux's
    /// `read(fd, NULL, 0)`), and a non-empty range that does not overflow
    /// and lies wholly between the null guard and `USER_MAP_END`.
    ///
    /// Bound: every pair of 64-bit `(addr, len)`; the function has no loop.
    #[kani::proof]
    fn user_range_exact() {
        let addr: u64 = kani::any();
        let len: u64 = kani::any();
        let want = if len == 0 {
            addr < USER_MAP_END
        } else {
            addr >= NULL_GUARD_LEN && addr.checked_add(len).is_some_and(|end| end <= USER_MAP_END)
        };
        let got = user_range_ok(addr, len);
        assert_eq!(got, want);
        // Each class bound first: a `cover!` over `&&` becomes one check per
        // branch.
        let empty = len == 0;
        let end = addr.checked_add(len);
        let null_ok = empty && addr == 0 && got;
        let empty_ok = empty && addr != 0 && got;
        let empty_high = empty && addr >= USER_MAP_END && !got;
        let null_guard = !empty && addr < NULL_GUARD_LEN && !got;
        let overflow = !empty && end.is_none() && !got;
        let past_end =
            !empty && addr >= NULL_GUARD_LEN && end.is_some_and(|e| e > USER_MAP_END) && !got;
        let last_byte = !empty && end == Some(USER_MAP_END) && got;
        let inside = !empty && got;
        kani::cover!(null_ok, "empty at null accepted");
        kani::cover!(empty_ok, "empty accepted");
        kani::cover!(empty_high, "empty at or above USER_MAP_END refused");
        kani::cover!(null_guard, "null guard refused");
        kani::cover!(overflow, "overflow refused");
        kani::cover!(past_end, "past USER_MAP_END refused");
        kani::cover!(last_byte, "last byte accepted");
        kani::cover!(inside, "non-empty accepted");
    }
}

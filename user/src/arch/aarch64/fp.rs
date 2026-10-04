//! aarch64 FP state (ROADMAP §11.6, F069): what the registers held at
//! the program's first instruction, which `_start` captures before anything
//! else runs, and a helper that sets FP state, makes one system call, and
//! reads the state back in one `asm!` block, so no compiled code touches the
//! registers in between.
//!
//! Field names match the x86_64 `FpState` so `/bin/tests` stays portable:
//! `fcw` is FPSR's low 16 bits, `mxcsr` is FPCR, `xmm0` is V0.

use core::arch::asm;
use core::cell::UnsafeCell;

/// The FP state the checks read: FPSR, FPCR, and V0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FpState {
    pub fcw: u16,
    pub mxcsr: u32,
    pub xmm0: [u8; 16],
}

impl FpState {
    /// `execve` starts every image with V0–V31, FPCR, and FPSR zero.
    pub const INITIAL: Self = Self {
        fcw: 0,
        mxcsr: 0,
        xmm0: [0; 16],
    };

    /// Whether this is [`FpState::INITIAL`].
    pub fn is_initial(&self) -> bool {
        *self == Self::INITIAL
    }

    /// The 32-byte image the asm reads and writes: FPSR at 0, FPCR at 4,
    /// V0 at 16.
    fn image(&self) -> Image {
        let mut b = [0u8; 32];
        b[..2].copy_from_slice(&self.fcw.to_le_bytes());
        b[4..8].copy_from_slice(&self.mxcsr.to_le_bytes());
        b[16..].copy_from_slice(&self.xmm0);
        Image(b)
    }

    fn from_image(b: &[u8; 32]) -> Self {
        let mut xmm0 = [0u8; 16];
        xmm0.copy_from_slice(&b[16..]);
        Self {
            fcw: u16::from_le_bytes([b[0], b[1]]),
            mxcsr: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            xmm0,
        }
    }
}

#[repr(C, align(16))]
struct Image([u8; 32]);

/// `_start`'s capture, written once by its first instructions, before any
/// Rust code runs, and only read afterwards.
#[repr(C, align(16))]
pub(crate) struct EntryFp(UnsafeCell<[u8; 32]>);

// SAFETY: `_start` writes the cell once, before the program's only thread
// runs any Rust code; every later access reads. Established here.
unsafe impl Sync for EntryFp {}

pub(crate) static ENTRY_FP: EntryFp = EntryFp(UnsafeCell::new([0; 32]));

/// The FP state at the program's first instruction.
pub fn initial_fp() -> FpState {
    // SAFETY: `ENTRY_FP` is written only by `_start` before any Rust code
    // runs, so this read races nothing; established here.
    let b = unsafe { core::ptr::read_volatile(ENTRY_FP.0.get()) };
    FpState::from_image(&b)
}

/// Whether the program started with the initial FP state.
pub fn is_initial() -> bool {
    initial_fp().is_initial()
}

/// Load `set` into FPSR, FPCR, and V0, make system call `nr` with
/// arguments `a`, `b`, `c`, and read the three back, all in one `asm!`
/// block; then restore [`FpState::INITIAL`]. Returns the call's raw
/// result and the state read after it, in the parent and, after a
/// `fork`, in the child.
///
/// # Safety
///
/// As the raw call `nr`: a call that writes through an argument needs it
/// valid for that write.
pub unsafe fn fp_syscall(
    set: &FpState,
    nr: usize,
    a: usize,
    b: usize,
    c: usize,
) -> (isize, FpState) {
    let input = set.image();
    let initial = FpState::INITIAL.image();
    let mut out = Image([0; 32]);
    let ret: isize;
    // SAFETY: the kernel's `svc` convention (`super`'s module docs);
    // the three images are live locals the asm reads or writes only in
    // their 32 bytes, and the caller's contract covers the call itself.
    // Established here.
    unsafe {
        asm!(
            "ldrh {t:w}, [{inp}]",
            "msr fpsr, {t}",
            "ldr {t:w}, [{inp}, #4]",
            "msr fpcr, {t}",
            "ldr q0, [{inp}, #16]",
            "svc #0",
            "mrs {t}, fpsr",
            "strh {t:w}, [{outp}]",
            "mrs {t}, fpcr",
            "str {t:w}, [{outp}, #4]",
            "str q0, [{outp}, #16]",
            "ldrh {t:w}, [{init}]",
            "msr fpsr, {t}",
            "ldr {t:w}, [{init}, #4]",
            "msr fpcr, {t}",
            inout("x8") nr => _,
            inout("x0") a as isize => ret,
            in("x1") b,
            in("x2") c,
            inp = in(reg) input.0.as_ptr(),
            outp = in(reg) out.0.as_mut_ptr(),
            init = in(reg) initial.0.as_ptr(),
            t = out(reg) _,
            out("v0") _,
            options(nostack),
        );
    }
    (ret, FpState::from_image(&out.0))
}

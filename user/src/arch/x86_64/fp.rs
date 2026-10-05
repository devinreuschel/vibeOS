//! x86_64 FP state (ROADMAP §10.6, F069, F129): what the registers held at
//! the program's first instruction, which `_start` captures before anything
//! else runs, and a helper that sets FP state, makes one system call, and
//! reads the state back in one `asm!` block, so no compiled code touches the
//! registers in between.

use core::arch::asm;
use core::cell::UnsafeCell;

/// The FP state the checks read: the x87 control word, MXCSR, and XMM0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FpState {
    pub fcw: u16,
    pub mxcsr: u32,
    pub xmm0: [u8; 16],
}

impl FpState {
    /// The psABI's initial state, which `execve` starts every image with:
    /// FCW `0x037F`, MXCSR `0x1F80`, the vector registers zero.
    pub const INITIAL: Self = Self {
        fcw: 0x037F,
        mxcsr: 0x1F80,
        xmm0: [0; 16],
    };

    /// Not the initial state in any field: MXCSR rounds toward zero
    /// (`0x7F80`, every exception still masked), FCW rounds toward zero,
    /// and the first vector register holds a pattern.
    pub const DIRTY: Self = Self {
        fcw: 0x0F7F,
        mxcsr: 0x7F80,
        xmm0: *b"vibeos fp state!",
    };

    /// Whether this is [`FpState::INITIAL`].
    pub fn is_initial(&self) -> bool {
        *self == Self::INITIAL
    }

    /// The 32-byte image the asm reads and writes: FCW at 0, MXCSR at 4,
    /// XMM0 at 16.
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

/// Whether the program started with the psABI's initial FP state.
pub fn is_initial() -> bool {
    initial_fp().is_initial()
}

/// Load `set` into FCW, MXCSR, and XMM0, make system call `nr` with
/// arguments `a`, `b`, `c`, and read the three back, all in one `asm!`
/// block; then restore [`FpState::INITIAL`]'s FCW and MXCSR. Returns the
/// call's raw result and the state read after it, in the parent and, after
/// a `fork`, in the child.
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
    // SAFETY: the kernel's `syscall` convention (`super`'s module docs);
    // the three images are live locals the asm reads or writes only in
    // their 32 bytes, and the caller's contract covers the call itself.
    // Established here.
    unsafe {
        asm!(
            "fldcw word ptr [r12]",
            "ldmxcsr dword ptr [r12 + 4]",
            "movdqa xmm0, xmmword ptr [r12 + 16]",
            "syscall",
            "fnstcw word ptr [r13]",
            "stmxcsr dword ptr [r13 + 4]",
            "movdqa xmmword ptr [r13 + 16], xmm0",
            "fldcw word ptr [r14]",
            "ldmxcsr dword ptr [r14 + 4]",
            in("r12") input.0.as_ptr(),
            in("r13") out.0.as_mut_ptr(),
            in("r14") initial.0.as_ptr(),
            inlateout("rax") nr as isize => ret,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            out("rcx") _,
            out("r11") _,
            out("xmm0") _,
            options(nostack),
        );
    }
    (ret, FpState::from_image(&out.0))
}

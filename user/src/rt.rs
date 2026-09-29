//! Program start and the panic handler (ROADMAP §10.5).

use core::fmt::Write;
use core::panic::PanicInfo;

use crate::env::Env;
use crate::sys;

unsafe extern "Rust" {
    /// The program's entry, defined by [`crate::main!`].
    fn __vibeos_user_main(env: &Env) -> i32;
}

/// The portable start: `arch`'s `_start` calls it with the initial stack
/// pointer, which points at `argc`.
///
/// # Safety
///
/// `sp` is the stack pointer the kernel handed the program at entry, with
/// argc, argv, envp and auxv laid out as the psABI's initial process stack.
pub(crate) unsafe extern "C" fn start(sp: *const usize) -> ! {
    // SAFETY: the kernel's initial stack is the psABI layout, the contract
    // of this function stated in its `# Safety` section, established here by
    // its only caller, the entry point.
    let env = unsafe { Env::from_stack(sp) };
    // SAFETY: `crate::main!` defines the symbol with this signature in every
    // program, established here by the macro's expansion in the binary.
    let code = unsafe { __vibeos_user_main(&env) };
    sys::exit(code)
}

/// The address of the program's entry point.
pub fn entry_address() -> usize {
    crate::arch::entry_address()
}

/// The panic message buffer: a `panicked at` line longer than this is cut.
const PANIC_BUF: usize = 512;

struct Buf {
    bytes: [u8; PANIC_BUF],
    len: usize,
}

impl Write for Buf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let room = PANIC_BUF - self.len;
        let n = s.len().min(room);
        self.bytes[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    let mut buf = Buf {
        bytes: [0; PANIC_BUF],
        len: 0,
    };
    // A `Display` that fails leaves what it wrote; the exit status still
    // reports the panic.
    #[expect(
        clippy::let_underscore_must_use,
        reason = "DESIGN §2.5: a failed format keeps the partial line; exit status 101 carries the failure"
    )]
    let _ = match info.location() {
        Some(l) => write!(
            buf,
            "panicked at {}:{}:{}:\n{}\n",
            l.file(),
            l.line(),
            l.column(),
            info.message()
        ),
        None => write!(buf, "panicked at <unknown>:\n{}\n", info.message()),
    };
    // Nothing is left to report a failed write to: the status is the report.
    #[expect(
        clippy::let_underscore_must_use,
        reason = "DESIGN §2.5: the panic path's write has no one to report to; exit status 101 carries the failure"
    )]
    let _ = sys::write(2, &buf.bytes[..buf.len]);
    sys::exit(101)
}

/// The unwinding personality routine. The prebuilt `core` for the user
/// triple is built to unwind, and its `.eh_frame` names this symbol
/// (`DW.ref.rust_eh_personality`); programs build with `-C panic=abort`, so
/// nothing unwinds and it is never called.
#[unsafe(no_mangle)]
extern "C" fn rust_eh_personality() {}

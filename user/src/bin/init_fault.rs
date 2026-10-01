//! The `/sbin/init` of `make test-e2e-init-fault`'s variant initrd only
//! (ROADMAP §10.5, F068): it stores to `0x1000`, which nothing maps, so
//! pid 1 dies of SIGSEGV and the kernel panics with
//! `vibeOS: init: pid 1 killed SIGSEGV addr=0x1000`. A store that does not
//! fault shows as `exited 1`.

#![no_std]
#![no_main]

use vibeos_user::env::Env;

vibeos_user::main!(main);

fn main(_env: &Env) -> i32 {
    // SAFETY: nothing is mapped at 0x1000 (the image loads at 1 GiB), so
    // the store faults and the kernel ends the process before it lands;
    // established here.
    unsafe { core::ptr::write_volatile(core::ptr::without_provenance_mut::<u8>(0x1000), 0) };
    1
}

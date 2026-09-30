//! `vmcore`: the core tool's readers (`vibeos::log::vmcore`), on the input
//! as a whole kernel ELF and as a whole physical core: headers, notes,
//! `NT_PRSTATUS`, VMCOREINFO, the page walk and the virtual-core writer
//! over a `SliceCore`.

use vibeos::arch::stub::Arch;
use vibeos::log::vmcore::walk::{self, Kernel};
use vibeos::log::vmcore::{KernelElf, NT_PRSTATUS, SliceCore, Vmcoreinfo, prstatus_regs};

/// Walk budget: small, so a looping table ends the input quickly.
const BUDGET: u64 = 1 << 14;

pub fn parse(data: &[u8]) {
    if let Ok(k) = KernelElf::parse(data) {
        let _ = k.build_id();
        for s in k.sections().take(256) {
            let _ = k.section_name(&s);
        }
        if let Ok(syms) = k.symbols() {
            for s in syms.take(1024) {
                let _ = k.in_text(s.value);
            }
        }
        let _ = k.loads().take(256).count();
    }
    let Ok(core) = SliceCore::new(data) else {
        return;
    };
    let _ = core.ram();
    for n in core.notes().take(256) {
        if n.kind == NT_PRSTATUS {
            let _ = prstatus_regs(n.desc);
        }
    }
    let Ok(vi) = Vmcoreinfo::find(core.notes()) else {
        return;
    };
    let _ = vi.build_id();
    let Ok(r) = vi.roots() else {
        return;
    };
    let Ok(k) = Kernel::<_, Arch>::new(&core, r.pgt_root, r.pgt_levels) else {
        return;
    };
    let k = k.with_budget(BUDGET);
    let _ = k.translate(r.log);
    let mut runs = 0usize;
    let _ = k.kernel_mappings(|_| runs += 1);
    let mut notes = Vec::new();
    core.note_bytes(|n| notes.push(n));
    let mut written = 0u64;
    let _ = walk::write_virtual_core(&k, &notes, |b| {
        written += b.len() as u64;
        Ok(())
    });
}

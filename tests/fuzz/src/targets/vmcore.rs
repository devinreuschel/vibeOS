//! `vmcore`: the core tool's readers (`vibeos::log::vmcore`), on the input
//! as a whole kernel ELF and as a whole physical core: headers, notes,
//! `NT_PRSTATUS`, VMCOREINFO, the page walk and the virtual-core writer
//! over a `SliceCore`, the unwinder, the kernel-table decoders and the
//! signature.

use vibeos::arch::stub::Arch;
use vibeos::log::trace::{RECORDS_PER_CPU, RecordData};
use vibeos::log::vmcore::sig::{normalize_message, pick_cpu, write_signature};
use vibeos::log::vmcore::walk::{self, Kernel};
use vibeos::log::vmcore::{BT_MAX, KernelElf, NT_PRSTATUS, SliceCore, Vmcoreinfo, prstatus_regs};

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
    let mut bt = [0u64; BT_MAX];
    let _ = k.unwind(r.log, r.tcbs, |a| a >= r.log, &mut bt);
    let mut cpus = Vec::new();
    for i in 0..r.cpus_len.min(4) {
        if let Ok(c) = k.cpu(r.cpus, i) {
            for q in 0..c.runq.len.min(8) {
                let _ = k.runq_id(&c.runq, q);
            }
            cpus.push((c.cpu_id, c.cur_tcb, c.idle_tcb));
        }
    }
    let _ = pick_cpu(None, &cpus);
    for i in 0..r.tcbs_len.min(8) {
        if let Ok(a) = k.tcb_slot(r.tcbs, i)
            && a != 0
        {
            let _ = k.thread(a);
        }
    }
    if let Ok(h) = k.log_header(r.log) {
        let _ = k.log_tail(r.log, &h, |_| {});
    }
    if k.trace_clock(r.cpus).is_ok() {
        let mut out = [RecordData::default(); RECORDS_PER_CPU];
        let _ = k.trace_ring(r.cpus, 0, &mut out);
    }
    if let Ok(Some((_, text))) = k.panic_line(r.log) {
        let msg = String::from_utf8_lossy(text.as_bytes());
        let mut s = String::new();
        let _ = normalize_message(&msg, &mut s);
        let _ = write_signature(&mut s, Some(&msg), [Some("rust_begin_unwind"), None]);
    }
}

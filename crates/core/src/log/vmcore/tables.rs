//! The kernel's tables as a core holds them (ROADMAP §10.7): each CPU's
//! `PerCpu` and remote view, the TCB table, the log ring, the flight
//! recorder and the panic line, decoded at the `offset_of!` offsets of the
//! portable types, whose layouts their own const assertions fix
//! (docs/VMCOREINFO.md, "Types the core tool reads"), with
//! `from_le_bytes`, never a transmute.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use core::mem::{offset_of, size_of};

use super::sig::{PanicLine, PanicText};
use super::walk::Kernel;
use super::{LOG_TAIL, PhysMem, VmError, le64, offset};
use crate::irq::stop::{CRASH_RBP, CRASH_RIP, CRASH_RSP, CRASH_WORDS, StopHow};
use crate::log::trace::{self, ClockInfo, RECORD_SIZE, RECORDS_PER_CPU, RecordData};
use crate::log::{KernelLogger, Level, MSG_CAP, RING_CAP, Record, Ring};
use crate::paging::PageTable;
use crate::per_cpu::{PerCpu, PerCpuRemote};
use crate::sched::ReadyQueue;
use crate::thread::{CpuContext, Tcb, ThreadState};

// ------------------------------------------------------------ kernel types

// The payload of a `ThreadState` variant sits after its `u32` tag at 8
// (`#[repr(u32)]`: the tag, then each variant's fields as a `repr(C)`
// struct), as docs/VMCOREINFO.md states: `Sleeping`'s deadline and
// `Blocked`'s queue at 8, `Blocked`'s deadline at 16. `sched/thread.rs`
// pins the size.
const STATE_ARG: usize = 8;
const _: () = assert!(size_of::<ThreadState>() == 24);

/// A TCB as the core holds it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThreadView {
    pub addr: u64,
    pub id: u32,
    /// `ThreadState`'s tag and its payload word (a deadline or a queue).
    pub state: u32,
    pub state_arg: u64,
    pub rbx: u64,
    pub rbp: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub rip: u64,
    pub cpu: u32,
    pub pid: u32,
}

/// `ThreadState`'s name for its tag (declaration order, docs/VMCOREINFO.md).
pub const fn state_name(tag: u32) -> &'static str {
    match tag {
        0 => "ready",
        1 => "running",
        2 => "sleeping",
        3 => "blocked",
        4 => "dead",
        _ => "<bad state>",
    }
}

/// A ready queue's ring: where its ids live and which are live.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueView {
    pub ids: u64,
    pub cap: u64,
    pub head: u64,
    pub len: u64,
}

/// A `PerCpu` entry and its `PerCpuRemote` view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuView {
    pub cpu_id: u32,
    pub apic_id: u32,
    /// The `Tcb` addresses of its current and idle threads.
    pub cur_tcb: u64,
    pub idle_tcb: u64,
    pub runq: QueueView,
    /// `irq::stop`'s `stopped` word.
    pub stopped: u32,
    /// The crash-register slot, `irq::stop::CRASH_*` order.
    pub crash: [u64; CRASH_WORDS],
}

impl CpuView {
    /// How the CPU stopped, if it did.
    pub fn stop_how(&self) -> Option<StopHow> {
        StopHow::from_code(self.stopped)
    }

    /// The registers its crash slot holds, when the slot is set: a stopped
    /// CPU's, or the dump owner's own (`log::panic`).
    pub fn slot(&self) -> Option<(u64, u64, u64)> {
        let get = |i: usize| self.crash.get(i).copied().unwrap_or(0);
        let rip = get(CRASH_RIP);
        (rip != 0).then(|| (rip, get(CRASH_RSP), get(CRASH_RBP)))
    }
}

/// The log ring's counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogHeader {
    pub head: u64,
    pub len: u64,
    pub dropped: u64,
    pub written: u64,
}

/// One log record from the core. `torn` is set when its level or length is
/// not one a whole record holds (a record `Ring::push` counted before it
/// wrote it); its text is then empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogRec {
    pub timestamp: u64,
    pub cpu: u8,
    pub level: Option<Level>,
    pub len: u8,
    pub msg: [u8; MSG_CAP],
    pub torn: bool,
}

impl LogRec {
    pub fn text(&self) -> &[u8] {
        self.msg.get(..usize::from(self.len)).unwrap_or(&[])
    }
}

type KRing = Ring<RING_CAP, MSG_CAP>;
type KRecord = Record<MSG_CAP>;

impl<'m, M: PhysMem, A: PageTable> Kernel<'m, M, A> {
    fn field(base: u64, index: u64, size: usize, off: usize) -> Result<u64, VmError> {
        index
            .checked_mul(offset(size))
            .and_then(|o| base.checked_add(o))
            .and_then(|b| b.checked_add(offset(off)))
            .ok_or(VmError::BadLayout)
    }

    /// `PerCpu` entry `i` of the array at `cpus`, and its remote view.
    pub fn cpu(&self, cpus: u64, i: u64) -> Result<CpuView, VmError> {
        let f = |off: usize| Self::field(cpus, i, size_of::<PerCpu>(), off);
        let q = |off: usize| f(offset_of!(PerCpu, runq).saturating_add(off));
        let [ids, cap, head, len] = ReadyQueue::CORE_OFFSETS;
        let remote = self.u64_at(f(offset_of!(PerCpu, remote))?)?;
        let r = |off: usize| Self::field(remote, 0, 0, off);
        let mut crash = [0u64; CRASH_WORDS];
        for (k, w) in crash.iter_mut().enumerate() {
            *w = self.u64_at(r(
                offset_of!(PerCpuRemote, crash).saturating_add(k.saturating_mul(8))
            )?)?;
        }
        Ok(CpuView {
            cpu_id: self.u32_at(f(offset_of!(PerCpu, cpu_id))?)?,
            apic_id: self.u32_at(r(offset_of!(PerCpuRemote, apic_id))?)?,
            cur_tcb: self.u64_at(f(PerCpu::CORE_CURRENT)?)?,
            idle_tcb: self.u64_at(f(PerCpu::CORE_IDLE)?)?,
            runq: QueueView {
                ids: self.u64_at(q(ids)?)?,
                cap: self.u64_at(q(cap)?)?,
                head: self.u64_at(q(head)?)?,
                len: self.u64_at(q(len)?)?,
            },
            stopped: self.u32_at(r(offset_of!(PerCpuRemote, stopped))?)?,
            crash,
        })
    }

    /// The `k`th id from the front of a ready queue.
    pub fn runq_id(&self, q: &QueueView, k: u64) -> Result<u32, VmError> {
        if q.cap == 0 || k >= q.len || q.len > q.cap {
            return Err(VmError::BadLayout);
        }
        let slot = q
            .head
            .checked_add(k)
            .and_then(|s| s.checked_rem(q.cap))
            .ok_or(VmError::BadLayout)?;
        self.u32_at(Self::field(q.ids, slot, 4, 0)?)
    }

    /// The TCB pointer in slot `i` of the table at `tcbs`, 0 for none.
    pub fn tcb_slot(&self, tcbs: u64, i: u64) -> Result<u64, VmError> {
        self.u64_at(Self::field(tcbs, i, 8, 0)?)
    }

    /// The TCB at `addr`.
    pub fn thread(&self, addr: u64) -> Result<ThreadView, VmError> {
        let f = |off: usize| Self::field(addr, 0, 0, off);
        let st = offset_of!(Tcb, state);
        let ctx = |r: usize| f(offset_of!(Tcb, context).saturating_add(r));
        Ok(ThreadView {
            addr,
            id: self.u32_at(f(offset_of!(Tcb, id))?)?,
            state: self.u32_at(f(st)?)?,
            state_arg: self.u64_at(f(st.saturating_add(STATE_ARG))?)?,
            rbx: self.u64_at(ctx(CpuContext::RBX)?)?,
            rbp: self.u64_at(ctx(CpuContext::RBP)?)?,
            r12: self.u64_at(ctx(CpuContext::R12)?)?,
            r13: self.u64_at(ctx(CpuContext::R13)?)?,
            r14: self.u64_at(ctx(CpuContext::R14)?)?,
            r15: self.u64_at(ctx(CpuContext::R15)?)?,
            rflags: self.u64_at(ctx(CpuContext::RFLAGS)?)?,
            rsp: self.u64_at(ctx(CpuContext::RSP)?)?,
            rip: self.u64_at(ctx(CpuContext::RIP)?)?,
            cpu: self.u32_at(f(offset_of!(Tcb, cpu))?)?,
            pid: self.u32_at(f(offset_of!(Tcb, pid))?)?,
        })
    }

    /// The log ring's counters, from the `KernelLog` at `log`, whose cell
    /// keeps the logger at 0 (`#[repr(C)]`, `data` first).
    pub fn log_header(&self, log: u64) -> Result<LogHeader, VmError> {
        let ring = |off: usize| {
            Self::field(
                log,
                0,
                0,
                offset_of!(KernelLogger, ring).saturating_add(off),
            )
        };
        let h = LogHeader {
            head: self.u64_at(ring(offset_of!(KRing, head))?)?,
            len: self.u64_at(ring(offset_of!(KRing, len))?)?,
            dropped: self.u64_at(ring(offset_of!(KRing, dropped))?)?,
            written: self.u64_at(ring(offset_of!(KRing, written))?)?,
        };
        if h.len > offset(RING_CAP) || h.head >= offset(RING_CAP) {
            return Err(VmError::BadLayout);
        }
        Ok(h)
    }

    /// The `i`th record oldest first, by `Ring::get`'s rule.
    pub fn log_record(&self, log: u64, h: &LogHeader, i: u64) -> Result<LogRec, VmError> {
        let cap = offset(RING_CAP);
        let idx = if h.len < cap {
            i
        } else {
            h.head
                .checked_add(i)
                .and_then(|x| x.checked_rem(cap))
                .ok_or(VmError::BadLayout)?
        };
        let base = Self::field(
            log,
            0,
            0,
            offset_of!(KernelLogger, ring).saturating_add(offset_of!(KRing, recs)),
        )?;
        let at = Self::field(base, idx, size_of::<KRecord>(), 0)?;
        let mut raw = [0u8; size_of::<KRecord>()];
        self.read(at, &mut raw)?;
        let byte = |off: usize| raw.get(off).copied().unwrap_or(0);
        let level = Level::from_u8(byte(offset_of!(KRecord, level)));
        let len = byte(offset_of!(KRecord, len));
        let torn = level.is_none() || usize::from(len) > MSG_CAP;
        let mut msg = [0u8; MSG_CAP];
        if !torn {
            let src = raw
                .get(offset_of!(KRecord, msg)..)
                .and_then(|s| s.get(..MSG_CAP))
                .unwrap_or(&[]);
            for (d, s) in msg.iter_mut().zip(src) {
                *d = *s;
            }
        }
        Ok(LogRec {
            timestamp: le64(&raw, offset(offset_of!(KRecord, timestamp)))?,
            cpu: byte(offset_of!(KRecord, cpu_id)),
            level,
            len: if torn { 0 } else { len },
            msg,
            torn,
        })
    }

    /// The last [`LOG_TAIL`] records, oldest first, into `out`.
    pub fn log_tail(
        &self,
        log: u64,
        h: &LogHeader,
        mut out: impl FnMut(&LogRec),
    ) -> Result<(), VmError> {
        let take = h.len.min(offset(LOG_TAIL));
        for i in h.len.saturating_sub(take)..h.len {
            out(&self.log_record(log, h, i)?);
        }
        Ok(())
    }

    /// The trace at `at` (the ELF's `VIBEOS_TRACE`): its clock, when it is
    /// live.
    pub fn trace_clock(&self, at: u64) -> Result<ClockInfo, VmError> {
        let f = |off: usize| Self::field(at, 0, 0, off);
        if self.u64_at(f(trace::TRACE_MAGIC_OFF)?)? != u64::from_le_bytes(trace::TRACE_MAGIC) {
            return Err(VmError::NoTrace);
        }
        let flags = self.u32_at(f(trace::TRACE_FLAGS_OFF)?)?;
        Ok(ClockInfo::from_header(
            self.u64_at(f(trace::TRACE_FREQ_OFF)?)?,
            self.u64_at(f(trace::TRACE_SKEW_OFF)?)?,
            flags,
        )
        .unwrap_or_default())
    }

    /// CPU `cpu`'s ring: its valid records oldest first, into the front of
    /// `out` (a torn or overwritten slot dropped by `trace::valid_at`).
    /// Returns how many.
    pub fn trace_ring(
        &self,
        at: u64,
        cpu: u32,
        out: &mut [RecordData; RECORDS_PER_CPU],
    ) -> Result<usize, VmError> {
        if cpu as usize >= trace::MAX_CPUS {
            return Err(VmError::BadLayout);
        }
        let ring = Self::field(
            at.checked_add(offset(trace::TRACE_RINGS_OFF))
                .ok_or(VmError::BadLayout)?,
            u64::from(cpu),
            trace::RING_SIZE,
            0,
        )?;
        let head = self.u64_at(Self::field(ring, 0, 0, trace::RING_HEAD_OFF)?)?;
        let mut slots = [RecordData::default(); RECORDS_PER_CPU];
        let mut raw = [0u8; RECORD_SIZE];
        for (i, s) in slots.iter_mut().enumerate() {
            let rec = Self::field(ring, offset(i), RECORD_SIZE, trace::RING_RECORDS_OFF)?;
            self.read(rec, &mut raw)?;
            *s = RecordData::from_le_bytes(&raw);
        }
        let mut n = 0usize;
        for r in trace::ordered(head, &slots) {
            if let Some(o) = out.get_mut(n) {
                *o = *r;
                n = n.saturating_add(1);
            }
        }
        Ok(n)
    }

    /// The [`PanicLine`] at `at`.
    pub fn panic_line(&self, at: u64) -> Result<Option<(u32, PanicText)>, VmError> {
        let mut raw = [0u8; size_of::<PanicLine>()];
        self.read(at, &mut raw)?;
        Ok(PanicLine::decode(&raw))
    }
}

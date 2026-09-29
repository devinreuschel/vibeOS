//! Host tests for the flight recorder, its warp test and its export.

extern crate std;

use std::sync::Mutex;
use std::vec::Vec;

use super::*;

#[test]
fn record_layout_is_fixed() {
    assert_eq!(size_of::<Record>(), 40);
    assert_eq!(offset_of!(Record, seq), 0);
    assert_eq!(offset_of!(Record, tsc), 8);
    assert_eq!(offset_of!(Record, a), 16);
    assert_eq!(offset_of!(Record, b), 24);
    assert_eq!(offset_of!(Record, cpu), 32);
    assert_eq!(offset_of!(Record, event), 36);
    assert_eq!(offset_of!(Ring<4>, records), 16);
    assert_eq!(offset_of!(KernelTrace, rings), 48);
    assert_eq!(
        size_of::<KernelTrace>(),
        48 + MAX_CPUS * (16 + RECORDS_PER_CPU * 40)
    );
    assert_eq!(MAX_CPUS, 64);
    assert_eq!(RECORDS_PER_CPU, 256);
}

#[test]
fn ring_push_assigns_positions() {
    let r: Ring<8> = Ring::new();
    for i in 0..5u64 {
        r.push(3, Event::Wake, 100 + i, i, i * 2);
    }
    assert_eq!(r.head(), 5);
    for pos in 0..5u64 {
        let d = r.get(pos).unwrap();
        assert_eq!(d.seq, pos + 1);
        assert_eq!(d.tsc, 100 + pos);
        assert_eq!(d.a, pos);
        assert_eq!(d.b, pos * 2);
        assert_eq!(d.cpu, 3);
        assert_eq!(d.event(), Some(Event::Wake));
    }
    assert_eq!(r.get(5), None);
}

#[test]
fn ring_wraps_keeping_newest() {
    let r: Ring<4> = Ring::new();
    for i in 0..10u64 {
        r.push(0, Event::Switch, i, i, 0);
    }
    // Positions 0 to 5 were overwritten; their slots hold 6 to 9.
    for pos in 0..6u64 {
        assert_eq!(r.get(pos), None, "pos {pos}");
    }
    for pos in 6..10u64 {
        assert_eq!(r.get(pos).unwrap().a, pos);
    }
    let mut snap = [RecordData::default(); 4];
    let head = r.snapshot(&mut snap);
    let got: Vec<u64> = ordered(head, &snap).map(|d| d.a).collect();
    assert_eq!(got, [6, 7, 8, 9]);
}

#[test]
fn torn_last_record_dropped() {
    let r: Ring<4> = Ring::new();
    for i in 0..3u64 {
        r.push(1, Event::IrqEnter, i, 32, 0);
    }
    // A writer stopped between its `seq = 0` store and its last store.
    r.records[2].seq.store(0, Ordering::Relaxed);
    r.records[2].a.store(0xdead, Ordering::Relaxed);
    assert_eq!(r.get(2), None);
    assert!(r.get(1).is_some());
    let mut snap = [RecordData::default(); 4];
    let head = r.snapshot(&mut snap);
    assert_eq!(ordered(head, &snap).count(), 2);
    // The same slot as a dump holds it: seq 0, fields half-written.
    let mut dump = [RecordData::default(); 4];
    for (i, d) in dump.iter_mut().enumerate().take(3) {
        *d = RecordData {
            seq: i as u64 + 1,
            tsc: i as u64,
            a: 32,
            b: 0,
            cpu: 1,
            event: Event::IrqEnter.as_u32(),
        };
    }
    dump[2].seq = 0;
    let seqs: Vec<u64> = ordered(3, &dump).map(|d| d.seq).collect();
    assert_eq!(seqs, [1, 2]);
    // A stale slot: its seq names another lap.
    dump[2].seq = 7;
    assert_eq!(ordered(3, &dump).count(), 2);
    assert!(!valid_at(2, &dump[2]));
}

#[test]
fn record_data_le_bytes_round_trip() {
    let d = RecordData {
        seq: 0x0102_0304_0506_0708,
        tsc: 0x1112_1314_1516_1718,
        a: 0x2122_2324_2526_2728,
        b: 0x3132_3334_3536_3738,
        cpu: 0x4142_4344,
        event: 0x5152_5354,
    };
    let b = d.to_le_bytes();
    assert_eq!(b[0], 0x08);
    assert_eq!(b[8], 0x18);
    assert_eq!(b[16], 0x28);
    assert_eq!(b[24], 0x38);
    assert_eq!(b[32], 0x44);
    assert_eq!(b[36], 0x54);
    assert_eq!(RecordData::from_le_bytes(&b), d);
    // The bytes a live `Record` holds are the same layout.
    let r: Ring<1> = Ring::new();
    r.push(7, Event::BlockSubmit, 99, 5, 6);
    let raw = &r.records[0] as *const Record as *const [u8; RECORD_SIZE];
    // SAFETY: `Record` is `#[repr(C)]`, 40 bytes of integers with no
    // padding (the const assertions above), established here.
    let got = RecordData::from_le_bytes(unsafe { &*raw });
    assert_eq!(got, r.get(0).unwrap());
}

#[test]
fn traced_vector_skips_ist_vectors() {
    for v in [1u8, 2, 8, 18] {
        assert_eq!(traced_vector(v), None, "vector {v}");
    }
    for v in 0u8..32 {
        let want = if v == 14 {
            Some(Event::PageFault)
        } else {
            None
        };
        assert_eq!(traced_vector(v), want, "vector {v}");
    }
    for v in 32u8..=255 {
        assert_eq!(traced_vector(v), Some(Event::IrqEnter), "vector {v}");
    }
}

#[test]
fn event_encoding_round_trips() {
    assert_eq!(Event::from_u32(0), None);
    assert_eq!(Event::from_u32(12), None);
    for (i, ev) in Event::ALL.iter().enumerate() {
        assert_eq!(ev.as_u32(), i as u32 + 1);
        assert_eq!(Event::from_u32(ev.as_u32()), Some(*ev));
        assert!(!ev.name().is_empty());
    }
    assert_eq!(Event::SyscallEnter.as_u32(), 1);
    assert_eq!(Event::BlockComplete.as_u32(), 11);
}

static SEEN: Mutex<Vec<(Event, u64, u64)>> = Mutex::new(Vec::new());

fn capture(ev: Event, a: u64, b: u64) {
    SEEN.lock().unwrap().push((ev, a, b));
}

#[test]
fn emit_reaches_installed_sink() {
    // The sink is global and other tests' code may emit, so look for
    // this test's own arguments only.
    const A: u64 = 0x5eed_0000_1234_5678;
    set_sink(capture);
    crate::trace!(IpiSend, A, 3);
    trap_enter(14, A, 7);
    trap_enter(2, A, 9);
    trap_enter(0x40, A, 0);
    trap_exit(0x40);
    trap_exit(18);
    let seen = SEEN.lock().unwrap();
    assert!(seen.contains(&(Event::IpiSend, A, 3)));
    assert!(seen.contains(&(Event::PageFault, A, 7)));
    assert!(!seen.iter().any(|e| e.1 == A && e.2 == 9));
    assert!(seen.contains(&(Event::IrqEnter, 0x40, 0)));
    assert!(seen.contains(&(Event::IrqExit, 0x40, 0)));
    assert!(!seen.contains(&(Event::IrqExit, 18, 0)));
}

#[test]
fn trace_init_writes_header() {
    let t: Trace<2, 4> = Trace::new();
    assert!(!t.is_live());
    t.init();
    assert!(t.is_live());
    assert_eq!(t.ring(1).unwrap().cap.load(Ordering::Relaxed), 4);
    assert!(t.ring(2).is_none());
}

#[test]
fn warp_step_scripted_backward() {
    let w = WarpLine::new();
    assert_eq!(w.step(|| 100), (100, 0));
    assert_eq!(w.step(|| 90), (90, 10));
    // The largest read stays published.
    assert_eq!(w.step(|| 95), (95, 5));
    assert_eq!(w.step(|| 120), (120, 0));
    let reads = [200u64, 210, 150, 220, 230];
    let i = std::cell::Cell::new(0usize);
    let w = WarpLine::new();
    let worst = w.run(
        || {
            let v = reads[i.get().min(reads.len() - 1)];
            i.set(i.get() + 1);
            v
        },
        25,
        100,
    );
    assert_eq!(worst, 60);
}

#[test]
fn warp_step_monotonic_sees_none() {
    let w = WarpLine::new();
    let t = std::cell::Cell::new(0u64);
    let clock = || {
        t.set(t.get() + 3);
        t.get()
    };
    assert_eq!(w.run(clock, 3000, WARP_MAX_ITERS), 0);
    // It stops at the span, well before the iteration cap.
    assert!(t.get() < 3100);
    // And at the cap when the span is never reached.
    let n = std::cell::Cell::new(0u32);
    let w = WarpLine::new();
    w.run(
        || {
            n.set(n.get() + 1);
            1
        },
        u64::MAX,
        50,
    );
    assert_eq!(n.get(), 51);
}

#[test]
fn warp_threads_synced_see_none() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64 as StdU64;
    // One shared counter both sides read: a synchronized clock.
    let counter = Arc::new(StdU64::new(1));
    let line = Arc::new(WarpLine::new());
    let side = |c: Arc<StdU64>, l: Arc<WarpLine>| {
        std::thread::spawn(move || {
            let read = || c.fetch_add(1, Ordering::Relaxed);
            assert!(l.arrive(read, 1 << 40));
            let back = l.run(read, 20_000, WARP_MAX_ITERS);
            l.leave();
            back
        })
    };
    let a = side(counter.clone(), line.clone());
    let b = side(counter.clone(), line.clone());
    assert_eq!(a.join().unwrap(), 0);
    assert_eq!(b.join().unwrap(), 0);
    assert!(line.wait_left(|| 0, 1));
    line.reset();
    assert_eq!(line.arrived.load(Ordering::Relaxed), 0);
    assert_eq!(line.left.load(Ordering::Relaxed), 0);
    assert_eq!(line.last.load(Ordering::Relaxed), 0);
}

#[test]
fn warp_arrive_times_out_alone() {
    let w = WarpLine::new();
    let t = std::cell::Cell::new(0u64);
    let clock = || {
        t.set(t.get() + 10);
        t.get()
    };
    assert!(!w.arrive(clock, 1000));
    assert!(t.get() >= 1000);
    // It withdrew, so the next side waits for a partner of its own.
    assert_eq!(w.arrived.load(Ordering::Relaxed), 0);
    assert!(!w.wait_left(clock, 100));
    // A partner already there: the second arrival meets it at once.
    w.arrived.store(1, Ordering::Relaxed);
    assert!(w.arrive(|| 0, 0));
}

#[test]
fn order_rule_matrix() {
    for invariant in [false, true] {
        for warp_measured in [false, true] {
            for max_skew in [0u64, 1, 5000] {
                let c = ClockInfo {
                    freq_hz: 1_000_000_000,
                    invariant,
                    warp_measured,
                    max_skew,
                };
                let global = invariant && warp_measured && max_skew == 0;
                assert_eq!(c.order() == Order::Global, global, "{c:?}");
                let f = c.flags();
                assert_eq!(f & CLOCK_PUBLISHED, CLOCK_PUBLISHED);
                assert_eq!(f & TSC_INVARIANT != 0, invariant);
                assert_eq!(f & WARP_MEASURED != 0, warp_measured);
                assert_eq!(f & WARP_BACKWARD != 0, max_skew != 0);
                assert_eq!(ClockInfo::from_header(c.freq_hz, max_skew, f), Some(c));
            }
        }
    }
    assert_eq!(ClockInfo::from_header(1, 0, 0), None);
    let t: Trace<1, 1> = Trace::new();
    assert_eq!(t.clock(), None);
    let c = ClockInfo {
        freq_hz: 3,
        invariant: true,
        warp_measured: false,
        max_skew: 0,
    };
    t.publish_clock(&c);
    assert_eq!(t.clock(), Some(c));
    assert_eq!(c.order(), Order::PerCpu("tsc warp test did not run"));
}

/// A JSON value, from the test-only reader below.
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(std::string::String),
    Arr(Vec<Json>),
    Obj(Vec<(std::string::String, Json)>),
}

impl Json {
    fn get(&self, k: &str) -> Option<&Json> {
        match self {
            Json::Obj(kv) => kv.iter().find(|(n, _)| n == k).map(|(_, v)| v),
            _ => None,
        }
    }

    fn str(&self) -> &str {
        match self {
            Json::Str(s) => s,
            other => panic!("not a string: {other:?}"),
        }
    }

    fn num(&self) -> f64 {
        match self {
            Json::Num(n) => *n,
            other => panic!("not a number: {other:?}"),
        }
    }

    fn arr(&self) -> &[Json] {
        match self {
            Json::Arr(a) => a,
            other => panic!("not an array: {other:?}"),
        }
    }
}

/// A small strict JSON reader: the whole input is one value.
fn parse_json(s: &str) -> Json {
    struct P<'a> {
        b: &'a [u8],
        i: usize,
    }
    impl P<'_> {
        fn ws(&mut self) {
            while self.i < self.b.len() && b" \t\r\n".contains(&self.b[self.i]) {
                self.i += 1;
            }
        }
        fn eat(&mut self, c: u8) {
            self.ws();
            assert_eq!(self.b.get(self.i), Some(&c), "at {}", self.i);
            self.i += 1;
        }
        fn peek(&mut self) -> u8 {
            self.ws();
            *self.b.get(self.i).expect("unexpected end")
        }
        fn lit(&mut self, word: &str, v: Json) -> Json {
            assert!(
                self.b[self.i..].starts_with(word.as_bytes()),
                "at {}",
                self.i
            );
            self.i += word.len();
            v
        }
        fn string(&mut self) -> std::string::String {
            self.eat(b'"');
            let mut out = std::string::String::new();
            loop {
                let c = self.b[self.i];
                self.i += 1;
                match c {
                    b'"' => return out,
                    b'\\' => {
                        let e = self.b[self.i];
                        self.i += 1;
                        out.push(match e {
                            b'"' => '"',
                            b'\\' => '\\',
                            b'/' => '/',
                            b'n' => '\n',
                            b't' => '\t',
                            _ => panic!("escape {e}"),
                        });
                    }
                    c if c < 0x20 => panic!("control byte in string"),
                    c => out.push(c as char),
                }
            }
        }
        fn value(&mut self) -> Json {
            match self.peek() {
                b'{' => {
                    self.eat(b'{');
                    let mut kv = Vec::new();
                    if self.peek() == b'}' {
                        self.eat(b'}');
                        return Json::Obj(kv);
                    }
                    loop {
                        let k = self.string();
                        self.eat(b':');
                        kv.push((k, self.value()));
                        if self.peek() == b',' {
                            self.eat(b',');
                        } else {
                            self.eat(b'}');
                            return Json::Obj(kv);
                        }
                    }
                }
                b'[' => {
                    self.eat(b'[');
                    let mut a = Vec::new();
                    if self.peek() == b']' {
                        self.eat(b']');
                        return Json::Arr(a);
                    }
                    loop {
                        a.push(self.value());
                        if self.peek() == b',' {
                            self.eat(b',');
                        } else {
                            self.eat(b']');
                            return Json::Arr(a);
                        }
                    }
                }
                b'"' => Json::Str(self.string()),
                b't' => self.lit("true", Json::Bool(true)),
                b'f' => self.lit("false", Json::Bool(false)),
                b'n' => self.lit("null", Json::Null),
                _ => {
                    let start = self.i;
                    while self.i < self.b.len()
                        && (self.b[self.i].is_ascii_digit() || b"+-.eE".contains(&self.b[self.i]))
                    {
                        self.i += 1;
                    }
                    let t = core::str::from_utf8(&self.b[start..self.i]).unwrap();
                    Json::Num(t.parse().unwrap_or_else(|_| panic!("number {t:?}")))
                }
            }
        }
    }
    let mut p = P {
        b: s.as_bytes(),
        i: 0,
    };
    let v = p.value();
    p.ws();
    assert_eq!(p.i, s.len(), "trailing bytes");
    v
}

fn rec(seq: u64, tsc: u64, cpu: u32, ev: Event, a: u64, b: u64) -> RecordData {
    RecordData {
        seq,
        tsc,
        a,
        b,
        cpu,
        event: ev.as_u32(),
    }
}

fn export(cpus: &[(u32, &[RecordData])], clock: &ClockInfo) -> Json {
    let mut s = std::string::String::new();
    export_chrome(cpus, clock, &mut s).unwrap();
    parse_json(&s)
}

const GLOBAL: ClockInfo = ClockInfo {
    freq_hz: 2_000_000_000,
    invariant: true,
    warp_measured: true,
    max_skew: 0,
};

#[test]
fn cycles_to_ns_exact() {
    assert_eq!(cycles_to_ns(0, 1_000_000_000), Some(0));
    assert_eq!(cycles_to_ns(3, 1_000_000_000), Some(3));
    assert_eq!(
        cycles_to_ns(3_000_000_000, 3_000_000_000),
        Some(1_000_000_000)
    );
    assert_eq!(cycles_to_ns(2_500, 2_500_000_000), Some(1_000));
    // No intermediate overflow: u64::MAX cycles at 4 GHz.
    assert_eq!(
        cycles_to_ns(u64::MAX, 4_000_000_000),
        Some((u128::from(u64::MAX) * 1_000_000_000 / 4_000_000_000) as u64)
    );
    // Truncates toward zero.
    assert_eq!(cycles_to_ns(1, 3_000_000_000), Some(0));
    assert_eq!(cycles_to_ns(10, 0), None);
    // Too many nanoseconds for a u64.
    assert_eq!(cycles_to_ns(u64::MAX, 1), None);
}

#[test]
fn export_chrome_required_fields() {
    let c0 = [
        rec(1, 1000, 0, Event::SyscallEnter, 39, 0),
        rec(2, 3000, 0, Event::SyscallExit, 39, 7),
    ];
    let c1 = [rec(1, 2000, 1, Event::IpiAck, 0xFB, u64::MAX)];
    for clock in [
        GLOBAL,
        ClockInfo {
            invariant: false,
            ..GLOBAL
        },
        ClockInfo {
            freq_hz: 0,
            ..GLOBAL
        },
    ] {
        let j = export(&[(0, &c0), (1, &c1)], &clock);
        let evs = j.get("traceEvents").unwrap().arr();
        assert!(!evs.is_empty());
        for e in evs {
            for k in ["name", "ph", "ts", "pid", "tid"] {
                assert!(e.get(k).is_some(), "{k} missing from {e:?}");
            }
            e.get("ts").unwrap().num();
            e.get("pid").unwrap().num();
            e.get("tid").unwrap().num();
            let ph = e.get("ph").unwrap().str();
            assert!(ph == "i" || ph == "M", "ph {ph}");
            if ph == "i" {
                assert_eq!(e.get("s").unwrap().str(), "t");
                let args = e.get("args").unwrap();
                assert!(args.get("seq").unwrap().str().starts_with("0x"));
            }
        }
        let tracepoints = evs.iter().filter(|e| e.get("ph").unwrap().str() == "i");
        assert_eq!(tracepoints.count(), 3);
        assert_eq!(j.get("displayTimeUnit").unwrap().str(), "ns");
        let other = j.get("otherData").unwrap();
        let clk = if clock.freq_hz == 0 { "cycles" } else { "tsc" };
        assert_eq!(other.get("clock").unwrap().str(), clk);
        assert_eq!(
            other.get("freq_hz").unwrap().str(),
            std::format!("{}", clock.freq_hz)
        );
        assert_eq!(other.get("skew_cycles").unwrap().str(), "0");
        assert!(other.get("reason").is_some());
    }
    // The named arguments.
    let j = export(&[(0, &c0), (1, &c1)], &GLOBAL);
    let exit = j
        .get("traceEvents")
        .unwrap()
        .arr()
        .iter()
        .find(|e| e.get("name").unwrap().str() == "syscall_exit")
        .unwrap()
        .clone();
    let args = exit.get("args").unwrap();
    assert_eq!(args.get("nr").unwrap().str(), "0x27");
    assert_eq!(args.get("ret").unwrap().str(), "0x7");
    assert_eq!(args.get("seq").unwrap().str(), "0x2");
    // An empty trace is still one object.
    let j = export(&[], &GLOBAL);
    assert_eq!(j.get("traceEvents").unwrap().arr().len(), 1);
}

#[test]
fn export_global_merges_by_timestamp() {
    let c0 = [
        rec(1, 100, 0, Event::Switch, 1, 2),
        rec(2, 400, 0, Event::Switch, 2, 1),
        rec(3, 700, 0, Event::Switch, 1, 2),
    ];
    let c2 = [
        rec(1, 200, 2, Event::Wake, 5, 2),
        rec(2, 500, 2, Event::Wake, 6, 2),
    ];
    let c3 = [rec(1, 50, 3, Event::IrqEnter, 32, 0)];
    let j = export(&[(0, &c0), (2, &c2), (3, &c3)], &GLOBAL);
    assert_eq!(
        j.get("otherData").unwrap().get("order").unwrap().str(),
        "global"
    );
    let evs: Vec<&Json> = j
        .get("traceEvents")
        .unwrap()
        .arr()
        .iter()
        .filter(|e| e.get("ph").unwrap().str() == "i")
        .collect();
    let tids: Vec<u32> = evs
        .iter()
        .map(|e| e.get("tid").unwrap().num() as u32)
        .collect();
    assert_eq!(tids, [3, 0, 2, 0, 2, 0]);
    // Counted from the smallest timestamp, 50 cycles, at 2 GHz.
    let ts: Vec<f64> = evs.iter().map(|e| e.get("ts").unwrap().num()).collect();
    assert_eq!(ts, [0.0, 0.025, 0.075, 0.175, 0.225, 0.325]);
    assert!(evs.iter().all(|e| e.get("pid").unwrap().num() == 0.0));
    // One name event per CPU track.
    let threads = j
        .get("traceEvents")
        .unwrap()
        .arr()
        .iter()
        .filter(|e| e.get("name").unwrap().str() == "thread_name")
        .count();
    assert_eq!(threads, 3);
}

#[test]
fn export_per_cpu_says_so() {
    // CPU 1's clock runs far behind CPU 0's: per-CPU order keeps each
    // track in ring order, counted from its own first record.
    let c0 = [
        rec(1, 10_000, 0, Event::IrqEnter, 32, 0),
        rec(2, 12_000, 0, Event::IrqExit, 32, 0),
    ];
    let c1 = [
        rec(1, 5, 1, Event::BlockSubmit, 9, 0),
        rec(2, 2_005, 1, Event::BlockComplete, 9, 0),
    ];
    for (clock, why) in [
        (
            ClockInfo {
                invariant: false,
                ..GLOBAL
            },
            "tsc not invariant",
        ),
        (
            ClockInfo {
                max_skew: 40,
                ..GLOBAL
            },
            "tsc warp test saw a backward step",
        ),
        (
            ClockInfo {
                warp_measured: false,
                ..GLOBAL
            },
            "tsc warp test did not run",
        ),
    ] {
        let j = export(&[(0, &c0), (1, &c1)], &clock);
        let other = j.get("otherData").unwrap();
        assert_eq!(other.get("order").unwrap().str(), "per-cpu");
        assert_eq!(other.get("reason").unwrap().str(), why);
        assert_eq!(
            other.get("skew_cycles").unwrap().str(),
            std::format!("{}", clock.max_skew)
        );
        let all = j.get("traceEvents").unwrap().arr();
        let evs: Vec<&Json> = all
            .iter()
            .filter(|e| e.get("ph").unwrap().str() == "i")
            .collect();
        let order: Vec<(u32, u32)> = evs
            .iter()
            .map(|e| {
                (
                    e.get("pid").unwrap().num() as u32,
                    e.get("tid").unwrap().num() as u32,
                )
            })
            .collect();
        assert_eq!(order, [(0, 0), (0, 0), (1, 1), (1, 1)]);
        let ts: Vec<f64> = evs.iter().map(|e| e.get("ts").unwrap().num()).collect();
        assert_eq!(ts, [0.0, 1.0, 0.0, 1.0]);
        let names: Vec<&str> = all
            .iter()
            .filter(|e| e.get("name").unwrap().str() == "process_name")
            .map(|e| e.get("args").unwrap().get("name").unwrap().str())
            .collect();
        assert_eq!(names, ["cpu 0 (own clock)", "cpu 1 (own clock)"]);
    }
}

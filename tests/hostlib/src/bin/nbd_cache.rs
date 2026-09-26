//! Host volatile-cache device for `make test-vibefs-crash` (ROADMAP §10.2, F080).
//!
//! `nbd-cache --socket <path> --image <path> --trace <path> --seed <n>` serves
//! `<image>` over NBD on a unix socket. It advertises flush, so QEMU forwards
//! every guest flush, and it replies to a write at once but to a flush only
//! after a delay the seed picks, reading and recording the requests that
//! arrive meanwhile. Every request and every reply goes to the JSONL trace
//! (`{"t":"write","id","off","len"}`, `{"t":"flush","id"}`, `{"t":"reply","id"}`),
//! and the payloads of the `write` records, concatenated in trace order, go to
//! `<trace>.data`. `tests/harness/nbd_trace.py` rebuilds crash images from them.
//!
//! Written from the NBD protocol specification (`doc/proto.md` of the NBD
//! project): fixed newstyle negotiation, simple replies only. Constants only;
//! no code is taken from qemu-nbd or nbd.

use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, ErrorKind, Read, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const NBDMAGIC: u64 = 0x4e42_444d_4147_4943;
const IHAVEOPT: u64 = 0x4948_4156_454f_5054;
const REPLY_MAGIC: u64 = 0x0003_e889_0455_65a9;
const REQUEST_MAGIC: u32 = 0x2560_9513;
const SIMPLE_REPLY_MAGIC: u32 = 0x6744_6698;

const FLAG_FIXED_NEWSTYLE: u16 = 1 << 0;
const FLAG_NO_ZEROES: u16 = 1 << 1;
const FLAG_C_NO_ZEROES: u32 = 1 << 1;

const OPT_EXPORT_NAME: u32 = 1;
const OPT_ABORT: u32 = 2;
const OPT_INFO: u32 = 6;
const OPT_GO: u32 = 7;

const REP_ACK: u32 = 1;
const REP_INFO: u32 = 3;
const REP_ERR_UNSUP: u32 = (1 << 31) + 1;
const REP_ERR_INVALID: u32 = (1 << 31) + 3;
const INFO_EXPORT: u16 = 0;

const TFLAG_HAS_FLAGS: u16 = 1 << 0;
const TFLAG_SEND_FLUSH: u16 = 1 << 2;
/// The only transmission flags this device advertises.
const TFLAGS: u16 = TFLAG_HAS_FLAGS | TFLAG_SEND_FLUSH;

const CMD_READ: u16 = 0;
const CMD_WRITE: u16 = 1;
const CMD_DISC: u16 = 2;
const CMD_FLUSH: u16 = 3;

const EIO: u32 = 5;
const EINVAL: u32 = 22;

/// A flush is replied `1 + xorshift(seed) % FLUSH_DELAY_MAX_MS` ms after it arrives.
const FLUSH_DELAY_MAX_MS: u64 = 20;
/// Largest option or write payload accepted; QEMU's requests are far smaller.
const MAX_PAYLOAD: u32 = 32 << 20;
const REQ_HDR: usize = 28;

struct Server {
    image: File,
    size: u64,
    trace: BufWriter<File>,
    data: BufWriter<File>,
    rng: u64,
    next_id: u64,
    /// A request the device does not support arrived (FUA, TRIM, unknown, out of range).
    breach: bool,
}

enum Negotiated {
    Transmission,
    Closed,
}

struct Pending {
    due: Instant,
    id: u64,
    cookie: u64,
}

fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn be64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_be_bytes(a)
}

fn read_n(s: &mut UnixStream, n: usize) -> io::Result<Vec<u8>> {
    let mut v = vec![0u8; n];
    s.read_exact(&mut v)?;
    Ok(v)
}

fn closed(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::UnexpectedEof
    )
}

impl Server {
    fn new(image: File, trace: File, data: File, seed: u64) -> io::Result<Self> {
        let size = image.metadata()?.len();
        Ok(Self {
            image,
            size,
            trace: BufWriter::new(trace),
            data: BufWriter::new(data),
            rng: seed ^ 0x9e37_79b9_7f4a_7c15,
            next_id: 0,
            breach: false,
        })
    }

    fn flush_delay(&mut self) -> Duration {
        if self.rng == 0 {
            self.rng = 1;
        }
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        Duration::from_millis(1 + self.rng % FLUSH_DELAY_MAX_MS)
    }

    fn id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn finish(&mut self) -> io::Result<()> {
        self.trace.flush()?;
        self.data.flush()?;
        self.image.sync_data()
    }

    fn opt_reply(s: &mut UnixStream, opt: u32, kind: u32, data: &[u8]) -> io::Result<()> {
        let mut m = Vec::with_capacity(20 + data.len());
        m.extend_from_slice(&REPLY_MAGIC.to_be_bytes());
        m.extend_from_slice(&opt.to_be_bytes());
        m.extend_from_slice(&kind.to_be_bytes());
        m.extend_from_slice(&(data.len() as u32).to_be_bytes());
        m.extend_from_slice(data);
        s.write_all(&m)
    }

    /// Fixed newstyle negotiation up to transmission, or until the client
    /// aborts or closes.
    fn negotiate(&mut self, s: &mut UnixStream) -> io::Result<Negotiated> {
        let mut hello = Vec::with_capacity(18);
        hello.extend_from_slice(&NBDMAGIC.to_be_bytes());
        hello.extend_from_slice(&IHAVEOPT.to_be_bytes());
        hello.extend_from_slice(&(FLAG_FIXED_NEWSTYLE | FLAG_NO_ZEROES).to_be_bytes());
        s.write_all(&hello)?;
        let cflags = be32(&read_n(s, 4)?, 0);
        let no_zeroes = cflags & FLAG_C_NO_ZEROES != 0;
        loop {
            let h = match read_n(s, 16) {
                Ok(h) => h,
                Err(e) if closed(&e) => return Ok(Negotiated::Closed),
                Err(e) => return Err(e),
            };
            if be64(&h, 0) != IHAVEOPT {
                return Ok(Negotiated::Closed);
            }
            let opt = be32(&h, 8);
            let len = be32(&h, 12);
            if len > MAX_PAYLOAD {
                return Ok(Negotiated::Closed);
            }
            let data = read_n(s, len as usize)?;
            match opt {
                OPT_EXPORT_NAME => {
                    let mut m = Vec::with_capacity(134);
                    m.extend_from_slice(&self.size.to_be_bytes());
                    m.extend_from_slice(&TFLAGS.to_be_bytes());
                    if !no_zeroes {
                        m.extend_from_slice(&[0u8; 124]);
                    }
                    s.write_all(&m)?;
                    return Ok(Negotiated::Transmission);
                }
                OPT_ABORT => {
                    Self::opt_reply(s, opt, REP_ACK, &[])?;
                    return Ok(Negotiated::Closed);
                }
                OPT_INFO | OPT_GO => {
                    // u32 name length, name, u16 count, count u16 info requests.
                    let ok = data.len() >= 6 && {
                        let nlen = be32(&data, 0) as usize;
                        data.len() >= 6 + nlen && {
                            let n = be16(&data, 4 + nlen) as usize;
                            data.len() == 6 + nlen + 2 * n
                        }
                    };
                    if !ok {
                        Self::opt_reply(s, opt, REP_ERR_INVALID, &[])?;
                        continue;
                    }
                    let mut info = Vec::with_capacity(12);
                    info.extend_from_slice(&INFO_EXPORT.to_be_bytes());
                    info.extend_from_slice(&self.size.to_be_bytes());
                    info.extend_from_slice(&TFLAGS.to_be_bytes());
                    Self::opt_reply(s, opt, REP_INFO, &info)?;
                    Self::opt_reply(s, opt, REP_ACK, &[])?;
                    if opt == OPT_GO {
                        return Ok(Negotiated::Transmission);
                    }
                }
                _ => Self::opt_reply(s, opt, REP_ERR_UNSUP, &[])?,
            }
        }
    }

    /// Sends one simple reply and records it. False when the peer is gone,
    /// in which case no reply is recorded.
    fn reply(
        &mut self,
        s: &mut UnixStream,
        id: u64,
        cookie: u64,
        err: u32,
        payload: &[u8],
    ) -> io::Result<bool> {
        let mut m = Vec::with_capacity(16 + payload.len());
        m.extend_from_slice(&SIMPLE_REPLY_MAGIC.to_be_bytes());
        m.extend_from_slice(&err.to_be_bytes());
        m.extend_from_slice(&cookie.to_be_bytes());
        if err == 0 {
            m.extend_from_slice(payload);
        }
        match s.write_all(&m) {
            Ok(()) => {
                writeln!(self.trace, "{{\"t\":\"reply\",\"id\":{id}}}")?;
                Ok(true)
            }
            Err(e) if closed(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn in_range(&self, off: u64, len: u32) -> bool {
        off.checked_add(u64::from(len))
            .is_some_and(|end| end <= self.size)
    }

    /// Handles one complete request. Returns false when the connection ends.
    fn request(
        &mut self,
        s: &mut UnixStream,
        req: &[u8],
        pending: &mut Vec<Pending>,
    ) -> io::Result<bool> {
        let flags = be16(req, 4);
        let cmd = be16(req, 6);
        let cookie = be64(req, 8);
        let off = be64(req, 16);
        let len = be32(req, 24);
        let id = self.id();
        if cmd == CMD_DISC {
            return Ok(false);
        }
        let valid = flags == 0
            && match cmd {
                CMD_READ | CMD_WRITE => self.in_range(off, len),
                CMD_FLUSH => true,
                _ => false,
            };
        if !valid {
            self.breach = true;
            return self.reply(s, id, cookie, EINVAL, &[]);
        }
        match cmd {
            CMD_READ => {
                let mut v = vec![0u8; len as usize];
                let err = match self.image.read_exact_at(&mut v, off) {
                    Ok(()) => 0,
                    Err(_) => EIO,
                };
                self.reply(s, id, cookie, err, &v)
            }
            CMD_WRITE => {
                let payload = &req[REQ_HDR..];
                writeln!(
                    self.trace,
                    "{{\"t\":\"write\",\"id\":{id},\"off\":{off},\"len\":{len}}}"
                )?;
                self.data.write_all(payload)?;
                let err = match self.image.write_all_at(payload, off) {
                    Ok(()) => 0,
                    Err(_) => EIO,
                };
                self.reply(s, id, cookie, err, &[])
            }
            _ => {
                writeln!(self.trace, "{{\"t\":\"flush\",\"id\":{id}}}")?;
                let due = Instant::now() + self.flush_delay();
                pending.push(Pending { due, id, cookie });
                Ok(true)
            }
        }
    }

    /// Transmission phase: one single-threaded loop over a byte buffer. The
    /// read timeout is the earliest pending flush deadline, so a partial
    /// request is never lost to a timeout.
    fn transmit(&mut self, s: &mut UnixStream) -> io::Result<()> {
        let mut buf: Vec<u8> = Vec::new();
        let mut pending: Vec<Pending> = Vec::new();
        let mut chunk = vec![0u8; 1 << 16];
        loop {
            let now = Instant::now();
            pending.sort_by_key(|p| p.due);
            while pending.first().is_some_and(|p| p.due <= now) {
                let p = pending.remove(0);
                if !self.reply(s, p.id, p.cookie, 0, &[])? {
                    return Ok(());
                }
            }
            while buf.len() >= REQ_HDR {
                if be32(&buf, 0) != REQUEST_MAGIC {
                    self.breach = true;
                    return Ok(());
                }
                let cmd = be16(&buf, 6);
                let len = be32(&buf, 24);
                let need = if cmd == CMD_WRITE {
                    if len > MAX_PAYLOAD {
                        self.breach = true;
                        return Ok(());
                    }
                    REQ_HDR + len as usize
                } else {
                    REQ_HDR
                };
                if buf.len() < need {
                    break;
                }
                let req: Vec<u8> = buf.drain(..need).collect();
                if !self.request(s, &req, &mut pending)? {
                    return Ok(());
                }
            }
            let wait = pending
                .iter()
                .map(|p| p.due.saturating_duration_since(Instant::now()))
                .min()
                .map(|d| d.max(Duration::from_millis(1)));
            s.set_read_timeout(wait)?;
            match s.read(&mut chunk) {
                Ok(0) => return Ok(()),
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) => {}
                Err(e) if closed(&e) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    /// Serves one client. True once a transmission connection has closed.
    fn serve(&mut self, s: &mut UnixStream) -> io::Result<bool> {
        match self.negotiate(s) {
            Ok(Negotiated::Transmission) => {}
            Ok(Negotiated::Closed) => return Ok(false),
            Err(e) if closed(&e) => return Ok(false),
            Err(e) => return Err(e),
        }
        self.transmit(s)?;
        self.finish()?;
        Ok(true)
    }
}

struct Args {
    socket: String,
    image: String,
    trace: String,
    seed: u64,
}

fn parse_args() -> Option<Args> {
    let mut a = env::args().skip(1);
    let (mut socket, mut image, mut trace, mut seed) = (None, None, None, None);
    while let Some(k) = a.next() {
        let v = a.next()?;
        match k.as_str() {
            "--socket" => socket = Some(v),
            "--image" => image = Some(v),
            "--trace" => trace = Some(v),
            "--seed" => seed = Some(v.parse().ok()?),
            _ => return None,
        }
    }
    Some(Args {
        socket: socket?,
        image: image?,
        trace: trace?,
        seed: seed?,
    })
}

fn run(a: &Args) -> io::Result<bool> {
    let image = OpenOptions::new().read(true).write(true).open(&a.image)?;
    let trace = File::create(&a.trace)?;
    let data = File::create(format!("{}.data", a.trace))?;
    let mut srv = Server::new(image, trace, data, a.seed)?;
    let listener = UnixListener::bind(&a.socket)?;
    println!("nbd-cache: listening");
    io::stdout().flush()?;
    for conn in listener.incoming() {
        let mut s = conn?;
        if srv.serve(&mut s)? {
            break;
        }
    }
    Ok(!srv.breach)
}

fn main() -> ExitCode {
    let Some(a) = parse_args() else {
        eprintln!("usage: nbd-cache --socket <path> --image <path> --trace <path> --seed <n>");
        return ExitCode::from(2);
    };
    match run(&a) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("nbd-cache: the client sent a request this device does not support");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("nbd-cache: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread::{self, JoinHandle};

    const OPT_LIST: u32 = 3;
    const OPT_STARTTLS: u32 = 5;
    const OPT_STRUCTURED_REPLY: u32 = 8;
    const OPT_SET_META_CONTEXT: u32 = 10;
    const OPT_EXTENDED_HEADERS: u32 = 11;
    const CMD_TRIM: u16 = 4;
    const CMD_FLAG_FUA: u16 = 1;

    static N: AtomicU32 = AtomicU32::new(0);

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(size: usize) -> Self {
            let dir = env::temp_dir().join(format!(
                "nbd-cache-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("img"), vec![0u8; size]).unwrap();
            Self { dir }
        }

        fn server(&self, seed: u64) -> Server {
            let image = OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.dir.join("img"))
                .unwrap();
            let trace = File::create(self.dir.join("trace")).unwrap();
            let data = File::create(self.dir.join("trace.data")).unwrap();
            Server::new(image, trace, data, seed).unwrap()
        }

        fn spawn(&self, seed: u64) -> (UnixStream, JoinHandle<(bool, Server)>) {
            let (c, mut s) = UnixStream::pair().unwrap();
            let mut srv = self.server(seed);
            let h = thread::spawn(move || {
                let done = srv.serve(&mut s).unwrap();
                (done, srv)
            });
            (c, h)
        }

        fn trace(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.join("trace"))
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A seed whose first flush waits at least 15 ms, so a request sent
    /// right after the flush arrives while it is still pending.
    fn slow_seed(f: &Fixture) -> u64 {
        (0..)
            .find(|&s| f.server(s).flush_delay() >= Duration::from_millis(15))
            .unwrap()
    }

    fn rd(c: &mut UnixStream, n: usize) -> Vec<u8> {
        read_n(c, n).unwrap()
    }

    /// Client hello: returns the server's handshake flags.
    fn hello(c: &mut UnixStream, cflags: u32) -> u16 {
        let h = rd(c, 18);
        assert_eq!(be64(&h, 0), NBDMAGIC);
        assert_eq!(be64(&h, 8), IHAVEOPT);
        c.write_all(&cflags.to_be_bytes()).unwrap();
        be16(&h, 16)
    }

    fn opt(c: &mut UnixStream, o: u32, data: &[u8]) {
        let mut m = Vec::new();
        m.extend_from_slice(&IHAVEOPT.to_be_bytes());
        m.extend_from_slice(&o.to_be_bytes());
        m.extend_from_slice(&(data.len() as u32).to_be_bytes());
        m.extend_from_slice(data);
        c.write_all(&m).unwrap();
    }

    /// One option reply: (option, type, data).
    fn opt_rep(c: &mut UnixStream) -> (u32, u32, Vec<u8>) {
        let h = rd(c, 20);
        assert_eq!(be64(&h, 0), REPLY_MAGIC);
        let d = rd(c, be32(&h, 16) as usize);
        (be32(&h, 8), be32(&h, 12), d)
    }

    fn go(c: &mut UnixStream) {
        assert_eq!(hello(c, 3), FLAG_FIXED_NEWSTYLE | FLAG_NO_ZEROES);
        opt(c, OPT_GO, &[0, 0, 0, 0, 0, 0]);
        assert_eq!(opt_rep(c).1, REP_INFO);
        assert_eq!(opt_rep(c).1, REP_ACK);
    }

    fn req(c: &mut UnixStream, flags: u16, cmd: u16, cookie: u64, off: u64, len: u32, data: &[u8]) {
        let mut m = Vec::new();
        m.extend_from_slice(&REQUEST_MAGIC.to_be_bytes());
        m.extend_from_slice(&flags.to_be_bytes());
        m.extend_from_slice(&cmd.to_be_bytes());
        m.extend_from_slice(&cookie.to_be_bytes());
        m.extend_from_slice(&off.to_be_bytes());
        m.extend_from_slice(&len.to_be_bytes());
        m.extend_from_slice(data);
        c.write_all(&m).unwrap();
    }

    /// One simple reply: (error, cookie).
    fn rep(c: &mut UnixStream) -> (u32, u64) {
        let h = rd(c, 16);
        assert_eq!(be32(&h, 0), SIMPLE_REPLY_MAGIC);
        (be32(&h, 4), be64(&h, 8))
    }

    #[test]
    fn handshake_go_advertises_flush_only() {
        let f = Fixture::new(8192);
        let (mut c, h) = f.spawn(1);
        assert_eq!(hello(&mut c, 3), FLAG_FIXED_NEWSTYLE | FLAG_NO_ZEROES);
        // Name "", one info request (NBD_INFO_BLOCK_SIZE = 3).
        opt(&mut c, OPT_GO, &[0, 0, 0, 0, 0, 1, 0, 3]);
        let (o, t, d) = opt_rep(&mut c);
        assert_eq!((o, t), (OPT_GO, REP_INFO));
        assert_eq!(be16(&d, 0), INFO_EXPORT);
        assert_eq!(be64(&d, 2), 8192);
        assert_eq!(be16(&d, 10), TFLAG_HAS_FLAGS | TFLAG_SEND_FLUSH);
        assert_eq!(opt_rep(&mut c).1, REP_ACK);
        req(&mut c, 0, CMD_DISC, 9, 0, 0, &[]);
        let (done, srv) = h.join().unwrap();
        assert!(done && !srv.breach);
    }

    #[test]
    fn handshake_export_name() {
        let f = Fixture::new(4096);
        let (mut c, h) = f.spawn(1);
        hello(&mut c, 1);
        opt(&mut c, OPT_EXPORT_NAME, b"");
        let d = rd(&mut c, 10 + 124);
        assert_eq!(be64(&d, 0), 4096);
        assert_eq!(be16(&d, 8), TFLAGS);
        assert!(d[10..].iter().all(|&b| b == 0));
        drop(c);
        assert!(h.join().unwrap().0);
    }

    #[test]
    fn unsupported_options_get_err_unsup() {
        let f = Fixture::new(4096);
        let (mut c, h) = f.spawn(1);
        hello(&mut c, 3);
        for o in [
            OPT_STRUCTURED_REPLY,
            OPT_EXTENDED_HEADERS,
            OPT_SET_META_CONTEXT,
            OPT_STARTTLS,
            OPT_LIST,
        ] {
            opt(&mut c, o, &[]);
            assert_eq!(opt_rep(&mut c), (o, REP_ERR_UNSUP, Vec::new()));
        }
        opt(&mut c, OPT_ABORT, &[]);
        assert_eq!(opt_rep(&mut c).1, REP_ACK);
        // A connection that ends before transmission does not end the server.
        assert!(!h.join().unwrap().0);
    }

    #[test]
    fn write_during_pending_flush_is_replied_first() {
        let f = Fixture::new(8192);
        let (mut c, h) = f.spawn(slow_seed(&f));
        go(&mut c);
        req(&mut c, 0, CMD_WRITE, 1, 0, 4, b"aaaa");
        req(&mut c, 0, CMD_FLUSH, 2, 0, 0, &[]);
        req(&mut c, 0, CMD_WRITE, 3, 4096, 4, b"bbbb");
        assert_eq!(rep(&mut c), (0, 1));
        assert_eq!(rep(&mut c), (0, 3));
        assert_eq!(rep(&mut c), (0, 2));
        req(&mut c, 0, CMD_DISC, 4, 0, 0, &[]);
        h.join().unwrap();
        assert_eq!(
            f.trace(),
            [
                r#"{"t":"write","id":0,"off":0,"len":4}"#,
                r#"{"t":"reply","id":0}"#,
                r#"{"t":"flush","id":1}"#,
                r#"{"t":"write","id":2,"off":4096,"len":4}"#,
                r#"{"t":"reply","id":2}"#,
                r#"{"t":"reply","id":1}"#,
            ]
        );
    }

    #[test]
    fn flush_reply_waits_seeded_delay() {
        let f = Fixture::new(4096);
        let want = f.server(42).flush_delay();
        assert!(
            want >= Duration::from_millis(1) && want <= Duration::from_millis(FLUSH_DELAY_MAX_MS)
        );
        let (mut c, h) = f.spawn(42);
        go(&mut c);
        let t0 = Instant::now();
        req(&mut c, 0, CMD_FLUSH, 5, 0, 0, &[]);
        assert_eq!(rep(&mut c), (0, 5));
        assert!(t0.elapsed() >= want, "{:?} < {want:?}", t0.elapsed());
        drop(c);
        h.join().unwrap();
    }

    #[test]
    fn trace_records_and_data_sidecar() {
        let f = Fixture::new(8192);
        let (mut c, h) = f.spawn(3);
        go(&mut c);
        req(&mut c, 0, CMD_WRITE, 1, 100, 3, b"xyz");
        assert_eq!(rep(&mut c), (0, 1));
        req(&mut c, 0, CMD_WRITE, 2, 5000, 2, b"pq");
        assert_eq!(rep(&mut c), (0, 2));
        req(&mut c, 0, CMD_READ, 3, 99, 5, &[]);
        assert_eq!(rep(&mut c), (0, 3));
        assert_eq!(rd(&mut c, 5), b"\0xyz\0");
        drop(c);
        let (done, srv) = h.join().unwrap();
        assert!(done && !srv.breach);
        assert_eq!(
            f.trace(),
            [
                r#"{"t":"write","id":0,"off":100,"len":3}"#,
                r#"{"t":"reply","id":0}"#,
                r#"{"t":"write","id":1,"off":5000,"len":2}"#,
                r#"{"t":"reply","id":1}"#,
                r#"{"t":"reply","id":2}"#,
            ]
        );
        assert_eq!(std::fs::read(f.dir.join("trace.data")).unwrap(), b"xyzpq");
        let img = std::fs::read(f.dir.join("img")).unwrap();
        assert_eq!(&img[100..103], b"xyz");
        assert_eq!(&img[5000..5002], b"pq");
    }

    #[test]
    fn eof_leaves_pending_flush_unreplied() {
        let f = Fixture::new(4096);
        let (mut c, h) = f.spawn(slow_seed(&f));
        go(&mut c);
        req(&mut c, 0, CMD_WRITE, 1, 0, 1, b"z");
        assert_eq!(rep(&mut c), (0, 1));
        req(&mut c, 0, CMD_FLUSH, 2, 0, 0, &[]);
        drop(c);
        let (done, _) = h.join().unwrap();
        assert!(done);
        let t = f.trace();
        assert_eq!(t.last().unwrap(), r#"{"t":"flush","id":1}"#);
        assert!(!t.contains(&r#"{"t":"reply","id":1}"#.to_owned()));
    }

    #[test]
    fn fua_or_unknown_command_is_einval() {
        let f = Fixture::new(4096);
        let (mut c, h) = f.spawn(1);
        go(&mut c);
        req(&mut c, CMD_FLAG_FUA, CMD_WRITE, 1, 0, 1, b"a");
        assert_eq!(rep(&mut c), (EINVAL, 1));
        req(&mut c, 0, CMD_TRIM, 2, 0, 512, &[]);
        assert_eq!(rep(&mut c), (EINVAL, 2));
        req(&mut c, 0, 77, 3, 0, 0, &[]);
        assert_eq!(rep(&mut c), (EINVAL, 3));
        req(&mut c, 0, CMD_READ, 4, 4000, 512, &[]);
        assert_eq!(rep(&mut c), (EINVAL, 4));
        drop(c);
        let (_, srv) = h.join().unwrap();
        assert!(srv.breach);
        assert!(
            std::fs::read(f.dir.join("img"))
                .unwrap()
                .iter()
                .all(|&b| b == 0)
        );
    }
}

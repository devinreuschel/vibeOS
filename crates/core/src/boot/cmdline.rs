//! The kernel command line (ROADMAP §10.2, BOOT.md §3.2): Limine's
//! `cmdline:` then QEMU's fw_cfg `opt/vibeos/cmdline`, parsed without
//! allocation. The kernel half captures the bytes (`boot::capture`).
//!
//! Word rules follow Linux's `Documentation/admin-guide/kernel-parameters.rst`:
//! words split at ASCII whitespace outside double quotes, and quotes that
//! enclose a word or its value are removed; `-` and `_` are equal in a
//! name; the last occurrence wins; `--` ends the kernel's words. A name in
//! [`OPTIONS`] and a `sysctl.<path>=` word are the kernel's. Any other
//! dotted word is dropped; an undotted `name=value` goes to init's
//! environment, and any other undotted word, or any word after `--`, is
//! one of init's arguments.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use core::fmt;

/// Longest command line kept, NUL excluded: Linux's x86
/// `COMMAND_LINE_SIZE` (`arch/x86/include/asm/setup.h`'s constant).
pub const CMDLINE_MAX: usize = 2048;
/// Init's argv entries, `argv[0]` included: `elf::build_initial_stack`'s
/// capacity until ROADMAP §10.6 raises it (Linux: 32).
pub const INIT_ARGV_MAX: usize = 8;
/// Init's environment strings: `elf::build_initial_stack`'s capacity.
pub const INIT_ENVP_MAX: usize = 8;

/// Who defines an option (DESIGN §3.2's naming rule).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Linux defines it: Linux's name and meaning.
    Linux,
    /// Only vibeOS defines it: `vibeos.<name>=`.
    Vibeos,
}

/// Stability class (ROADMAP §39.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Only the harness or a test sets it.
    Internal,
    Unstable,
    Stable,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Internal => "internal",
            Class::Unstable => "unstable",
            Class::Stable => "stable",
        }
    }
}

/// One option the kernel consumes. BOOT.md §3.2's table lists each row.
#[derive(Clone, Copy, Debug)]
pub struct Opt {
    pub name: &'static str,
    pub origin: Origin,
    pub class: Class,
}

/// Every option the kernel recognizes; later phases append theirs.
pub const OPTIONS: &[Opt] = &[
    Opt {
        name: "vibeos.strace",
        origin: Origin::Vibeos,
        // ROADMAP §19.1 settles it.
        class: Class::Unstable,
    },
    Opt {
        name: "loglevel",
        origin: Origin::Linux,
        class: Class::Unstable,
    },
    Opt {
        name: "vibeos.ktest",
        origin: Origin::Vibeos,
        class: Class::Internal,
    },
    Opt {
        name: "vibeos.ktest_repeat",
        origin: Origin::Vibeos,
        class: Class::Internal,
    },
];

/// Sysctl paths vibeOS implements, dotted. None yet: every
/// `sysctl.<path>=` word is logged and ignored.
pub const SYSCTLS: &[&str] = &[];

fn is_space(b: u8) -> bool {
    b.is_ascii_whitespace()
}

/// The command line as the kernel keeps it: Limine's part, then one space
/// and fw_cfg's, at most [`CMDLINE_MAX`] bytes.
pub struct CmdlineBuf {
    buf: [u8; CMDLINE_MAX],
    len: usize,
    limine_len: usize,
    truncated: bool,
}

#[allow(clippy::new_without_default)]
impl CmdlineBuf {
    pub const fn new() -> Self {
        Self {
            buf: [0; CMDLINE_MAX],
            len: 0,
            limine_len: 0,
            truncated: false,
        }
    }

    /// Append `part` with leading and trailing whitespace and NULs
    /// stripped, after one space when the buffer is not empty. What does
    /// not fit is dropped and [`truncated`](Self::truncated) set.
    pub fn append(&mut self, part: &[u8]) {
        let trim = |b: &u8| is_space(*b) || *b == 0;
        let start = part.iter().position(|b| !trim(b)).unwrap_or(part.len());
        let end = part
            .iter()
            .rposition(|b| !trim(b))
            .map_or(start, |i| i.saturating_add(1));
        let part = part.get(start..end).unwrap_or(&[]);
        if part.is_empty() {
            return;
        }
        if self.len != 0 {
            match self.buf.get_mut(self.len) {
                Some(b) => {
                    *b = b' ';
                    self.len = self.len.saturating_add(1);
                }
                None => {
                    self.truncated = true;
                    return;
                }
            }
        }
        let room = CMDLINE_MAX.saturating_sub(self.len);
        let n = part.len().min(room);
        if n < part.len() {
            self.truncated = true;
        }
        let end = self.len.saturating_add(n);
        if let (Some(dst), Some(src)) = (self.buf.get_mut(self.len..end), part.get(..n)) {
            dst.copy_from_slice(src);
            self.len = end;
        }
    }

    /// Mark everything appended so far as Limine's part.
    pub fn seal_limine(&mut self) {
        self.limine_len = self.len;
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.buf.get(..self.len).unwrap_or(&[])
    }

    /// Length of Limine's part, a prefix of [`as_bytes`](Self::as_bytes).
    pub fn limine_len(&self) -> usize {
        self.limine_len
    }

    /// Whether an append did not fit in [`CMDLINE_MAX`].
    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

/// A word for init: init sees `head` then `tail` as one string, so
/// `name="a b"` reaches it as `name=a b`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitWord<'a> {
    pub head: &'a [u8],
    pub tail: &'a [u8],
}

impl InitWord<'_> {
    pub fn len(&self) -> usize {
        self.head.len().saturating_add(self.tail.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A `sysctl.<path>=<value>` word, path as written after `sysctl.`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sysctl<'a> {
    pub path: &'a [u8],
    pub value: &'a [u8],
}

impl Sysctl<'_> {
    /// Whether `table` (dotted paths) holds this path, `/` read as `.`.
    pub fn known_in(&self, table: &[&str]) -> bool {
        table.iter().any(|t| {
            t.len() == self.path.len()
                && t.bytes().zip(self.path).all(|(a, &b)| {
                    let b = if b == b'/' { b'.' } else { b };
                    name_byte(a) == name_byte(b)
                })
        })
    }
}

/// What a word is to the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// An [`OPTIONS`] name.
    Option,
    Sysctl,
    /// A dotted name the kernel does not know.
    Dropped,
    Env,
    Arg,
}

/// One word, quotes removed.
#[derive(Clone, Copy, Debug)]
struct Word<'a> {
    /// The word without its enclosing quotes.
    whole: &'a [u8],
    name: &'a [u8],
    /// The value after the first `=`, its enclosing quotes removed.
    value: Option<&'a [u8]>,
    /// `whole` up to and including the `=`.
    head: &'a [u8],
    after_dashes: bool,
}

/// Remove one leading `"` and, then, one trailing `"`.
fn unquote(w: &[u8]) -> &[u8] {
    match w.split_first() {
        Some((b'"', rest)) => match rest.split_last() {
            Some((b'"', inner)) => inner,
            _ => rest,
        },
        _ => w,
    }
}

impl<'a> Word<'a> {
    fn new(raw: &'a [u8], after_dashes: bool) -> Self {
        let whole = unquote(raw);
        match whole.iter().position(|&b| b == b'=') {
            Some(eq) => {
                let (name, rest) = whole.split_at(eq);
                let head = whole.get(..=eq).unwrap_or(whole);
                let value = unquote(rest.get(1..).unwrap_or(&[]));
                Word {
                    whole,
                    name,
                    value: Some(value),
                    head,
                    after_dashes,
                }
            }
            None => Word {
                whole,
                name: whole,
                value: None,
                head: whole,
                after_dashes,
            },
        }
    }

    fn kind(&self) -> Kind {
        if self.after_dashes {
            return Kind::Arg;
        }
        if OPTIONS.iter().any(|o| name_eq(self.name, o.name)) {
            return Kind::Option;
        }
        if self.value.is_some() && sysctl_path(self.name).is_some() {
            return Kind::Sysctl;
        }
        if self.name.contains(&b'.') {
            return Kind::Dropped;
        }
        if self.value.is_some() && !self.name.is_empty() {
            Kind::Env
        } else {
            Kind::Arg
        }
    }

    fn init_word(&self) -> InitWord<'a> {
        match self.value {
            Some(v) if !self.after_dashes && self.kind() == Kind::Env => InitWord {
                head: self.head,
                tail: v,
            },
            _ => InitWord {
                head: self.whole,
                tail: &[],
            },
        }
    }
}

fn sysctl_path(name: &[u8]) -> Option<&[u8]> {
    let path = name.strip_prefix(b"sysctl.")?;
    (!path.is_empty()).then_some(path)
}

fn name_byte(b: u8) -> u8 {
    if b == b'-' { b'_' } else { b }
}

/// Whether option names `a` and `b` are equal, `-` and `_` alike.
fn name_eq(a: &[u8], b: &str) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.bytes())
            .all(|(&x, y)| name_byte(x) == name_byte(y))
}

/// The words of `raw`, in order, with `--` consumed.
struct Words<'a> {
    rest: &'a [u8],
    after_dashes: bool,
}

impl<'a> Iterator for Words<'a> {
    type Item = Word<'a>;

    fn next(&mut self) -> Option<Word<'a>> {
        loop {
            let start = self.rest.iter().position(|&b| !is_space(b))?;
            let rest = self.rest.get(start..).unwrap_or(&[]);
            let mut in_quote = false;
            let end = rest
                .iter()
                .position(|&b| {
                    if b == b'"' {
                        in_quote = !in_quote;
                    }
                    is_space(b) && !in_quote
                })
                .unwrap_or(rest.len());
            let (raw, tail) = rest.split_at(end);
            self.rest = tail;
            if raw == b"--" && !self.after_dashes {
                self.after_dashes = true;
                continue;
            }
            return Some(Word::new(raw, self.after_dashes));
        }
    }
}

/// A parsed command line. Parsing is lazy: each query walks the words.
#[derive(Clone, Copy, Debug)]
pub struct Cmdline<'a> {
    raw: &'a [u8],
}

/// Parse `raw`. Never fails: every byte string is a command line.
pub fn parse(raw: &[u8]) -> Cmdline<'_> {
    Cmdline { raw }
}

impl<'a> Cmdline<'a> {
    pub const EMPTY: Cmdline<'static> = Cmdline { raw: &[] };

    pub fn raw(&self) -> &'a [u8] {
        self.raw
    }

    fn words(&self) -> Words<'a> {
        Words {
            rest: self.raw,
            after_dashes: false,
        }
    }

    fn of_kind(&self, kind: Kind) -> impl Iterator<Item = Word<'a>> + 'a {
        self.words().filter(move |w| w.kind() == kind)
    }

    /// The value of the last kernel word named `key` (`-` and `_` alike):
    /// `Some(b"")` for a bare `key`, `None` when absent.
    pub fn get(&self, key: &str) -> Option<&'a [u8]> {
        self.words()
            .filter(|w| !w.after_dashes && name_eq(w.name, key))
            .last()
            .map(|w| w.value.unwrap_or(&[]))
    }

    /// Whether `key` is on: bare, `1`, `y`, `Y` or `on` (Linux's
    /// `kstrtobool` subset). Absent or any other value is off.
    pub fn flag(&self, key: &str) -> bool {
        matches!(self.get(key), Some(b"" | b"1" | b"y" | b"Y" | b"on"))
    }

    /// Init's arguments after `argv[0]`, in order.
    pub fn init_args(&self) -> impl Iterator<Item = InitWord<'a>> + 'a {
        self.of_kind(Kind::Arg).map(|w| w.init_word())
    }

    /// Init's environment strings, in order.
    pub fn init_env(&self) -> impl Iterator<Item = InitWord<'a>> + 'a {
        self.of_kind(Kind::Env).map(|w| w.init_word())
    }

    /// The `sysctl.<path>=` words, each consumed by the kernel.
    pub fn sysctls(&self) -> impl Iterator<Item = Sysctl<'a>> + 'a {
        self.of_kind(Kind::Sysctl).filter_map(|w| {
            Some(Sysctl {
                path: sysctl_path(w.name)?,
                value: w.value.unwrap_or(&[]),
            })
        })
    }

    /// The names of dotted words neither the kernel nor init takes.
    pub fn dropped(&self) -> impl Iterator<Item = &'a [u8]> + 'a {
        self.of_kind(Kind::Dropped).map(|w| w.name)
    }

    /// Init's `argv` (`argv0`, then [`init_args`](Self::init_args)) and
    /// `envp`, copied NUL-free into `buf`, at most [`INIT_ARGV_MAX`] and
    /// [`INIT_ENVP_MAX`] entries. A word past a cap or past `buf`'s end
    /// counts in [`InitVectors::dropped`].
    pub fn init_vectors<'b>(&self, argv0: &[u8], buf: &'b mut [u8]) -> InitVectors<'b> {
        let mut argv = [(0usize, 0usize); INIT_ARGV_MAX];
        let mut envp = [(0usize, 0usize); INIT_ENVP_MAX];
        let (mut argc, mut envc, mut dropped) = (0usize, 0usize, 0usize);
        let mut used = 0usize;
        let mut put = |w: InitWord<'_>, used: &mut usize| -> Option<(usize, usize)> {
            let start = *used;
            let mid = start.checked_add(w.head.len())?;
            let end = mid.checked_add(w.tail.len())?;
            buf.get_mut(start..mid)?.copy_from_slice(w.head);
            buf.get_mut(mid..end)?.copy_from_slice(w.tail);
            *used = end;
            Some((start, end))
        };
        let words = core::iter::once(InitWord {
            head: argv0,
            tail: &[],
        })
        .map(|w| (true, w))
        .chain(self.init_args().map(|w| (true, w)))
        .chain(self.init_env().map(|w| (false, w)));
        for (is_arg, w) in words {
            let (slots, n) = if is_arg {
                (argv.as_mut_slice(), &mut argc)
            } else {
                (envp.as_mut_slice(), &mut envc)
            };
            match slots.get_mut(*n) {
                Some(slot) => match put(w, &mut used) {
                    Some(r) => {
                        *slot = r;
                        *n = n.saturating_add(1);
                    }
                    None => dropped = dropped.saturating_add(1),
                },
                None => dropped = dropped.saturating_add(1),
            }
        }
        let buf: &'b [u8] = buf;
        let slice = |(s, e): (usize, usize)| buf.get(s..e).unwrap_or(&[]);
        InitVectors {
            argv: argv.map(slice),
            argc,
            envp: envp.map(slice),
            envc,
            dropped,
        }
    }
}

/// Init's `argv` and `envp`, built by [`Cmdline::init_vectors`].
#[derive(Debug)]
pub struct InitVectors<'b> {
    argv: [&'b [u8]; INIT_ARGV_MAX],
    argc: usize,
    envp: [&'b [u8]; INIT_ENVP_MAX],
    envc: usize,
    /// Words that did not fit.
    pub dropped: usize,
}

impl<'b> InitVectors<'b> {
    pub fn argv(&self) -> &[&'b [u8]] {
        self.argv.get(..self.argc).unwrap_or(&[])
    }

    pub fn envp(&self) -> &[&'b [u8]] {
        self.envp.get(..self.envc).unwrap_or(&[])
    }
}

/// Bytes for a log line: `0x20..=0x7E` as themselves, anything else `?`.
pub struct Escaped<'a>(pub &'a [u8]);

impl fmt::Display for Escaped<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use fmt::Write;
        for &b in self.0 {
            f.write_char(if (0x20..=0x7E).contains(&b) {
                b as char
            } else {
                '?'
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use std::string::String;
    use std::vec::Vec;

    fn s(w: InitWord<'_>) -> String {
        let mut v = w.head.to_vec();
        v.extend_from_slice(w.tail);
        String::from_utf8(v).unwrap()
    }

    fn args(c: &str) -> Vec<String> {
        parse(c.as_bytes()).init_args().map(s).collect()
    }

    fn env(c: &str) -> Vec<String> {
        parse(c.as_bytes()).init_env().map(s).collect()
    }

    fn dropped(c: &str) -> Vec<&[u8]> {
        parse(c.as_bytes()).dropped().collect()
    }

    #[test]
    fn cmdline_env_from_undotted_name_value() {
        assert_eq!(env("TERM=vt100 HOME=/ quiet"), ["TERM=vt100", "HOME=/"]);
        assert!(args("TERM=vt100").is_empty());
    }

    #[test]
    fn cmdline_arg_from_undotted_word() {
        assert_eq!(args("single emergency TERM=x"), ["single", "emergency"]);
        assert!(env("single").is_empty());
    }

    #[test]
    fn cmdline_args_after_double_dash() {
        let c = "a=1 -- vibeos.strace=1 b=2 x.y -- c";
        assert_eq!(args(c), ["vibeos.strace=1", "b=2", "x.y", "--", "c"]);
        assert_eq!(env(c), ["a=1"]);
        assert!(!parse(c.as_bytes()).flag("vibeos.strace"));
        assert!(dropped(c).is_empty());
    }

    #[test]
    fn cmdline_dotted_unknown_dropped() {
        let c = "vibeos.nope=1 foo.bar baz.q=\"a b\"";
        assert_eq!(dropped(c), [&b"vibeos.nope"[..], b"foo.bar", b"baz.q"]);
        assert!(args(c).is_empty());
        assert!(env(c).is_empty());
    }

    #[test]
    fn cmdline_known_option_consumed() {
        for c in ["vibeos.strace=1", "vibeos.strace", "vibeos.strace=\"0\""] {
            assert!(args(c).is_empty(), "{c}");
            assert!(env(c).is_empty(), "{c}");
            assert!(dropped(c).is_empty(), "{c}");
        }
    }

    #[test]
    fn cmdline_sysctl_consumed_and_reported() {
        const TABLE: &[&str] = &["kernel.pid_max"];
        let c = parse(b"sysctl.kernel.pid_max=99 sysctl.vm/swappiness=1 sysctl.kernel/pid-max=7");
        let v: Vec<_> = c.sysctls().collect();
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].path, b"kernel.pid_max");
        assert_eq!(v[0].value, b"99");
        assert!(v[0].known_in(TABLE));
        assert_eq!(v[1].path, b"vm/swappiness");
        assert!(!v[1].known_in(TABLE));
        assert!(v[2].known_in(TABLE));
        assert!(v.iter().all(|s| !s.known_in(SYSCTLS)));
        assert_eq!(
            c.init_args().count() + c.init_env().count() + c.dropped().count(),
            0
        );
        // Without a value it is a dotted word like any other.
        assert_eq!(dropped("sysctl.kernel.x"), [&b"sysctl.kernel.x"[..]]);
    }

    #[test]
    fn cmdline_get_last_wins_and_dash_underscore() {
        let c = parse(b"vibeos.strace=0 vibeos.strace=1 a_b=1 a-b=2 c-d");
        assert_eq!(c.get("vibeos.strace"), Some(&b"1"[..]));
        // `-` and `_` are alike; `.` is not.
        assert_eq!(c.get("vibeos_strace"), None);
        assert_eq!(c.get("a-b"), Some(&b"2"[..]));
        assert_eq!(c.get("a_b"), Some(&b"2"[..]));
        assert_eq!(c.get("c_d"), Some(&b""[..]));
        assert_eq!(c.get("missing"), None);
        assert!(parse(b"vibeos.strace=1 vibeos.strace=0").get("vibeos.strace") == Some(b"0"));
    }

    #[test]
    fn cmdline_flag_values() {
        for v in [
            "vibeos.strace",
            "vibeos.strace=1",
            "vibeos.strace=y",
            "vibeos.strace=Y",
            "vibeos.strace=on",
        ] {
            assert!(parse(v.as_bytes()).flag("vibeos.strace"), "{v}");
        }
        for v in [
            "vibeos.strace=0",
            "vibeos.strace=n",
            "vibeos.strace=off",
            "vibeos.strace=2",
            "",
        ] {
            assert!(!parse(v.as_bytes()).flag("vibeos.strace"), "{v}");
        }
    }

    #[test]
    fn cmdline_quotes_protect_spaces() {
        let c = "msg=\"hello world\" \"two words\" vibeos.strace=\"1\"";
        assert_eq!(env(c), ["msg=hello world"]);
        assert_eq!(args(c), ["two words"]);
        assert!(parse(c.as_bytes()).flag("vibeos.strace"));
        let w = parse(c.as_bytes()).init_env().next().unwrap();
        assert_eq!((w.head, w.tail), (&b"msg="[..], &b"hello world"[..]));
        assert_eq!(env("\"x=a b\""), ["x=a b"]);
    }

    #[test]
    fn cmdline_non_utf8_and_edge_words() {
        for c in [
            &b"="[..],
            b"=x",
            b"\"",
            b"\xff",
            b"",
            b"   ",
            b"a=\"",
            b"\"\"",
            b"--",
            b"-- --",
            b"\x00\xff=\xfe",
        ] {
            let p = parse(c);
            let _ = p.init_args().count() + p.init_env().count() + p.dropped().count();
            let _ = p.sysctls().count();
            let _ = p.get("=");
            let mut buf = [0u8; 64];
            let _ = p.init_vectors(b"/sbin/init", &mut buf);
        }
        assert_eq!(args("= =x"), ["=", "=x"]);
        let w: Vec<_> = parse(b"\xff \xfe=\x01").init_args().collect();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].head, b"\xff");
        let e: Vec<_> = parse(b"\xff \xfe=\x01").init_env().collect();
        assert_eq!((e[0].head, e[0].tail), (&b"\xfe="[..], &b"\x01"[..]));
        assert_eq!(args("\""), [""]);
    }

    #[test]
    fn cmdline_buf_append_and_truncate() {
        let mut b = CmdlineBuf::new();
        b.append(b"  vibeos.strace=0 \n");
        b.seal_limine();
        b.append(b"vibeos.strace=1\n\0\0");
        assert_eq!(b.as_bytes(), b"vibeos.strace=0 vibeos.strace=1");
        assert_eq!(b.limine_len(), 15);
        assert!(!b.truncated());
        b.append(b"");
        b.append(b" \0");
        assert_eq!(b.as_bytes().len(), 31);

        let mut b = CmdlineBuf::new();
        b.append(&[b'a'; CMDLINE_MAX - 1]);
        assert!(!b.truncated());
        b.append(b"xyz");
        assert!(b.truncated());
        assert_eq!(b.as_bytes().len(), CMDLINE_MAX);
        assert_eq!(b.as_bytes()[CMDLINE_MAX - 1], b' ');
        b.append(b"more");
        assert_eq!(b.as_bytes().len(), CMDLINE_MAX);
    }

    #[test]
    fn cmdline_init_vectors_cap_and_drop() {
        let c = parse(b"a b c d e f g h i E1=1 E2=2 E3=3 E4=4 E5=5 E6=6 E7=7 E8=8 E9=9");
        let mut buf = [0u8; CMDLINE_MAX];
        let v = c.init_vectors(b"/sbin/init", &mut buf);
        assert_eq!(v.argv().len(), INIT_ARGV_MAX);
        assert_eq!(v.argv()[0], b"/sbin/init");
        assert_eq!(v.argv()[7], b"g");
        assert_eq!(v.envp().len(), INIT_ENVP_MAX);
        assert_eq!(v.envp()[7], b"E8=8");
        assert_eq!(v.dropped, 3);

        // A buffer too small drops what does not fit.
        let mut small = [0u8; 14];
        let v = parse(b"xy long-word Z=1").init_vectors(b"/sbin/init", &mut small);
        assert_eq!(v.argv(), [&b"/sbin/init"[..], b"xy"]);
        assert!(v.envp().is_empty());
        assert_eq!(v.dropped, 2);

        let mut buf = [0u8; 8];
        let v = parse(b"").init_vectors(b"/init", &mut buf);
        assert_eq!(v.argv(), [&b"/init"[..]]);
        assert_eq!(v.dropped, 0);
    }

    #[test]
    fn cmdline_init_vectors_on_initial_stack() {
        use crate::elf::build_initial_stack;
        let c = parse(b"vibeos.strace=1 single TERM=\"vt 100\" -- x.y");
        let mut buf = [0u8; CMDLINE_MAX];
        let v = c.init_vectors(b"/sbin/init", &mut buf);
        let top = 0x7fff_0000u64;
        let mut mem = [0u8; 4096];
        let rsp = build_initial_stack(top, &mut mem, v.argv(), v.envp(), &[], &[7; 16]).unwrap();
        let base = top - mem.len() as u64;
        let word = |va: u64| {
            let o = (va - base) as usize;
            u64::from_le_bytes(mem[o..o + 8].try_into().unwrap())
        };
        let cstr = |va: u64| {
            let o = (va - base) as usize;
            let n = mem[o..].iter().position(|&b| b == 0).unwrap();
            String::from_utf8(mem[o..o + n].to_vec()).unwrap()
        };
        let argc = word(rsp);
        assert_eq!(argc, 3);
        let argv: Vec<_> = (0..argc).map(|i| cstr(word(rsp + 8 + 8 * i))).collect();
        assert_eq!(argv, ["/sbin/init", "single", "x.y"]);
        assert_eq!(word(rsp + 8 + 8 * argc), 0);
        let env_at = rsp + 16 + 8 * argc;
        assert_eq!(cstr(word(env_at)), "TERM=vt 100");
        assert_eq!(word(env_at + 8), 0);
    }

    #[test]
    fn cmdline_option_names_follow_rule() {
        for o in OPTIONS {
            match o.origin {
                Origin::Vibeos => {
                    let rest = o
                        .name
                        .strip_prefix("vibeos.")
                        .unwrap_or_else(|| panic!("{}", o.name));
                    assert!(!rest.is_empty());
                    assert!(
                        rest.bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                        "{}",
                        o.name
                    );
                }
                Origin::Linux => {
                    assert!(
                        !o.name.starts_with("vibeos.") && !o.name.starts_with("sysctl."),
                        "{}",
                        o.name
                    );
                }
            }
        }
        for (i, a) in OPTIONS.iter().enumerate() {
            assert!(
                OPTIONS[..i]
                    .iter()
                    .all(|b| !name_eq(a.name.as_bytes(), b.name)),
                "{}",
                a.name
            );
        }
    }

    #[test]
    fn cmdline_options_documented() {
        const BOOT_MD: &str = include_str!("../../../../docs/BOOT.md");
        for o in OPTIONS {
            let name = std::format!("`{}`", o.name);
            let class = std::format!("| {} |", o.class.as_str());
            assert!(
                BOOT_MD
                    .lines()
                    .any(|l| l.starts_with(&std::format!("| {name}")) && l.contains(&class)),
                "BOOT.md §3.2 has no options-table row for {} with class {}",
                o.name,
                o.class.as_str()
            );
        }
    }

    #[test]
    fn cmdline_escaped_display() {
        assert_eq!(
            std::format!("{}", Escaped(b"ok ~\x1f\x7f\n\xff\x1e!")),
            "ok ~?????!"
        );
        assert_eq!(std::format!("{}", Escaped(b"")), "");
    }
}

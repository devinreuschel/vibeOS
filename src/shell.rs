//! Line editor, tokenizer, command registry. ROADMAP §5.4.
//!
//! Library half: no port I/O. The kernel thread feeds decoded keys,
//! looks up names in the registry, and runs the `fn` pointer. Subsystems
//! register commands into the table; there is no growing `match` on name.

use crate::kbd::{DecodedKey, NamedKey};

pub const LINE_CAP: usize = 128;
pub const HIST_CAP: usize = 16;
pub const MAX_TOKENS: usize = 16;
pub const MAX_COMMANDS: usize = 48;
pub const PROMPT: &str = "vibeos> ";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feed {
    Pending,
    Submit,
    Cancel,
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenError {
    UnclosedQuote,
    TooMany,
}

impl TokenError {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenError::UnclosedQuote => "unclosed quote",
            TokenError::TooMany => "too many tokens",
        }
    }
}

/// Insert-mode line with history. Cursor is a byte index in `0..=len`.
#[derive(Clone, Copy)]
pub struct LineEditor {
    buf: [u8; LINE_CAP],
    len: usize,
    cur: usize,
    hist: [[u8; LINE_CAP]; HIST_CAP],
    hist_len: [usize; HIST_CAP],
    hist_n: usize,
    hist_head: usize,
    /// `None` = live buffer. `Some(i)` = viewing history slot `i`.
    hist_view: Option<usize>,
    draft: [u8; LINE_CAP],
    draft_len: usize,
}

impl LineEditor {
    pub const fn new() -> Self {
        Self {
            buf: [0; LINE_CAP],
            len: 0,
            cur: 0,
            hist: [[0; LINE_CAP]; HIST_CAP],
            hist_len: [0; HIST_CAP],
            hist_n: 0,
            hist_head: 0,
            hist_view: None,
            draft: [0; LINE_CAP],
            draft_len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn cursor(&self) -> usize {
        self.cur
    }

    pub fn line(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    pub fn line_str(&self) -> Result<&str, core::str::Utf8Error> {
        core::str::from_utf8(self.line())
    }

    pub fn clear(&mut self) {
        self.len = 0;
        self.cur = 0;
        self.hist_view = None;
        self.draft_len = 0;
    }

    pub fn set_line(&mut self, s: &[u8]) {
        let n = s.len().min(LINE_CAP);
        self.buf[..n].copy_from_slice(&s[..n]);
        self.len = n;
        self.cur = n;
        self.hist_view = None;
    }

    pub fn feed(&mut self, key: DecodedKey) -> Feed {
        match key {
            DecodedKey::Char(c) => self.feed_char(c),
            DecodedKey::Named(n) => self.feed_named(n),
        }
    }

    fn feed_char(&mut self, c: u8) -> Feed {
        match c {
            0x03 => {
                self.clear();
                Feed::Cancel
            }
            0x15 => {
                self.kill_to_start();
                Feed::Pending
            }
            0x0B => {
                self.kill_to_end();
                Feed::Pending
            }
            0x01 => {
                self.cur = 0;
                Feed::Pending
            }
            0x05 => {
                self.cur = self.len;
                Feed::Pending
            }
            0x02 => {
                self.left();
                Feed::Pending
            }
            0x06 => {
                self.right();
                Feed::Pending
            }
            0x10 => {
                self.history_up();
                Feed::Pending
            }
            0x0E => {
                self.history_down();
                Feed::Pending
            }
            0x08 | 0x7F => {
                self.backspace();
                Feed::Pending
            }
            b'\n' | b'\r' => self.submit(),
            b'\t' => Feed::Complete,
            0x20..=0x7E => {
                self.insert(c);
                Feed::Pending
            }
            _ => Feed::Pending,
        }
    }

    fn feed_named(&mut self, n: NamedKey) -> Feed {
        match n {
            NamedKey::Left => self.left(),
            NamedKey::Right => self.right(),
            NamedKey::Home => self.cur = 0,
            NamedKey::End => self.cur = self.len,
            NamedKey::Up => self.history_up(),
            NamedKey::Down => self.history_down(),
            NamedKey::Backspace => self.backspace(),
            NamedKey::Delete => self.delete(),
            NamedKey::Enter => return self.submit(),
            NamedKey::Esc => {
                self.clear();
                return Feed::Cancel;
            }
            NamedKey::Tab => return Feed::Complete,
            NamedKey::Insert
            | NamedKey::PageUp
            | NamedKey::PageDown
            | NamedKey::F1
            | NamedKey::F2
            | NamedKey::F3
            | NamedKey::F4
            | NamedKey::F5
            | NamedKey::F6
            | NamedKey::F7
            | NamedKey::F8
            | NamedKey::F9
            | NamedKey::F10
            | NamedKey::F11
            | NamedKey::F12
            | NamedKey::CapsLock
            | NamedKey::NumLock
            | NamedKey::ScrollLock
            | NamedKey::LeftShift
            | NamedKey::RightShift
            | NamedKey::LeftCtrl
            | NamedKey::RightCtrl
            | NamedKey::LeftAlt
            | NamedKey::RightAlt => {}
        }
        Feed::Pending
    }

    fn left(&mut self) {
        if self.cur > 0 {
            self.cur -= 1;
        }
    }

    fn right(&mut self) {
        if self.cur < self.len {
            self.cur += 1;
        }
    }

    fn insert(&mut self, c: u8) {
        if self.len >= LINE_CAP {
            return;
        }
        let i = self.cur;
        let mut j = self.len;
        while j > i {
            self.buf[j] = self.buf[j - 1];
            j -= 1;
        }
        self.buf[i] = c;
        self.len += 1;
        self.cur += 1;
        self.hist_view = None;
    }

    fn backspace(&mut self) {
        if self.cur == 0 {
            return;
        }
        self.cur -= 1;
        self.delete();
        self.hist_view = None;
    }

    fn delete(&mut self) {
        if self.cur >= self.len {
            return;
        }
        let mut i = self.cur;
        while i + 1 < self.len {
            self.buf[i] = self.buf[i + 1];
            i += 1;
        }
        self.len -= 1;
        self.hist_view = None;
    }

    fn kill_to_start(&mut self) {
        if self.cur == 0 {
            return;
        }
        let rest = self.len - self.cur;
        let mut i = 0;
        while i < rest {
            self.buf[i] = self.buf[self.cur + i];
            i += 1;
        }
        self.len = rest;
        self.cur = 0;
        self.hist_view = None;
    }

    fn kill_to_end(&mut self) {
        self.len = self.cur;
        self.hist_view = None;
    }

    fn submit(&mut self) -> Feed {
        if self.len > 0 {
            self.push_history();
        }
        self.hist_view = None;
        self.draft_len = 0;
        Feed::Submit
    }

    fn push_history(&mut self) {
        if self.len == 0 {
            return;
        }
        if self.hist_n > 0 {
            let last = hist_index(self.hist_head, self.hist_n - 1);
            if self.hist_len[last] == self.len
                && self.hist[last][..self.len] == self.buf[..self.len]
            {
                return;
            }
        }
        let slot = if self.hist_n < HIST_CAP {
            let s = (self.hist_head + self.hist_n) % HIST_CAP;
            self.hist_n += 1;
            s
        } else {
            let s = self.hist_head;
            self.hist_head = (self.hist_head + 1) % HIST_CAP;
            s
        };
        self.hist[slot] = self.buf;
        self.hist_len[slot] = self.len;
    }

    fn history_up(&mut self) {
        if self.hist_n == 0 {
            return;
        }
        match self.hist_view {
            None => {
                self.save_draft();
                self.load_hist(self.hist_n - 1);
            }
            Some(0) => {}
            Some(i) => self.load_hist(i - 1),
        }
    }

    fn history_down(&mut self) {
        let Some(i) = self.hist_view else {
            return;
        };
        if i + 1 >= self.hist_n {
            self.restore_draft();
            self.hist_view = None;
            return;
        }
        self.load_hist(i + 1);
    }

    fn save_draft(&mut self) {
        self.draft = self.buf;
        self.draft_len = self.len;
    }

    fn restore_draft(&mut self) {
        self.buf = self.draft;
        self.len = self.draft_len;
        self.cur = self.len;
    }

    fn load_hist(&mut self, view: usize) {
        let slot = hist_index(self.hist_head, view);
        let n = self.hist_len[slot];
        self.buf = self.hist[slot];
        self.len = n;
        self.cur = n;
        self.hist_view = Some(view);
    }
}

fn hist_index(head: usize, view: usize) -> usize {
    (head + view) % HIST_CAP
}

/// Split `line` on whitespace. Quotes (`'` / `"`) keep interior spaces.
/// No escapes. Unclosed quote is an error. Empty unquoted spans are dropped.
pub fn tokenize<'a>(line: &'a str, out: &mut [&'a str]) -> Result<usize, TokenError> {
    let b = line.as_bytes();
    let mut i = 0usize;
    let mut n = 0usize;
    while i < b.len() {
        while i < b.len() && is_ws(b[i]) {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        if n >= out.len() {
            return Err(TokenError::TooMany);
        }
        let (tok, next) = match b[i] {
            q @ (b'"' | b'\'') => take_quoted(line, i, q)?,
            _ => take_word(line, i),
        };
        out[n] = tok;
        n += 1;
        i = next;
    }
    Ok(n)
}

fn is_ws(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

fn take_quoted(line: &str, start: usize, q: u8) -> Result<(&str, usize), TokenError> {
    let b = line.as_bytes();
    let mut i = start + 1;
    while i < b.len() {
        if b[i] == q {
            return Ok((&line[start + 1..i], i + 1));
        }
        i += 1;
    }
    Err(TokenError::UnclosedQuote)
}

fn take_word(line: &str, start: usize) -> (&str, usize) {
    let b = line.as_bytes();
    let mut i = start;
    while i < b.len() && !is_ws(b[i]) && b[i] != b'"' && b[i] != b'\'' {
        i += 1;
    }
    (&line[start..i], i)
}

pub type CmdFn = fn(args: &[&str]);

#[derive(Clone, Copy)]
pub struct Command {
    pub name: &'static str,
    pub help: &'static str,
    pub run: CmdFn,
}

/// Table subsystems register into. Lookup is linear; N is small.
pub struct Registry {
    cmds: [Option<Command>; MAX_COMMANDS],
    n: usize,
}

impl Registry {
    pub const fn new() -> Self {
        Self {
            cmds: [None; MAX_COMMANDS],
            n: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// `false` if full or the name is already present.
    pub fn register(&mut self, cmd: Command) -> bool {
        if self.n >= MAX_COMMANDS || self.lookup(cmd.name).is_some() {
            return false;
        }
        self.cmds[self.n] = Some(cmd);
        self.n += 1;
        true
    }

    pub fn lookup(&self, name: &str) -> Option<Command> {
        let mut i = 0;
        while i < self.n {
            if let Some(c) = self.cmds[i] {
                if c.name == name {
                    return Some(c);
                }
            }
            i += 1;
        }
        None
    }

    pub fn get(&self, i: usize) -> Option<Command> {
        if i >= self.n {
            return None;
        }
        self.cmds[i]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_str(ed: &mut LineEditor, s: &str) {
        for &c in s.as_bytes() {
            assert_eq!(ed.feed(DecodedKey::Char(c)), Feed::Pending);
        }
    }

    fn submit(ed: &mut LineEditor) -> String {
        assert_eq!(ed.feed(DecodedKey::Char(b'\n')), Feed::Submit);
        let s = core::str::from_utf8(ed.line()).unwrap().to_string();
        ed.clear();
        s
    }

    fn toks(s: &str) -> Vec<&str> {
        let mut buf = [""; MAX_TOKENS];
        let n = tokenize(s, &mut buf).unwrap();
        buf[..n].to_vec()
    }

    fn noop(_: &[&str]) {}

    #[test]
    fn echo_insert_and_backspace() {
        let mut ed = LineEditor::new();
        feed_str(&mut ed, "abc");
        assert_eq!(ed.line(), b"abc");
        assert_eq!(ed.cursor(), 3);
        assert_eq!(ed.feed(DecodedKey::Char(0x08)), Feed::Pending);
        assert_eq!(ed.line(), b"ab");
        assert_eq!(ed.cursor(), 2);
        assert_eq!(
            ed.feed(DecodedKey::Named(NamedKey::Backspace)),
            Feed::Pending
        );
        assert_eq!(ed.line(), b"a");
    }

    #[test]
    fn cursor_move_and_insert_middle() {
        let mut ed = LineEditor::new();
        feed_str(&mut ed, "ac");
        assert_eq!(ed.feed(DecodedKey::Named(NamedKey::Left)), Feed::Pending);
        assert_eq!(ed.cursor(), 1);
        assert_eq!(ed.feed(DecodedKey::Char(b'b')), Feed::Pending);
        assert_eq!(ed.line(), b"abc");
        assert_eq!(ed.cursor(), 2);
        assert_eq!(ed.feed(DecodedKey::Named(NamedKey::Home)), Feed::Pending);
        assert_eq!(ed.cursor(), 0);
        assert_eq!(ed.feed(DecodedKey::Named(NamedKey::End)), Feed::Pending);
        assert_eq!(ed.cursor(), 3);
        assert_eq!(ed.feed(DecodedKey::Named(NamedKey::Left)), Feed::Pending);
        assert_eq!(ed.feed(DecodedKey::Named(NamedKey::Delete)), Feed::Pending);
        assert_eq!(ed.line(), b"ab");
    }

    #[test]
    fn ctrl_u_kills_to_start() {
        let mut ed = LineEditor::new();
        feed_str(&mut ed, "hello");
        ed.feed(DecodedKey::Named(NamedKey::Left));
        ed.feed(DecodedKey::Named(NamedKey::Left));
        assert_eq!(ed.feed(DecodedKey::Char(0x15)), Feed::Pending);
        assert_eq!(ed.line(), b"lo");
        assert_eq!(ed.cursor(), 0);
    }

    #[test]
    fn ctrl_c_cancels() {
        let mut ed = LineEditor::new();
        feed_str(&mut ed, "nope");
        assert_eq!(ed.feed(DecodedKey::Char(0x03)), Feed::Cancel);
        assert_eq!(ed.len(), 0);
    }

    #[test]
    fn history_up_down() {
        let mut ed = LineEditor::new();
        feed_str(&mut ed, "one");
        assert_eq!(submit(&mut ed), "one");
        feed_str(&mut ed, "two");
        assert_eq!(submit(&mut ed), "two");
        assert_eq!(ed.feed(DecodedKey::Named(NamedKey::Up)), Feed::Pending);
        assert_eq!(ed.line(), b"two");
        assert_eq!(ed.feed(DecodedKey::Named(NamedKey::Up)), Feed::Pending);
        assert_eq!(ed.line(), b"one");
        assert_eq!(ed.feed(DecodedKey::Named(NamedKey::Up)), Feed::Pending);
        assert_eq!(ed.line(), b"one");
        assert_eq!(ed.feed(DecodedKey::Named(NamedKey::Down)), Feed::Pending);
        assert_eq!(ed.line(), b"two");
        assert_eq!(ed.feed(DecodedKey::Named(NamedKey::Down)), Feed::Pending);
        assert_eq!(ed.line(), b"");
    }

    #[test]
    fn history_skips_duplicate_of_last() {
        let mut ed = LineEditor::new();
        feed_str(&mut ed, "same");
        submit(&mut ed);
        feed_str(&mut ed, "same");
        submit(&mut ed);
        ed.feed(DecodedKey::Named(NamedKey::Up));
        assert_eq!(ed.line(), b"same");
        ed.feed(DecodedKey::Named(NamedKey::Up));
        assert_eq!(ed.line(), b"same");
        ed.feed(DecodedKey::Named(NamedKey::Down));
        assert_eq!(ed.line(), b"");
    }

    #[test]
    fn empty_submit_is_empty() {
        let mut ed = LineEditor::new();
        assert_eq!(ed.feed(DecodedKey::Char(b'\n')), Feed::Submit);
        assert_eq!(ed.line(), b"");
    }

    #[test]
    fn tokenize_whitespace_and_quotes() {
        assert_eq!(toks("echo hi"), ["echo", "hi"]);
        assert_eq!(toks("  echo   hi  "), ["echo", "hi"]);
        assert_eq!(toks("echo \"hello world\""), ["echo", "hello world"]);
        assert_eq!(toks("echo 'a  b' c"), ["echo", "a  b", "c"]);
        assert_eq!(toks(""), Vec::<&str>::new());
        assert_eq!(toks("   \t  "), Vec::<&str>::new());
        assert_eq!(toks("echo \"\""), ["echo", ""]);
    }

    #[test]
    fn tokenize_unclosed_quote() {
        let mut buf = [""; 4];
        assert_eq!(
            tokenize("echo \"hi", &mut buf),
            Err(TokenError::UnclosedQuote)
        );
        assert_eq!(TokenError::UnclosedQuote.as_str(), "unclosed quote");
        assert_eq!(TokenError::TooMany.as_str(), "too many tokens");
    }

    #[test]
    fn tokenize_too_many() {
        let mut buf = [""; 1];
        assert_eq!(tokenize("a b", &mut buf), Err(TokenError::TooMany));
    }

    #[test]
    fn registry_lookup_not_match() {
        let mut r = Registry::new();
        assert!(r.register(Command {
            name: "help",
            help: "list commands",
            run: noop,
        }));
        assert!(r.register(Command {
            name: "echo",
            help: "print args",
            run: noop,
        }));
        assert!(!r.register(Command {
            name: "help",
            help: "dup",
            run: noop,
        }));
        assert!(r.lookup("help").is_some());
        assert!(r.lookup("echo").is_some());
        assert!(r.lookup("panic").is_none());
        assert_eq!(r.len(), 2);
        assert_eq!(r.get(0).unwrap().name, "help");
        assert!(r.get(9).is_none());
    }

    #[test]
    fn tab_is_complete() {
        let mut ed = LineEditor::new();
        feed_str(&mut ed, "ls he");
        assert_eq!(ed.feed(DecodedKey::Char(b'\t')), Feed::Complete);
        assert_eq!(ed.feed(DecodedKey::Named(NamedKey::Tab)), Feed::Complete);
        ed.set_line(b"cat hello");
        assert_eq!(ed.line(), b"cat hello");
        assert_eq!(ed.cursor(), 9);
    }

    #[test]
    fn named_keys_are_exhaustive_in_editor() {
        // Touch every NamedKey through the editor so a new variant fails
        // the match in feed_named at compile time; this keeps the test
        // honest about the list.
        let mut ed = LineEditor::new();
        let all = [
            NamedKey::Esc,
            NamedKey::Enter,
            NamedKey::Backspace,
            NamedKey::Tab,
            NamedKey::Left,
            NamedKey::Right,
            NamedKey::Up,
            NamedKey::Down,
            NamedKey::Home,
            NamedKey::End,
            NamedKey::Delete,
            NamedKey::Insert,
            NamedKey::PageUp,
            NamedKey::PageDown,
            NamedKey::F1,
            NamedKey::F2,
            NamedKey::F3,
            NamedKey::F4,
            NamedKey::F5,
            NamedKey::F6,
            NamedKey::F7,
            NamedKey::F8,
            NamedKey::F9,
            NamedKey::F10,
            NamedKey::F11,
            NamedKey::F12,
            NamedKey::CapsLock,
            NamedKey::NumLock,
            NamedKey::ScrollLock,
            NamedKey::LeftShift,
            NamedKey::RightShift,
            NamedKey::LeftCtrl,
            NamedKey::RightCtrl,
            NamedKey::LeftAlt,
            NamedKey::RightAlt,
        ];
        for k in all {
            let _ = ed.feed(DecodedKey::Named(k));
        }
    }
}

//! PS/2 scan-code set 1 decoder and ISR ring. ROADMAP §5.2, DESIGN §9.4.
//!
//! Library half: no port I/O. The ISR writes the ring; the consumer
//! drains with interrupts off.

/// i8042 ports and bits. Hardware pokes stay in the binary crate.
pub const DATA: u16 = 0x60;
pub const STATUS: u16 = 0x64;
pub const CMD: u16 = 0x64;

pub const STAT_OBF: u8 = 1 << 0;
pub const STAT_IBF: u8 = 1 << 1;
pub const STAT_MOUSE: u8 = 1 << 5;

pub const CMD_READ_CFG: u8 = 0x20;
pub const CMD_WRITE_CFG: u8 = 0x60;
pub const CMD_DISABLE_2: u8 = 0xA7;
pub const CMD_SELF_TEST: u8 = 0xAA;
pub const CMD_TEST_1: u8 = 0xAB;
pub const CMD_DISABLE_1: u8 = 0xAD;
pub const CMD_ENABLE_1: u8 = 0xAE;
/// Next data byte is presented as keyboard input and raises IRQ1 if INT1.
pub const CMD_WRITE_KBD_OUT: u8 = 0xD2;

pub const SELF_TEST_OK: u8 = 0x55;
pub const PORT_TEST_OK: u8 = 0x00;

pub const CFG_INT1: u8 = 1 << 0;
pub const CFG_INT2: u8 = 1 << 1;
/// First PS/2 port clock. 1 = disabled. Command 0xAD sets this; 0xAE
/// clears it. A config rewrite that leaves it set kills IRQ1.
pub const CFG_CLOCK1_OFF: u8 = 1 << 4;
pub const CFG_CLOCK2_OFF: u8 = 1 << 5;
pub const CFG_TRANSLATE: u8 = 1 << 6;

/// IRQs off, keyboard clock on, aux clock off, set-1 translate.
/// `DISABLE_1` sets bit 4; this must clear it or the later live write
/// re-disables the port.
pub const fn cfg_probe(raw: u8) -> u8 {
    (raw & !(CFG_INT1 | CFG_INT2 | CFG_CLOCK1_OFF)) | CFG_CLOCK2_OFF | CFG_TRANSLATE
}

/// Port-1 IRQ on, keyboard clock on, aux clock off, set-1 translate.
pub const fn cfg_run(raw: u8) -> u8 {
    cfg_probe(raw) | CFG_INT1
}

pub const fn cfg_clock1_on(cfg: u8) -> bool {
    cfg & CFG_CLOCK1_OFF == 0
}

pub const fn cfg_int1_on(cfg: u8) -> bool {
    cfg & CFG_INT1 != 0
}

pub const KBD_RESET: u8 = 0xFF;
pub const KBD_ACK: u8 = 0xFA;
pub const KBD_BAT_OK: u8 = 0xAA;

pub const SC_E0: u8 = 0xE0;
pub const SC_E1: u8 = 0xE1;
pub const SC_RELEASE: u8 = 0x80;

pub const RING_CAP: usize = 64;

// Keys whose make is an edge, not a hold. Typematic repeats the last
// scancode; treating those as new presses flips Caps/Num and floods
// the ISR ring with modifiers.
const DOWN_LSHIFT: u16 = 1 << 0;
const DOWN_RSHIFT: u16 = 1 << 1;
const DOWN_LCTRL: u16 = 1 << 2;
const DOWN_RCTRL: u16 = 1 << 3;
const DOWN_LALT: u16 = 1 << 4;
const DOWN_RALT: u16 = 1 << 5;
const DOWN_CAPS: u16 = 1 << 6;
const DOWN_NUM: u16 = 1 << 7;
const DOWN_SCROLL: u16 = 1 << 8;
const DOWN_SHIFT: u16 = DOWN_LSHIFT | DOWN_RSHIFT;
const DOWN_CTRL: u16 = DOWN_LCTRL | DOWN_RCTRL;
const DOWN_ALT: u16 = DOWN_LALT | DOWN_RALT;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamedKey {
    Esc,
    Enter,
    Backspace,
    Tab,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Delete,
    Insert,
    PageUp,
    PageDown,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    CapsLock,
    NumLock,
    ScrollLock,
    LeftShift,
    RightShift,
    LeftCtrl,
    RightCtrl,
    LeftAlt,
    RightAlt,
}

impl NamedKey {
    pub const fn as_str(self) -> &'static str {
        match self {
            NamedKey::Esc => "esc",
            NamedKey::Enter => "enter",
            NamedKey::Backspace => "backspace",
            NamedKey::Tab => "tab",
            NamedKey::Left => "left",
            NamedKey::Right => "right",
            NamedKey::Up => "up",
            NamedKey::Down => "down",
            NamedKey::Home => "home",
            NamedKey::End => "end",
            NamedKey::Delete => "delete",
            NamedKey::Insert => "insert",
            NamedKey::PageUp => "pageup",
            NamedKey::PageDown => "pagedown",
            NamedKey::F1 => "f1",
            NamedKey::F2 => "f2",
            NamedKey::F3 => "f3",
            NamedKey::F4 => "f4",
            NamedKey::F5 => "f5",
            NamedKey::F6 => "f6",
            NamedKey::F7 => "f7",
            NamedKey::F8 => "f8",
            NamedKey::F9 => "f9",
            NamedKey::F10 => "f10",
            NamedKey::F11 => "f11",
            NamedKey::F12 => "f12",
            NamedKey::CapsLock => "caps",
            NamedKey::NumLock => "num",
            NamedKey::ScrollLock => "scroll",
            NamedKey::LeftShift => "lshift",
            NamedKey::RightShift => "rshift",
            NamedKey::LeftCtrl => "lctrl",
            NamedKey::RightCtrl => "rctrl",
            NamedKey::LeftAlt => "lalt",
            NamedKey::RightAlt => "ralt",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodedKey {
    Char(u8),
    Named(NamedKey),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub caps: bool,
    pub num: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Decoder {
    e0: bool,
    e1: u8,
    mods: Mods,
    down: u16,
}

impl Decoder {
    pub const fn new() -> Self {
        Self {
            e0: false,
            e1: 0,
            mods: Mods {
                shift: false,
                ctrl: false,
                alt: false,
                caps: false,
                num: false,
            },
            down: 0,
        }
    }

    pub fn mods(&self) -> Mods {
        self.mods
    }

    /// Consume one set-1 byte. `None` for prefix, break, unknown, or a
    /// typematic make of a lock/modifier (already down).
    pub fn feed(&mut self, sc: u8) -> Option<DecodedKey> {
        if self.e1 > 0 {
            self.e1 -= 1;
            return None;
        }
        if sc == SC_E1 {
            self.e1 = 5; // Pause: E1 1D 45 E1 9D C5
            self.e0 = false;
            return None;
        }
        if sc == SC_E0 {
            self.e0 = true;
            return None;
        }
        let e0 = self.e0;
        self.e0 = false;
        let release = sc & SC_RELEASE != 0;
        let make = sc & !SC_RELEASE;
        if e0 {
            self.feed_e0(make, release)
        } else {
            self.feed_plain(make, release)
        }
    }

    /// First make → true. Typematic make and every break → false.
    fn make_edge(&mut self, bit: u16, release: bool) -> bool {
        if release {
            self.down &= !bit;
            return false;
        }
        if self.down & bit != 0 {
            return false;
        }
        self.down |= bit;
        true
    }

    fn sync_mods(&mut self) {
        self.mods.shift = self.down & DOWN_SHIFT != 0;
        self.mods.ctrl = self.down & DOWN_CTRL != 0;
        self.mods.alt = self.down & DOWN_ALT != 0;
    }

    fn modifier(&mut self, bit: u16, release: bool, named: NamedKey) -> Option<DecodedKey> {
        let first = self.make_edge(bit, release);
        self.sync_mods();
        if first {
            Some(DecodedKey::Named(named))
        } else {
            None
        }
    }

    fn lock_toggle(
        &mut self,
        bit: u16,
        release: bool,
        named: NamedKey,
        toggle: impl FnOnce(&mut Mods),
    ) -> Option<DecodedKey> {
        if !self.make_edge(bit, release) {
            return None;
        }
        toggle(&mut self.mods);
        Some(DecodedKey::Named(named))
    }

    fn feed_plain(&mut self, make: u8, release: bool) -> Option<DecodedKey> {
        match make {
            0x2A => self.modifier(DOWN_LSHIFT, release, NamedKey::LeftShift),
            0x36 => self.modifier(DOWN_RSHIFT, release, NamedKey::RightShift),
            0x1D => self.modifier(DOWN_LCTRL, release, NamedKey::LeftCtrl),
            0x38 => self.modifier(DOWN_LALT, release, NamedKey::LeftAlt),
            0x3A => self.lock_toggle(DOWN_CAPS, release, NamedKey::CapsLock, |m| {
                m.caps = !m.caps;
            }),
            0x45 => self.lock_toggle(DOWN_NUM, release, NamedKey::NumLock, |m| m.num = !m.num),
            0x46 => {
                if self.make_edge(DOWN_SCROLL, release) {
                    Some(DecodedKey::Named(NamedKey::ScrollLock))
                } else {
                    None
                }
            }
            _ => {
                if release {
                    return None;
                }
                self.decode_make(make, false)
            }
        }
    }

    fn feed_e0(&mut self, make: u8, release: bool) -> Option<DecodedKey> {
        match make {
            0x1D => self.modifier(DOWN_RCTRL, release, NamedKey::RightCtrl),
            0x38 => self.modifier(DOWN_RALT, release, NamedKey::RightAlt),
            _ => {
                if release {
                    None
                } else {
                    self.decode_make(make, true)
                }
            }
        }
    }

    fn decode_make(&self, make: u8, e0: bool) -> Option<DecodedKey> {
        if e0 {
            return e0_named(make).map(DecodedKey::Named).or_else(|| {
                if make == 0x1C {
                    Some(DecodedKey::Char(b'\n'))
                } else {
                    None
                }
            });
        }
        if let Some(n) = plain_named(make) {
            return Some(DecodedKey::Named(n));
        }
        let shifted = self.mods.shift;
        let ch = match make {
            0x01 => return Some(DecodedKey::Named(NamedKey::Esc)),
            0x0E => return Some(DecodedKey::Char(0x08)),
            0x0F => return Some(DecodedKey::Char(b'\t')),
            0x1C => return Some(DecodedKey::Char(b'\n')),
            0x39 => b' ',
            0x02..=0x0D => unshifted_top(make, shifted)?,
            0x10..=0x1B => from_row(make, 0x10, b"qwertyuiop[]", shifted, self.mods.caps)?,
            0x1E..=0x28 => from_row(make, 0x1E, b"asdfghjkl;'", shifted, self.mods.caps)?,
            0x2B => {
                if shifted {
                    b'|'
                } else {
                    b'\\'
                }
            }
            0x2C..=0x35 => from_row(make, 0x2C, b"zxcvbnm,./", shifted, self.mods.caps)?,
            0x29 => {
                if shifted {
                    b'~'
                } else {
                    b'`'
                }
            }
            0x37 => b'*',
            0x47..=0x53 => return keypad(make, self.mods.num, shifted),
            _ => return None,
        };
        Some(self.apply_ctrl(ch))
    }

    fn apply_ctrl(&self, ch: u8) -> DecodedKey {
        if self.mods.ctrl {
            let up = ch.to_ascii_uppercase();
            if (b'A'..=b'Z').contains(&up) {
                return DecodedKey::Char(up - b'A' + 1);
            }
        }
        DecodedKey::Char(ch)
    }
}

fn unshifted_top(make: u8, shifted: bool) -> Option<u8> {
    const U: &[u8] = b"1234567890-=";
    const S: &[u8] = b"!@#$%^&*()_+";
    let i = (make - 0x02) as usize;
    if i >= U.len() {
        return None;
    }
    Some(if shifted { S[i] } else { U[i] })
}

fn from_row(make: u8, base: u8, map: &[u8], shifted: bool, caps: bool) -> Option<u8> {
    let i = (make - base) as usize;
    if i >= map.len() {
        return None;
    }
    Some(case_letter(map[i], shifted, caps))
}

fn case_letter(c: u8, shifted: bool, caps: bool) -> u8 {
    if (b'a'..=b'z').contains(&c) {
        let upper = shifted != caps;
        if upper {
            c.to_ascii_uppercase()
        } else {
            c
        }
    } else if shifted {
        match c {
            b'[' => b'{',
            b']' => b'}',
            b';' => b':',
            b'\'' => b'"',
            b',' => b'<',
            b'.' => b'>',
            b'/' => b'?',
            _ => c,
        }
    } else {
        c
    }
}

fn plain_named(make: u8) -> Option<NamedKey> {
    Some(match make {
        0x3B => NamedKey::F1,
        0x3C => NamedKey::F2,
        0x3D => NamedKey::F3,
        0x3E => NamedKey::F4,
        0x3F => NamedKey::F5,
        0x40 => NamedKey::F6,
        0x41 => NamedKey::F7,
        0x42 => NamedKey::F8,
        0x43 => NamedKey::F9,
        0x44 => NamedKey::F10,
        0x57 => NamedKey::F11,
        0x58 => NamedKey::F12,
        _ => return None,
    })
}

fn e0_named(make: u8) -> Option<NamedKey> {
    Some(match make {
        0x47 => NamedKey::Home,
        0x48 => NamedKey::Up,
        0x49 => NamedKey::PageUp,
        0x4B => NamedKey::Left,
        0x4D => NamedKey::Right,
        0x4F => NamedKey::End,
        0x50 => NamedKey::Down,
        0x51 => NamedKey::PageDown,
        0x52 => NamedKey::Insert,
        0x53 => NamedKey::Delete,
        _ => return None,
    })
}

fn keypad(make: u8, num: bool, _shifted: bool) -> Option<DecodedKey> {
    if num {
        let ch = match make {
            0x47 => b'7',
            0x48 => b'8',
            0x49 => b'9',
            0x4A => b'-',
            0x4B => b'4',
            0x4C => b'5',
            0x4D => b'6',
            0x4E => b'+',
            0x4F => b'1',
            0x50 => b'2',
            0x51 => b'3',
            0x52 => b'0',
            0x53 => b'.',
            _ => return None,
        };
        Some(DecodedKey::Char(ch))
    } else {
        e0_named(make).map(DecodedKey::Named).or(match make {
            0x4A => Some(DecodedKey::Char(b'-')),
            0x4C => Some(DecodedKey::Char(b'5')),
            0x4E => Some(DecodedKey::Char(b'+')),
            _ => None,
        })
    }
}

/// Fixed ring. Wrap drops the oldest. ISR is the only producer.
pub struct Ring<T: Copy, const N: usize> {
    buf: [T; N],
    head: usize,
    tail: usize,
    len: usize,
    dropped: u64,
}

impl<T: Copy + Default, const N: usize> Ring<T, N> {
    pub fn new() -> Self {
        Self {
            buf: [T::default(); N],
            head: 0,
            tail: 0,
            len: 0,
            dropped: 0,
        }
    }
}

impl<T: Copy, const N: usize> Ring<T, N> {
    pub const fn empty(fill: T) -> Self {
        Self {
            buf: [fill; N],
            head: 0,
            tail: 0,
            len: 0,
            dropped: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Push. If full, the oldest key is dropped.
    pub fn push(&mut self, v: T) {
        if N == 0 {
            return;
        }
        if self.len == N {
            self.tail = (self.tail + 1) % N;
            self.len -= 1;
            self.dropped = self.dropped.saturating_add(1);
        }
        self.buf[self.head] = v;
        self.head = (self.head + 1) % N;
        self.len += 1;
    }

    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let v = self.buf[self.tail];
        self.tail = (self.tail + 1) % N;
        self.len -= 1;
        Some(v)
    }
}

impl Default for DecodedKey {
    fn default() -> Self {
        DecodedKey::Char(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(bytes: &[u8]) -> Vec<DecodedKey> {
        let mut d = Decoder::new();
        bytes.iter().filter_map(|b| d.feed(*b)).collect()
    }

    #[test]
    fn cfg_run_clears_clock1_left_by_disable() {
        // 0xAD sets bit 4. Rewriting that byte with only INT1 or'd on
        // used to leave the keyboard clock off (#66).
        let after_disable = CFG_CLOCK1_OFF | CFG_TRANSLATE;
        let probe = cfg_probe(after_disable);
        assert!(cfg_clock1_on(probe));
        assert!(!cfg_int1_on(probe));
        assert_eq!(probe & CFG_INT2, 0);
        assert_ne!(probe & CFG_CLOCK2_OFF, 0);
        assert_ne!(probe & CFG_TRANSLATE, 0);
        let live = cfg_run(after_disable);
        assert!(cfg_clock1_on(live));
        assert!(cfg_int1_on(live));
        assert_eq!(live & CFG_INT2, 0);
        assert_ne!(live & CFG_CLOCK2_OFF, 0);
        assert_ne!(live & CFG_TRANSLATE, 0);
        // Stale rewrite: INT1 on, bit 4 still set. That is the bug.
        let stale = after_disable | CFG_INT1;
        assert!(!cfg_clock1_on(stale));
        assert!(cfg_int1_on(stale));
    }

    #[test]
    fn letter_and_shift() {
        assert_eq!(feed(&[0x1E]), [DecodedKey::Char(b'a')]);
        assert_eq!(
            feed(&[0x2A, 0x1E, 0x9E, 0xAA, 0x1E]),
            [
                DecodedKey::Named(NamedKey::LeftShift),
                DecodedKey::Char(b'A'),
                DecodedKey::Char(b'a'),
            ]
        );
    }

    #[test]
    fn caps_xor_shift() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(0x3A), Some(DecodedKey::Named(NamedKey::CapsLock)));
        assert_eq!(d.feed(0x1E), Some(DecodedKey::Char(b'A')));
        assert_eq!(d.feed(0x2A), Some(DecodedKey::Named(NamedKey::LeftShift)));
        assert_eq!(d.feed(0x1E), Some(DecodedKey::Char(b'a')));
    }

    #[test]
    fn break_is_make_plus_0x80() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(0x1E), Some(DecodedKey::Char(b'a')));
        assert_eq!(d.feed(0x9E), None); // break
        assert_eq!(d.feed(0x2A), Some(DecodedKey::Named(NamedKey::LeftShift)));
        assert_eq!(d.feed(0xAA), None);
        assert!(!d.mods().shift);
    }

    #[test]
    fn e0_arrows_make_and_break() {
        assert_eq!(feed(&[0xE0, 0x4B]), [DecodedKey::Named(NamedKey::Left)]);
        assert_eq!(feed(&[0xE0, 0xCB]), Vec::<DecodedKey>::new());
        assert_eq!(
            feed(&[0xE0, 0x48, 0xE0, 0xC8, 0xE0, 0x50]),
            [
                DecodedKey::Named(NamedKey::Up),
                DecodedKey::Named(NamedKey::Down),
            ]
        );
        assert_eq!(feed(&[0xE0, 0x1C]), [DecodedKey::Char(b'\n')]);
    }

    #[test]
    fn ctrl_c_is_etx() {
        let mut d = Decoder::new();
        d.feed(0x1D);
        assert_eq!(d.feed(0x2E), Some(DecodedKey::Char(3)));
    }

    #[test]
    fn digits_and_shift() {
        assert_eq!(feed(&[0x02]), [DecodedKey::Char(b'1')]);
        assert_eq!(
            feed(&[0x36, 0x02]),
            [
                DecodedKey::Named(NamedKey::RightShift),
                DecodedKey::Char(b'!'),
            ]
        );
    }

    #[test]
    fn named_as_str_is_exhaustive() {
        // Touch every variant so a new arm fails this match.
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
            assert!(!k.as_str().is_empty());
        }
    }

    #[test]
    fn ring_wrap_drops_oldest() {
        let mut r: Ring<u8, 4> = Ring::new();
        r.push(1);
        r.push(2);
        r.push(3);
        r.push(4);
        assert_eq!(r.len(), 4);
        r.push(5);
        assert_eq!(r.dropped(), 1);
        assert_eq!(r.pop(), Some(2));
        assert_eq!(r.pop(), Some(3));
        assert_eq!(r.pop(), Some(4));
        assert_eq!(r.pop(), Some(5));
        assert_eq!(r.pop(), None);
    }

    #[test]
    fn ring_drain_is_sequential() {
        // Host stand-in for "IRQ-off drain": producer then consumer,
        // never overlapping. The kernel wraps pop in InterruptGuard.
        let mut r: Ring<DecodedKey, 8> = Ring::new();
        let mut d = Decoder::new();
        for b in [0x1E, 0x30, 0x2E] {
            if let Some(k) = d.feed(b) {
                r.push(k);
            }
        }
        let mut out = Vec::new();
        while let Some(k) = r.pop() {
            out.push(k);
        }
        assert_eq!(
            out,
            [
                DecodedKey::Char(b'a'),
                DecodedKey::Char(b'b'),
                DecodedKey::Char(b'c'),
            ]
        );
    }

    #[test]
    fn numlock_keypad() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(0x45), Some(DecodedKey::Named(NamedKey::NumLock)));
        assert_eq!(d.feed(0xC5), None); // break, else the next make is typematic
        assert_eq!(d.feed(0x47), Some(DecodedKey::Char(b'7')));
        assert_eq!(d.feed(0x45), Some(DecodedKey::Named(NamedKey::NumLock)));
        assert_eq!(d.feed(0xC5), None);
        assert!(!d.mods().num);
        assert_eq!(d.feed(0x47), Some(DecodedKey::Named(NamedKey::Home)));
    }

    #[test]
    fn typematic_caps_does_not_retoggle() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(0x3A), Some(DecodedKey::Named(NamedKey::CapsLock)));
        assert!(d.mods().caps);
        assert_eq!(d.feed(0x3A), None);
        assert_eq!(d.feed(0x3A), None);
        assert!(d.mods().caps);
        assert_eq!(d.feed(0xBA), None);
        assert!(d.mods().caps);
        assert_eq!(d.feed(0x1E), Some(DecodedKey::Char(b'A')));
        assert_eq!(d.feed(0x3A), Some(DecodedKey::Named(NamedKey::CapsLock)));
        assert!(!d.mods().caps);
        assert_eq!(d.feed(0x1E), Some(DecodedKey::Char(b'a')));
    }

    #[test]
    fn typematic_num_does_not_retoggle() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(0x45), Some(DecodedKey::Named(NamedKey::NumLock)));
        assert!(d.mods().num);
        assert_eq!(d.feed(0x45), None);
        assert_eq!(d.feed(0x45), None);
        assert!(d.mods().num);
        assert_eq!(d.feed(0xC5), None);
        assert!(d.mods().num);
        assert_eq!(d.feed(0x47), Some(DecodedKey::Char(b'7')));
    }

    #[test]
    fn typematic_shift_does_not_reenqueue() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(0x2A), Some(DecodedKey::Named(NamedKey::LeftShift)));
        assert!(d.mods().shift);
        assert_eq!(d.feed(0x2A), None);
        assert_eq!(d.feed(0x2A), None);
        assert!(d.mods().shift);
        assert_eq!(d.feed(0x1E), Some(DecodedKey::Char(b'A')));
        assert_eq!(d.feed(0xAA), None);
        assert!(!d.mods().shift);
        assert_eq!(d.feed(0x1E), Some(DecodedKey::Char(b'a')));
    }

    #[test]
    fn typematic_letter_still_repeats() {
        assert_eq!(
            feed(&[0x1E, 0x1E, 0x1E, 0x9E]),
            [
                DecodedKey::Char(b'a'),
                DecodedKey::Char(b'a'),
                DecodedKey::Char(b'a'),
            ]
        );
    }
}

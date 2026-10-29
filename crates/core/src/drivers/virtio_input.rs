//! virtio-input event decode. ROADMAP §11.5.
//!
//! Event wire format is virtio 1.2 §5.8.6 (`le16 type`, `le16 code`,
//! `le32 value`). Key codes are Linux `input-event-codes.h` (`EV_KEY`,
//! `KEY_*`), cited not copied (DESIGN §1.5). Pointer `EV_REL`/`EV_ABS`
//! events are consumed and produce no console key.

use crate::kbd::{DecodedKey, Mods, NamedKey};

/// `EV_SYN` — `include/uapi/linux/input-event-codes.h`.
pub const EV_SYN: u16 = 0;
/// `EV_KEY`.
pub const EV_KEY: u16 = 1;
/// `EV_REL`.
pub const EV_REL: u16 = 2;
/// `EV_ABS`.
pub const EV_ABS: u16 = 3;

/// virtio 1.2 §5.8.6 event size.
pub const EVENT_SIZE: usize = 8;

/// One virtio-input event, host endian after a little-endian load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub ty: u16,
    pub code: u16,
    pub value: i32,
}

impl Event {
    /// Parse one little-endian event. `None` when `raw` is short.
    pub fn from_le_bytes(raw: &[u8]) -> Option<Self> {
        let b0 = *raw.first()?;
        let b1 = *raw.get(1)?;
        let b2 = *raw.get(2)?;
        let b3 = *raw.get(3)?;
        let b4 = *raw.get(4)?;
        let b5 = *raw.get(5)?;
        let b6 = *raw.get(6)?;
        let b7 = *raw.get(7)?;
        Some(Self {
            ty: u16::from_le_bytes([b0, b1]),
            code: u16::from_le_bytes([b2, b3]),
            value: i32::from_le_bytes([b4, b5, b6, b7]),
        })
    }
}

/// `KEY_*` used for the console (Linux `input-event-codes.h`).
pub const KEY_ESC: u16 = 1;
pub const KEY_1: u16 = 2;
pub const KEY_MINUS: u16 = 12;
pub const KEY_EQUAL: u16 = 13;
pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_TAB: u16 = 15;
pub const KEY_Q: u16 = 16;
pub const KEY_ENTER: u16 = 28;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_A: u16 = 30;
pub const KEY_SEMICOLON: u16 = 39;
pub const KEY_APOSTROPHE: u16 = 40;
pub const KEY_GRAVE: u16 = 41;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_BACKSLASH: u16 = 43;
pub const KEY_Z: u16 = 44;
pub const KEY_COMMA: u16 = 51;
pub const KEY_DOT: u16 = 52;
pub const KEY_SLASH: u16 = 53;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_SPACE: u16 = 57;
pub const KEY_CAPSLOCK: u16 = 58;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_RIGHTALT: u16 = 100;

/// Press / release / repeat as Linux `EV_KEY` `value`.
pub const KEY_RELEASE: i32 = 0;
pub const KEY_PRESS: i32 = 1;
pub const KEY_REPEAT: i32 = 2;

/// Evdev `EV_KEY` to a console key, with shift/caps/ctrl.
pub struct EvDecoder {
    mods: Mods,
}

impl Default for EvDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl EvDecoder {
    pub const fn new() -> Self {
        Self {
            mods: Mods {
                shift: false,
                ctrl: false,
                alt: false,
                caps: false,
                num: false,
            },
        }
    }

    /// One event. `EV_REL`/`EV_ABS`/`EV_SYN` return `None` (pointer /
    /// sync). `EV_KEY` press or repeat may yield a `DecodedKey`.
    pub fn feed(&mut self, ev: Event) -> Option<DecodedKey> {
        if ev.ty != EV_KEY {
            return None;
        }
        match ev.code {
            KEY_LEFTSHIFT | KEY_RIGHTSHIFT => {
                self.mods.shift = ev.value != KEY_RELEASE;
                if ev.value == KEY_PRESS {
                    return Some(DecodedKey::Named(if ev.code == KEY_LEFTSHIFT {
                        NamedKey::LeftShift
                    } else {
                        NamedKey::RightShift
                    }));
                }
                None
            }
            KEY_LEFTCTRL | KEY_RIGHTCTRL => {
                self.mods.ctrl = ev.value != KEY_RELEASE;
                if ev.value == KEY_PRESS {
                    return Some(DecodedKey::Named(if ev.code == KEY_LEFTCTRL {
                        NamedKey::LeftCtrl
                    } else {
                        NamedKey::RightCtrl
                    }));
                }
                None
            }
            KEY_LEFTALT | KEY_RIGHTALT => {
                self.mods.alt = ev.value != KEY_RELEASE;
                if ev.value == KEY_PRESS {
                    return Some(DecodedKey::Named(if ev.code == KEY_LEFTALT {
                        NamedKey::LeftAlt
                    } else {
                        NamedKey::RightAlt
                    }));
                }
                None
            }
            KEY_CAPSLOCK => {
                if ev.value == KEY_PRESS {
                    self.mods.caps = !self.mods.caps;
                    return Some(DecodedKey::Named(NamedKey::CapsLock));
                }
                None
            }
            _ => {
                if ev.value == KEY_RELEASE {
                    return None;
                }
                key_to_decoded(ev.code, &self.mods)
            }
        }
    }
}

fn key_to_decoded(code: u16, mods: &Mods) -> Option<DecodedKey> {
    match code {
        KEY_ESC => Some(DecodedKey::Named(NamedKey::Esc)),
        KEY_BACKSPACE => Some(DecodedKey::Char(0x08)),
        KEY_TAB => Some(DecodedKey::Char(b'\t')),
        KEY_ENTER => Some(DecodedKey::Char(b'\n')),
        KEY_SPACE => Some(DecodedKey::Char(b' ')),
        KEY_1..=11 => Some(DecodedKey::Char(digit(code, mods.shift))),
        KEY_MINUS => Some(DecodedKey::Char(if mods.shift { b'_' } else { b'-' })),
        KEY_EQUAL => Some(DecodedKey::Char(if mods.shift { b'+' } else { b'=' })),
        KEY_LEFTBRACE => Some(DecodedKey::Char(if mods.shift { b'{' } else { b'[' })),
        KEY_RIGHTBRACE => Some(DecodedKey::Char(if mods.shift { b'}' } else { b']' })),
        KEY_SEMICOLON => Some(DecodedKey::Char(if mods.shift { b':' } else { b';' })),
        KEY_APOSTROPHE => Some(DecodedKey::Char(if mods.shift { b'"' } else { b'\'' })),
        KEY_GRAVE => Some(DecodedKey::Char(if mods.shift { b'~' } else { b'`' })),
        KEY_BACKSLASH => Some(DecodedKey::Char(if mods.shift { b'|' } else { b'\\' })),
        KEY_COMMA => Some(DecodedKey::Char(if mods.shift { b'<' } else { b',' })),
        KEY_DOT => Some(DecodedKey::Char(if mods.shift { b'>' } else { b'.' })),
        KEY_SLASH => Some(DecodedKey::Char(if mods.shift { b'?' } else { b'/' })),
        KEY_Q..=25 | KEY_A..=38 | KEY_Z..=50 => letter(code, mods).map(DecodedKey::Char),
        _ => None,
    }
}

const KEY_LEFTBRACE: u16 = 26;
const KEY_RIGHTBRACE: u16 = 27;

fn digit(code: u16, shift: bool) -> u8 {
    const UNSHIFT: &[u8] = b"1234567890";
    const SHIFT: &[u8] = b"!@#$%^&*()";
    let i = (code - KEY_1) as usize;
    let t = if shift { SHIFT } else { UNSHIFT };
    t.get(i).copied().unwrap_or(b'?')
}

fn letter(code: u16, mods: &Mods) -> Option<u8> {
    let ch = match code {
        16 => b'q',
        17 => b'w',
        18 => b'e',
        19 => b'r',
        20 => b't',
        21 => b'y',
        22 => b'u',
        23 => b'i',
        24 => b'o',
        25 => b'p',
        30 => b'a',
        31 => b's',
        32 => b'd',
        33 => b'f',
        34 => b'g',
        35 => b'h',
        36 => b'j',
        37 => b'k',
        38 => b'l',
        44 => b'z',
        45 => b'x',
        46 => b'c',
        47 => b'v',
        48 => b'b',
        49 => b'n',
        50 => b'm',
        _ => return None,
    };
    let upper = mods.shift != mods.caps;
    let out = if upper { ch.to_ascii_uppercase() } else { ch };
    if mods.ctrl && out.is_ascii_alphabetic() {
        Some(out.to_ascii_uppercase() & 0x1F)
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: u16) -> Event {
        Event {
            ty: EV_KEY,
            code,
            value: KEY_PRESS,
        }
    }

    fn release(code: u16) -> Event {
        Event {
            ty: EV_KEY,
            code,
            value: KEY_RELEASE,
        }
    }

    #[test]
    fn event_from_le_bytes() {
        let raw = [0x01, 0x00, 0x1e, 0x00, 0x01, 0x00, 0x00, 0x00];
        assert_eq!(
            Event::from_le_bytes(&raw),
            Some(Event {
                ty: EV_KEY,
                code: KEY_A,
                value: KEY_PRESS
            })
        );
        assert_eq!(Event::from_le_bytes(&[0; 7]), None);
    }

    #[test]
    fn letters_and_echo_keys() {
        let mut d = EvDecoder::new();
        assert_eq!(d.feed(press(KEY_A)), Some(DecodedKey::Char(b'a')));
        assert_eq!(d.feed(release(KEY_A)), None);
        assert_eq!(d.feed(press(KEY_ENTER)), Some(DecodedKey::Char(b'\n')));
        assert_eq!(d.feed(press(KEY_SPACE)), Some(DecodedKey::Char(b' ')));
        assert_eq!(d.feed(press(KEY_MINUS)), Some(DecodedKey::Char(b'-')));
        assert_eq!(d.feed(press(KEY_1)), Some(DecodedKey::Char(b'1')));
        assert_eq!(d.feed(press(18)), Some(DecodedKey::Char(b'e')));
    }

    #[test]
    fn shift_and_caps() {
        let mut d = EvDecoder::new();
        assert_eq!(
            d.feed(press(KEY_LEFTSHIFT)),
            Some(DecodedKey::Named(NamedKey::LeftShift))
        );
        assert_eq!(d.feed(press(KEY_A)), Some(DecodedKey::Char(b'A')));
        assert_eq!(d.feed(press(KEY_MINUS)), Some(DecodedKey::Char(b'_')));
        assert_eq!(d.feed(release(KEY_LEFTSHIFT)), None);
        assert_eq!(d.feed(press(KEY_A)), Some(DecodedKey::Char(b'a')));
        assert_eq!(
            d.feed(press(KEY_CAPSLOCK)),
            Some(DecodedKey::Named(NamedKey::CapsLock))
        );
        assert_eq!(d.feed(press(KEY_A)), Some(DecodedKey::Char(b'A')));
    }

    #[test]
    fn pointer_events_are_consumed() {
        let mut d = EvDecoder::new();
        assert_eq!(
            d.feed(Event {
                ty: EV_REL,
                code: 0,
                value: 1
            }),
            None
        );
        assert_eq!(
            d.feed(Event {
                ty: EV_ABS,
                code: 0,
                value: 10
            }),
            None
        );
        assert_eq!(
            d.feed(Event {
                ty: EV_SYN,
                code: 0,
                value: 0
            }),
            None
        );
    }
}

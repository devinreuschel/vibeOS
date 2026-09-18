//! Console multiplexer. ROADMAP §5.3.
//!
//! Trait + fan-out/merge helpers. Kernel backends must not call `log!`.
//! Input merge is lock-free vs the keyboard ISR: the PS/2 ring is
//! drained with IRQs off (DESIGN §9.4); these helpers do not take a lock.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendId {
    Serial,
    Framebuffer,
}

impl BackendId {
    pub const fn as_str(self) -> &'static str {
        match self {
            BackendId::Serial => "serial",
            BackendId::Framebuffer => "fb",
        }
    }
}

pub trait ConsoleBackend {
    fn write(&mut self, bytes: &[u8]);
    fn read(&mut self) -> Option<u8> {
        None
    }
    fn set_enabled(&mut self, on: bool);
    fn enabled(&self) -> bool;
}

/// Fan writes to every enabled backend.
pub fn fanout(backends: &mut [&mut dyn ConsoleBackend], bytes: &[u8]) {
    for b in backends.iter_mut() {
        if b.enabled() {
            b.write(bytes);
        }
    }
}

/// First enabled backend that has a byte.
pub fn merge_read(backends: &mut [&mut dyn ConsoleBackend]) -> Option<u8> {
    for b in backends.iter_mut() {
        if b.enabled() {
            if let Some(c) = b.read() {
                return Some(c);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct Fake {
        on: bool,
        out: Vec<u8>,
        inp: VecDeque<u8>,
    }

    impl Fake {
        fn new(_name: BackendId) -> Self {
            Self {
                on: true,
                out: Vec::new(),
                inp: VecDeque::new(),
            }
        }
    }

    impl ConsoleBackend for Fake {
        fn write(&mut self, bytes: &[u8]) {
            self.out.extend_from_slice(bytes);
        }
        fn read(&mut self) -> Option<u8> {
            self.inp.pop_front()
        }
        fn set_enabled(&mut self, on: bool) {
            self.on = on;
        }
        fn enabled(&self) -> bool {
            self.on
        }
    }

    #[test]
    fn fanout_hits_enabled_only() {
        let mut a = Fake::new(BackendId::Serial);
        let mut b = Fake::new(BackendId::Framebuffer);
        b.set_enabled(false);
        {
            let mut refs: [&mut dyn ConsoleBackend; 2] = [&mut a, &mut b];
            fanout(&mut refs, b"hi");
        }
        assert_eq!(a.out, b"hi");
        assert_eq!(b.out, b"");
        b.set_enabled(true);
        {
            let mut refs: [&mut dyn ConsoleBackend; 2] = [&mut a, &mut b];
            fanout(&mut refs, b"!");
        }
        assert_eq!(a.out, b"hi!");
        assert_eq!(b.out, b"!");
        assert_eq!(BackendId::Serial.as_str(), "serial");
        assert_eq!(BackendId::Framebuffer.as_str(), "fb");
    }

    #[test]
    fn merge_prefers_first_enabled_with_data() {
        let mut a = Fake::new(BackendId::Serial);
        let mut b = Fake::new(BackendId::Framebuffer);
        b.inp.push_back(b'k');
        a.inp.push_back(b's');
        let mut refs: [&mut dyn ConsoleBackend; 2] = [&mut a, &mut b];
        assert_eq!(merge_read(&mut refs), Some(b's'));
        assert_eq!(merge_read(&mut refs), Some(b'k'));
        assert_eq!(merge_read(&mut refs), None);
    }

    #[test]
    fn disabled_backend_is_skipped_on_read() {
        let mut a = Fake::new(BackendId::Serial);
        a.inp.push_back(b'x');
        a.set_enabled(false);
        let mut b = Fake::new(BackendId::Framebuffer);
        b.inp.push_back(b'y');
        let mut refs: [&mut dyn ConsoleBackend; 2] = [&mut a, &mut b];
        assert_eq!(merge_read(&mut refs), Some(b'y'));
    }
}

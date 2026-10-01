//! `shell_tokenize` and `kbd_decode`: the console's input parsers.

use vibeos::kbd::Decoder;
use vibeos::shell::{self, Feed, LineEditor, MAX_TOKENS};

/// `shell::tokenize` into `data[0] % (MAX_TOKENS + 1)` slots, over the rest
/// of the input as lossy UTF-8.
pub fn tokenize(data: &[u8]) {
    let Some((&n, rest)) = data.split_first() else {
        return;
    };
    let line = String::from_utf8_lossy(rest);
    let mut out = [""; MAX_TOKENS];
    let slots = usize::from(n) % (MAX_TOKENS + 1);
    let _ = shell::tokenize(&line, &mut out[..slots]);
}

/// Each byte through `kbd::Decoder::feed` as a set-1 scancode, each key
/// through `shell::LineEditor::feed`; a submitted line is tokenized and
/// the editor cleared.
pub fn kbd(data: &[u8]) {
    let mut dec = Decoder::new();
    let mut ed = Box::new(LineEditor::new());
    for &sc in data {
        let Some(key) = dec.feed(sc) else {
            continue;
        };
        if ed.feed(key) == Feed::Submit {
            if let Ok(line) = ed.line_str() {
                let mut out = [""; MAX_TOKENS];
                let _ = shell::tokenize(line, &mut out);
            }
            ed.clear();
        }
    }
}

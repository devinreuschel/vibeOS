//! Tab completion for the kernel shell: a command name at the start of
//! the line, otherwise a path in the working directory (ROADMAP §5.4).

use vibeos::fs::MAX_NAME;
use vibeos::shell::LineEditor;

use crate::file_init::list_dir;

fn names_in(
    dirp: &[u8],
    prefix: &[u8],
    out: &mut [[u8; MAX_NAME]; 16],
    lens: &mut [u8; 16],
) -> usize {
    let path: &[u8] = if dirp.is_empty() { b"." } else { dirp };
    let mut n = 0usize;
    #[expect(
        clippy::let_underscore_must_use,
        reason = "completion in a directory that cannot be listed offers no names, as a shell's does: no failure anyone could act on (DESIGN §2.5)"
    )]
    let _ = list_dir(path, &mut |d| {
        let nm = d.name.as_bytes();
        if nm.len() >= prefix.len() && nm[..prefix.len()].eq_ignore_ascii_case(prefix) && n < 16 {
            let l = nm.len().min(MAX_NAME);
            out[n][..l].copy_from_slice(&nm[..l]);
            lens[n] = l as u8;
            n += 1;
        }
    });
    n
}

/// Tab: complete the word at the cursor against the current directory
/// (or the directory prefix of that word).
pub fn complete_line(ed: &mut LineEditor, command_at: fn(usize) -> Option<&'static str>) {
    let mut line_buf = [0u8; 128];
    let line_n = ed.line().len().min(128);
    line_buf[..line_n].copy_from_slice(&ed.line()[..line_n]);
    let line = &line_buf[..line_n];
    let cur = ed.cursor();
    let mut start = cur;
    while start > 0 && line[start - 1] != b' ' && line[start - 1] != b'\t' {
        start -= 1;
    }
    let word = &line[start..cur];
    let first = {
        let mut i = 0usize;
        while i < start && (line[i] == b' ' || line[i] == b'\t') {
            i += 1;
        }
        i == start
    };
    if first && !word.contains(&b'/') {
        complete_cmd(ed, start, word, command_at);
        return;
    }
    let slash = word.iter().rposition(|&c| c == b'/');
    let (dirp, pref) = match slash {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => (&b""[..], word),
    };
    let mut names = [[0u8; MAX_NAME]; 16];
    let mut lens = [0u8; 16];
    let n = names_in(dirp, pref, &mut names, &mut lens);
    if n == 0 {
        return;
    }
    let common = common_prefix(&names, &lens, n);
    if common <= pref.len() && n > 1 {
        return;
    }
    let fill = &names[0][..common];
    apply_word(ed, start, cur, dirp, fill, n == 1);
}

fn complete_cmd(
    ed: &mut LineEditor,
    start: usize,
    pref: &[u8],
    command_at: fn(usize) -> Option<&'static str>,
) {
    let mut hit: Option<&'static str> = None;
    let mut n = 0u32;
    let mut i = 0usize;
    while let Some(nm) = command_at(i) {
        if nm.as_bytes().starts_with(pref) {
            n += 1;
            hit = Some(nm);
            if n > 1 {
                break;
            }
        }
        i += 1;
    }
    if n == 1
        && let Some(h) = hit
    {
        apply_word(ed, start, ed.cursor(), b"", h.as_bytes(), true);
    }
}

fn common_prefix(names: &[[u8; MAX_NAME]; 16], lens: &[u8; 16], n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let mut c = lens[0] as usize;
    let mut i = 1usize;
    while i < n {
        let mut k = 0usize;
        let a = &names[0][..c.min(lens[0] as usize)];
        let b = &names[i][..lens[i] as usize];
        while k < a.len() && k < b.len() && to_up(a[k]) == to_up(b[k]) {
            k += 1;
        }
        if k < c {
            c = k;
        }
        i += 1;
    }
    c
}

fn to_up(c: u8) -> u8 {
    if c.is_ascii_lowercase() {
        c - b'a' + b'A'
    } else {
        c
    }
}

fn apply_word(
    ed: &mut LineEditor,
    start: usize,
    cur: usize,
    dirp: &[u8],
    name: &[u8],
    unique: bool,
) {
    let mut tmp = [0u8; 128];
    let mut n = 0usize;
    n += copy_to(&mut tmp[n..], dirp);
    n += copy_to(&mut tmp[n..], name);
    if unique {
        n += copy_to(&mut tmp[n..], b" ");
    }
    let line = ed.line();
    let rest = if cur < line.len() { &line[cur..] } else { &[] };
    let mut neu = [0u8; 128];
    let mut m = 0usize;
    m += copy_to(&mut neu[m..], &line[..start]);
    m += copy_to(&mut neu[m..], &tmp[..n]);
    m += copy_to(&mut neu[m..], rest);
    ed.set_line(&neu[..m]);
}

fn copy_to(dst: &mut [u8], src: &[u8]) -> usize {
    let n = src.len().min(dst.len());
    dst[..n].copy_from_slice(&src[..n]);
    n
}

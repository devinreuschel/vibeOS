//! The portable half of the in-guest test runner (DESIGN §8.2, ROADMAP
//! §10.2): which rows `vibeos.ktest=` and `vibeos.ktest_repeat=` select,
//! how many runs that makes, and the deadline and timing arithmetic. The
//! kernel half, `src/ktest/mod.rs`, runs the rows.
//!
//! `vibeos.ktest=` is a comma-separated list of globs: `*` matches any run
//! of characters and `?` one character. With no item, every row that is
//! not opt-in is selected. An opt-in row runs only when an item without a
//! wildcard equals its name. `vibeos.ktest_repeat=` runs the selection 1
//! to [`REPEAT_MAX`] times, pass by pass, and a `once` row runs in the
//! first pass only. `vibeos.ktest_range=` limits the rows to one stretch of
//! the registry ([`Range`]).

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

/// A row's deadline when the registry sets none.
pub const DEFAULT_DEADLINE_MS: u32 = 10_000;
/// The largest `vibeos.ktest_repeat=`.
pub const REPEAT_MAX: u32 = 1_000;

/// Whether glob `pat` matches all of `name`: `*` any run of bytes, `?` one
/// byte, anything else itself. Iterative: on a mismatch it backtracks to
/// the last `*`, so it takes no stack and no allocation.
pub fn glob_match(pat: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0usize, 0usize);
    // The pattern index after the last `*`, and the name index it resumes at.
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match (pat.get(p), name.get(n)) {
            (Some(b'*'), _) => {
                p = p.saturating_add(1);
                star = Some((p, n));
            }
            (Some(&c), Some(&b)) if c == b'?' || c == b => {
                p = p.saturating_add(1);
                n = n.saturating_add(1);
            }
            _ => match star {
                Some((sp, sn)) => {
                    let sn = sn.saturating_add(1);
                    star = Some((sp, sn));
                    p = sp;
                    n = sn;
                }
                None => return false,
            },
        }
    }
    while pat.get(p) == Some(&b'*') {
        p = p.saturating_add(1);
    }
    p == pat.len()
}

fn has_wildcard(item: &[u8]) -> bool {
    item.iter().any(|&b| b == b'*' || b == b'?')
}

/// The rows `vibeos.ktest=` selects.
#[derive(Clone, Copy, Debug)]
pub struct Selection<'a> {
    list: &'a [u8],
}

impl<'a> Selection<'a> {
    /// The option's value, or `None` when the option is absent.
    pub fn parse(value: Option<&'a [u8]>) -> Self {
        Selection {
            list: value.unwrap_or(&[]),
        }
    }

    /// Every row that is not opt-in.
    pub fn all() -> Selection<'static> {
        Selection { list: &[] }
    }

    /// The non-empty items of the list.
    pub fn items(&self) -> impl Iterator<Item = &'a [u8]> + 'a {
        self.list.split(|&b| b == b',').filter(|i| !i.is_empty())
    }

    /// True when the list has no item, which selects every row that is not
    /// opt-in.
    pub fn is_all(&self) -> bool {
        self.items().next().is_none()
    }

    /// Whether the row `name` runs. An opt-in row needs an item that is
    /// its name, with no wildcard.
    pub fn selects(&self, name: &str, opt_in: bool) -> bool {
        let name = name.as_bytes();
        if self.is_all() {
            return !opt_in;
        }
        self.items().any(|item| {
            if opt_in {
                !has_wildcard(item) && item == name
            } else {
                glob_match(item, name)
            }
        })
    }
}

/// A `vibeos.ktest_repeat=` value that is not a decimal from 1 to
/// [`REPEAT_MAX`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadOption;

/// `vibeos.ktest_repeat=`'s value: 1 when the option is absent.
pub fn parse_repeat(value: Option<&[u8]>) -> Result<u32, BadOption> {
    let Some(v) = value else {
        return Ok(1);
    };
    if v.is_empty() {
        return Err(BadOption);
    }
    let mut n: u32 = 0;
    for &b in v {
        if !b.is_ascii_digit() {
            return Err(BadOption);
        }
        n = n
            .checked_mul(10)
            .and_then(|n| n.checked_add(u32::from(b.wrapping_sub(b'0'))))
            .ok_or(BadOption)?;
    }
    if (1..=REPEAT_MAX).contains(&n) {
        Ok(n)
    } else {
        Err(BadOption)
    }
}

/// `vibeos.ktest_range=<from>..<to>`: the rows from the one named `from` up
/// to, not including, the one named `to`, in run order. An empty `from`
/// starts at the first row and an empty `to` runs past the last, so ranges
/// that share their bounds, `..a`, `a..b`, `b..`, split the registry with
/// every row in exactly one (DESIGN §8.6's per-push shards). A row runs
/// when it is in the range and [`Selection`] selects it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range<'a> {
    from: &'a [u8],
    to: &'a [u8],
}

/// A registry row's name: `[a-z0-9_]+`, which `ktest_names_unique` holds
/// every row to.
fn is_row_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name
            .iter()
            .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl<'a> Range<'a> {
    /// Every row: the option absent.
    pub fn all() -> Range<'static> {
        Range { from: &[], to: &[] }
    }

    /// The option's value, or `None` when it is absent. `BadOption` unless
    /// the value is `<from>..<to>`, each side empty or a row name.
    pub fn parse(value: Option<&'a [u8]>) -> Result<Self, BadOption> {
        let Some(v) = value else {
            return Ok(Range::all());
        };
        let at = v.windows(2).position(|w| w == b"..").ok_or(BadOption)?;
        let from = v.get(..at).ok_or(BadOption)?;
        let to = v
            .get(at.checked_add(2).ok_or(BadOption)?..)
            .ok_or(BadOption)?;
        for side in [from, to] {
            if !side.is_empty() && !is_row_name(side) {
                return Err(BadOption);
            }
        }
        Ok(Range { from, to })
    }

    /// The indexes `[start, end)` of the range's rows among `names`, the
    /// registry's row names in run order. `BadOption` when a bound names
    /// no row, or when `to` does not come after `from`, which would select
    /// nothing.
    pub fn bounds<'n>(
        &self,
        names: impl IntoIterator<Item = &'n str>,
    ) -> Result<(usize, usize), BadOption> {
        let mut start = if self.from.is_empty() { Some(0) } else { None };
        let mut end = None;
        let mut count = 0usize;
        for (i, name) in names.into_iter().enumerate() {
            if start.is_none() && name.as_bytes() == self.from {
                start = Some(i);
            }
            if end.is_none() && !self.to.is_empty() && name.as_bytes() == self.to {
                end = Some(i);
            }
            count = i.checked_add(1).ok_or(BadOption)?;
        }
        let start = start.ok_or(BadOption)?;
        let end = if self.to.is_empty() {
            count
        } else {
            end.ok_or(BadOption)?
        };
        if end <= start {
            return Err(BadOption);
        }
        Ok((start, end))
    }
}

/// Whether a selected row runs in `pass` (1-based): a `once` row runs in
/// pass 1 only. The runner's loop and [`run_count`] both ask this.
pub fn runs_in_pass(once: bool, pass: u32) -> bool {
    pass == 1 || !once
}

/// How many times a selected row runs in `repeat` passes.
pub fn runs_of(once: bool, repeat: u32) -> u32 {
    if once { repeat.min(1) } else { repeat }
}

/// The runs a boot makes: every selected row's [`runs_of`], summed. `rows`
/// yields `(name, once, opt_in)`. `None` when the sum overflows `u32`.
pub fn run_count<'r>(
    rows: impl IntoIterator<Item = (&'r str, bool, bool)>,
    sel: &Selection<'_>,
    repeat: u32,
) -> Option<u32> {
    let mut n: u32 = 0;
    for (name, once, opt_in) in rows {
        if sel.selects(name, opt_in) {
            n = n.checked_add(runs_of(once, repeat))?;
        }
    }
    Some(n)
}

/// `ms` milliseconds in cycles of a `freq_hz` counter, saturating at
/// `u64::MAX`.
pub fn deadline_cycles(ms: u32, freq_hz: u64) -> u64 {
    let c = u128::from(ms)
        .checked_mul(u128::from(freq_hz))
        .and_then(|c| c.checked_div(1000))
        .unwrap_or(u128::MAX);
    u64::try_from(c).unwrap_or(u64::MAX)
}

/// `cycles` of a `freq_hz` counter in microseconds, saturating at
/// `u64::MAX`; 0 when the frequency is 0.
pub fn cycles_to_us(cycles: u64, freq_hz: u64) -> u64 {
    let us = u128::from(cycles)
        .checked_mul(1_000_000)
        .and_then(|c| c.checked_div(u128::from(freq_hz)))
        .unwrap_or(0);
    u64::try_from(us).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    fn sel(s: &str) -> Selection<'_> {
        Selection::parse(Some(s.as_bytes()))
    }

    #[test]
    fn glob_exact() {
        assert!(glob_match(b"map_unmap", b"map_unmap"));
        assert!(!glob_match(b"map_unmap", b"map_unma"));
        assert!(!glob_match(b"map_unma", b"map_unmap"));
        assert!(glob_match(b"", b""));
        assert!(!glob_match(b"", b"x"));
    }

    #[test]
    fn glob_prefix_suffix_mid() {
        assert!(glob_match(b"lifetime_*", b"lifetime_stack_reclaim"));
        assert!(glob_match(b"lifetime_*", b"lifetime_"));
        assert!(!glob_match(b"lifetime_*", b"xlifetime_a"));
        assert!(glob_match(b"*_asserts", b"rank_same_rank_lock_asserts"));
        assert!(!glob_match(b"*_asserts", b"asserts_x"));
        assert!(glob_match(b"*vblk*", b"block_vblk_rw"));
        assert!(glob_match(b"*vblk*", b"vblk"));
        assert!(!glob_match(b"*vblk*", b"block_blk"));
        // Backtracking: the first `a` after `*` is not the right one.
        assert!(glob_match(b"*ab", b"aab"));
        assert!(glob_match(b"a*b*c", b"axxbyybzc"));
        assert!(!glob_match(b"a*b*c", b"axxbyyb"));
    }

    #[test]
    fn glob_question_and_double_star() {
        assert!(glob_match(b"fb_???ch", b"fb_pitch"));
        assert!(!glob_match(b"fb_???ch", b"fb_pich"));
        assert!(glob_match(b"?", b"x"));
        assert!(!glob_match(b"?", b""));
        assert!(glob_match(b"**", b""));
        assert!(glob_match(b"**", b"anything"));
        assert!(glob_match(b"a**b", b"ab"));
        assert!(glob_match(b"*", b""));
    }

    #[test]
    fn glob_pattern_longer_than_name() {
        assert!(!glob_match(b"heap_growth_x", b"heap_growth"));
        assert!(!glob_match(b"heap_??????x", b"heap_growth"));
        assert!(glob_match(b"heap_growth***", b"heap_growth"));
    }

    #[test]
    fn select_absent_or_empty_is_all() {
        for s in [
            Selection::parse(None),
            Selection::all(),
            sel(""),
            sel(","),
            sel(",,"),
        ] {
            assert!(s.is_all());
            assert!(s.selects("map_unmap", false));
            assert!(!s.selects("ktest_optin_probe", true));
        }
    }

    #[test]
    fn select_list_with_empty_items() {
        let s = sel(",map_unmap,,heap_*,");
        assert!(!s.is_all());
        assert_eq!(s.items().count(), 2);
        assert!(s.selects("map_unmap", false));
        assert!(s.selects("heap_box", false));
        assert!(!s.selects("vmap", false));
        assert!(!s.selects("", false));
    }

    #[test]
    fn opt_in_named_literally() {
        let s = sel("ktest_*,ktest_optin_probe");
        assert!(s.selects("ktest_optin_probe", true));
        assert!(s.selects("ktest_names_unique", false));
        assert!(!s.selects("ktest_deadline_hang", true));
    }

    #[test]
    fn opt_in_not_matched_by_wildcard() {
        for pat in [
            "*",
            "ktest_*",
            "ktest_deadline_han?",
            "*hang",
            "ktest_deadline_hang*",
        ] {
            assert!(!sel(pat).selects("ktest_deadline_hang", true), "{pat}");
        }
        assert!(sel("ktest_deadline_hang").selects("ktest_deadline_hang", true));
    }

    #[test]
    fn repeat_absent_is_one() {
        assert_eq!(parse_repeat(None), Ok(1));
    }

    #[test]
    fn repeat_bounds() {
        assert_eq!(parse_repeat(Some(b"1")), Ok(1));
        assert_eq!(parse_repeat(Some(b"20")), Ok(20));
        assert_eq!(parse_repeat(Some(b"1000")), Ok(1000));
        assert_eq!(parse_repeat(Some(b"0")), Err(BadOption));
        assert_eq!(parse_repeat(Some(b"1001")), Err(BadOption));
    }

    #[test]
    fn repeat_not_a_number() {
        for v in [
            &b"x"[..],
            b"",
            b"-1",
            b"+1",
            b"1 ",
            b"1e3",
            b"18446744073709551616",
        ] {
            assert_eq!(parse_repeat(Some(v)), Err(BadOption), "{v:?}");
        }
    }

    const NAMES: [&str; 6] = ["a", "b", "c", "d", "e", "f"];

    fn range(v: &str) -> Result<(usize, usize), BadOption> {
        Range::parse(Some(v.as_bytes()))?.bounds(NAMES)
    }

    #[test]
    fn range_absent_is_every_row() {
        assert_eq!(Range::parse(None), Ok(Range::all()));
        assert_eq!(Range::all().bounds(NAMES), Ok((0, 6)));
        assert_eq!(range(".."), Ok((0, 6)));
    }

    #[test]
    fn range_bounds() {
        assert_eq!(range("..c"), Ok((0, 2)));
        assert_eq!(range("c.."), Ok((2, 6)));
        assert_eq!(range("b..e"), Ok((1, 4)));
        assert_eq!(range("a..b"), Ok((0, 1)));
        assert_eq!(range("..f"), Ok((0, 5)));
        assert_eq!(range("f.."), Ok((5, 6)));
    }

    #[test]
    fn range_bad() {
        for v in [
            "", "c", "c.", ".c", "x..", "..x", "c..c", "d..b", "..a", "C..", "c ..", "c...d",
            "c..d..e", "c,d..",
        ] {
            assert_eq!(range(v), Err(BadOption), "{v:?}");
        }
        assert_eq!(Range::all().bounds([]), Err(BadOption));
    }

    /// Shards that share their bounds (`..b`, `b..e`, `e..`) put every row
    /// in exactly one, whatever bounds they pick (DESIGN §8.6).
    #[test]
    fn ranges_sharing_bounds_partition_the_rows() {
        // Every ordered choice of up to three inner bounds from NAMES[1..].
        let inner = &NAMES[1..];
        for mask in 0u32..(1 << inner.len()) {
            let cuts: std::vec::Vec<&str> = inner
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, n)| *n)
                .collect();
            let mut seen = [0u32; NAMES.len()];
            let mut lo = "";
            for hi in cuts.iter().copied().chain([""]) {
                let (s, e) = range(&std::format!("{lo}..{hi}")).unwrap();
                for n in &mut seen[s..e] {
                    *n += 1;
                }
                lo = hi;
            }
            assert_eq!(seen, [1; NAMES.len()], "cuts {cuts:?}");
        }
    }

    #[test]
    fn run_count_once_and_repeat() {
        let rows = [("a", false, false), ("b", true, false), ("c", false, true)];
        assert_eq!(run_count(rows, &Selection::all(), 1), Some(2));
        assert_eq!(run_count(rows, &Selection::all(), 3), Some(4));
        assert_eq!(run_count(rows, &sel("c"), 3), Some(3));
        assert_eq!(run_count(rows, &sel("b,c"), 5), Some(6));
        assert_eq!(run_count(rows, &sel("zz"), 5), Some(0));
        assert_eq!(runs_of(true, 1000), 1);
        assert_eq!(runs_of(false, 1000), 1000);
        let passes = |once| (1..=7).filter(|&p| runs_in_pass(once, p)).count() as u32;
        assert_eq!(passes(true), runs_of(true, 7));
        assert_eq!(passes(false), runs_of(false, 7));
    }

    #[test]
    #[cfg_attr(miri, ignore = "4.3 million rows to overflow: hours under Miri")]
    fn run_count_overflow() {
        let rows = core::iter::repeat_n(("x", false, false), 4_294_968);
        assert_eq!(run_count(rows, &Selection::all(), REPEAT_MAX), None);
        let rows = core::iter::repeat_n(("x", false, false), 4_294_967);
        assert_eq!(
            run_count(rows, &Selection::all(), REPEAT_MAX),
            Some(4_294_967_000)
        );
    }

    #[test]
    fn deadline_cycles_converts_and_saturates() {
        assert_eq!(
            deadline_cycles(DEFAULT_DEADLINE_MS, 2_000_000_000),
            20_000_000_000
        );
        assert_eq!(deadline_cycles(500, 3_000_000_000), 1_500_000_000);
        assert_eq!(deadline_cycles(0, 3_000_000_000), 0);
        assert_eq!(deadline_cycles(u32::MAX, u64::MAX), u64::MAX);
        assert_eq!(deadline_cycles(1, 999), 0);
    }

    #[test]
    fn deadline_cycles_to_us() {
        assert_eq!(cycles_to_us(3_000_000_000, 3_000_000_000), 1_000_000);
        assert_eq!(cycles_to_us(1_500, 3_000_000_000), 0);
        assert_eq!(cycles_to_us(3_000, 3_000_000_000), 1);
        assert_eq!(cycles_to_us(5, 0), 0);
        assert_eq!(cycles_to_us(u64::MAX, 1), u64::MAX);
    }
}

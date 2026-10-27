// Heddle — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! Line-level three-way merge, so two threads that edited DIFFERENT parts
//! of one file (two symbol leases in `src/auth.rs`) don't have to be
//! reconciled by hand.
//!
//! Deliberately conservative: hunks that overlap OR touch are a conflict
//! (git's rule too), identical hunks merge, and anything the diff can't
//! do cheaply (enormous rewrites) is a conflict rather than a slow guess.
//! A clean merge is never trusted on its own — the merged file goes
//! through the thread's next verify like any other edit before it can land.
//!
//! Myers' O((N+M)·D) diff over line hashes, hand-rolled (no dependency).

/// A change against the base: base lines `[start, end)` become `lines`.
#[derive(Debug, Clone, PartialEq)]
pub struct Hunk {
    pub start: usize,
    pub end: usize,
    pub lines: Vec<String>,
}

/// Past this many differing lines a diff is "a rewrite", not an edit —
/// call it a conflict instead of spending quadratic memory on it.
const MAX_D: usize = 4000;

fn split(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

fn h(s: &str) -> u64 {
    let mut x: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        x ^= b as u64;
        x = x.wrapping_mul(0x0100_0000_01b3);
    }
    x
}

/// Hunks turning `a` into `b` (both as lines), or None past [`MAX_D`].
pub fn diff(a: &[&str], b: &[&str]) -> Option<Vec<Hunk>> {
    let (ha, hb): (Vec<u64>, Vec<u64>) = (a.iter().map(|l| h(l)).collect(), b.iter().map(|l| h(l)).collect());
    // Common prefix/suffix first: most real edits are small and local.
    let mut pre = 0;
    while pre < ha.len() && pre < hb.len() && ha[pre] == hb[pre] && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < ha.len() - pre && suf < hb.len() - pre && ha[ha.len() - 1 - suf] == hb[hb.len() - 1 - suf] {
        suf += 1;
    }
    let (xa, xb) = (&ha[pre..ha.len() - suf], &hb[pre..hb.len() - suf]);
    let (n, m) = (xa.len() as isize, xb.len() as isize);
    if n == 0 && m == 0 {
        return Some(vec![]);
    }
    let max = (n + m) as usize;
    if max == 0 {
        return Some(vec![]);
    }
    let off = max as isize;
    let mut v = vec![0isize; 2 * max + 2];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut found = false;
    for d in 0..=max.min(MAX_D) as isize {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let idx = (k + off) as usize;
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) { v[idx + 1] } else { v[idx - 1] + 1 };
            let mut y = x - k;
            while x < n && y < m && xa[x as usize] == xb[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x >= n && y >= m {
                found = true;
                break;
            }
            k += 2;
        }
        if found {
            break;
        }
    }
    if !found {
        return None;
    }
    // Backtrack into a list of (x, y) matches → edit script.
    let mut x = n;
    let mut y = m;
    let mut ops: Vec<(bool, isize, isize)> = Vec::new(); // (is_match, x, y) per step, reversed
    for d in (0..trace.len() as isize).rev() {
        let vd = &trace[d as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && vd[(k - 1 + off) as usize] < vd[(k + 1 + off) as usize]) { k + 1 } else { k - 1 };
        let prev_x = vd[(prev_k + off) as usize];
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push((true, x - 1, y - 1));
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == prev_x {
                ops.push((false, -1, y - 1)); // insertion of b[y-1]
            } else {
                ops.push((false, x - 1, -1)); // deletion of a[x-1]
            }
        }
        x = prev_x;
        y = prev_y;
    }
    ops.reverse();
    // Group consecutive non-matches into hunks (indices into the full a/b).
    let mut hunks = Vec::new();
    let (mut ai, mut bi) = (0usize, 0usize); // positions in xa/xb
    let mut cur: Option<Hunk> = None;
    for (is_match, ox, oy) in ops {
        if is_match {
            if let Some(hk) = cur.take() {
                hunks.push(hk);
            }
            ai = ox as usize + 1;
            bi = oy as usize + 1;
            continue;
        }
        let hk = cur.get_or_insert(Hunk { start: pre + ai, end: pre + ai, lines: vec![] });
        if ox >= 0 {
            hk.end = pre + ox as usize + 1;
            ai = ox as usize + 1;
        } else {
            hk.lines.push(b[pre + oy as usize].to_string());
            bi = oy as usize + 1;
        }
    }
    let _ = bi;
    if let Some(hk) = cur {
        hunks.push(hk);
    }
    Some(hunks)
}

/// Apply `hunks` (sorted, non-overlapping) to `base`.
fn apply(base: &[&str], hunks: &[&Hunk]) -> String {
    let mut out = String::new();
    let mut at = 0;
    for hk in hunks {
        for l in &base[at..hk.start] {
            out.push_str(l);
        }
        for l in &hk.lines {
            out.push_str(l);
        }
        at = hk.end;
    }
    for l in &base[at..] {
        out.push_str(l);
    }
    out
}

/// Outcome of a three-way merge.
#[derive(Debug, PartialEq)]
pub enum Merged {
    Clean(String),
    /// Base line ranges (1-based, inclusive) both sides changed.
    Conflict(Vec<(usize, usize)>),
}

/// Merge `ours` and `theirs`, both derived from `base`.
pub fn merge3(base: &str, ours: &str, theirs: &str) -> Merged {
    if ours == theirs {
        return Merged::Clean(ours.to_string());
    }
    if ours == base {
        return Merged::Clean(theirs.to_string());
    }
    if theirs == base {
        return Merged::Clean(ours.to_string());
    }
    let (b, o, t) = (split(base), split(ours), split(theirs));
    let (Some(ho), Some(ht)) = (diff(&b, &o), diff(&b, &t)) else {
        return Merged::Conflict(vec![(1, b.len().max(1))]);
    };
    let mut all: Vec<&Hunk> = Vec::new();
    let mut conflicts = Vec::new();
    for x in &ho {
        for y in &ht {
            // Overlapping or touching ranges conflict — except a hunk both
            // sides made identically.
            let touch = x.start <= y.end && y.start <= x.end;
            if touch && x != y {
                conflicts.push((x.start.min(y.start) + 1, x.end.max(y.end).max(x.start.min(y.start) + 1)));
            }
        }
    }
    if !conflicts.is_empty() {
        conflicts.sort();
        conflicts.dedup();
        return Merged::Conflict(conflicts);
    }
    all.extend(ho.iter());
    for y in &ht {
        if !ho.contains(y) {
            all.push(y);
        }
    }
    all.sort_by_key(|hk| (hk.start, hk.end));
    Merged::Clean(apply(&b, &all))
}

/// 1-based inclusive line ranges of `new` that differ from `old` (for
/// "you edited outside your leased symbols" warnings).
pub fn changed_ranges(old: &str, new: &str) -> Vec<(usize, usize)> {
    let (a, b) = (split(old), split(new));
    let Some(hs) = diff(&a, &b) else { return vec![(1, b.len().max(1))] };
    // Map each hunk to its position in `new`.
    let mut out = Vec::new();
    let mut shift: isize = 0;
    for hk in hs {
        let start_new = (hk.start as isize + shift) as usize;
        if hk.lines.is_empty() {
            // Pure deletion: mark the line it happened at.
            out.push((start_new.max(1), start_new.max(1)));
        } else {
            out.push((start_new + 1, start_new + hk.lines.len()));
        }
        shift += hk.lines.len() as isize - (hk.end - hk.start) as isize;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "fn a() {\n    1\n}\n\nfn b() {\n    2\n}\n\nfn c() {\n    3\n}\n";

    #[test]
    fn diff_round_trips() {
        let cases = [
            (BASE, BASE.replace("    2\n", "    20\n    21\n")),
            (BASE, BASE.replace("fn a() {\n    1\n}\n\n", "")),
            (BASE, format!("// top\n{BASE}// end\n")),
            ("", "x\ny\n".to_string()),
            ("x\ny\n", String::new()),
            ("a\nb\nc\nd\n", "d\nc\nb\na\n".to_string()),
            ("no newline at end", "no newline at end\nmore".to_string()),
        ];
        for (a, b) in cases {
            let (sa, sb) = (split(a), split(&b));
            let hs = diff(&sa, &sb).unwrap();
            let refs: Vec<&Hunk> = hs.iter().collect();
            assert_eq!(apply(&sa, &refs), b, "{a:?} -> {b:?}: {hs:?}");
        }
    }

    #[test]
    fn edits_in_different_functions_merge() {
        let ours = BASE.replace("    1\n", "    100\n");
        let theirs = BASE.replace("    3\n", "    300\n    301\n");
        match merge3(BASE, &ours, &theirs) {
            Merged::Clean(m) => {
                assert!(m.contains("    100\n") && m.contains("    300\n    301\n") && m.contains("    2\n"), "{m}");
            }
            c => panic!("{c:?}"),
        }
    }

    #[test]
    fn same_or_touching_lines_conflict_identical_ones_merge() {
        let ours = BASE.replace("    2\n", "    20\n");
        let theirs = BASE.replace("    2\n", "    22\n");
        assert!(matches!(merge3(BASE, &ours, &theirs), Merged::Conflict(_)));
        // Adjacent: ours edits `fn b() {`, theirs edits the body line under it.
        let ours = BASE.replace("fn b() {\n", "fn b(x: u8) {\n");
        assert!(matches!(merge3(BASE, &ours, &theirs), Merged::Conflict(_)));
        // Both made the same change.
        let same = BASE.replace("    2\n", "    9\n");
        let also = format!("{same}// tail\n");
        assert_eq!(merge3(BASE, &same, &also), Merged::Clean(also.clone()));
    }

    #[test]
    fn changed_ranges_point_at_new_lines() {
        let new = BASE.replace("    3\n", "    30\n    31\n");
        assert_eq!(changed_ranges(BASE, &new), vec![(10, 11)]);
        let new = format!("// top\n{BASE}");
        assert_eq!(changed_ranges(BASE, &new), vec![(1, 1)]);
    }
}

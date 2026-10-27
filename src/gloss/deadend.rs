//! Dead ends — negative knowledge, pinned to the code it is about.
//!
//! Git keeps what worked and throws away what didn't. An agent that spends
//! an hour learning "rewriting `Session::refresh` around a single lock
//! deadlocks under `evict`" leaves nothing behind when it gives up, so the
//! next agent spends the same hour. [`record`] turns an abandoned thread into
//! `deadend` gloss notes anchored to the symbols the thread actually changed:
//! the goal, why it failed, a digest of the last red verify, and the commit.
//! They surface first when anyone later leases that scope or reads those
//! files, and like every gloss they go STALE when the code under them moves
//! on — "tried against older code" is still worth knowing, and it is labeled.

use std::collections::BTreeMap;

use serde::Serialize;

use super::anchor::{self, Resolution};
use super::{Author, Note, NoteView, Repo};
use crate::{store, AbandonOutcome, Heddle, TOMBSTONE};

/// At most this many anchored sites per dead end: the point is to be seen
/// where the attempt happened, not to carpet the repo.
pub const MAX_SITES: usize = 8;
const MAX_SYMBOLS_PER_FILE: usize = 3;

#[derive(Debug, Serialize)]
pub struct Recorded {
    /// Shared by every note of this dead end.
    pub group: String,
    pub text: String,
    pub notes: Vec<NoteView>,
    /// Sites that could not be anchored (new files, code gone from the
    /// fabric) — named in the text instead.
    pub unanchored: Vec<String>,
}

/// The lines of a verify log that say why it was red: errors, failures,
/// panics, assertions — the last few of them, each capped. Lines that look
/// like secrets are dropped (the note would be refused, and should be).
pub fn digest(log_tail: &str) -> String {
    let interesting = |l: &str| {
        let t = l.to_ascii_lowercase();
        ["error", "failed", "panicked", "assert", "fail:", "exception", "fatal", "timed out"]
            .iter()
            .any(|k| t.contains(k))
    };
    let lines: Vec<&str> = log_tail.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let mut picked: Vec<&str> = lines.iter().copied().filter(|l| interesting(l)).collect();
    if picked.is_empty() {
        picked = lines.iter().rev().take(4).rev().copied().collect();
    }
    let keep = picked.len().saturating_sub(6);
    picked[keep..]
        .iter()
        .filter(|l| super::secrets::scan(l).is_none())
        .map(|l| crate::cap(l, 160))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn normalized(lines: &[&str], s: &anchor::Symbol) -> String {
    lines
        .get(s.start - 1..s.end.min(lines.len()))
        .unwrap_or(&[])
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Symbols of `before` whose bodies differ in `after` (or vanished),
/// innermost only: if `Cache::get` changed, report it, not also `Cache`.
pub fn changed_symbols(path: &str, before: &str, after: &str) -> Vec<String> {
    let (b, a) = (anchor::outline(path, before), anchor::outline(path, after));
    let (bl, al): (Vec<&str>, Vec<&str>) = (before.lines().collect(), after.lines().collect());
    let changed: Vec<&anchor::Symbol> = b
        .iter()
        .filter(|s| match a.iter().find(|x| x.path == s.path) {
            Some(x) => normalized(&bl, s) != normalized(&al, x),
            None => true,
        })
        .collect();
    changed
        .iter()
        .filter(|s| !changed.iter().any(|o| o.path != s.path && s.start <= o.start && o.end <= s.end))
        .map(|s| s.path.clone())
        .collect()
}

/// UTC calendar date for epoch milliseconds (no chrono in this crate).
pub fn date(ms: u64) -> String {
    // Howard Hinnant's civil_from_days.
    let z = (ms / 86_400_000) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Pin an abandoned thread's attempt to the code it touched.
pub fn record(engine: &Heddle, out: &AbandonOutcome, why: &str, by: &str) -> Result<Recorded, String> {
    let repo = Repo::open(&out.repo.path)?;
    let objects = store::objects_dir(&engine.base(), &out.repo.id);
    let blob = |h: &str| -> Option<String> {
        if h == TOMBSTONE {
            return None;
        }
        store::read_blob(&objects, h).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
    };

    // What changed: head vs base (isolated), or head vs HEAD (in-place).
    let mut sites: Vec<String> = Vec::new(); // gloss targets
    let mut described: BTreeMap<String, String> = BTreeMap::new(); // path → what
    let mut unanchored = Vec::new();
    for (path, head_h) in &out.head {
        let base_h = out.base.get(path);
        if base_h == Some(head_h) {
            continue;
        }
        let before = match base_h {
            Some(h) => blob(h),
            None if out.base.is_empty() => repo.head_content(path),
            None => None,
        };
        let after = blob(head_h);
        match (before, after) {
            (None, _) => {
                described.insert(path.clone(), "new file".into());
                unanchored.push(format!("{path} (new file)"));
            }
            (Some(_), None) => {
                described.insert(path.clone(), "deleted".into());
                sites.push(path.clone());
            }
            (Some(b), Some(a)) => {
                let syms = changed_symbols(path, &b, &a);
                if syms.is_empty() {
                    described.insert(path.clone(), "edited".into());
                    sites.push(path.clone());
                } else {
                    let shown: Vec<String> = syms.iter().take(MAX_SYMBOLS_PER_FILE).cloned().collect();
                    described.insert(path.clone(), shown.join(", "));
                    sites.extend(shown.iter().map(|s| format!("{path}::{s}")));
                }
            }
        }
    }
    if out.head.is_empty() {
        // Nothing was ever stitched: fall back to the lease's literal paths
        // that exist, so the dead end is still seen where it was attempted.
        let scope = engine
            .snapshot()
            .repo_states
            .get(&out.repo.id)
            .and_then(|rs| rs.leases.iter().find(|l| Some(&l.id) == out.lease_id.as_ref()).map(|l| l.scope.clone()))
            .unwrap_or_default();
        for p in scope.iter().filter(|p| !p.contains(['*', '?', '['])) {
            if repo.root.join(p).is_file() {
                sites.push(p.clone());
            }
        }
    }
    sites.truncate(MAX_SITES);

    let t = &out.thread;
    let mut text = format!("Tried: {}\nFailed: {}", t.goal, why.trim());
    if let Some(v) = &out.last_red {
        text.push_str(&format!("\nVerify `{}` was red: {}", v.cmd, digest(&v.log_tail)));
    }
    if !described.is_empty() {
        let parts: Vec<String> = described.iter().map(|(p, w)| format!("{p} ({w})")).collect();
        text.push_str(&format!("\nChanged: {}", parts.join("; ")));
    }
    text.push_str(&format!("\nThread {}, abandoned {} UTC.", t.id, date(crate::now_ms())));
    let text: String = text.chars().take(super::MAX_NOTE_CHARS).collect();

    let group = format!("d{}", &store::content_hash(format!("{}{}", t.id, crate::now_ms()).as_bytes())[..10]);
    let author = Author { by: by.to_string(), thread: Some(t.id.clone()), lease: out.lease_id.clone() };
    let mut notes = Vec::new();
    for site in &sites {
        let res = super::add_grouped(&repo, site, &text, "deadend", author.clone(), Some(group.clone())).or_else(|e| {
            // The symbol may be gone from the fabric since: pin the file.
            match site.split_once("::") {
                Some((file, _)) => super::add_grouped(&repo, file, &text, "deadend", author.clone(), Some(group.clone())),
                None => Err(e),
            }
        });
        match res {
            Ok(n) => notes.push(NoteView::of(&n, None)),
            Err(e) => unanchored.push(format!("{site} ({e})")),
        }
    }
    // Same text on several sites of one file collapses to one note per
    // anchor already (dedup in add); report each once.
    notes.dedup_by(|a, b| a.id == b.id);
    Ok(Recorded { group, text, notes, unanchored })
}

/// Live dead ends whose path falls under any of `scope`'s globs — one entry
/// per dead end (a group pinned to several sites is reported once).
pub fn in_scope(repo: &Repo, scope: &[String]) -> Vec<(Note, Option<Resolution>)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (path, notes) in super::load_all(repo) {
        if !scope.iter().any(|g| crate::lease::glob_match(g, &path)) {
            continue;
        }
        for n in notes.into_iter().filter(|n| n.kind == "deadend" && n.retired.is_none()) {
            let key = n.group.clone().unwrap_or_else(|| n.id.clone());
            if seen.insert(key) {
                let r = repo.content(&path).map(|c| anchor::resolve(&n.anchor, &c));
                out.push((n, r));
            }
        }
    }
    out.sort_by_key(|(n, _)| std::cmp::Reverse(n.created_at));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_keeps_the_reasons_and_drops_secrets() {
        let log = "Compiling x\nrunning 3 tests\ntest a ... ok\ntest b ... FAILED\nthread 'b' panicked at src/x.rs:3: assertion failed: lock held\nTOKEN=ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789 error\nerror: test failed, to rerun pass `--lib`\n";
        let d = digest(log);
        assert!(d.contains("panicked at src/x.rs:3"), "{d}");
        assert!(d.contains("test b ... FAILED"), "{d}");
        assert!(!d.contains("ghp_"), "{d}");
        assert_eq!(digest("all\nquiet\n"), "all | quiet");
    }

    #[test]
    fn changed_symbols_are_innermost() {
        let before = "struct C;\nimpl C {\n    fn a(&self) {\n        1;\n    }\n    fn b(&self) {\n        2;\n    }\n}\n";
        let after = before.replace("2;", "3;");
        assert_eq!(changed_symbols("x.rs", before, &after), vec!["C::b".to_string()]);
    }

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_793_000_000_000), "2026-10-26");
        assert_eq!(date(951_782_400_000), "2000-02-29");
    }
}

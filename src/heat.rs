// Heddle — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! `heddle heat` — where agents burn effort, by file and symbol.
//!
//! Only what heddle actually observed, never an estimate:
//!
//! * **retries** — red verifies, lands refused because the fabric moved,
//!   and abandoned threads (each a counted event);
//! * **rebases** and **stitches** — counted events;
//! * **wall time** — first to last event of each thread. That is elapsed
//!   time, not work time; an agent that idles for an hour counts the hour,
//!   and the column is labeled as such;
//! * **tokens** — only real counts attached with `heddle savings
//!   --record-tokens`. A thread's tokens are NOT split across the regions it
//!   touched (that would be a guess); each region shows the tokens of the
//!   threads that touched it, and says how many of those threads had a
//!   count at all.
//!
//! A region is a file, or a symbol when the thread's base and head
//! snapshots are still in the store (the gloss outliner names the symbols
//! whose bodies changed). Threads whose snapshots were pruned (heddle keeps
//! a bounded history) are counted as "unattributed", never silently dropped.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::{gloss, savings, store, Heddle, ThreadStatus, TOMBSTONE};

#[derive(Debug, Default, Clone, Serialize)]
pub struct Region {
    pub region: String,
    pub threads: usize,
    pub red_verifies: usize,
    pub refused_lands: usize,
    pub abandoned: usize,
    pub rebases: usize,
    pub stitches: usize,
    /// Sum of each touching thread's first-to-last-event span.
    pub wall_ms: u64,
    /// Sum of recorded tokens of touching threads (not split).
    pub tokens: u64,
    pub threads_with_tokens: usize,
    /// Live gloss notes on this region — hot and unannotated is the cue.
    pub notes: usize,
}

impl Region {
    pub fn retries(&self) -> usize {
        self.red_verifies + self.refused_lands + self.abandoned
    }
}

#[derive(Debug, Serialize)]
pub struct Heat {
    pub regions: Vec<Region>,
    pub threads_seen: usize,
    /// Threads with events but no recoverable region: they never stitched a
    /// change, or their snapshots were pruned.
    pub unattributed: usize,
    pub sort: String,
}

#[derive(Default)]
struct ThreadFacts {
    red: usize,
    refused: usize,
    rebases: usize,
    stitches: usize,
    first: u64,
    last: u64,
}

/// Aggregate effort per region for a registered repo.
pub fn compute(engine: &Heddle, repo_id: &str, symbols: bool, sort: &str) -> Result<Heat, String> {
    let snap = engine.snapshot();
    let repo = snap.repos.iter().find(|r| r.id == repo_id).ok_or("repo not registered")?;
    let rs = snap.repo_states.get(repo_id).cloned().unwrap_or_default();
    let base = engine.base();
    let objects = store::objects_dir(&base, repo_id);
    let tokens = savings::load_tokens(&base, repo_id);

    // Counted facts per thread, from the event log.
    let mut facts: BTreeMap<String, ThreadFacts> = BTreeMap::new();
    for e in store::read_events(&base, repo_id, usize::MAX) {
        let Some(t) = e.get("thread").and_then(|v| v.as_str()) else { continue };
        let ts = e.get("ts_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let f = facts.entry(t.to_string()).or_default();
        if f.first == 0 || ts < f.first {
            f.first = ts;
        }
        f.last = f.last.max(ts);
        match e.get("kind").and_then(|v| v.as_str()).unwrap_or("") {
            "weave_red" => f.red += 1,
            "weave_conflict" => f.refused += 1,
            "rebased" => f.rebases += 1,
            "stitch" => f.stitches += 1,
            _ => {}
        }
    }

    let blob = |h: &str| -> Option<String> {
        if h == TOMBSTONE {
            return None;
        }
        store::read_blob(&objects, h).ok().and_then(|b| String::from_utf8(b).ok())
    };
    let g = gloss::Repo::open(&repo.path).ok();
    let mut regions: BTreeMap<String, Region> = BTreeMap::new();
    let mut unattributed = 0;
    for (tid, f) in &facts {
        let thread = rs.threads.iter().find(|t| &t.id == tid);
        let manifest = |id: Option<&String>| {
            id.and_then(|i| rs.stitches.iter().find(|st| &st.id == i)).map(|st| st.files.clone())
        };
        let head = thread.and_then(|t| manifest(t.head_stitch.as_ref()));
        let base_m = thread.and_then(|t| manifest(t.base_stitch.as_ref())).unwrap_or_default();
        let Some(head) = head else {
            unattributed += 1;
            continue;
        };
        let mut touched: BTreeSet<String> = BTreeSet::new();
        for (path, h) in &head {
            if base_m.get(path) == Some(h) || (h == TOMBSTONE && !base_m.contains_key(path)) {
                continue;
            }
            let syms = if symbols {
                match (base_m.get(path).and_then(|b| blob(b)), blob(h)) {
                    (Some(b), Some(a)) => gloss::deadend::changed_symbols(path, &b, &a),
                    _ => vec![],
                }
            } else {
                vec![]
            };
            if syms.is_empty() {
                touched.insert(path.clone());
            } else {
                touched.extend(syms.into_iter().map(|s| format!("{path}::{s}")));
            }
        }
        if touched.is_empty() {
            unattributed += 1;
            continue;
        }
        let abandoned = thread.is_some_and(|t| t.status == ThreadStatus::Abandoned);
        let tok = tokens.get(tid).copied();
        for r in touched {
            let e = regions.entry(r.clone()).or_insert_with(|| Region { region: r, ..Default::default() });
            e.threads += 1;
            e.red_verifies += f.red;
            e.refused_lands += f.refused;
            e.rebases += f.rebases;
            e.stitches += f.stitches;
            e.abandoned += abandoned as usize;
            e.wall_ms += f.last.saturating_sub(f.first);
            if let Some(t) = tok {
                e.tokens += t;
                e.threads_with_tokens += 1;
            }
        }
    }

    // Annotation coverage: live notes per file / symbol.
    if let Some(g) = &g {
        let all = gloss::load_all(g);
        for r in regions.values_mut() {
            let (file, sym) = crate::lease::split_symbol(&r.region);
            r.notes = all
                .get(file)
                .map(|ns| {
                    ns.iter()
                        .filter(|n| n.retired.is_none())
                        .filter(|n| sym.is_none_or(|s| n.anchor.symbol.as_deref().is_some_and(|ns| crate::lease::symbols_overlap(ns, s))))
                        .count()
                })
                .unwrap_or(0);
        }
    }

    let mut list: Vec<Region> = regions.into_values().collect();
    match sort {
        "time" => list.sort_by_key(|r| std::cmp::Reverse(r.wall_ms)),
        "tokens" => list.sort_by_key(|r| std::cmp::Reverse(r.tokens)),
        _ => list.sort_by_key(|r| (std::cmp::Reverse(r.retries()), std::cmp::Reverse(r.rebases), std::cmp::Reverse(r.wall_ms))),
    }
    Ok(Heat { regions: list, threads_seen: facts.len(), unattributed, sort: sort.to_string() })
}

fn dur(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        _ => format!("{:.1}h", s as f64 / 3600.0),
    }
}

pub fn render(h: &Heat, limit: usize) -> String {
    let mut out = String::new();
    if h.regions.is_empty() {
        out.push_str("no attributed agent effort yet — heat comes from threads' stitches, verifies and lands\n");
    } else {
        out.push_str(&format!(
            "{:<52} {:>4} {:>7} {:>5} {:>8} {:>7} {:>7} {:>9} {:>5}\n",
            "region", "thr", "retries", "red", "refused", "rebases", "elapsed", "tokens", "notes"
        ));
        for r in h.regions.iter().take(limit) {
            let tok = if r.threads_with_tokens == 0 {
                "-".to_string()
            } else if r.threads_with_tokens < r.threads {
                format!("{}*", r.tokens)
            } else {
                r.tokens.to_string()
            };
            let flag = if r.retries() > 0 && r.notes == 0 { "  ← hot, no notes" } else { "" };
            out.push_str(&format!(
                "{:<52} {:>4} {:>7} {:>5} {:>8} {:>7} {:>7} {:>9} {:>5}{flag}\n",
                crate::cap(&r.region, 52),
                r.threads,
                r.retries(),
                r.red_verifies,
                r.refused_lands,
                r.rebases,
                dur(r.wall_ms),
                tok,
                r.notes
            ));
        }
        if h.regions.len() > limit {
            out.push_str(&format!("… {} more regions (--limit N)\n", h.regions.len() - limit));
        }
    }
    out.push_str(&format!(
        "\n{} thread(s) seen; {} unattributed (never stitched a change, or its snapshots were pruned). Sorted by {}.\n\
         retries = red verifies + refused lands + abandons (counted). elapsed = first-to-last event \
         per thread, summed (wall clock, includes idle). tokens = recorded counts only, not split \
         across regions; * = some threads had none. A hot region with no notes is where a gloss \
         note or a refactor pays off.\n",
        h.threads_seen,
        h.unattributed,
        match h.sort.as_str() {
            "time" => "elapsed",
            "tokens" => "tokens",
            _ => "retries, then rebases, then elapsed",
        }
    ));
    out
}

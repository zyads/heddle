// Heddle — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! Context packs — the smallest context that lets an agent start a task.
//!
//! An agent starting cold reads whole files to find the forty lines that
//! matter, and still misses the note that says why those lines are odd, the
//! dead end someone already hit, and the other agent leasing the file next
//! door. [`build`] assembles all of that under a token budget:
//!
//! 1. **dead ends** in the files the task lands in, always first;
//! 2. **relevant symbols**: BM25 (plus local re-ranking when Ollama is up)
//!    over the blob-keyed code index, widened from chunks to the enclosing
//!    function or type, each with the gloss notes pinned inside it;
//! 3. **signatures** of what those symbols call (one hop, within the files
//!    already in play), so the agent sees the API without the bodies;
//! 4. **work nearby**: live heddle leases whose scope covers those files;
//! 5. **recent intent**: the last commits that touched them.
//!
//! Tokens are estimated as chars/4 and the estimate is labeled as one. The
//! comparison number is honest and conservative: "naive" is reading every
//! file the pack cites in full, which is less than an agent exploring with
//! grep usually reads.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use serde::Serialize;

use crate::gloss::{self, anchor, Repo};
use crate::Heddle;

/// Default budget when the caller gives none.
pub const DEFAULT_BUDGET: usize = 30_000;
/// A symbol longer than this is shown as the matching chunk, not whole.
const MAX_UNIT_LINES: usize = 200;
const MAX_SIGNATURES: usize = 20;
/// Symbols come from at most this many files, best-ranked first: a pack
/// that wanders into twenty files is a search result, not a context.
const MAX_FILES: usize = 6;
const MAX_UNITS: usize = 14;

pub fn tokens(s: &str) -> usize {
    s.len().div_ceil(4)
}

/// `30k`, `30000`, `1.5k` → tokens.
pub fn parse_budget(s: &str) -> Option<usize> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mul) = match s.strip_suffix('k') {
        Some(n) => (n, 1000.0),
        None => (s.as_str(), 1.0),
    };
    num.parse::<f64>().ok().filter(|v| *v > 0.0).map(|v| (v * mul) as usize)
}

#[derive(Debug, Serialize)]
pub struct Section {
    pub kind: &'static str,
    pub title: String,
    pub body: String,
    pub tokens: usize,
}

#[derive(Debug, Serialize)]
pub struct Pack {
    pub task: String,
    pub budget: usize,
    /// Estimated tokens of the rendered pack (chars/4).
    pub tokens: usize,
    /// Estimated tokens to read the top 3 files in full — what a competent
    /// agent opens first. Conservative: exploring usually reads more.
    pub naive_tokens: usize,
    /// Estimated tokens to read every file the pack cites in full.
    pub cited_files_tokens: usize,
    pub files: Vec<String>,
    pub sections: Vec<Section>,
    /// Relevant but cut by the budget: pointers, so nothing is silently lost.
    pub omitted: Vec<String>,
    pub ranker: String,
    pub elapsed_ms: u64,
}

struct Unit {
    path: String,
    label: String,
    start: usize,
    end: usize,
}

/// Build a context pack for `task` in the repo at `repo`.
pub fn build(engine: Option<&Heddle>, repo: &Repo, data_dir: &Path, task: &str, budget: usize) -> Result<Pack, String> {
    let t0 = std::time::Instant::now();
    let found = gloss::search(repo, data_dir, task, 24)?;

    // Contents of every file a code hit lands in (one cat-file batch).
    let mut order: Vec<String> = Vec::new();
    let mut blob_of: BTreeMap<String, String> = BTreeMap::new();
    for h in &found.code {
        if !blob_of.contains_key(&h.path) {
            blob_of.insert(h.path.clone(), h.blob.clone());
            order.push(h.path.clone());
        }
    }
    let specs: Vec<String> = order.iter().map(|p| blob_of[p].clone()).collect();
    let contents: BTreeMap<String, String> = order
        .iter()
        .zip(gloss::cat_batch(repo, &specs))
        .filter_map(|(p, b)| b.map(|b| (p.clone(), String::from_utf8_lossy(&b).into_owned())))
        .collect();
    let outlines: BTreeMap<String, Vec<anchor::Symbol>> =
        contents.iter().map(|(p, c)| (p.clone(), anchor::outline(p, c))).collect();

    // Chunks → enclosing symbols (innermost that fits), deduped, from the
    // best-ranked files only; then grouped by file so related code reads
    // together.
    let top_files: Vec<&String> = order.iter().take(MAX_FILES).collect();
    let qterms: HashSet<String> = gloss::index::tokenize(task).into_iter().collect();
    let mut units: Vec<Unit> = Vec::new();
    for h in &found.code {
        if !top_files.contains(&&h.path) || units.len() >= MAX_UNITS {
            continue;
        }
        let Some(syms) = outlines.get(&h.path) else { continue };
        let lines: Vec<&str> = contents[&h.path].lines().collect();
        // A 50-line chunk overlaps many symbols; keep the ones that actually
        // contain the query's terms (most distinct terms first, then the
        // tighter span), at most two per chunk.
        let mut scored: Vec<(usize, &anchor::Symbol)> = syms
            .iter()
            .filter(|s| s.start <= h.end_line && s.end >= h.start_line && s.end - s.start < MAX_UNIT_LINES)
            .map(|s| {
                let body = lines.get(s.start - 1..s.end.min(lines.len())).unwrap_or(&[]).join("\n");
                let have: HashSet<String> = gloss::index::tokenize(&body).into_iter().collect();
                (qterms.iter().filter(|t| have.contains(*t)).count(), s)
            })
            .filter(|(n, _)| *n > 0)
            .collect();
        scored.sort_by_key(|(n, s)| (std::cmp::Reverse(*n), s.end - s.start));
        let picks: Vec<(String, usize, usize)> = if scored.is_empty() {
            vec![(format!("{}:{}-{}", h.path, h.start_line, h.end_line), h.start_line, h.end_line)]
        } else {
            scored.iter().map(|(_, sym)| (format!("{}::{}", h.path, sym.path), sym.start, sym.end)).collect()
        };
        let mut taken = 0;
        for (label, s, e) in picks {
            if taken == 2 || units.len() >= MAX_UNITS {
                break;
            }
            if units.iter().any(|u| u.path == h.path && u.start <= e && s <= u.end) {
                continue;
            }
            units.push(Unit { path: h.path.clone(), label, start: s, end: e });
            taken += 1;
        }
    }
    units.sort_by_key(|u| (top_files.iter().position(|f| **f == u.path), u.start));

    // Notes for every file in play (live), resolved against the content we show.
    let mut notes_by_file: BTreeMap<String, Vec<(gloss::Note, anchor::Resolution)>> = BTreeMap::new();
    for p in contents.keys() {
        let rs = gloss::resolved(repo, p, &contents[p]);
        if !rs.is_empty() {
            notes_by_file.insert(p.clone(), rs);
        }
    }

    let mut sections: Vec<Section> = Vec::new();
    let mut omitted: Vec<String> = Vec::new();
    let mut used = 0usize;
    let header = format!("# Context for: {task}\n");
    used += tokens(&header);
    let mut push = |sections: &mut Vec<Section>, omitted: &mut Vec<String>, kind: &'static str, title: String, body: String| -> bool {
        let t = tokens(&body) + tokens(&title) + 4;
        if used + t > budget {
            omitted.push(title);
            return false;
        }
        used += t;
        sections.push(Section { kind, title, body, tokens: t });
        true
    };

    // 1. Dead ends — in the files in play, plus any the note search hit.
    let mut seen_groups: HashSet<String> = HashSet::new();
    let mut dead: Vec<(gloss::Note, Option<anchor::Resolution>)> = Vec::new();
    for rs in notes_by_file.values() {
        for (n, r) in rs.iter().filter(|(n, _)| n.kind == "deadend") {
            if seen_groups.insert(n.group.clone().unwrap_or(n.id.clone())) {
                dead.push((n.clone(), Some(r.clone())));
            }
        }
    }
    for hit in found.notes.iter().filter(|h| h.note.kind == "deadend") {
        if let Ok((_, n)) = gloss::find(repo, &hit.note.id) {
            if seen_groups.insert(n.group.clone().unwrap_or(n.id.clone())) {
                dead.push((n, None));
            }
        }
    }
    for (n, r) in &dead {
        let state = r.as_ref().map_or("", |r| r.status.word());
        let title = format!("DEAD END at {} ({state})", n.anchor.label());
        push(&mut sections, &mut omitted, "dead_end", title, n.text.clone());
    }

    // 2. Relevant symbols, with their notes inline.
    let mut shown_syms: BTreeSet<(String, String)> = BTreeSet::new();
    for u in &units {
        let content = &contents[&u.path];
        let lines: Vec<&str> = content.lines().collect();
        let (pre, suf) = gloss::render::comment_style(&u.path);
        let mut body = String::new();
        if let Some(ns) = notes_by_file.get(&u.path) {
            for (n, r) in ns.iter().filter(|(n, r)| n.kind != "deadend" && (r.start == 0 || (r.start <= u.end && r.end >= u.start))) {
                body.push_str(&format!("{pre}{}{suf}\n", gloss::render::header(n, r)));
                for l in n.text.lines() {
                    body.push_str(&format!("{pre}  {l}{suf}\n"));
                }
            }
        }
        let e = u.end.min(lines.len());
        let s = u.start.clamp(1, e.max(1));
        body.push_str(&lines[s - 1..e].join("\n"));
        let title = format!("{} (L{s}-{e})", u.label);
        if push(&mut sections, &mut omitted, "symbol", title, format!("```\n{body}\n```")) {
            if let Some((_, sym)) = u.label.split_once("::") {
                shown_syms.insert((u.path.clone(), sym.to_string()));
            }
        }
    }

    // 3. One-hop signatures: identifiers called from the shown symbols that
    //    are defined in a file already in play, first line only.
    let mut sigs: Vec<String> = Vec::new();
    let mut called: BTreeSet<String> = BTreeSet::new();
    for sec in sections.iter().filter(|s| s.kind == "symbol") {
        for w in sec.body.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '(')) {
            if let Some(name) = w.strip_suffix('(').filter(|n| n.len() > 2 && !n.contains('(')) {
                called.insert(name.to_string());
            }
        }
    }
    'outer: for (p, syms) in &outlines {
        for s in syms {
            let leaf = s.path.rsplit("::").next().unwrap_or(&s.path);
            if called.contains(leaf) && !shown_syms.contains(&(p.clone(), s.path.clone())) {
                let line = contents[p].lines().nth(s.start - 1).unwrap_or("").trim();
                // Skip doc/attribute lines pulled into the span.
                let line = if line.starts_with("///") || line.starts_with("#[") || line.starts_with('@') {
                    contents[p].lines().skip(s.start - 1).find(|l| {
                        let t = l.trim();
                        !(t.starts_with("///") || t.starts_with("#[") || t.starts_with('@') || t.is_empty())
                    }).unwrap_or("").trim()
                } else {
                    line
                };
                sigs.push(format!("{p}:{} {line}", s.start));
                if sigs.len() >= MAX_SIGNATURES {
                    break 'outer;
                }
            }
        }
    }
    if !sigs.is_empty() {
        push(&mut sections, &mut omitted, "signatures", "Called from the code above (signatures)".into(), sigs.join("\n"));
    }

    // Other notes the search matched (not already shown inline).
    let inline: HashSet<String> = sections
        .iter()
        .filter(|s| s.kind == "symbol")
        .flat_map(|s| s.body.match_indices("[gloss ").map(|(i, _)| s.body[i + 7..].split(' ').next().unwrap_or("").to_string()).collect::<Vec<_>>())
        .collect();
    let extra: Vec<String> = found
        .notes
        .iter()
        .filter(|h| h.note.kind != "deadend" && !inline.contains(&h.note.id))
        .take(8)
        .map(|h| format!("- {} [{}{}] {}", h.note.anchor, h.note.kind, h.note.status.map(|s| format!(", {s}")).unwrap_or_default(), h.note.text.replace('\n', " ")))
        .collect();
    if !extra.is_empty() {
        push(&mut sections, &mut omitted, "notes", "Related notes elsewhere".into(), extra.join("\n"));
    }

    // 4. Live leases whose scope covers a file in play.
    let files: Vec<String> = units.iter().map(|u| u.path.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    if let Some(engine) = engine {
        if let Some(cfg) = engine.repo_containing(&repo.root.to_string_lossy()) {
            let snap = engine.snapshot();
            let now = crate::now_ms();
            let near: Vec<String> = snap
                .repo_states
                .get(&cfg.id)
                .map(|rs| {
                    rs.leases
                        .iter()
                        .filter(|l| !l.expired(now))
                        .filter(|l| files.iter().any(|f| l.scope.iter().any(|g| crate::lease::glob_match(g, f))))
                        .map(|l| format!("- {} is working on \"{}\" (scope {}; lease {}, expires in {}m)", l.holder, l.goal, l.scope.join(", "), l.id, l.expires_in_ms(now) / 60_000))
                        .collect()
                })
                .unwrap_or_default();
            if !near.is_empty() {
                push(&mut sections, &mut omitted, "leases", "Work in progress nearby — coordinate before editing".into(), near.join("\n"));
            }
        }
    }

    // 5. Recent intent: last commits on the top files.
    let mut intent = Vec::new();
    for f in files.iter().take(4) {
        if let Ok(log) = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo.root)
            .args(["log", "-n", "3", "--date=short", "--format=%h %ad %s", "--", f])
            .output()
        {
            for l in String::from_utf8_lossy(&log.stdout).lines() {
                intent.push(format!("- {f}: {l}"));
            }
        }
    }
    if !intent.is_empty() {
        push(&mut sections, &mut omitted, "intent", "Recent commits on these files".into(), intent.join("\n"));
    }

    let cited_files_tokens = files.iter().filter_map(|f| contents.get(f)).map(|c| tokens(c)).sum();
    let naive_tokens = top_files.iter().take(3).filter_map(|f| contents.get(*f)).map(|c| tokens(c)).sum();
    Ok(Pack {
        task: task.to_string(),
        budget,
        tokens: used,
        naive_tokens,
        cited_files_tokens,
        files,
        sections,
        omitted,
        ranker: found.ranker,
        elapsed_ms: t0.elapsed().as_millis() as u64,
    })
}

/// The pack as markdown, ready to paste into (or return to) an agent.
pub fn render(p: &Pack) -> String {
    let mut out = format!("# Context for: {}\n", p.task);
    for s in &p.sections {
        out.push_str(&format!("\n## {}\n{}\n", s.title, s.body));
    }
    if !p.omitted.is_empty() {
        out.push_str("\n## Cut by the budget (ask for these by name if needed)\n");
        for o in &p.omitted {
            out.push_str(&format!("- {o}\n"));
        }
    }
    out.push_str(&format!(
        "\n---\n~{} tokens (chars/4) of a {} budget. Reading the top 3 files in full: ~{}; all {} cited files: ~{}. Ranked by {}.\n",
        p.tokens,
        p.budget,
        p.naive_tokens,
        p.files.len(),
        p.cited_files_tokens,
        p.ranker
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn pack_puts_dead_ends_first_inlines_notes_and_respects_the_budget() {
        let dir = std::env::temp_dir().join(format!("heddle-ctx-{}-{}", std::process::id(), crate::now_ms()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let mut big = String::new();
        for i in 0..400 {
            big.push_str(&format!("pub fn filler_{i}(x: u32) -> u32 {{\n    x + {i}\n}}\n\n"));
        }
        std::fs::write(
            dir.join("src/session.rs"),
            format!("pub struct Session {{ token: String }}\n\nimpl Session {{\n    pub fn refresh(&mut self) {{\n        self.token = rotate_token(&self.token);\n    }}\n}}\n\nfn rotate_token(t: &str) -> String {{\n    t.chars().rev().collect()\n}}\n\n{big}"),
        )
        .unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "init"]);
        let repo = Repo::open(&dir).unwrap();
        let me = gloss::Author { by: "t".into(), thread: None, lease: None };
        gloss::add(&repo, "src/session.rs::Session::refresh", "refresh must rotate before expiry, never after", "invariant", me.clone()).unwrap();
        gloss::add(&repo, "src/session.rs::rotate_token", "Tried: cache rotated tokens\nFailed: replay window", "deadend", me).unwrap();
        let data = dir.join("data");
        std::env::set_var("HEDDLE_EMBED", "off");
        let p = build(None, &repo, &data, "session token refresh rotate", 30_000).unwrap();
        assert_eq!(p.sections[0].kind, "dead_end", "{:?}", p.sections.iter().map(|s| s.kind).collect::<Vec<_>>());
        let titles: Vec<&str> = p.sections.iter().map(|s| s.title.as_str()).collect();
        let refresh = p
            .sections
            .iter()
            .find(|s| s.kind == "symbol" && s.body.contains("pub fn refresh"))
            .unwrap_or_else(|| panic!("symbol, not the whole file: {titles:?}"));
        assert!(refresh.body.contains("refresh must rotate before expiry"), "note inline: {}", refresh.body);
        assert!(p.sections.iter().all(|s| !s.body.contains("filler_399")), "never the whole file");
        assert!(p.tokens <= p.budget);
        assert!(p.naive_tokens > p.tokens * 5, "{} vs {}", p.naive_tokens, p.tokens);

        // A tiny budget cuts sections and names what was cut.
        let small = build(None, &repo, &data, "session token refresh rotate", 60).unwrap();
        assert!(small.tokens <= 60);
        assert!(!small.omitted.is_empty());
        assert!(render(&small).contains("Cut by the budget"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn budgets_parse() {
        assert_eq!(parse_budget("30k"), Some(30_000));
        assert_eq!(parse_budget("1.5K"), Some(1500));
        assert_eq!(parse_budget("800"), Some(800));
        assert_eq!(parse_budget("-1"), None);
        assert_eq!(parse_budget("lots"), None);
    }
}

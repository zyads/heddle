// Heddle — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! Races — N attempts at one task, a verifier picks the winner, and the
//! failed attempts become dead ends instead of vanishing.
//!
//! `heddle race "<goal>" <scope...> --n 3` declares N sibling leases, each
//! with its own isolated worktree. Siblings share a race id, so they never
//! toe-step each other. Point one agent (or one model, or one prompt) at
//! each worktree; heddle does not run agents, it coordinates them.
//!
//! `heddle race judge <race>` verifies every attempt in a scratch copy (the
//! ordinary weave gate) and picks a winner by facts, in this order:
//! **green** beats red; among greens, the **smallest change** (changed
//! lines vs base) wins; ties go to the **fastest verify**. Then:
//!
//! * red attempts are abandoned AND pinned as dead ends — the failure
//!   digest lands on the symbols they changed;
//! * green attempts that lost are abandoned WITHOUT a dead end — a working
//!   alternative is not a failed approach, and calling it one would teach
//!   the next agent something false. Their note says what beat them;
//! * the winner is returned to Active, verified; landing it goes through
//!   the normal consent gate (`heddle propose`), which re-verifies against
//!   the fabric as it is at that moment.

use serde::Serialize;

use crate::{gloss, merge, store, DeclareOutcome, Heddle, IsolationMode, ThreadStatus, VerifyResult, TOMBSTONE};

pub const MAX_ATTEMPTS: usize = 8;

/// Start a race: `n` isolated sibling attempts at `goal` over `scope`.
pub fn start(engine: &Heddle, repo_id: &str, holder: &str, goal: &str, scope: Vec<String>, n: usize) -> Result<(String, Vec<DeclareOutcome>), String> {
    if !(2..=MAX_ATTEMPTS).contains(&n) {
        return Err(format!("a race needs 2–{MAX_ATTEMPTS} attempts"));
    }
    let race = format!("race-{}", &store::content_hash(format!("{goal}{}", crate::now_ms()).as_bytes())[..10]);
    let mut out = Vec::new();
    for i in 1..=n {
        let d = engine.declare_lease_raced(
            repo_id,
            holder,
            &format!("{goal} [attempt {i}/{n}]"),
            scope.clone(),
            vec![],
            None,
            IsolationMode::Isolated,
            Some(race.clone()),
        );
        match d {
            Ok(d) => out.push(d),
            Err(e) => {
                // All or nothing: release what was declared.
                for d in &out {
                    let _ = engine.release_lease(&d.lease.id);
                }
                return Err(format!("race not started (attempt {i}): {e}"));
            }
        }
    }
    Ok((race, out))
}

#[derive(Debug, Serialize)]
pub struct Entry {
    pub thread: String,
    pub goal: String,
    /// None: nothing was stitched, so there was nothing to verify.
    pub green: Option<bool>,
    pub changed_lines: usize,
    pub verify_ms: u64,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dead_end: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Verdict {
    pub race: String,
    pub winner: Option<String>,
    pub entries: Vec<Entry>,
}

fn changed_lines(engine: &Heddle, repo_id: &str, thread_id: &str) -> usize {
    let snap = engine.snapshot();
    let Some(rs) = snap.repo_states.get(repo_id) else { return 0 };
    let Some(t) = rs.threads.iter().find(|t| t.id == thread_id) else { return 0 };
    let m = |id: &Option<String>| {
        id.as_ref().and_then(|i| rs.stitches.iter().find(|s| &s.id == i)).map(|s| s.files.clone()).unwrap_or_default()
    };
    let (base, head) = (m(&t.base_stitch), m(&t.head_stitch));
    let objects = store::objects_dir(&engine.base(), repo_id);
    let text = |h: Option<&String>| {
        h.filter(|h| h.as_str() != TOMBSTONE)
            .and_then(|h| store::read_blob(&objects, h).ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    };
    head.iter()
        .filter(|(p, h)| base.get(*p) != Some(*h))
        .map(|(p, h)| {
            let (old, new) = (text(base.get(p)), text(Some(h)));
            let added: usize = merge::changed_ranges(&old, &new).iter().map(|(a, b)| b + 1 - a).sum();
            // Count removed lines too, so a pure deletion isn't "free".
            let removed: usize = merge::changed_ranges(&new, &old).iter().map(|(a, b)| b + 1 - a).sum();
            added.max(removed)
        })
        .sum()
}

/// Verify every attempt, pick the winner, retire the losers.
pub fn judge(engine: &Heddle, repo_id: &str, race: &str, by: &str) -> Result<Verdict, String> {
    let snap = engine.snapshot();
    let rs = snap.repo_states.get(repo_id).ok_or("repo has no state")?;
    let attempts: Vec<(String, String)> = rs
        .leases
        .iter()
        .filter(|l| l.race.as_deref() == Some(race))
        .filter(|l| rs.threads.iter().any(|t| t.id == l.thread_id && t.status.is_live()))
        .map(|l| (l.thread_id.clone(), l.goal.clone()))
        .collect();
    if attempts.is_empty() {
        return Err(format!("no live attempts in race {race}"));
    }
    let mut entries = Vec::new();
    for (tid, goal) in attempts {
        let stitched = engine
            .snapshot()
            .repo_states
            .get(repo_id)
            .and_then(|rs| rs.threads.iter().find(|t| t.id == tid).map(|t| t.head_stitch.is_some()))
            .unwrap_or(false);
        if !stitched {
            entries.push(Entry { thread: tid, goal, green: None, changed_lines: 0, verify_ms: 0, outcome: String::new(), dead_end: None });
            continue;
        }
        let t0 = std::time::Instant::now();
        let p = engine.propose(&tid)?;
        let verify_ms = t0.elapsed().as_millis() as u64;
        entries.push(Entry {
            changed_lines: changed_lines(engine, repo_id, &tid),
            thread: tid,
            goal,
            green: Some(p.weave.verify.result == VerifyResult::Green),
            verify_ms,
            outcome: String::new(),
            dead_end: None,
        });
    }
    let winner = entries
        .iter()
        .filter(|e| e.green == Some(true))
        .min_by_key(|e| (e.changed_lines, e.verify_ms))
        .map(|e| e.thread.clone());
    let win_lines = entries.iter().find(|e| Some(&e.thread) == winner.as_ref()).map(|e| e.changed_lines);
    for e in entries.iter_mut() {
        if Some(&e.thread) == winner.as_ref() {
            // Verified, but landing is a human's call: back to Active so the
            // ordinary `heddle propose` (re-verify + consent) can land it.
            if engine.snapshot().repo_states[repo_id].threads.iter().any(|t| t.id == e.thread && t.status == ThreadStatus::Proposed) {
                engine.withdraw(&e.thread, "won its race — `heddle propose` to land (re-verifies, asks you)")?;
            }
            e.outcome = "winner".into();
            continue;
        }
        let why = match (e.green, &winner, win_lines) {
            (None, _, _) => "race attempt never stitched a change".to_string(),
            (Some(false), _, _) => "race attempt: verify was red".to_string(),
            (Some(true), Some(w), Some(wl)) => format!(
                "race attempt: green, but {w} won with a smaller change ({wl} vs {} changed lines)",
                e.changed_lines
            ),
            (Some(true), _, _) => "race attempt: green, not chosen".to_string(),
        };
        let out = engine.abandon(&e.thread, &why)?;
        e.outcome = why.clone();
        if e.green == Some(false) {
            // Only failures are dead ends; the digest of the red verify
            // rides along automatically.
            match gloss::deadend::record(engine, &out, &why, by) {
                Ok(r) => e.dead_end = Some(r.group),
                Err(err) => e.outcome.push_str(&format!(" (dead end not recorded: {err})")),
            }
        }
    }
    Ok(Verdict { race: race.to_string(), winner, entries })
}

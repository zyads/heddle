// Heddle — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! `heddle gloss …` — the CLI face of [`heddle::gloss`]. Works in any git
//! repo (no `heddle init` needed): notes ride `refs/gloss/notes`, the cache
//! lives under the heddle data dir.

use std::io::{BufRead, IsTerminal, Write};

use heddle::gloss::{self, anchor::Status, render::RenderOpts, Author, Repo};

pub const USAGE: &str = "\
heddle gloss — notes for the next agent, anchored to the code they describe

  TARGET is  path  |  path:12  |  path:12-30  |  path::Type::method

  gloss read <file> [--plain] [--trusted]      the file with its notes inlined as
                                              comments (line numbers are the real
                                              file's; --trusted hides STALE notes)
  gloss add <TARGET> \"<text>\" [--kind K]      pin a note (K: note why invariant
       [--by NAME]                            gotcha todo deadend). Refuses
                                              secrets. Records commit + author
  gloss outline <file>                         symbol paths you can target
  gloss check [prefix] [--fix] [--kind K]      every note: fresh / moved / STALE /
                                              ORPHANED. --fix re-anchors moved
                                              notes (never blesses stale ones)
  gloss verify <id> [--text \"<new text>\"]     the note still holds: rebaseline it
  gloss move <id> <TARGET>                     re-target a note (keeps history)
  gloss drop <id> [--reason \"…\"]               retire it (stays in `why` + log)
  gloss why <TARGET>                           every note that ever covered it,
                                              plus the commits that touched it
  gloss log <id>                               one note's history
  gloss search \"<query>\" [-k N] [--json]       notes + code; BM25, plus local
                                              embeddings when Ollama is up
  gloss index                                  bring the code index up to HEAD
  gloss materialize                            write refs/gloss/llm: HEAD with
                                              notes inlined, for tools that only
                                              read files (`git worktree add
                                              <dir> refs/gloss/llm`)
  gloss sync <remote> [--anyway]               opt-in: share notes with a remote
                                              (refuses world-readable remotes)

Notes live in refs/gloss/notes — not carried by an ordinary git push, a clone or heddle sync
(only `git push --mirror` or an explicit refs/* refspec would).";

fn flag(rest: &[String], name: &str) -> Option<String> {
    rest.iter().position(|a| a == name).and_then(|i| rest.get(i + 1).cloned())
}

fn has(rest: &[String], name: &str) -> bool {
    rest.iter().any(|a| a == name)
}

/// Positional args, skipping flags and their values.
fn positional(rest: &[String], valued: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if valued.contains(&a.as_str()) {
            it.next();
        } else if !a.starts_with("--") && a != "-k" {
            out.push(a.clone());
        }
    }
    out
}

/// Who is writing: --by, else $HEDDLE_AGENT, else $USER; plus the heddle
/// thread/lease when standing in a thread's worktree.
pub fn author(by: Option<String>) -> Author {
    let by = by
        .or_else(|| std::env::var("HEDDLE_AGENT").ok())
        .or_else(|| std::env::var("USER").ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "unknown".into());
    let (thread, lease) = heddle::store()
        .thread_containing(".")
        .map(|(_, t)| (Some(t.id.clone()), t.lease_id.clone()))
        .unwrap_or((None, None));
    Author { by, thread, lease }
}

pub fn run(rest: &[String]) -> Result<(), String> {
    let verb = rest.first().map(String::as_str).unwrap_or("");
    let rest = rest.get(1..).unwrap_or(&[]);
    if matches!(verb, "" | "help" | "--help" | "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let repo = Repo::open(".")?;
    let data = heddle::default_data_dir();
    match verb {
        "read" => {
            let p = positional(rest, &[]);
            let file = p.first().ok_or("gloss read <file>")?;
            let opts = RenderOpts { numbers: !has(rest, "--plain"), show_untrusted: !has(rest, "--trusted") };
            let (text, notes) = gloss::read(&repo, file, &opts)?;
            print!("{text}");
            let dead = notes.iter().filter(|(n, _)| n.kind == "deadend").count();
            if dead > 0 {
                eprintln!("gloss: {dead} DEAD END(s) in this file — approaches already tried and failed; read them first");
            }
            let bad = notes.iter().filter(|(_, r)| !r.status.trustworthy()).count();
            if bad > 0 {
                eprintln!("gloss: {bad} note(s) here are STALE/ORPHANED — `heddle gloss check` lists them");
            }
            Ok(())
        }
        "add" => {
            let p = positional(rest, &["--kind", "--by"]);
            let (Some(target), Some(text)) = (p.first(), p.get(1)) else {
                return Err("gloss add <TARGET> \"<text>\" [--kind K]".into());
            };
            let kind = flag(rest, "--kind").unwrap_or_else(|| "note".into());
            let n = gloss::add(&repo, target, text, &kind, author(flag(rest, "--by")))?;
            println!("{} pinned to {} @ {}{}", n.id, n.anchor.label(), short(&n.commit), if n.dirty { " (uncommitted edits)" } else { "" });
            Ok(())
        }
        "outline" => {
            let p = positional(rest, &[]);
            let file = repo.rel(p.first().ok_or("gloss outline <file>")?)?;
            let content = repo.content(&file).ok_or_else(|| format!("{file}: no such file"))?;
            let syms = gloss::anchor::outline(&file, &content);
            if syms.is_empty() {
                println!("no symbols found (unsupported language?) — target lines instead: {file}:12-30");
            }
            for s in syms {
                println!("{:>6}-{:<6} {file}::{}", s.start, s.end, s.path);
            }
            Ok(())
        }
        "check" | "ls" => {
            let fix = has(rest, "--fix");
            let p = positional(rest, &["--kind"]);
            let mut checked = gloss::check(&repo, p.first().map(String::as_str), fix)?;
            if let Some(k) = flag(rest, "--kind") {
                checked.retain(|c| c.note.kind == k);
            }
            if has(rest, "--json") {
                let views: Vec<_> = checked
                    .iter()
                    .map(|c| serde_json::json!({"path": c.renamed_to.as_ref().unwrap_or(&c.path), "note": gloss::NoteView::of(&c.note, Some(&c.resolution))}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&views).map_err(|e| e.to_string())?);
                return Ok(());
            }
            let (mut fresh, mut moved, mut stale, mut orphan) = (0, 0, 0, 0);
            for c in &checked {
                match c.resolution.status {
                    Status::Fresh => fresh += 1,
                    Status::Moved { .. } => moved += 1,
                    Status::Stale => stale += 1,
                    Status::Orphaned => orphan += 1,
                }
                let at = c.renamed_to.clone().unwrap_or(c.path.clone());
                println!(
                    "{:<8} {:<9} {}:{} [{}] {}",
                    c.note.id,
                    c.resolution.status.word(),
                    at,
                    if c.resolution.start == 0 { "-".into() } else { c.resolution.start.to_string() },
                    c.note.kind,
                    heddle::cap(&c.note.text.replace('\n', " "), 70)
                );
            }
            println!(
                "{} note(s): {fresh} fresh, {moved} moved{}, {stale} stale, {orphan} orphaned",
                checked.len(),
                if fix && moved > 0 { " (re-anchored)" } else { "" }
            );
            if stale + orphan > 0 {
                println!("stale/orphaned notes may be wrong: read the code, then `gloss verify <id>` or `gloss drop <id>`");
            }
            Ok(())
        }
        "verify" => {
            let p = positional(rest, &["--text", "--by"]);
            let id = p.first().ok_or("gloss verify <id> [--text …]")?;
            let n = gloss::verify(&repo, id, &author(flag(rest, "--by")).by, flag(rest, "--text").as_deref())?;
            println!("{} verified at {} — anchored to {}", n.id, short(&repo.head()), n.anchor.label());
            Ok(())
        }
        "move" => {
            let p = positional(rest, &["--by"]);
            let (Some(id), Some(t)) = (p.first(), p.get(1)) else { return Err("gloss move <id> <TARGET>".into()) };
            let n = gloss::move_note(&repo, id, t, &author(flag(rest, "--by")).by)?;
            println!("{} now anchored to {}", n.id, n.anchor.label());
            Ok(())
        }
        "drop" => {
            let p = positional(rest, &["--reason", "--by"]);
            let id = p.first().ok_or("gloss drop <id> [--reason …]")?;
            let reason = flag(rest, "--reason").unwrap_or_else(|| "no longer true".into());
            let n = gloss::drop_note(&repo, id, &author(flag(rest, "--by")).by, &reason)?;
            println!("{} retired — still visible in `gloss why` and `gloss log {}`", n.id, n.id);
            Ok(())
        }
        "why" => {
            let p = positional(rest, &[]);
            let w = gloss::why(&repo, p.first().ok_or("gloss why <TARGET>")?)?;
            if has(rest, "--json") {
                println!("{}", serde_json::to_string_pretty(&w.to_json()).map_err(|e| e.to_string())?);
                return Ok(());
            }
            println!("{}:{}-{}", w.path, w.start, w.end);
            if w.notes.is_empty() && w.retired.is_empty() {
                println!("  no notes cover this span");
            }
            for (n, r) in &w.notes {
                println!("  {} [{}] {} — {} @ {} ({})", n.id, n.kind, r.status.word(), n.author.by, short(&n.commit), age(n.created_at));
                for l in n.text.lines() {
                    println!("      {l}");
                }
            }
            for n in &w.retired {
                let r = n.retired.as_ref().expect("filtered");
                println!("  {} [{}] retired {} by {}: {}", n.id, n.kind, age(r.at), r.by, r.reason);
                for l in n.text.lines() {
                    println!("      was: {l}");
                }
            }
            if !w.commits.is_empty() {
                println!("  commits touching these lines (newest first):");
                for c in &w.commits {
                    println!("    {c}");
                }
            }
            Ok(())
        }
        "log" => {
            let p = positional(rest, &[]);
            let id = p.first().ok_or("gloss log <id>")?;
            let (_, n) = gloss::find(&repo, id)?;
            for l in gloss::history(&repo, &n.id) {
                println!("{l}");
            }
            Ok(())
        }
        "search" => {
            let p = positional(rest, &["-k"]);
            let q = p.join(" ");
            if q.trim().is_empty() {
                return Err("gloss search \"<query>\"".into());
            }
            let k = flag(rest, "-k").and_then(|v| v.parse().ok()).unwrap_or(8);
            let res = gloss::search(&repo, &data, &q, k)?;
            if has(rest, "--json") {
                println!("{}", serde_json::to_string_pretty(&res).map_err(|e| e.to_string())?);
                return Ok(());
            }
            if let Some(w) = &res.warning {
                eprintln!("gloss: {w}");
            }
            if let Some(i) = &res.index {
                if i.refreshed {
                    eprintln!("gloss: code index refreshed to {} in {} ms", short(&i.commit), i.refresh_ms);
                }
            }
            println!("notes ({}):", res.ranker);
            if res.notes.is_empty() {
                println!("  none match");
            }
            for h in &res.notes {
                println!("  {} {} [{}] {}", h.note.id, h.note.anchor, h.note.kind, heddle::cap(&h.note.text.replace('\n', " "), 90));
            }
            println!("code:");
            for h in &res.code {
                println!("  {}:{}-{}  ({:.2})", h.path, h.start_line, h.end_line, h.score);
            }
            Ok(())
        }
        "index" => {
            let s = gloss::index_update(&repo, &data)?;
            println!(
                "indexed {} @ {}: {} live blobs ({} new, {} skipped), {} chunks, {} segment(s), {:.1} MB on disk, {} ms{}",
                repo.root.display(),
                short(&s.commit),
                s.blobs_live,
                s.blobs_new,
                s.skipped,
                s.chunks_live,
                s.segments,
                s.bytes_on_disk as f64 / 1e6,
                s.elapsed_ms,
                if s.compacted { " (compacted)" } else { "" }
            );
            Ok(())
        }
        "materialize" => {
            let (c, files) = gloss::materialize(&repo)?;
            println!("{} → {} ({files} annotated file(s) inlined; everything else is HEAD as-is)", gloss::LLM_REF, short(&c));
            println!("read it with: git worktree add --detach <dir> {}", gloss::LLM_REF);
            Ok(())
        }
        "sync" => {
            let p = positional(rest, &[]);
            let remote = p.first().ok_or("gloss sync <remote> [--anyway]")?;
            let (merged, total) = gloss::sync(&repo, remote, has(rest, "--anyway"))?;
            println!("synced notes with {remote}: {merged} merged in, {total} total");
            Ok(())
        }
        other => Err(format!("unknown gloss verb '{other}'\n\n{USAGE}")),
    }
}

fn short(c: &str) -> String {
    gloss::short(c, 8)
}

fn age(ms: u64) -> String {
    let d = heddle::gloss::now_ms().saturating_sub(ms) / 1000;
    match d {
        0..=119 => format!("{d}s ago"),
        120..=7199 => format!("{}m ago", d / 60),
        7200..=172_799 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

/// After a weave lands at a terminal: ask what the agent (or human) learned,
/// while it is fresh. One note per line, `TARGET text`; blank line ends.
/// Silent when stdin is not a terminal — never blocks a script.
pub fn capture_learned(repo_root: &std::path::Path, thread_id: &str) {
    if !std::io::stdin().is_terminal() {
        println!("tip: record what you learned for the next agent — `heddle gloss add <path::Symbol> \"…\" --kind why`");
        return;
    }
    let Ok(repo) = Repo::open(repo_root) else { return };
    println!("anything the next agent should know? one per line as `<path[:lines|::Symbol]> <note>`; blank to finish");
    let stdin = std::io::stdin();
    loop {
        print!("gloss> ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        let Some((t, text)) = line.trim().split_once(char::is_whitespace) else {
            println!("  need a target and a note");
            continue;
        };
        let mut a = author(None);
        a.thread = Some(thread_id.to_string());
        match gloss::add(&repo, t, text, "why", a) {
            Ok(n) => println!("  {} pinned to {}", n.id, n.anchor.label()),
            Err(e) => println!("  not stored: {e}"),
        }
    }
}

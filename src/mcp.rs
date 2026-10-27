// Heddle — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! `heddle mcp` — a minimal MCP (Model Context Protocol) stdio server, so any
//! agent session (Claude Code or otherwise) can lease/stitch/propose without
//! any other infrastructure.
//!
//! Hand-rolled on purpose: MCP's stdio transport is newline-delimited
//! JSON-RPC 2.0, and this server needs exactly three methods — `initialize`,
//! `tools/list`, `tools/call` — so ~200 lines beat an SDK dependency.
//!
//! **Honesty at the protocol edge:** this process's stdin is the protocol
//! channel, so there is no terminal to ask a human at. `heddle_propose`
//! therefore uses [`AutoDeny`]: the verify runs for real (scratch copy,
//! never the working tree) and a green result is recorded, but the apply is
//! always refused with instructions to run `heddle propose` at a terminal.
//! The tool descriptions say so, in those words — an agent reading them
//! learns the truth, not a euphemism.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use heddle::consent::{AutoDeny, WeaveDisposition};
use heddle::{gloss, solo, Heddle};

/// Serve MCP over stdio until stdin closes.
pub fn serve(engine: &Heddle) {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue; // not JSON — nothing sane to answer
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        // Notifications (no id) get no response, per JSON-RPC.
        let Some(id) = id else { continue };
        let reply = match method {
            "initialize" => json!({"jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "heddle", "version": env!("CARGO_PKG_VERSION")},
            }}),
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools()}}),
            "tools/call" => {
                let name = msg
                    .pointer("/params/name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                let args = msg
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match call(engine, name, &args) {
                    Ok(text) => json!({"jsonrpc": "2.0", "id": id, "result": {
                        "content": [{"type": "text", "text": text}],
                        "isError": false,
                    }}),
                    Err(e) => json!({"jsonrpc": "2.0", "id": id, "result": {
                        "content": [{"type": "text", "text": e}],
                        "isError": true,
                    }}),
                }
            }
            other => json!({"jsonrpc": "2.0", "id": id, "error": {
                "code": -32601, "message": format!("method not found: {other}"),
            }}),
        };
        let Ok(body) = serde_json::to_string(&reply) else { continue };
        if writeln!(stdout, "{body}").is_err() || stdout.flush().is_err() {
            break;
        }
    }
}

fn tools() -> Value {
    let repo_prop = json!({"type": "string", "description":
        "Repo directory (default: the server's working directory). Must already be `heddle init`-ed."});
    let learned_prop = json!({"type": "array", "description":
        "What you learned that the next agent should know, pinned to code (optional, but this is \
         the moment you know it best). Each item: {target: path | path:12-30 | path::Sym, text, \
         kind?: why|invariant|gotcha|deadend|note}. Stored as gloss notes in your worktree.",
        "items": {"type": "object", "properties": {
            "target": {"type": "string"}, "text": {"type": "string"}, "kind": {"type": "string"}},
            "required": ["target", "text"]}});
    let gloss_repo_prop = json!({"type": "string", "description":
        "Directory inside the git repo or thread worktree (default: the server's working directory). \
         Pass your working_dir when working in a heddle worktree."});
    json!([
        {
            "name": "heddle_status",
            "description": "Threads, live leases (with expiry), orphan queue, toe-step warnings and \
                fabric history for the repo. Read-only.",
            "inputSchema": {"type": "object", "properties": {"repo": repo_prop}},
        },
        {
            "name": "heddle_lease",
            "description": "Declare an intent lease before editing: a one-sentence goal plus \
                repo-relative path globs (e.g. src/parse/**). Overlap with live leases SUCCEEDS \
                and returns toe_step warnings — a lease is knowledge, not a lock. On a git repo \
                the thread gets its OWN isolated worktree: the response's working_dir is where \
                you MUST cd and do ALL file edits (the repo tree itself changes only when your \
                weave lands). Becomes the current lease for heddle_stitch/heddle_propose.",
            "inputSchema": {"type": "object", "properties": {
                "goal": {"type": "string", "description": "One sentence: what you are about to do."},
                "scope": {"type": "array", "items": {"type": "string"},
                          "description": "Repo-relative path globs you intend to touch, or symbols \
                          (`src/auth.rs::Session::refresh`): a symbol lease only collides with leases \
                          on the same symbol, so agents can share a file."},
                "criteria": {"type": "array", "items": {"type": "string"},
                             "description": "Acceptance criteria (optional)."},
                "ttl_ms": {"type": "integer", "description":
                    "Lease TTL in ms (default 30min; clamped 10s–24h). Stitching heartbeats it."},
                "in_place": {"type": "boolean", "description":
                    "true = edit the repo tree directly instead of an isolated worktree \
                     (v0.1 behavior; collisions then only warn)."},
                "repo": repo_prop,
            }, "required": ["goal", "scope"]},
        },
        {
            "name": "heddle_stitch",
            "description": "Checkpoint the leased scope from the thread's working_dir (its \
                worktree when isolated): the server reads the files itself and snapshots them \
                content-addressed, deletions included (you never upload content, and nothing is \
                written to the repo). Cheap — call every few edits. Also heartbeats the lease.",
            "inputSchema": {"type": "object", "properties": {
                "lease_id": {"type": "string", "description": "Defaults to the thread whose \
                    worktree contains `repo`, else the repo's ONLY live thread; with several \
                    live threads a bare call is refused with the list — pass YOUR lease id."},
                "repo": repo_prop,
            }},
        },
        {
            "name": "heddle_rebase",
            "description": "After a land was refused with 'fabric moved under you': refresh the \
                thread's worktree from the fabric. Fabric-only changes fast-forward in; files \
                changed on BOTH sides keep YOUR version and come back as conflicts to review \
                (the fabric's copy is in the repo tree). Then heddle_stitch and heddle_propose again.",
            "inputSchema": {"type": "object", "properties": {
                "thread_id": {"type": "string", "description": "Defaults to the thread whose \
                    worktree contains `repo`, else the repo's ONLY live thread; ambiguous \
                    bare calls are refused with the list."},
                "repo": repo_prop.clone(),
            }},
        },
        {
            "name": "heddle_propose",
            "description": "Run the weave gate: the repo's verify command runs in a scratch copy, \
                never the working tree. Red is reported with the log tail. Green is recorded, but \
                landing it needs a human yes at a terminal (`heddle propose`) — this server has no \
                terminal, so the apply is always refused here and the thread returns to active, \
                re-proposable. Honest summary: proposing verifies; it never lands.",
            "inputSchema": {"type": "object", "properties": {
                "thread_id": {"type": "string", "description": "Defaults to the thread whose \
                    worktree contains `repo`, else the repo's ONLY live thread; ambiguous \
                    bare calls are refused with the list."},
                "learned": learned_prop.clone(),
                "repo": repo_prop,
            }},
        },
        {
            "name": "heddle_adopt",
            "description": "Take over an orphaned thread (its holder's lease expired — crashed \
                session, dead machine). You inherit the SAME lease: goal, criteria and scope \
                preserved, fresh TTL. Continue from its last stitch.",
            "inputSchema": {"type": "object", "properties": {
                "thread_id": {"type": "string"},
                "repo": repo_prop,
            }, "required": ["thread_id"]},
        },
        {
            "name": "heddle_abandon",
            "description": "Give up on your thread when the approach does not work. The lease is \
                released and what you tried is pinned to the symbols you changed as a DEAD END \
                note — goal, your `why`, the last red verify — which the next agent sees first \
                when it leases that scope or reads those files. Git forgets failed attempts; this \
                is how the next agent avoids repeating yours. Be specific in `why`: what broke and \
                what you would try instead.",
            "inputSchema": {"type": "object", "properties": {
                "why": {"type": "string", "description": "What failed and why (one to three sentences)."},
                "thread_id": {"type": "string", "description": "Defaults like heddle_propose."},
                "repo": repo_prop.clone(),
            }, "required": ["why"]},
        },
        {
            "name": "heddle_context",
            "description": "START HERE for a task in an unfamiliar area: returns the smallest context \
                that lets you begin — the relevant functions/types (not whole files) with the notes \
                earlier agents pinned to them, DEAD ENDS already tried there, signatures of what \
                that code calls, live leases by other agents on those files, and recent commits — \
                under a token budget. Whatever the budget cut is listed by name so you can ask for \
                it. Local only.",
            "inputSchema": {"type": "object", "properties": {
                "task": {"type": "string", "description": "What you are about to do, in a sentence."},
                "budget": {"type": "integer", "description": "Token budget (default 30000)."},
                "repo": gloss_repo_prop.clone(),
            }, "required": ["task"]},
        },
        {
            "name": "heddle_gloss_read",
            "description": "Read a file WITH the notes earlier agents pinned to it, interleaved as \
                comments in the file's own syntax, above the code they describe. Each note says \
                who wrote it, at which commit, and whether it is fresh or STALE (the code under it \
                changed since — verify before trusting). Line numbers are the real file's. Prefer \
                this over a plain read for files you are about to change. Read-only.",
            "inputSchema": {"type": "object", "properties": {
                "path": {"type": "string", "description": "File path (repo-relative or absolute)."},
                "trusted_only": {"type": "boolean", "description": "Hide STALE/ORPHANED notes (default false: you see them, clearly marked)."},
                "repo": gloss_repo_prop.clone(),
            }, "required": ["path"]},
        },
        {
            "name": "heddle_gloss_add",
            "description": "Pin what you learned to the code it is about, for the next agent: WHY \
                code is shaped this way, an invariant that is not written down, a gotcha, an \
                approach that failed. Target: `path`, `path:12-30`, or `path::Type::method` (best: \
                symbols survive moves). Records your commit and identity; the note goes STALE if \
                that code later changes. Stored locally in refs/gloss/notes; an ordinary git push never carries it. Text \
                that looks like a secret is refused. Keep it to what the code cannot say itself.",
            "inputSchema": {"type": "object", "properties": {
                "target": {"type": "string"},
                "text": {"type": "string"},
                "kind": {"type": "string", "enum": heddle::gloss::KINDS},
                "repo": gloss_repo_prop.clone(),
            }, "required": ["target", "text"]},
        },
        {
            "name": "heddle_gloss_search",
            "description": "Search pinned notes and the code itself (BM25 over a blob-keyed \
                incremental index; fused with local embeddings when a loopback Ollama model is \
                available — nothing leaves the machine). Returns notes with anchors and code \
                hits with line ranges.",
            "inputSchema": {"type": "object", "properties": {
                "query": {"type": "string"},
                "k": {"type": "integer", "description": "Max hits per list (default 8)."},
                "repo": gloss_repo_prop.clone(),
            }, "required": ["query"]},
        },
        {
            "name": "heddle_gloss_why",
            "description": "\"Why is this like this?\" for a target: every note that covers it \
                (live, with fresh/STALE state, and retired ones with why they were retired) plus \
                the commits that touched those lines.",
            "inputSchema": {"type": "object", "properties": {
                "target": {"type": "string"},
                "repo": gloss_repo_prop.clone(),
            }, "required": ["target"]},
        },
        {
            "name": "heddle_gloss_verify",
            "description": "You read the code under a STALE note and it still holds (optionally \
                with corrected text): rebaseline it to the current code and record the \
                verification. If it no longer holds, use heddle_gloss_drop instead.",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "string"},
                "text": {"type": "string", "description": "Replacement text (optional)."},
                "repo": gloss_repo_prop.clone(),
            }, "required": ["id"]},
        },
        {
            "name": "heddle_gloss_drop",
            "description": "Retire a note that is no longer true. It leaves reads but stays in \
                heddle_gloss_why and the notes history.",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "string"},
                "reason": {"type": "string"},
                "repo": gloss_repo_prop,
            }, "required": ["id", "reason"]},
        },
    ])
}

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty())
}

fn repo_of(engine: &Heddle, args: &Value) -> Result<heddle::RepoConfig, String> {
    let path = arg(args, "repo").unwrap_or(".");
    engine.repo_containing(path).ok_or_else(|| {
        format!("no registered heddle repo contains '{path}' — run `heddle init` there first")
    })
}

fn holder() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .map(|u| format!("{u} (mcp agent)"))
        .unwrap_or_else(|| "mcp agent".to_string())
}

/// Bare-call targeting (no lease_id/thread_id): the same rule as the CLI —
/// the thread whose worktree contains the `repo` path (pass your working_dir
/// to be unambiguous), else the repo's ONLY live thread, else an honest
/// refusal listing the candidates. The shared solo pointer is deliberately
/// NOT consulted: with several agents on one data dir it may name another
/// agent's thread.
fn bare_target(engine: &Heddle, args: &Value) -> Result<(heddle::RepoConfig, heddle::Thread), String> {
    engine.resolve_bare_target(arg(args, "repo").unwrap_or("."))
}

fn call(engine: &Heddle, name: &str, args: &Value) -> Result<String, String> {
    match name {
        "heddle_status" => {
            let repo = repo_of(engine, args)?;
            let snap = engine.snapshot();
            let rs = snap.repo_states.get(&repo.id).cloned().unwrap_or_default();
            let now = heddle_now();
            let leases: Vec<Value> = rs
                .leases
                .iter()
                .filter(|l| !l.expired(now))
                .map(|l| json!({"lease": l, "expires_in_ms": l.expires_in_ms(now)}))
                .collect();
            Ok(json!({
                "repo": repo,
                "fabric": rs.fabric,
                "threads": rs.threads,
                "live_leases": leases,
                "orphans": rs.threads.iter()
                    .filter(|t| t.status == heddle::ThreadStatus::Orphaned)
                    .collect::<Vec<_>>(),
                "toe_steps": rs.toe_steps.iter().rev().take(5).collect::<Vec<_>>(),
                "note": "leases warn, never block; weaves land only past a human yes at a terminal",
            })
            .to_string())
        }
        "heddle_lease" => {
            let repo = repo_of(engine, args)?;
            let goal = arg(args, "goal").ok_or("heddle_lease needs a goal")?;
            let scope: Vec<String> = args
                .get("scope")
                .and_then(|s| s.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let criteria: Vec<String> = args
                .get("criteria")
                .and_then(|s| s.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let ttl_ms = args.get("ttl_ms").and_then(|v| v.as_u64());
            let mode = if args.get("in_place").and_then(|v| v.as_bool()).unwrap_or(false) {
                heddle::IsolationMode::InPlace
            } else {
                heddle::IsolationMode::Auto
            };
            let out = engine
                .declare_lease_mode(&repo.id, &holder(), goal, scope, criteria, ttl_ms, mode)?;
            solo::set(&engine.base(), &repo.id, &out.lease.id, &out.thread.id);
            let isolated = out.thread.worktree.is_some();
            let dead_ends: Vec<Value> = gloss::Repo::open(&repo.path)
                .map(|g| {
                    gloss::deadend::in_scope(&g, &out.lease.scope)
                        .iter()
                        .take(8)
                        .map(|(n, r)| json!(gloss::NoteView::of(n, r.as_ref())))
                        .collect()
                })
                .unwrap_or_default();
            Ok(json!({
                "dead_ends": if dead_ends.is_empty() { Value::Null } else { json!({
                    "read_first": "approaches already tried in this scope that FAILED — do not repeat them blindly",
                    "notes": dead_ends,
                }) },
                "lease": out.lease,
                "thread": out.thread,
                "toe_steps": out.toe_steps,
                "working_dir": out.working_dir,
                "working_dir_note": if isolated {
                    "your isolated worktree — cd there and make ALL edits in it; the repo \
                     tree changes only when your weave lands"
                } else {
                    "no isolation (not a git repo, or in_place requested) — you are editing \
                     the repo tree directly"
                },
                "note": if out.toe_steps.is_empty() {
                    "no toe-steps — the scope is yours to work"
                } else {
                    "toe-step warnings above: another live lease overlaps your scope; \
                     the lease still succeeded — coordinate or continue"
                },
            })
            .to_string())
        }
        "heddle_rebase" => {
            let thread_id = match arg(args, "thread_id") {
                Some(t) => t.to_string(),
                None => bare_target(engine, args)?.1.id,
            };
            let out = engine.rebase_thread(&thread_id)?;
            Ok(json!({
                "thread": out.thread,
                "fast_forwarded": out.fast_forwarded,
                "merged": out.merged,
                "conflicts": out.conflicts,
                "note": if out.conflicts.is_empty() {
                    "rebased clean — heddle_stitch, then heddle_propose when ready".to_string()
                } else {
                    format!(
                        "CONFLICTS on {} file(s): both the fabric and this thread changed \
                         them. Your version was kept in the worktree; the fabric's is in \
                         the repo tree. Reconcile in the worktree, then heddle_stitch and \
                         heddle_propose.",
                        out.conflicts.len()
                    )
                },
            })
            .to_string())
        }
        "heddle_stitch" => {
            let (repo, lease_id) = match arg(args, "lease_id") {
                Some(l) => (repo_of(engine, args)?, l.to_string()),
                None => {
                    let (repo, thread) = bare_target(engine, args)?;
                    let lease = thread
                        .lease_id
                        .ok_or_else(|| format!("thread {} has no lease", thread.id))?;
                    (repo, lease)
                }
            };
            engine.heartbeat(&lease_id)?;
            let out = engine.stitch(&lease_id)?;
            Ok(json!({
                "stitch": out.stitch.id,
                "unchanged": out.unchanged,
                "files": out.stitch.files.len(),
                "skipped": out.skipped,
                "outside_leased_symbols": if out.outside_symbols.is_empty() { Value::Null } else { json!({
                    "edits": out.outside_symbols,
                    "note": "these edits are kept, but they sit outside the symbols you leased — another agent's \
                             lease on this file could collide there; widen your lease if you need them",
                }) },
                "lease_expires_in_ms": out.lease.expires_in_ms(heddle_now()),
                "auto_sync": autosync_note(engine, &repo),
            })
            .to_string())
        }
        "heddle_propose" => {
            let (repo, thread_id) = match arg(args, "thread_id") {
                Some(t) => (repo_of(engine, args)?, t.to_string()),
                None => {
                    let (repo, thread) = bare_target(engine, args)?;
                    (repo, thread.id)
                }
            };
            let learned = record_learned(engine, args, &thread_id);
            match engine.propose_with_consent(&thread_id, &AutoDeny)? {
                WeaveDisposition::Red { weave, .. } => Ok(json!({
                    "verify": "red",
                    "cmd": weave.verify.cmd,
                    "log_tail": weave.verify.log_tail,
                    "landed": false,
                    "note": "verify was red — nothing can land; fix and re-propose",
                    "learned": learned,
                })
                .to_string()),
                WeaveDisposition::Refused { weave, reason, .. } => Ok(json!({
                    "verify": "green",
                    "cmd": weave.verify.cmd,
                    "landed": false,
                    "reason": reason,
                    "auto_sync": autosync_note(engine, &repo),
                    "learned": learned,
                    "note": "verified green; NOTHING was applied — landing requires a human at a \
                             terminal: run `heddle propose` there (it re-runs the verify and asks)",
                })
                .to_string()),
                WeaveDisposition::Landed { .. } => {
                    Err("unreachable: AutoDeny never consents".to_string())
                }
            }
        }
        "heddle_adopt" => {
            let thread_id = arg(args, "thread_id").ok_or("heddle_adopt needs a thread_id")?;
            // Local first; unknown + a sync remote → the cross-machine
            // claims flow (first CAS push wins; losing names the winner).
            let (thread, lease) = match engine.adopt(thread_id, &holder()) {
                Ok(pair) => pair,
                Err(e) if e.contains("no thread with id") => {
                    let repo = repo_of(engine, args)?;
                    if repo.sync_remote.is_some() {
                        heddle::sync::adopt_remote(engine, &repo.id, thread_id, &holder())?
                    } else {
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            };
            solo::set(&engine.base(), &thread.repo_id, &lease.id, &thread.id);
            let working_dir = thread.worktree.clone();
            Ok(json!({
                "thread": thread,
                "lease": lease,
                "working_dir": working_dir,
                "note": "you inherited the goal, criteria and scope; the last stitch is \
                         materialized in working_dir (when set) — cd there and continue",
            })
            .to_string())
        }
        "heddle_abandon" => {
            let why = arg(args, "why").ok_or("heddle_abandon needs `why` — that sentence is the dead end")?;
            let (repo, thread_id) = match arg(args, "thread_id") {
                Some(t) => (repo_of(engine, args)?, t.to_string()),
                None => {
                    let (repo, thread) = bare_target(engine, args)?;
                    (repo, thread.id)
                }
            };
            let out = engine.abandon(&thread_id, why)?;
            if solo::get(&engine.base(), &repo.id).is_some_and(|p| p.thread_id == thread_id) {
                solo::clear(&engine.base(), &repo.id);
            }
            let recorded = match gloss::deadend::record(engine, &out, why, &holder()) {
                Ok(r) => json!(r),
                Err(e) => json!({"not_recorded": e}),
            };
            Ok(json!({"thread": out.thread.id, "status": "abandoned", "dead_end": recorded}).to_string())
        }
        "heddle_context" => {
            let repo = gloss::Repo::open(arg(args, "repo").unwrap_or("."))?;
            let task = arg(args, "task").ok_or("heddle_context needs `task`")?;
            let budget = args
                .get("budget")
                .and_then(|v| v.as_u64())
                .map(|b| b.clamp(1_000, 400_000) as usize)
                .unwrap_or(heddle::context::DEFAULT_BUDGET);
            let pack = heddle::context::build(Some(engine), &repo, &heddle::default_data_dir(), task, budget)?;
            Ok(heddle::context::render(&pack))
        }
        g if g.starts_with("heddle_gloss_") => gloss_call(g, args),
        other => Err(format!("unknown tool: {other}")),
    }
}

/// Store a propose call's `learned` items as gloss notes, written from the
/// thread's worktree (so anchors match the code the agent just changed).
/// Returns per-item outcomes; failures never fail the propose.
fn record_learned(engine: &Heddle, args: &Value, thread_id: &str) -> Value {
    let Some(items) = args.get("learned").and_then(|v| v.as_array()) else {
        return json!("none recorded — pass `learned` next time: what you know now is gone after this session");
    };
    let snap = engine.snapshot();
    let thread = snap.repo_states.values().flat_map(|rs| rs.threads.iter()).find(|t| t.id == thread_id);
    let dir = thread
        .and_then(|t| t.worktree.clone())
        .or_else(|| thread.and_then(|t| snap.repos.iter().find(|r| r.id == t.repo_id)).map(|r| r.path.clone()))
        .unwrap_or_else(|| ".".into());
    let Ok(repo) = gloss::Repo::open(&dir) else { return json!("not a git repo — notes need git") };
    let author = gloss::Author {
        by: holder(),
        thread: Some(thread_id.to_string()),
        lease: thread.and_then(|t| t.lease_id.clone()),
    };
    items
        .iter()
        .map(|it| {
            let t = it.get("target").and_then(|v| v.as_str()).unwrap_or("");
            let text = it.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let kind = it.get("kind").and_then(|v| v.as_str()).unwrap_or("why");
            match gloss::add(&repo, t, text, kind, author.clone()) {
                Ok(n) => json!({"id": n.id, "anchor": n.anchor.label()}),
                Err(e) => json!({"target": t, "not_stored": e}),
            }
        })
        .collect()
}

fn gloss_call(name: &str, args: &Value) -> Result<String, String> {
    let repo = gloss::Repo::open(arg(args, "repo").unwrap_or("."))?;
    let need = |k: &str| arg(args, k).ok_or_else(|| format!("{name} needs `{k}`"));
    match name {
        "heddle_gloss_read" => {
            let trusted = args.get("trusted_only").and_then(|v| v.as_bool()).unwrap_or(false);
            let opts = gloss::render::RenderOpts { numbers: true, show_untrusted: !trusted };
            let (text, notes) = gloss::read(&repo, need("path")?, &opts)?;
            let stale = notes.iter().filter(|(_, r)| !r.status.trustworthy()).count();
            let dead = notes.iter().filter(|(n, _)| n.kind == "deadend").count();
            let text = if dead > 0 {
                format!("[{dead} DEAD END note(s) in this file: approaches already tried here that failed. Read them before changing the code they sit on.]\n{text}")
            } else {
                text
            };
            Ok(if stale > 0 {
                format!("{text}\n[{stale} note(s) above are STALE/ORPHANED: the code changed since they were written. \
                         If you confirm one still holds, heddle_gloss_verify it; if not, heddle_gloss_drop it.]")
            } else {
                text
            })
        }
        "heddle_gloss_add" => {
            let kind = arg(args, "kind").unwrap_or("note");
            let author = gloss::Author { by: holder(), thread: None, lease: None };
            let n = gloss::add(&repo, need("target")?, need("text")?, kind, author)?;
            Ok(json!({"id": n.id, "anchor": n.anchor.label(), "commit": n.commit, "dirty": n.dirty,
                      "note": "stored locally in refs/gloss/notes (not carried by an ordinary git push, a clone, or heddle sync)"})
                .to_string())
        }
        "heddle_gloss_search" => {
            let k = args.get("k").and_then(|v| v.as_u64()).unwrap_or(8).clamp(1, 50) as usize;
            let res = gloss::search(&repo, &heddle::default_data_dir(), need("query")?, k)?;
            serde_json::to_string(&res).map_err(|e| e.to_string())
        }
        "heddle_gloss_why" => {
            let w = gloss::why(&repo, need("target")?)?;
            Ok(w.to_json().to_string())
        }
        "heddle_gloss_verify" => {
            let n = gloss::verify(&repo, need("id")?, &holder(), arg(args, "text"))?;
            Ok(json!({"id": n.id, "anchor": n.anchor.label(), "verified_at": repo.head()}).to_string())
        }
        "heddle_gloss_drop" => {
            let n = gloss::drop_note(&repo, need("id")?, &holder(), need("reason")?)?;
            Ok(json!({"id": n.id, "retired": n.retired}).to_string())
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// Run the repo's opted-in auto-sync (a no-op without `--auto`) and shape
/// an honest one-line summary; sync failures never fail the local call.
fn autosync_note(engine: &Heddle, repo: &heddle::RepoConfig) -> Value {
    // Re-read the config in case sync flags changed since `repo` was read.
    let repo = engine
        .snapshot()
        .repos
        .into_iter()
        .find(|r| r.id == repo.id)
        .unwrap_or_else(|| repo.clone());
    match heddle::sync::maybe_autosync(engine, &repo) {
        None => Value::Null,
        Some(Ok(out)) => json!({
            "remote": out.remote,
            "fabric_pulled": out.fabric_pulled,
            "fabric_pushed": out.fabric_pushed,
            "cas_refused": out.cas_refused,
            "cross_machine_toe_steps": out.toe_steps.len(),
        }),
        Some(Err(e)) => json!({"failed": e, "note": "local operation unaffected"}),
    }
}

fn heddle_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

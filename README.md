# Heddle

*Version control for many hands moving at once.*

Git is the filesystem; Heddle is air traffic control.

A heddle is the part of a loom that keeps threads separated so they never
tangle. That's the product.

Formerly known as loom-vcs; renamed to avoid collision with several
unrelated Loom projects.

## Two timelines

**Without Heddle.** You point two Claude Code sessions at your repo. Agent A
spends 40 minutes and ~200k tokens rewriting `src/auth/` around a new session
model. Agent B, told to "clean up the login flow", touches the same files with
the opposite assumption. Neither knows the other exists — git has nowhere to
put that knowledge. B finishes last, so B's edits win; A's work is silently
gone from the tree. The test suite goes red at commit time, and you spend the
evening (and another few hundred thousand tokens) having a third session
diagnose a breakage that is really two AIs' half-rewrites interleaved. The
most expensive thing agents do is semantically reconcile two overlapping
rewrites after the fact — and this workflow makes it routine.

**With Heddle.** A leases `src/auth/**` with the goal "move auth to the new
session model". When B's session asks Heddle what's being worked on — one MCP
call — it sees that lease, goal attached, and self-partitions: it either takes
the suggested disjoint scope or waits. The collision is reported at the
*start*, when handling it costs one tool call, not at the *end*, after both
budgets are spent. If both proceed anyway, each works in its own worktree,
nothing is overwritten, and the second landing is told exactly which files
moved and rebases once — the expensive reconciliation becomes rare instead of
fast.

That is the entire bet: **git reports collisions after the tokens are spent;
Heddle reports them before.**

## The problem

You run several coding agents on one repo. They overwrite each other's files,
land red on top of red, and die mid-task leaving dirty worktrees nobody owns.
You can't see who is doing what, so every collision is discovered at the end
— at merge or in CI — when the only fix is expensive re-work. Git has no
concept of *intent*: a branch name is a string, so agents can't ask "who is
already working on this?" and route around each other.

## What Heddle does

- **Isolation** — every task gets its own git worktree; two threads
  *physically cannot* clobber each other's edits.
- **Coordination** — before editing, a thread declares an **intent lease**
  (machine-readable goal + file globs); overlaps warn at declaration time,
  which lets agents self-partition work over MCP with no human traffic cop.
  Leases warn, they never block — an agent is never stopped; the only gate is
  at landing.
- **Integration** — the mainline (**fabric**) advances only through a green
  verify run plus a human/consent gate, with an honest file-level merge:
  a file changed in both the fabric and the thread refuses to land ("fabric
  moved under you — rebase") instead of overwriting either side. Never-red
  main means no fleet-wide token burn diagnosing a stranger's breakage.
- **Memory** (`heddle gloss`) — what an agent learned is pinned to the
  symbol it is about, versioned against commits, and marked **STALE** the
  moment the code under it changes. Agents read files with those notes
  inlined; nothing is written into the human tree. See
  [Gloss](#gloss--memory-pinned-to-code).
- **Recovery** — a dead agent's thread becomes an **orphan**: goal, acceptance
  criteria, and a seconds-old checkpoint attached, claimable by anyone. That
  is schedulable work, not a mystery dangling branch.

An honest concession: two careful humans working slowly don't need this. The
value scales with writer count and edit frequency — it exists for fleets of
agents (and humans who work like them).

Heddle is not alone in this space. Several projects have converged on
worktree isolation + gated merging — that convergence is evidence the
problem is real. What Heddle claims as its own is narrower and specific:
**leaderless, warn-only intent leases** (no dispatcher, no locks), an
**always-green fabric advanced only by compare-and-swap over bare git
remotes** (no server), **decentralized orphan adoption**, and
**stitch-level history** — a serverless, git-native *protocol* and one
binary, not a platform. See [Adjacent projects](#adjacent-projects) for the
neighbors and what each does that Heddle doesn't.

## What it's not for

- **N agents racing the SAME task.** Heddle coordinates *different* tasks on
  one repo. For redundant attempts at one task, use a judge/tournament
  harness — then lease the winner's landing.
- **Replacing git.** Heddle sits on top; git stays your history, remotes, and
  review tooling. The optional bridge projects each landed weave into local
  git history — one commit per weave by default, checkpoint-level if you ask
  (see `bridge_mode` below) — and never pushes.
- **Humans doing normal PRs.** Slow, coordinated-out-of-band work is what
  git+GitHub already does well.

## Two-minute quickstart

The disaster being prevented, on your own machine. One repo, two terminals
("agents" A and B):

```bash
cargo install --path .        # one binary: heddle
cd your-repo
heddle init --verify "cargo check"     # or any command that exits 0 on green
```

**Keep the verify fast.** It runs on *every* `heddle propose`, and every agent
waits for it before its work can land — a 49-second suite is 49 seconds of
every agent's time, every attempt. Point it at a quick subset (a few seconds:
`cargo check`, `pytest -q -m "not slow"`, `npm run test:fast`) and let the full
suite run in CI. `heddle init` times the command once and tells you if it is
slow (it skips that when stdout isn't a terminal, or with
`HEDDLE_SKIP_VERIFY_TIMING=1`).

**A:**

```bash
heddle lease "greet in french" 'greeting.txt'
#   WORK IN: ~/.heddle/<repo>/worktrees/<thread-A>     ← A's own worktree
```

**B, at the same time:**

```bash
heddle lease "greet louder" 'greeting.txt'
#   TOE-STEP: your 'greet louder' overlaps 'greet in french'
#   (a lease warns, it never blocks — coordinate or continue)
#   WORK IN: ~/.heddle/<repo>/worktrees/<thread-B>     ← a DIFFERENT worktree
```

Both edit the same file — in their own worktrees, so nothing clobbers. A
finishes first:

```bash
# A, from inside A's worktree — a bare verb targets the thread whose
# worktree you are standing in. Anywhere else, with two live threads,
# heddle refuses and lists them (pass --lease <id>); it never guesses,
# because a wrong guess would stitch onto B's thread.
cd ~/.heddle/<repo>/worktrees/<thread-A>
heddle stitch          # checkpoint (content-addressed; deletions tracked)
heddle propose         # verify runs in a scratch copy; on green it asks y/N
# → woven: 1 files applied
```

B proposes next, and gets the truth instead of a silent overwrite:

```bash
heddle stitch && heddle propose
# → verify green … but landing refuses:
#   fabric moved under you on greeting.txt — rebase the thread and re-propose
heddle rebase
#   CONFLICTS — both the fabric and this thread changed:
#     greeting.txt (your version kept in the worktree; the fabric's is in the repo tree)
# reconcile the file in B's worktree, then:
heddle stitch && heddle propose
# → woven — B's landing includes A's work instead of erasing it
```

Kill a session mid-task (or let its lease TTL lapse) and `heddle status` shows
the thread as an **orphan** — `heddle adopt <thread-id>` hands the next agent
its goal, criteria, and worktree, checkpoint intact.

## Agents (Claude Code or any MCP client)

Zero setup on Claude Code: `heddle init` (or `heddle adopt` in an
already-initialized repo) wires the repo so agents pick Heddle up with **no
prompting** — it registers the MCP server in `.mcp.json`, pre-approves it and
adds a `SessionStart` hook (`heddle status --brief`, silent when nothing is
live) in `.claude/settings.json`, and appends a short rules section to
`CLAUDE.md` between markers. Everything is shown before it is written,
idempotent, and previewable with `heddle adopt --dry-run`; `heddle init
--no-adopt` opts out. Other MCP clients register the server themselves:

```bash
claude mcp add heddle -- heddle mcp
```

The tool flow an agent follows:

1. `heddle_status` — who's working on what (threads, live leases, orphans).
2. `heddle_lease` — declare goal + scope. The response's `working_dir` is the
   thread's worktree: **cd there and make all edits in it.** Toe-step
   warnings come back in the same response — the moment to renegotiate scope.
3. work → `heddle_stitch` every few edits (checkpoints + heartbeats the lease).
4. `heddle_propose` — runs the verify in a scratch copy and reports green/red.
   The apply is always refused over MCP: landing takes a human at a terminal
   (`heddle propose`) or a host with an approvals queue. Proposing verifies;
   it never lands.
5. On "fabric moved": `heddle_rebase`, reconcile any conflicts in the worktree,
   stitch, re-propose. `heddle_adopt` picks up orphans (local or a synced
   peer's).

Memory tools (work in any git repo, no `heddle init` needed):
`heddle_gloss_read` (a file with its notes inlined), `heddle_gloss_add`,
`heddle_gloss_search`, `heddle_gloss_why`, `heddle_gloss_verify`,
`heddle_gloss_drop`. `heddle_propose` also takes `learned: [{target, text,
kind}]`: the moment an agent knows the most is the moment it proposes.

## Gloss — memory pinned to code

Agents forget everything between sessions, so we give them memory files:
`CLAUDE.md`, `memory/*.md`, a vector store of past chats. Those files have
three flaws. They are **unanchored**: they don't know which code they
describe. They are **unversioned**: nothing records what was true when. And
they **never go stale, they go wrong**: the code changes, the memory doesn't,
and the next agent trusts it.

A gloss is the marginal note a scribe wrote next to the text. In heddle it is
a note pinned to a symbol (`src/sync.rs::remote_is_world_readable`) or a line
span, recording the commit it was written against, who wrote it (agent,
thread, lease), and a fingerprint of the code under it.

```console
$ heddle gloss add 'src/sync.rs::remote_is_world_readable' --kind why \
    "Probes with NO -h flag: -h restricts ls-remote to branch heads, HEAD isn't one,
     and the empty result read as 'could not tell', so the public-repo guard never fired."
g9e43a5b pinned to src/sync.rs::remote_is_world_readable @ d09cd5d0

$ heddle gloss read src/sync.rs          # what an agent sees (MCP: heddle_gloss_read)
      ┆ // [gloss g9e43a5b · why · claude-auth-refactor · written @d09cd5d0 · fresh]
      ┆ //   Probes with NO -h flag: -h restricts ls-remote to branch heads, ...
 1124  │ /// Ask the remote **with every credential path shut off**. If it still answers,
```

Weeks later a different agent "tidies" the call and adds `-h` back:

```console
$ heddle gloss check
g9e43a5b STALE     src/sync.rs:1126 [why] Probes with NO -h flag: -h restricts …
1 note(s): 0 fresh, 0 moved, 1 stale, 0 orphaned

$ heddle gloss why src/sync.rs::remote_is_world_readable
src/sync.rs:1126-1171
  g9e43a5b [why] STALE — claude-auth-refactor @ d09cd5d0
      Probes with NO -h flag: ...
  commits touching these lines (newest first):
    d040875 2026-10-26 tidy ls-remote flags
    0818339 2026-07-26 Refuse to publish work-in-progress to a repo anyone can read
```

The note didn't silently turn into a lie. It says the code changed under it,
and the agent reading it sees the warning inline, right above the line it is
about to edit. Someone who reads the code then runs `gloss verify <id>`
(rebaseline it, optionally with corrected text) or `gloss drop <id>` (retire
it; it stays in `why` and in the history).

**Why this beats memory files**

| | CLAUDE.md / memory files | gloss |
|---|---|---|
| knows which code it describes | no | symbol path, line span, content fingerprint |
| survives refactors | n/a | re-found by symbol, then exact text, then fuzzy match; renamed files are followed |
| tells you when it's wrong | never | `STALE` the moment the code under it changes; whitespace-only edits don't count |
| history | whatever git blame on the .md says | every add/verify/move/drop is a commit on `refs/gloss/notes`; `gloss why` joins notes with `git log -L` |
| shown where it matters | loaded in full every session | inline above the code, only when that file is read |
| searchable | grep | BM25 over notes + code; fused with local embeddings when Ollama is up |
| private | committed and pushed with everything | local refs, never pushed by `git push` or `heddle sync`; secrets refused |

**How it's built**

- **No shadow copy to strip.** Writing comments into a mirror and stripping
  them for humans needs a stripper per language, can't tell an agent note
  from a human comment, and needs a 3-way re-placement whenever a human
  edits the mirror. Gloss stores notes beside the code, so the human tree is
  never touched. The "LLM copy" is rendered on read in the file's own
  comment syntax. For tools that only read plain files, `heddle gloss
  materialize` writes `refs/gloss/llm` (HEAD with notes inlined; only
  annotated files are re-rendered, every other blob is reused) and you can
  `git worktree add` it.
- **Anchors.** A small outliner (brace matching for C-family, Rust, Go, JS/TS
  and Java-likes; indentation for Python) gives symbol paths like
  `Heddle::stitch` or `Server::Serve`. It isn't tree-sitter, on purpose: that
  would add a C toolchain and one grammar crate per language to a crate
  whose dependencies are serde, sha2 and hex. A missed symbol costs
  precision, never correctness, because staleness is decided by the content
  fingerprint, not by the symbol.
- **Storage.** `refs/gloss/notes` is a commit chain whose tree holds one JSON
  file per annotated path. Writes are compare-and-swap on the ref with
  jittered retry. Worktrees share refs, so every heddle thread sees the same
  notes. `gloss sync <remote>` is the opt-in way to share them: it merges by
  note id and refuses world-readable remotes, the same rule `heddle sync`
  applies.
- **Code index.** An LSM-style BM25 index keyed by git blob id. Only blobs it
  hasn't seen are read and tokenized, so a commit costs what it changed.
  Segments are immutable, liveness is a bitmap, and compaction rewrites
  postings without touching git. Search reads only the queried terms'
  postings. It lives in `~/.heddle/gloss/<repo>/`, is rebuildable, and
  refreshes itself when HEAD moves.
- **Semantic ranking** comes from a loopback-only Ollama (`nomic-embed-text`
  by default, `HEDDLE_EMBED_MODEL` to change it, `HEDDLE_EMBED=off` to
  disable). An `OLLAMA_HOST` that isn't loopback is ignored. Vectors are
  cached by content hash, so a note or chunk is embedded once until it
  changes. With no model available, everything falls back to BM25 and the
  output says `ranker: bm25`.

**Numbers on the Linux kernel**: `git clone --depth 1 torvalds/linux`
(d135ab45), 24 cores, release build. Reproduce the index numbers with
`cargo run --release --example gloss_bench [repo]`.

| | |
|---|---|
| tree | 97,152 files, 1.19 GB of text (122 binary/oversize blobs skipped) |
| full code index, cold | **4.9–5.4 s**: 96,392 blobs → 873,422 chunks; peak RSS 1.6 GB |
| index on disk | **331 MB** (0.28× the text) |
| refresh, HEAD unchanged | **6–8 ms** |
| refresh after a 5-file commit | **154–184 ms** (5 blobs re-read, not 96k) |
| compaction (40%+ dead chunks) | 0.78 s, zero git reads |
| code search, warm | **p50 9.4 ms, p95 ≤ 11.8 ms** (10 queries × 5 rounds) |
| `gloss search` end to end, 1,952 notes | 80 ms BM25 only; 150 ms with embeddings (8.2 s once, cold, to embed every note) |
| `gloss read kernel/sched/core.c` (11,365 lines) | ~10 ms |
| `gloss add` | ~25 ms; 2,000 adds from 8 concurrent writers in 51 s, **0 lost** |
| `gloss check`, 1,950 notes | 0.12 s (after a 5-file commit: exactly the 4 notes on edited files go STALE) |
| `gloss materialize`, 1,950 annotated files | 1.2 s |

**Known gaps, stated plainly**

- The outliner is a heuristic. It misses JS/TS class methods written without
  a keyword (`foo() {`), macro-generated C functions, and languages outside
  the families above. Those still anchor by line span and content
  fingerprint.
- Code embeddings only *re-rank* the top lexical candidates; there is no
  full semantic index of the code. Embedding 873k chunks on one machine is
  hours of work for little gain over BM25 plus re-ranking. Notes are always
  fully embedded.
- A note written in a thread's worktree, against code that hasn't landed,
  reads as STALE from the main checkout until that code lands. That's
  correct, but it can surprise you.
- `gloss sync` merges per note, last writer wins (by its latest
  verify/retire time). Concurrent edits to the *same* note keep one side.

## Work with a friend in 5 minutes

Two people, one private GitHub repo. Heddle syncs through it over hidden
`refs/heddle/*` refs — no server, no daemon, nothing new to host.

```bash
# Both of you, once, in your own clone:
heddle init --verify "npm test"        # whatever green means for the project
heddle sync --remote origin            # remembers the remote for this repo
```

Then work exactly as in the quickstart — lease, edit in your worktree,
stitch, propose — and run `heddle sync` whenever you want to exchange state
(or opt in to `heddle sync --auto` to sync after every stitch/propose):

```bash
you>    heddle lease "dark mode styles" 'style.css'      # + edit, stitch
friend> heddle lease "compact header" 'style.css'        # + edit, stitch
you>    heddle propose && heddle sync                      # lands, publishes
friend> heddle sync        # pulls your weave into their tree
friend> heddle propose     # green — but landing says: fabric moved under you
friend> heddle rebase      # your change fast-forwards in; real conflicts kept
friend> heddle stitch && heddle propose && heddle sync       # merged result lands
you>    heddle sync        # both trees now identical, both changes present
```

Zero merge fear in two sentences: neither of you can overwrite the other,
because edits live in per-thread worktrees and the shared line only advances
through a compare-and-swap ref push that refuses when it moved under you.
The worst case is not a broken tree — it is being told, by name of file,
what to look at before your work lands.

What sync shares, plainly: your heddle metadata (goals, scopes, holders) and
the file content of your stitched scope go to that remote — the same
exposure as pushing a branch there. `--auto` is opt-in per repo. Dead
machines' threads show up as adoptable orphans; claims are first-push-wins
on a git ref, and the loser is told who won.

## Did it earn its keep? (`heddle savings`)

Heddle counts what it prevents instead of asserting it. `heddle savings
[--json]` reports the facts first — collisions warned at lease time, same-file
concurrent edits the isolation absorbed (pair and file names attached), lands
refused because the fabric moved, rebases — then ONE clearly-labeled estimate
whose every constant is either measured from the repo's own data or printed
as a stated assumption beside the number it produced. Feed it ground truth
and the assumption leaves the model:

```bash
heddle savings --record-tokens <thread-id> <tokens>   # from your harness
```

Two things it will never do: turn lease-time *warnings* into a token number
(an agent that self-partitioned saved re-work Heddle cannot honestly
measure), and hide the zero — when nothing collision-shaped ever happened,
the report says *"Heddle has not yet prevented anything measurable in this
repo"* in those words. That sentence is the feature: a tool that can admit
it wasn't needed is one you can believe when it says it was.

## Why not just git merge/rebase?

One concrete breakage. Agents A and B both edit `config.rs` on branches.
Merge time: git sees the same *lines* touched, declares a conflict, and hands
it to whoever merges last — a fresh agent session with no memory of either
intent, reconciling two rewrites token-by-token. Or worse: they touched
*different* lines, git auto-merges silently, and main is red with a breakage
neither author can reproduce alone. Every agent then pulls red main and burns
tokens on a failure that was manufactured by the workflow.

Heddle's answer, point by point:

- The overlap was knowable at lease time; git had nowhere to record it. Heddle
  reports it before either agent spends anything.
- Reconciliation, when it must happen, goes to the *surviving author* (who
  has the context), scoped to named files, against a green tree — not to a
  stranger at merge time against a red one.
- Rebases are made **rare**, not fast: self-partitioning at lease time means
  most collisions never happen.

## Details that keep it honest

- **Landing is consent-gated.** A green verify is evidence, not an action.
  The terminal asks y/N; MCP always refuses the apply; embedding hosts park
  an approval. Nothing reaches the repo tree without a yes.
- **Deletions are first-class.** A file you delete in your worktree is
  recorded as a tombstone, lands as a deletion, and delete-vs-edit collisions
  refuse like any other conflict.
- **Big files are skipped, loudly.** Files over 8 MiB are not snapshotted;
  each one is named in the stitch result. Raise the cap with
  `HEDDLE_MAX_FILE_MB` if you must; prefer a `.heddleignore` at the repo root
  (gitignore-lite: literal dirs and simple globs, one per line) — it extends
  the built-in `.git`/`target`/`node_modules` excludes and cannot un-ignore
  them.
- **`heddle clean`** removes worktrees of woven threads — and refuses if the
  worktree holds anything not captured in a stitch.
- **Verify is whole-repo, in a scratch copy, with a hard timeout.** Red never
  lands; a gate that can't even stage its check reports red, not silence.
- **Rename compatibility (loom-vcs → heddle).** The old `LOOM_DATA` and
  `LOOM_MAX_FILE_MB` env vars are honored as silent fallbacks when the
  `HEDDLE_*` ones are unset, and a `.loomignore` is read when no
  `.heddleignore` exists. If `~/.loom` exists and `~/.heddle` doesn't,
  Heddle keeps using `~/.loom` (with a one-line notice) — data is never
  moved silently; migrate with `mv ~/.loom ~/.heddle` when convenient.

## Git history granularity (`bridge_mode`)

The philosophy: **one lease = one goal = one commit.** Scope leases small and
the bridge's default gives you a semantic git log where every commit is a
goal that verified green. `bridge_mode` exists for teams who want
checkpoint-level history in git itself — set it at `heddle init --bridge-mode
<mode>` or later with `heddle config --bridge-mode <mode>`:

| mode | what lands in git | one-line guidance |
|---|---|---|
| `squash` (default) | one commit per weave: goal + criteria + verify | scope leases small; keep the log semantic |
| `stitches` | every checkpoint as a commit on a `heddle/<thread>-<goal>` branch, then a merge commit carrying the weave message | you want checkpoint-level `git bisect`/review without losing the semantic landing |
| `both` | the squash commit + the per-thread branch preserved, unmerged | clean mainline log, archaeology on the side |

Checkpoint replay is pure git plumbing (a temporary index; `read-tree` →
`hash-object` → `update-index` → `write-tree` → `commit-tree`) — it never
touches your working tree, real index, or current branch; empty-diff
checkpoints are skipped. Nothing is ever pushed, in any mode.

`heddle export [--thread <id>]` writes an **unlanded** thread's checkpoints to
the same per-thread branch — review an agent's in-flight work with plain
`git log -p heddle/<thread>-<goal>`; nothing lands, nothing moves.

## Scale ladder

| rung | status |
|---|---|
| Several agents, one machine | shipped — worktree isolation, leases, green gate, orphans, configurable git bridge + draft-branch export, honest savings metrics, zero-config agent enrollment (72 tests) |
| Memory that survives sessions (`heddle gloss`) | shipped — anchored, versioned notes with honest staleness; inline LLM view; incremental blob-keyed BM25 + local-embedding search, benchmarked on the Linux kernel |
| A few people/machines, one shared git remote | shipped — `heddle sync`: state over `refs/heddle/*`, CAS fabric ref, cross-machine adoption claims; metadata is unsigned (machine ids are identity, not authentication) |
| Team knobs (consent dials, envelopes) | exists in the Aether integration, which embeds this crate; the generic mailbox namespace here is the hook it rides on |
| Many peers, no blessed remote (gossip) | design only — see [docs/DESIGN.md](docs/DESIGN.md), "Federation" |

## Comparison

Against the substrates Heddle builds on or replaces the workflow of:

| | git worktrees alone | Jujutsu | CI-gated trunk | Heddle |
|---|---|---|---|---|
| Per-task isolation | yes | yes (working-copy commits) | no | yes (worktree per thread) |
| Machine-readable intent, collision warning at start | no | no | no | yes (leases + toe-steps) |
| Mainline can't go red | no | no | yes (post-hoc, in CI) | yes (verify before land, local) |
| Refuses to overwrite concurrent work | manual merge | manual resolve | last merge wins | yes (three-way refusal + rebase) |
| Crashed work is claimable with its goal attached | no | no | no | yes (orphans + adoption) |
| Agent-native interface (MCP) | no | no | no | yes |
| Multi-machine without new infra | via remotes, manual | via git remotes, manual | server | any shared git remote (`heddle sync`) |
| Is your git history | yes | yes | yes | yes (bridge: squash, per-checkpoint, or both — never pushed) |

Against the adjacent agent-coordination projects (see
[Adjacent projects](#adjacent-projects) for links; claims below are from
their own docs as of July 2026):

| | what it is | needs a server / control plane? | coordination style | multi-machine without new infra? | always-green invariant? | crash-recovery semantics |
|---|---|---|---|---|---|---|
| aweb | platform (coordination server + identity registry) | yes — server (FastAPI/Postgres/Redis) plus `awid` identity service | mail, chat, tasks, roles, presence, **file locks**; MCP tools | yes, but through its server (team certificates) | not documented | not documented |
| batty | platform (Rust daemon driving tmux agent teams) | yes — persistent daemon | hierarchical dispatch: architects plan, managers route, engineers execute — each engineer in its own worktree | single host (daemon + tmux) | yes — daemon auto-tests completions and merges on green; no agent in the merge path | crash respawn, stall detection (all roles), auto-restart |
| stoneforge | platform (TypeScript web control plane) | yes — local server + web dashboard | dispatch daemon assigns dependency-ordered tasks; Director / Worker / Steward roles; worktree per worker | not addressed — local orchestration | merge steward runs your test command, squash-merges on pass, hands failures to a new worker | event-sourced log; session resumption not yet implemented |
| valkor-ai/loom | delivery harness (local MCP state machine for one agent) | no | single-agent plan → build → test → fix loop; not multi-agent coordination | n/a (per-machine, per-project) | review/repair loop with recorded evidence; no hard merge block | state saved under `.loom/`; resume with `/loom continue` |
| Heddle | protocol + one binary on top of git | no — state lives in your repo and any ordinary git remote | leaderless, **warn-only** intent leases; no dispatcher, no locks | yes — any shared git remote (`heddle sync`) | yes — verify in a scratch copy + consent gate; fabric advances only by CAS ref push | orphans: goal, criteria, seconds-old checkpoint attached; adoptable cross-machine, first CAS claim wins |

Read the columns honestly: the platforms above do things Heddle deliberately
does not — orchestration, role hierarchies, task boards, chat, presence,
automated dispatch. Heddle only coordinates writers and keeps the shared
line green.

## Adjacent projects

- [aweb](https://github.com/awebai/aweb) — a team-coordination platform for
  agents: mail, chat, tasks, file locks, presence, and roles behind a server
  and an identity registry.
- [batty](https://github.com/battysh/batty) — a Rust daemon that runs
  hierarchical agent teams in tmux, with a worktree per engineer and a
  verify-then-auto-merge loop that keeps agents out of the merge path.
- [stoneforge](https://github.com/stoneforge-ai/stoneforge) — a TypeScript
  web control plane with Director/Worker/Steward roles, a dispatch daemon,
  worktree isolation, and test-gated merge review.
- [valkor-ai/loom](https://github.com/valkor-ai/loom) — a delivery harness
  that keeps a single agent on track through a multi-step task (plan, build,
  test, fix) with resumable state between sessions.
- [rjwalters/loom](https://github.com/rjwalters/loom) — orchestration that
  uses your git forge as the coordination layer, driving agents through
  labels on issues and PRs.

Heddle is deliberately the thin layer: a protocol and a binary, not a
platform. If you want orchestration on top, these are good; Heddle aims to
be what they could coordinate through.

## License

MIT — see [LICENSE](LICENSE).

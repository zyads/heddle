//! **Gloss** — notes for the next agent, anchored to the code they describe.
//!
//! A gloss is the marginal note a scribe wrote beside the text. Here it is
//! what an agent learned about a piece of code — *why* it is shaped this way,
//! the invariant that is not written down, the approach that already failed
//! — pinned to a symbol or line span, versioned, and honest about rot.
//!
//! **Why not a shadow branch full of comments?** Writing notes into a copy of
//! the code and stripping them for humans needs a per-language stripper,
//! cannot tell an agent's note from a human's comment, and forces a 3-way
//! re-placement every time a human edits the mirror. A sidecar has none of
//! those problems: the human tree is never touched, so there is nothing to
//! strip, and the "LLM copy" is rendered on read ([`read`]) or, for tools
//! that only read plain files, materialized on demand ([`materialize`]).
//!
//! **Why not CLAUDE.md / memory files?** They are unanchored (they do not
//! know which code they describe), unversioned (no record of what was true
//! when), and they never go stale — they go *wrong*, silently. A gloss knows
//! its span; when the code under it changes it says STALE until someone
//! verifies or drops it.
//!
//! ```text
//! refs/gloss/notes        commit chain; tree = files/<repo path> → JSON [Note]
//!                         every add/verify/drop is one commit: the ref's log
//!                         IS the temporal history
//! refs/gloss/llm          (optional) materialized tree with notes inlined
//! <data dir>/gloss/<id>/  rebuildable cache: code index, embeddings
//! ```
//!
//! **Privacy.** The refs live outside `refs/heads`, `refs/tags` and
//! `refs/heddle/*` — so an ordinary `git push`, a clone, and `heddle sync`
//! (which fetches `refs/heddle/*` with `--prune`, and would delete anything
//! local it does not see remotely) never touch them. Only `git push
//! --mirror` or an explicit `refs/*` refspec would carry them. Sharing
//! notes is an explicit `heddle gloss sync <remote>`, which refuses a
//! world-readable remote. Notes are scanned for secrets before they are
//! stored ([`secrets`]); embeddings only ever talk to a loopback Ollama.

pub mod anchor;
pub mod deadend;
pub mod embed;
pub mod index;
pub mod render;
pub mod secrets;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use anchor::{Anchor, Resolution, Status};

pub const NOTES_REF: &str = "refs/gloss/notes";
pub const LLM_REF: &str = "refs/gloss/llm";
pub const MAX_NOTE_CHARS: usize = 4000;
pub const MAX_NOTES_PER_FILE: usize = 300;
/// Retired notes kept per file for `why` — history beyond this is still in
/// the ref's commit log.
pub const MAX_RETIRED_PER_FILE: usize = 50;
pub const KINDS: &[&str] = &["note", "why", "invariant", "gotcha", "todo", "deadend"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Author {
    /// Who wrote it: an agent name, `$USER`, or `user (mcp agent)`.
    pub by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Verification {
    pub commit: String,
    pub at: u64,
    pub by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Retired {
    pub at: u64,
    pub by: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Note {
    pub id: String,
    pub kind: String,
    pub text: String,
    pub anchor: Anchor,
    pub author: Author,
    /// Commit the note was written against (HEAD of the tree it was written in).
    pub commit: String,
    /// The anchored file differed from `commit` when written (uncommitted work).
    #[serde(default)]
    pub dirty: bool,
    pub created_at: u64,
    /// Last change of any kind (add, verify, move, drop, re-anchor): what
    /// `sync` compares to keep the newer copy of a note.
    #[serde(default)]
    pub updated_at: u64,
    /// Commit the anchor was last placed at. Rename detection diffs from
    /// here, so a note that already followed one rename can follow the next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchored_at: Option<String>,
    /// Each time someone confirmed the note still holds (newest last, capped).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verified: Vec<Verification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired: Option<Retired>,
    /// Notes that are one fact pinned to several sites (a dead end that
    /// touched three functions) share a group id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// What an agent sees of a note: no fingerprints (an anchor's line hashes
/// are hundreds of numbers of pure token waste), a readable anchor label,
/// and the trust state when known.
#[derive(Debug, Clone, Serialize)]
pub struct NoteView {
    pub id: String,
    pub kind: String,
    pub text: String,
    pub anchor: String,
    pub by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    pub commit: String,
    pub written_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified_at_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired: Option<Retired>,
}

impl NoteView {
    pub fn of(n: &Note, r: Option<&Resolution>) -> NoteView {
        let short = |c: &str| short(c, 12);
        NoteView {
            id: n.id.clone(),
            kind: n.kind.clone(),
            text: n.text.clone(),
            anchor: n.anchor.label(),
            by: n.author.by.clone(),
            thread: n.author.thread.clone(),
            commit: short(&n.commit),
            written_at: n.created_at,
            verified_at_commit: n.verified.last().map(|v| short(&v.commit)),
            status: r.map(|r| r.status.word()),
            lines: r.filter(|r| r.start > 0).map(|r| format!("{}-{}", r.start, r.end)),
            retired: n.retired.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Repo handle + git plumbing
// ---------------------------------------------------------------------------

/// A git working tree (main checkout or a heddle thread's worktree — they
/// share refs, so they share notes).
#[derive(Debug, Clone)]
pub struct Repo {
    /// Top of the working tree; note paths are relative to it.
    pub root: PathBuf,
    /// Shared `.git` dir (same for every worktree of the repo).
    pub common_dir: PathBuf,
}

impl Repo {
    pub fn open(dir: impl AsRef<Path>) -> Result<Repo, String> {
        let dir = dir.as_ref();
        let root = git(dir, &["rev-parse", "--show-toplevel"])
            .map_err(|_| format!("{} is not inside a git repository — gloss anchors to git history", dir.display()))?;
        let common = git(dir, &["rev-parse", "--git-common-dir"])?;
        let root = PathBuf::from(root.trim());
        let mut common_dir = PathBuf::from(common.trim());
        if common_dir.is_relative() {
            common_dir = dir.join(common_dir);
        }
        let common_dir = common_dir.canonicalize().unwrap_or(common_dir);
        Ok(Repo { root, common_dir })
    }

    /// Rebuildable cache dir: shared by every worktree of this repo.
    pub fn cache_dir(&self, data_dir: &Path) -> PathBuf {
        let key = crate::repo_id_for(&self.common_dir.to_string_lossy());
        data_dir.join("gloss").join(key)
    }

    pub fn head(&self) -> String {
        git(&self.root, &["rev-parse", "HEAD"]).map(|s| s.trim().to_string()).unwrap_or_default()
    }

    fn git(&self, args: &[&str]) -> Result<String, String> {
        git(&self.root, args)
    }

    /// Repo-relative, `/`-separated path for a user-supplied path (relative
    /// to the cwd, relative to the root, or absolute).
    pub fn rel(&self, p: &str) -> Result<String, String> {
        let pb = Path::new(p);
        let candidates = [
            if pb.is_absolute() { pb.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(pb) },
            self.root.join(pb),
        ];
        let root = self.root.canonicalize().unwrap_or(self.root.clone());
        for c in candidates {
            if let Ok(abs) = c.canonicalize() {
                if let Ok(r) = abs.strip_prefix(&root) {
                    return Ok(r.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        // Not on disk (deleted, or a path from history): accept it as given
        // if it is plainly repo-relative.
        if !pb.is_absolute() && !p.contains("..") {
            return Ok(p.trim_start_matches("./").to_string());
        }
        Err(format!("{p} is not inside {}", self.root.display()))
    }

    /// `path` as committed at HEAD (not the working tree).
    pub fn head_content(&self, path: &str) -> Option<String> {
        self.git(&["show", &format!("HEAD:{path}")]).ok()
    }

    /// Current content of `path` in this working tree (falls back to HEAD
    /// for files not on disk).
    pub fn content(&self, path: &str) -> Option<String> {
        match std::fs::read(self.root.join(path)) {
            Ok(b) => Some(String::from_utf8_lossy(&b).into_owned()),
            Err(_) => self.git(&["show", &format!("HEAD:{path}")]).ok(),
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    git_io(dir, args, None, &[])
}

/// One git call; `stdin` bytes are fed from a helper thread (no pipe
/// deadlock on big batches), `env` extends the environment.
fn git_io(dir: &Path, args: &[&str], stdin: Option<&[u8]>, env: &[(&str, &str)]) -> Result<String, String> {
    git_bytes(dir, args, stdin, env).map(|b| String::from_utf8_lossy(&b).into_owned())
}

fn git_bytes(dir: &Path, args: &[&str], stdin: Option<&[u8]>, env: &[(&str, &str)]) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args).envs(env.iter().copied());
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("cannot run git: {e}"))?;
    let feeder = stdin.map(|b| {
        let mut pipe = child.stdin.take().expect("piped");
        let b = b.to_vec();
        std::thread::spawn(move || {
            let _ = pipe.write_all(&b);
        })
    });
    let out = child.wait_with_output().map_err(|e| format!("git: {e}"))?;
    if let Some(f) = feeder {
        let _ = f.join();
    }
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

/// Many objects in one `git cat-file --batch`: `None` for a missing one.
/// Byte-exact (sizes are bytes; source files are not always UTF-8).
fn cat_batch(repo: &Repo, specs: &[String]) -> Vec<Option<Vec<u8>>> {
    if specs.is_empty() {
        return vec![];
    }
    let input: String = specs.iter().map(|s| format!("{s}\n")).collect();
    let Ok(bytes) = git_bytes(&repo.root, &["cat-file", "--batch"], Some(input.as_bytes()), &[]) else {
        return vec![None; specs.len()];
    };
    let mut out = Vec::with_capacity(specs.len());
    let mut pos = 0;
    for _ in specs {
        let Some(nl) = bytes[pos.min(bytes.len())..].iter().position(|&b| b == b'\n') else {
            out.push(None);
            continue;
        };
        let head = String::from_utf8_lossy(&bytes[pos..pos + nl]).into_owned();
        pos += nl + 1;
        if head.ends_with(" missing") || head.ends_with(" ambiguous") {
            out.push(None);
            continue;
        }
        let size: usize = head.split_whitespace().nth(2).and_then(|s| s.parse().ok()).unwrap_or(0);
        let end = (pos + size).min(bytes.len());
        out.push(Some(bytes[pos..end].to_vec()));
        pos = end + 1;
    }
    out
}

/// Commits on the notes ref carry a neutral identity: notes are about code,
/// not about whoever's git config happened to be loaded, and a synced note
/// should not leak an email address.
const GLOSS_IDENTITY: &[(&str, &str)] = &[
    ("GIT_AUTHOR_NAME", "heddle gloss"),
    ("GIT_AUTHOR_EMAIL", "gloss@localhost"),
    ("GIT_COMMITTER_NAME", "heddle gloss"),
    ("GIT_COMMITTER_EMAIL", "gloss@localhost"),
];

/// A private index file per write: unique across processes (pid) AND threads
/// of one process (counter) — two writers sharing a temp index would trip
/// over each other's `.lock`.
fn temp_index(repo: &Repo, tag: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    repo.common_dir.join(format!("gloss-{tag}-{}-{n}", std::process::id()))
}

fn tree_path(path: &str) -> String {
    format!("files/{path}")
}

/// All notes (live and retired) stored for one path.
pub fn load_path(repo: &Repo, path: &str) -> Vec<Note> {
    repo.git(&["cat-file", "blob", &format!("{NOTES_REF}:{}", tree_path(path))])
        .ok()
        .and_then(|b| serde_json::from_str(&b).ok())
        .unwrap_or_default()
}

/// Every path's notes, in one `ls-tree` + one `cat-file --batch`.
pub fn load_all(repo: &Repo) -> BTreeMap<String, Vec<Note>> {
    load_all_at(repo, NOTES_REF)
}

fn load_all_at(repo: &Repo, rev: &str) -> BTreeMap<String, Vec<Note>> {
    let mut out = BTreeMap::new();
    let Ok(ls) = repo.git(&["ls-tree", "-r", "-z", rev, "--", "files/"]) else { return out };
    let entries: Vec<(String, String)> = ls
        .split('\0')
        .filter_map(|e| {
            let (meta, path) = e.split_once('\t')?;
            let oid = meta.split_whitespace().nth(2)?;
            Some((oid.to_string(), path.strip_prefix("files/")?.to_string()))
        })
        .collect();
    if entries.is_empty() {
        return out;
    }
    let ids: Vec<String> = entries.iter().map(|(o, _)| o.clone()).collect();
    for ((_, path), body) in entries.iter().zip(cat_batch(repo, &ids)) {
        if let Some(notes) = body.and_then(|b| serde_json::from_slice::<Vec<Note>>(&b).ok()) {
            out.insert(path.clone(), notes);
        }
    }
    out
}

/// Apply `f` to the notes of `paths` and commit the result to the notes
/// ref, compare-and-swap: if another agent moved the ref in between, reload
/// and re-apply (up to a few times). `f` may add entries for other paths
/// (renames); an empty list removes the path.
pub fn mutate<F>(repo: &Repo, paths: &[String], msg: &str, mut f: F) -> Result<(), String>
where
    F: FnMut(&mut BTreeMap<String, Vec<Note>>) -> Result<(), String>,
{
    for attempt in 0..40u32 {
        if attempt > 0 {
            // Jittered backoff: N writers retrying in lockstep would just
            // collide again. Jitter from the clock's low bits — no rand crate.
            let jitter = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0)
                ^ std::process::id().wrapping_mul(2_654_435_761)) as u64;
            let cap = 2u64.pow(attempt.min(6)) * 4; // 4ms … 256ms
            std::thread::sleep(std::time::Duration::from_millis(1 + jitter % cap));
        }
        let old = repo.git(&["rev-parse", "--verify", "-q", NOTES_REF]).ok().map(|s| s.trim().to_string());
        let mut set: BTreeMap<String, Vec<Note>> = paths.iter().map(|p| (p.clone(), load_path(repo, p))).collect();
        let before = set.clone();
        f(&mut set)?;
        if set == before {
            return Ok(());
        }
        let tmp_index = temp_index(repo, "index");
        let ix = tmp_index.to_string_lossy().to_string();
        let env = [("GIT_INDEX_FILE", ix.as_str())];
        let result = (|| -> Result<String, String> {
            if let Some(o) = &old {
                git_io(&repo.root, &["read-tree", o], None, &env)?;
            } else {
                git_io(&repo.root, &["read-tree", "--empty"], None, &env)?;
            }
            let mut info = String::new();
            for (path, notes) in &set {
                if before.get(path) == Some(notes) {
                    continue;
                }
                if notes.is_empty() {
                    // Mode 0 removes the path; the oid must still be the repo's
                    // hash length (sha256 repos reject a 40-char one).
                    let zero = "0".repeat(old.as_ref().map_or(40, |o| o.len()));
                    info.push_str(&format!("0 {zero}\t{}\n", tree_path(path)));
                } else {
                    let json = serde_json::to_string_pretty(notes).map_err(|e| e.to_string())?;
                    let oid = git_io(&repo.root, &["hash-object", "-w", "--stdin"], Some(json.as_bytes()), &[])?;
                    info.push_str(&format!("100644 {}\t{}\n", oid.trim(), tree_path(path)));
                }
            }
            git_io(&repo.root, &["update-index", "--index-info"], Some(info.as_bytes()), &env)?;
            let tree = git_io(&repo.root, &["write-tree"], None, &env)?;
            let mut args = vec!["commit-tree", tree.trim(), "-m", msg];
            if let Some(o) = &old {
                args.extend(["-p", o.as_str()]);
            }
            Ok(git_io(&repo.root, &args, None, GLOSS_IDENTITY)?.trim().to_string())
        })();
        let _ = std::fs::remove_file(&tmp_index);
        let new = result?;
        let zero = "0".repeat(new.len());
        let expect = old.clone().unwrap_or(zero);
        if repo.git(&["update-ref", "-m", msg, NOTES_REF, &new, &expect]).is_ok() {
            return Ok(());
        }
        // Lost the race: someone else wrote notes. Reload and re-apply.
    }
    Err("notes ref kept moving under us (40 attempts) — too many concurrent writers; try again".into())
}

// ---------------------------------------------------------------------------
// Targets: `path`, `path:12`, `path:12-30`, `path::Sym::name`
// ---------------------------------------------------------------------------

/// First `n` chars of a commit id — by chars, since ids can arrive from a
/// synced peer's JSON and must never panic a byte slice.
pub fn short(c: &str, n: usize) -> String {
    c.chars().take(n).collect()
}

pub fn now_ms() -> u64 {
    crate::now_ms()
}

/// Parse a target spec into (repo path, anchor) against the current content.
pub fn target(repo: &Repo, spec: &str) -> Result<Anchor, String> {
    let (file, sym, lines) = if let Some((f, s)) = spec.split_once("::") {
        (f, Some(s), None)
    } else if let Some((f, l)) = spec.rsplit_once(':').filter(|(_, l)| l.chars().all(|c| c.is_ascii_digit() || c == '-') && !l.is_empty()) {
        let (a, b) = match l.split_once('-') {
            Some((a, b)) => (a.parse::<usize>().ok(), b.parse::<usize>().ok()),
            None => (l.parse().ok(), l.parse().ok()),
        };
        match (a, b) {
            (Some(a), Some(b)) if a >= 1 && b >= a => (f, None, Some((a, b))),
            _ => return Err(format!("bad line range in '{spec}' — use path:12 or path:12-30")),
        }
    } else {
        (spec, None, None)
    };
    let path = repo.rel(file)?;
    let content = repo.content(&path).ok_or_else(|| format!("{path}: no such file in the working tree or HEAD"))?;
    match (sym, lines) {
        (Some(s), _) => {
            let syms = anchor::outline(&path, &content);
            let found = syms.iter().find(|x| x.path == s).or_else(|| {
                // `stitch` alone is fine when unambiguous.
                let tail: Vec<_> = syms.iter().filter(|x| x.path.rsplit("::").next() == Some(s)).collect();
                (tail.len() == 1).then(|| tail[0])
            });
            let Some(found) = found else {
                let near: Vec<String> = syms.iter().filter(|x| x.path.contains(s)).take(8).map(|x| x.path.clone()).collect();
                return Err(if near.is_empty() {
                    format!("no symbol '{s}' in {path} (use path:LINE-LINE for a line range)")
                } else {
                    format!("no symbol '{s}' in {path}; did you mean: {}", near.join(", "))
                });
            };
            Ok(anchor::anchor_span(&path, &content, found.start, found.end, Some(found.path.clone())))
        }
        (None, Some((a, b))) => {
            let n = content.lines().count();
            if a > n {
                return Err(format!("{path} has {n} lines; line {a} does not exist"));
            }
            let anc = anchor::anchor_span(&path, &content, a, b.min(n), None);
            if anc.lines.is_empty() {
                return Err(format!("{path}:{a}-{b} is blank — target the code the note is about"));
            }
            Ok(anc)
        }
        (None, None) => Ok(anchor::anchor_file(&path)),
    }
}

// ---------------------------------------------------------------------------
// The verbs
// ---------------------------------------------------------------------------

fn note_id(seed: &str) -> String {
    let h = crate::store::content_hash(format!("{seed}{}{}", now_ms(), std::process::id()).as_bytes());
    // 12 hex = 48 bits: a collision (which would make an id ambiguous
    // forever) stays negligible even at millions of notes.
    format!("g{}", &h[..12])
}

/// Does the file on disk differ from HEAD? Compared by blob hash rather
/// than `git diff`, which refreshes (and briefly locks) the human's index.
fn file_dirty(repo: &Repo, path: &str) -> bool {
    let disk = repo.git(&["hash-object", "--", path]);
    let head = repo.git(&["rev-parse", "-q", "--verify", &format!("HEAD:{path}")]);
    match (disk, head) {
        (Ok(d), Ok(h)) => d.trim() != h.trim(),
        (Ok(_), Err(_)) => true, // not committed yet
        _ => false,              // not on disk: reading HEAD's copy
    }
}

/// Add a note at `spec`. Refuses secrets, empty text, unknown kinds, and
/// exact duplicates (same anchor, same text).
pub fn add(repo: &Repo, spec: &str, text: &str, kind: &str, author: Author) -> Result<Note, String> {
    add_grouped(repo, spec, text, kind, author, None)
}

/// [`add`], tagging the note as one site of a multi-site fact.
pub fn add_grouped(repo: &Repo, spec: &str, text: &str, kind: &str, author: Author, group: Option<String>) -> Result<Note, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("empty note".into());
    }
    if text.chars().count() > MAX_NOTE_CHARS {
        return Err(format!("note is over {MAX_NOTE_CHARS} chars — notes are margin notes, not documents"));
    }
    if !KINDS.contains(&kind) {
        return Err(format!("unknown kind '{kind}' (one of: {})", KINDS.join(", ")));
    }
    if let Some(why) = secrets::scan(text) {
        return Err(format!("refused: the note contains {why}. Notes live in git history forever — reword without the secret."));
    }
    let anchor = target(repo, spec)?;
    let path = anchor.path.clone();
    let note = Note {
        id: note_id(&format!("{spec}{text}")),
        kind: kind.to_string(),
        text: text.to_string(),
        dirty: file_dirty(repo, &path),
        commit: repo.head(),
        created_at: now_ms(),
        updated_at: now_ms(),
        anchored_at: None,
        verified: vec![],
        retired: None,
        group,
        author,
        anchor,
    };
    let mut stored = note.clone();
    mutate(repo, std::slice::from_ref(&path), &format!("gloss add {} {}", note.id, note.anchor.label()), |set| {
        let list = set.entry(path.clone()).or_default();
        if let Some(dup) = list.iter().find(|n| n.retired.is_none() && n.anchor == note.anchor && n.text == note.text) {
            stored = dup.clone();
            return Ok(());
        }
        if list.iter().filter(|n| n.retired.is_none()).count() >= MAX_NOTES_PER_FILE {
            return Err(format!("{path} already has {MAX_NOTES_PER_FILE} live notes — drop stale ones first"));
        }
        list.push(note.clone());
        Ok(())
    })?;
    Ok(stored)
}

/// Where each live note on `path` lands in the current working tree.
pub fn resolved(repo: &Repo, path: &str, content: &str) -> Vec<(Note, Resolution)> {
    let mut v: Vec<(Note, Resolution)> = load_path(repo, path)
        .into_iter()
        .filter(|n| n.retired.is_none())
        .map(|n| {
            let r = anchor::resolve(&n.anchor, content);
            (n, r)
        })
        .collect();
    v.sort_by_key(|(_, r)| r.start);
    v
}

/// The file with its notes interleaved as comments — the LLM's view.
pub fn read(repo: &Repo, path: &str, opts: &render::RenderOpts) -> Result<(String, Vec<(Note, Resolution)>), String> {
    let path = repo.rel(path)?;
    let content = repo.content(&path).ok_or_else(|| format!("{path}: no such file"))?;
    let notes = resolved(repo, &path, &content);
    Ok((render::render(&path, &content, &notes, opts), notes))
}

/// Find a note by id (or unique id prefix) across all paths.
pub fn find(repo: &Repo, id: &str) -> Result<(String, Note), String> {
    let all = load_all(repo);
    let hits: Vec<(String, Note)> = all
        .into_iter()
        .flat_map(|(p, ns)| ns.into_iter().map(move |n| (p.clone(), n)))
        .filter(|(_, n)| n.id.starts_with(id))
        .collect();
    match hits.len() {
        0 => Err(format!("no gloss note '{id}'")),
        1 => Ok(hits.into_iter().next().unwrap()),
        _ => Err(format!("'{id}' is ambiguous ({} notes) — use more of the id", hits.len())),
    }
}

/// Confirm a note still holds for the current code: re-anchor it to where
/// it resolves now (rebaselining the span hash) and record the verification.
/// Optionally replace its text in the same step.
pub fn verify(repo: &Repo, id: &str, by: &str, new_text: Option<&str>) -> Result<Note, String> {
    let (path, note) = find(repo, id)?;
    if note.retired.is_some() {
        return Err(format!("{} was dropped; add a new note instead", note.id));
    }
    if let Some(t) = new_text {
        if let Some(why) = secrets::scan(t) {
            return Err(format!("refused: the new text contains {why}"));
        }
    }
    let content = repo.content(&path).ok_or_else(|| format!("{path}: no such file"))?;
    let r = anchor::resolve(&note.anchor, &content);
    if r.status == Status::Orphaned {
        return Err(format!(
            "{} is orphaned — the code it described is gone. Re-target it with `heddle gloss move {} <path:lines|path::Symbol>` or drop it.",
            note.id, note.id
        ));
    }
    let fresh_anchor = if note.anchor.is_file_level() {
        note.anchor.clone()
    } else {
        anchor::anchor_span(&path, &content, r.start, r.end, note.anchor.symbol.clone())
    };
    let commit = repo.head();
    let mut out = note.clone();
    mutate(repo, std::slice::from_ref(&path), &format!("gloss verify {}", note.id), |set| {
        let list = set.entry(path.clone()).or_default();
        let n = list.iter_mut().find(|n| n.id == note.id).ok_or("note vanished mid-update")?;
        n.anchor = fresh_anchor.clone();
        if let Some(t) = new_text {
            n.text = t.trim().to_string();
        }
        n.dirty = file_dirty(repo, &path);
        n.updated_at = now_ms();
        n.anchored_at = Some(commit.clone());
        n.verified.push(Verification { commit: commit.clone(), at: now_ms(), by: by.to_string() });
        if n.verified.len() > 10 {
            n.verified.remove(0);
        }
        out = n.clone();
        Ok(())
    })?;
    Ok(out)
}

/// Re-target a note to a new spec (keeps its id, history and author).
pub fn move_note(repo: &Repo, id: &str, spec: &str, by: &str) -> Result<Note, String> {
    let (old_path, note) = find(repo, id)?;
    let anchor = target(repo, spec)?;
    let new_path = anchor.path.clone();
    let commit = repo.head();
    let mut out = note.clone();
    mutate(repo, &[old_path.clone(), new_path.clone()], &format!("gloss move {} {}", note.id, anchor.label()), |set| {
        let mut n = set
            .get_mut(&old_path)
            .and_then(|l| l.iter().position(|n| n.id == note.id).map(|i| l.remove(i)))
            .ok_or("note vanished mid-update")?;
        n.anchor = anchor.clone();
        n.updated_at = now_ms();
        n.anchored_at = Some(commit.clone());
        n.verified.push(Verification { commit: commit.clone(), at: now_ms(), by: by.to_string() });
        out = n.clone();
        set.entry(new_path.clone()).or_default().push(n);
        Ok(())
    })?;
    Ok(out)
}

/// Retire a note. It disappears from reads but stays in `why` and in the
/// ref's history — knowledge that was once true is still history.
pub fn drop_note(repo: &Repo, id: &str, by: &str, reason: &str) -> Result<Note, String> {
    let (path, note) = find(repo, id)?;
    let mut out = note.clone();
    mutate(repo, std::slice::from_ref(&path), &format!("gloss drop {}", note.id), |set| {
        let list = set.entry(path.clone()).or_default();
        let n = list.iter_mut().find(|n| n.id == note.id).ok_or("note vanished mid-update")?;
        n.updated_at = now_ms();
        n.retired = Some(Retired { at: now_ms(), by: by.to_string(), reason: crate::cap(reason, 300) });
        out = n.clone();
        // Bound the retired tail; the commit log keeps the rest.
        let retired: Vec<usize> = list.iter().enumerate().filter(|(_, n)| n.retired.is_some()).map(|(i, _)| i).collect();
        if retired.len() > MAX_RETIRED_PER_FILE {
            let cut = retired[0];
            list.remove(cut);
        }
        Ok(())
    })?;
    Ok(out)
}

/// One note's state in the current tree, for `check` / `list`.
#[derive(Debug, Clone, Serialize)]
pub struct Checked {
    pub path: String,
    pub note: Note,
    pub resolution: Resolution,
    /// Set when the file was renamed and the note followed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renamed_to: Option<String>,
}

/// Renames since `commit`: old path → new path (git's own -M detection).
fn renames_since(repo: &Repo, commit: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    if commit.is_empty() {
        return m;
    }
    if let Ok(out) = repo.git(&["diff", "--name-status", "-M", "--diff-filter=R", "-z", commit, "HEAD"]) {
        let parts: Vec<&str> = out.split('\0').filter(|s| !s.is_empty()).collect();
        let mut i = 0;
        while i < parts.len() {
            if parts[i].starts_with('R') && i + 2 < parts.len() {
                m.insert(parts[i + 1].to_string(), parts[i + 2].to_string());
                i += 3;
            } else {
                i += 1;
            }
        }
    }
    m
}

/// Resolve every live note (optionally under a path prefix). With `fix`,
/// persist what is safe to persist: moved anchors get their new lines, and
/// notes on renamed files follow the rename. Stale notes are NEVER
/// auto-verified — only an agent or human who read the code can say a note
/// still holds.
pub fn check(repo: &Repo, prefix: Option<&str>, fix: bool) -> Result<Vec<Checked>, String> {
    let all = load_all(repo);
    let mut out = Vec::new();
    let mut rename_cache: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (path, notes) in &all {
        if prefix.is_some_and(|p| !path.starts_with(p)) {
            continue;
        }
        let content = std::fs::read(repo.root.join(path)).ok().map(|b| String::from_utf8_lossy(&b).into_owned());
        for n in notes.iter().filter(|n| n.retired.is_none()) {
            let (res, renamed_to) = match &content {
                Some(c) => (anchor::resolve(&n.anchor, c), None),
                None => {
                    let base = n.anchored_at.clone().unwrap_or_else(|| n.commit.clone());
                    let renames = rename_cache.entry(base.clone()).or_insert_with(|| renames_since(repo, &base));
                    match renames.get(path).and_then(|np| repo.content(np).map(|c| (np.clone(), c))) {
                        Some((np, c)) => {
                            let mut r = anchor::resolve(&n.anchor, &c);
                            if r.status == Status::Fresh {
                                r.status = Status::Moved { from: n.anchor.start };
                            }
                            (r, Some(np))
                        }
                        None => (Resolution { status: Status::Orphaned, start: n.anchor.start, end: n.anchor.end }, None),
                    }
                }
            };
            out.push(Checked { path: path.clone(), note: n.clone(), resolution: res, renamed_to });
        }
    }
    if fix {
        let head = repo.head();
        let movable: Vec<&Checked> = out
            .iter()
            .filter(|c| matches!(c.resolution.status, Status::Moved { .. }) || (c.renamed_to.is_some() && c.resolution.status.trustworthy()))
            .collect();
        if !movable.is_empty() {
            let mut paths: Vec<String> = movable.iter().map(|c| c.path.clone()).collect();
            paths.extend(movable.iter().filter_map(|c| c.renamed_to.clone()));
            paths.sort();
            paths.dedup();
            mutate(repo, &paths, &format!(
                    "gloss check --fix: re-anchored {}",
                    movable.iter().map(|c| c.note.id.as_str()).collect::<Vec<_>>().join(" ")
                ), |set| {
                for c in &movable {
                    let dest = c.renamed_to.clone().unwrap_or(c.path.clone());
                    let Some(content) = repo.content(&dest) else { continue };
                    let Some(list) = set.get_mut(&c.path) else { continue };
                    let Some(i) = list.iter().position(|n| n.id == c.note.id) else { continue };
                    let mut n = list.remove(i);
                    n.updated_at = now_ms();
                    n.anchored_at = Some(head.clone());
                    n.anchor = if n.anchor.is_file_level() {
                        anchor::anchor_file(&dest)
                    } else {
                        let mut a = anchor::anchor_span(&dest, &content, c.resolution.start, c.resolution.end, n.anchor.symbol.clone());
                        // Moved means content-identical: keep the original
                        // fingerprint so a later edit is judged against
                        // what the note was written about.
                        a.span_hash = n.anchor.span_hash.clone();
                        a.lines = n.anchor.lines.clone();
                        a
                    };
                    set.entry(dest).or_default().push(n);
                }
                Ok(())
            })?;
        }
    }
    Ok(out)
}

/// "Why is this like this?" for a line or span: every note that ever
/// covered it (live first, then retired — with who retired it and why), and
/// the commits that touched those lines.
#[derive(Debug)]
pub struct Why {
    pub path: String,
    pub start: usize,
    pub end: usize,
    pub notes: Vec<(Note, Resolution)>,
    pub retired: Vec<Note>,
    /// `git log -L` over the span: "<short sha> <date> <subject>".
    pub commits: Vec<String>,
}

impl Why {
    /// Compact JSON for agents (see [`NoteView`]).
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "path": self.path,
            "lines": format!("{}-{}", self.start, self.end),
            "notes": self.notes.iter().map(|(n, r)| NoteView::of(n, Some(r))).collect::<Vec<_>>(),
            "retired": self.retired.iter().map(|n| NoteView::of(n, None)).collect::<Vec<_>>(),
            "commits": self.commits,
        })
    }
}

pub fn why(repo: &Repo, spec: &str) -> Result<Why, String> {
    let a = target(repo, spec)?;
    let content = repo.content(&a.path).unwrap_or_default();
    let (start, end) = if a.is_file_level() { (1, content.lines().count().max(1)) } else { (a.start, a.end) };
    let overlaps = |s: usize, e: usize| s == 0 || (s <= end && e >= start);
    let all = load_path(repo, &a.path);
    let mut notes = Vec::new();
    let mut retired = Vec::new();
    for n in all {
        if n.retired.is_some() {
            if overlaps(n.anchor.start, n.anchor.end) {
                retired.push(n);
            }
            continue;
        }
        let r = anchor::resolve(&n.anchor, &content);
        if overlaps(if r.status == Status::Orphaned { 0 } else { r.start }, r.end) {
            notes.push((n, r));
        }
    }
    let commits = repo
        .git(&["log", "--no-patch", "-n", "12", "--date=short", "--format=%h %ad %s", &format!("-L{start},{end}:{}", a.path)])
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).map(String::from).collect())
        .unwrap_or_default();
    Ok(Why { path: a.path, start, end, notes, retired, commits })
}

/// The ref log for one note: every commit that touched it, oldest first.
pub fn history(repo: &Repo, id: &str) -> Vec<String> {
    repo.git(&["log", "--reverse", "--date=iso", "--format=%h %ad %s", NOTES_REF, "--grep", id])
        .map(|s| s.lines().map(String::from).collect())
        .unwrap_or_default()
}

/// Write a shadow tree — HEAD with every annotated file rendered with its
/// notes inlined — as a commit on [`LLM_REF`], for tools that can only read
/// plain files. Only annotated files are re-rendered; every other blob is
/// reused as-is, so cost scales with notes, not repo size. Returns the
/// commit and how many files carry notes. Never touches the human tree,
/// index or branches.
pub fn materialize(repo: &Repo) -> Result<(String, usize), String> {
    let head = repo.head();
    if head.is_empty() {
        return Err("no HEAD commit to materialize from".into());
    }
    let all = load_all(repo);
    let tmp_index = temp_index(repo, "llm-index");
    let ix = tmp_index.to_string_lossy().to_string();
    let env = [("GIT_INDEX_FILE", ix.as_str())];
    let res = (|| -> Result<(String, usize), String> {
        git_io(&repo.root, &["read-tree", &head], None, &env)?;
        // Batched end to end — one cat-file for every annotated file's
        // content, one hash-object for every rendered result — so cost is
        // three git processes total, not three per file.
        // Paths with a newline would break cat-file/index-info framing; such
        // files are left as HEAD has them.
        let paths: Vec<&String> = all
            .iter()
            .filter(|(p, ns)| !p.contains('\n') && ns.iter().any(|n| n.retired.is_none()))
            .map(|(p, _)| p)
            .collect();
        let specs: Vec<String> = paths.iter().map(|p| format!("{head}:{p}")).collect();
        let stage = temp_index(repo, "llm-stage");
        std::fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
        let mut staged: Vec<(&String, PathBuf)> = Vec::new();
        let stage_result = (|| -> Result<(), String> {
        for (i, (path, body)) in paths.iter().zip(cat_batch(repo, &specs)).enumerate() {
            let Some(body) = body else { continue }; // annotated but not in HEAD (uncommitted file)
            let content = String::from_utf8_lossy(&body);
            let live: Vec<(Note, Resolution)> = all[*path]
                .iter()
                .filter(|n| n.retired.is_none())
                .map(|n| (n.clone(), anchor::resolve(&n.anchor, &content)))
                .collect();
            let text = render::render(path, &content, &live, &render::RenderOpts { numbers: false, show_untrusted: true });
            let f = stage.join(i.to_string());
            std::fs::write(&f, text).map_err(|e| e.to_string())?;
            staged.push((path, f));
        }
        Ok(())
        })();
        if let Err(e) = stage_result {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(e);
        }
        let list: String = staged.iter().map(|(_, f)| format!("{}\n", f.display())).collect();
        let oids = git_io(&repo.root, &["hash-object", "-w", "--no-filters", "--stdin-paths"], Some(list.as_bytes()), &[]);
        let _ = std::fs::remove_dir_all(&stage);
        let oids = oids?;
        // Keep each file's mode (an executable script stays executable).
        let modes: BTreeMap<String, String> = git_io(&repo.root, &["ls-files", "-s", "-z"], None, &env)?
            .split('\0')
            .filter_map(|e| {
                let (meta, p) = e.split_once('\t')?;
                Some((p.to_string(), meta.split_whitespace().next()?.to_string()))
            })
            .filter(|(p, _)| all.contains_key(p))
            .collect();
        let mut info = String::new();
        let files = staged.len();
        for ((path, _), oid) in staged.iter().zip(oids.lines()) {
            let mode = modes.get(*path).map(String::as_str).unwrap_or("100644");
            info.push_str(&format!("{mode} {}\t{path}\n", oid.trim()));
        }
        git_io(&repo.root, &["update-index", "--index-info"], Some(info.as_bytes()), &env)?;
        let tree = git_io(&repo.root, &["write-tree"], None, &env)?;
        let msg = format!("gloss: {head} with {files} annotated file(s) inlined (LLM view; never merge)");
        let mut args = vec!["commit-tree", tree.trim(), "-m", &msg];
        let prev = repo.git(&["rev-parse", "--verify", "-q", LLM_REF]).ok().map(|s| s.trim().to_string());
        if let Some(p) = &prev {
            args.extend(["-p", p.as_str()]);
        }
        let commit = git_io(&repo.root, &args, None, GLOSS_IDENTITY)?.trim().to_string();
        repo.git(&["update-ref", LLM_REF, &commit])?;
        Ok((commit, files))
    })();
    let _ = std::fs::remove_file(&tmp_index);
    res
}

/// Opt-in sharing of notes with a remote: fetch theirs, union by note id
/// (newest change wins per note), push ours compare-and-swap. Refuses a
/// world-readable remote unless `anyway` — notes describe your code's
/// internals and your agents' mistakes.
pub fn sync(repo: &Repo, remote: &str, anyway: bool) -> Result<(usize, usize), String> {
    if !anyway && crate::sync::remote_is_world_readable(&repo.root, remote) == Some(true) {
        return Err(format!(
            "refused: '{remote}' is readable by anyone, and notes describe your code's internals and your agents' \
             dead ends. Pass --anyway if you really mean to publish them."
        ));
    }
    // Tracking ref: a configured remote's name as-is; a URL or path gets a
    // hash (it is not a valid ref component).
    let named = repo.git(&["remote"]).map(|r| r.lines().any(|l| l == remote)).unwrap_or(false);
    let key = if named { remote.to_string() } else { format!("url-{}", &crate::store::content_hash(remote.as_bytes())[..16]) };
    let theirs_ref = format!("refs/gloss/remotes/{key}/notes");
    // Ask first, so "the remote has no notes yet" and "could not reach the
    // remote" are different answers.
    let listed = repo
        .git(&["ls-remote", remote, NOTES_REF])
        .map_err(|e| format!("cannot reach '{remote}': {e}"))?;
    let has_theirs = !listed.trim().is_empty();
    let mut merged_in = 0;
    if has_theirs {
        repo.git(&["fetch", "--quiet", remote, &format!("+{NOTES_REF}:{theirs_ref}")])?;
        let theirs = load_all_at(repo, &theirs_ref);
        // Notes are matched by id across ALL paths: a note that moved with a
        // rename on one side must replace, not duplicate, the other copy.
        let ids: std::collections::HashSet<&str> = theirs.values().flatten().map(|n| n.id.as_str()).collect();
        let mut paths: Vec<String> = theirs.keys().cloned().collect();
        for (p, ns) in load_all(repo) {
            if ns.iter().any(|n| ids.contains(n.id.as_str())) {
                paths.push(p);
            }
        }
        paths.sort();
        paths.dedup();
        let stamp = |n: &Note| {
            n.updated_at
                .max(n.created_at)
                .max(n.verified.last().map_or(0, |v| v.at))
                .max(n.retired.as_ref().map_or(0, |r| r.at))
        };
        mutate(repo, &paths, &format!("gloss sync: merge {key}"), |set| {
            merged_in = 0;
            for (tpath, tnotes) in &theirs {
                for t in tnotes {
                    let here = set.iter().find_map(|(p, ns)| ns.iter().position(|n| n.id == t.id).map(|i| (p.clone(), i)));
                    match here {
                        Some((p, i)) if stamp(t) > stamp(&set[&p][i]) => {
                            set.get_mut(&p).expect("found above").remove(i);
                            set.entry(tpath.clone()).or_default().push(t.clone());
                            merged_in += 1;
                        }
                        Some(_) => {}
                        None => {
                            set.entry(tpath.clone()).or_default().push(t.clone());
                            merged_in += 1;
                        }
                    }
                }
            }
            Ok(())
        })?;
    }
    let total: usize = load_all(repo).values().map(|v| v.len()).sum();
    // Ours is now a union, not a descendant of theirs, so the push replaces
    // their ref — guarded by a lease on exactly what we merged.
    let lease = if has_theirs {
        let t = repo.git(&["rev-parse", &theirs_ref])?;
        format!("--force-with-lease={NOTES_REF}:{}", t.trim())
    } else {
        format!("--force-with-lease={NOTES_REF}:")
    };
    repo.git(&["push", "--quiet", &lease, remote, &format!("{NOTES_REF}:{NOTES_REF}")])
        .map_err(|e| format!("push of notes to '{remote}' failed (someone pushed in between? re-run): {e}"))?;
    Ok((merged_in, total))
}

// ---------------------------------------------------------------------------
// Search: notes + code, lexical always, semantic when a local model is up
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct NoteHit {
    pub path: String,
    pub note: NoteView,
    pub score: f32,
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub notes: Vec<NoteHit>,
    pub code: Vec<index::CodeHit>,
    /// "bm25" or "bm25+<model>" — what actually ranked these.
    pub ranker: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<IndexInfo>,
    /// Set when the code half degraded (index busy/corrupt); notes still answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct IndexInfo {
    pub commit: String,
    pub refreshed: bool,
    pub refresh_ms: u64,
}

/// Bring the code index up to HEAD (incremental — only new blobs).
pub fn index_update(repo: &Repo, data_dir: &Path) -> Result<index::IndexStats, String> {
    index::update(&repo.root, &repo.cache_dir(data_dir).join("code"), "HEAD")
}

/// Search notes and code. The code index is refreshed first if HEAD moved
/// (cheap: only changed blobs). With a loopback embedding model available,
/// note ranks fuse BM25 with cosine similarity, and the top code hits are
/// re-ranked semantically (each chunk embedded once, cached by content).
pub fn search(repo: &Repo, data_dir: &Path, query: &str, k: usize) -> Result<SearchResult, String> {
    let cache = repo.cache_dir(data_dir);
    let code_dir = cache.join("code");
    let head = repo.head();
    let mut info = None;
    let mut warning = None;
    if !head.is_empty() {
        let t0 = std::time::Instant::now();
        let refreshed = index::indexed_commit(&code_dir).as_deref() != Some(head.as_str());
        if refreshed {
            if let Err(e) = index::update(&repo.root, &code_dir, "HEAD") {
                warning = Some(format!("code index refresh failed ({e}); code hits may lag HEAD"));
            }
        }
        info = Some(IndexInfo { commit: head.clone(), refreshed, refresh_ms: t0.elapsed().as_millis() as u64 });
    }
    let qtok = index::tokenize(query);

    // Notes: BM25 over text + anchor label.
    let live: Vec<(String, Note)> = load_all(repo)
        .into_iter()
        .flat_map(|(p, ns)| ns.into_iter().filter(|n| n.retired.is_none()).map(move |n| (p.clone(), n)))
        .collect();
    let docs: Vec<Vec<String>> = live
        .iter()
        .map(|(p, n)| index::tokenize(&format!("{} {} {} {}", n.text, n.kind, p, n.anchor.symbol.clone().unwrap_or_default())))
        .collect();
    let lex = index::bm25_rank(&docs, &qtok);
    // A broken cache must never take the notes down with it: retry once
    // after a rebuild, then answer without code hits.
    let want = (k * 3).max(30);
    let mut code = if head.is_empty() {
        vec![]
    } else {
        match index::search(&code_dir, query, want) {
            Ok(c) => c,
            Err(_) => match index::update(&repo.root, &code_dir, "HEAD").and_then(|_| index::search(&code_dir, query, want)) {
                Ok(c) => c,
                Err(e) => {
                    warning = Some(format!("code search unavailable: {e}"));
                    vec![]
                }
            },
        }
    };

    let embedder = embed::Embedder::local(&cache);
    let mut ranker = "bm25".to_string();
    let mut note_scores: Vec<(usize, f32)> = lex.clone();
    if let Some(e) = &embedder {
        if let Ok(qv) = e.embed_one(query) {
            ranker = format!("bm25+{}", e.model);
            // Notes: fuse lexical rank with semantic rank (reciprocal rank
            // fusion — no score calibration between the two needed).
            let texts: Vec<String> = live.iter().map(|(_, n)| match &n.anchor.symbol { Some(s) => format!("{s}: {}", n.text), None => n.text.clone() }).collect();
            if let Ok(vs) = e.embed_many(&texts) {
                let mut sem: Vec<(usize, f32)> = vs.iter().enumerate().map(|(i, v)| (i, embed::cosine(&qv, v))).collect();
                sem.sort_by(|a, b| b.1.total_cmp(&a.1));
                note_scores = rrf(&lex, &sem, live.len());
            }
            // Code: re-rank the lexical candidates semantically.
            let chunks: Vec<String> = code.iter().map(|h| chunk_text(repo, h)).collect();
            if let Ok(vs) = e.embed_many(&chunks) {
                let lexc: Vec<(usize, f32)> = (0..code.len()).map(|i| (i, code[i].score)).collect();
                let mut sem: Vec<(usize, f32)> = vs.iter().enumerate().map(|(i, v)| (i, embed::cosine(&qv, v))).collect();
                sem.sort_by(|a, b| b.1.total_cmp(&a.1));
                let fused = rrf(&lexc, &sem, code.len());
                let mut re: Vec<index::CodeHit> = Vec::new();
                for (i, s) in fused {
                    let mut h = code[i].clone();
                    h.score = s;
                    re.push(h);
                }
                code = re;
            }
        }
    }
    code.truncate(k);
    let notes = note_scores
        .into_iter()
        .take(k)
        .map(|(i, score)| {
            let (p, n) = &live[i];
            let r = repo.content(p).map(|c| anchor::resolve(&n.anchor, &c));
            NoteHit { path: p.clone(), note: NoteView::of(n, r.as_ref()), score }
        })
        .collect();
    Ok(SearchResult { notes, code, ranker, index: info, warning })
}

/// Reciprocal rank fusion of two ranked lists over `n` items.
fn rrf(a: &[(usize, f32)], b: &[(usize, f32)], n: usize) -> Vec<(usize, f32)> {
    let mut s = vec![0f32; n];
    let mut hit = vec![false; n];
    for list in [a, b] {
        for (rank, (i, _)) in list.iter().enumerate() {
            s[*i] += 1.0 / (60.0 + rank as f32);
            hit[*i] = true;
        }
    }
    let mut v: Vec<(usize, f32)> = (0..n).filter(|i| hit[*i]).map(|i| (i, s[i])).collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1));
    v
}

fn chunk_text(repo: &Repo, h: &index::CodeHit) -> String {
    let body = repo.git(&["cat-file", "blob", &h.blob]).unwrap_or_default();
    let lines: Vec<&str> = body.lines().collect();
    let s = h.start_line.saturating_sub(1).min(lines.len());
    let e = h.end_line.min(lines.len());
    format!("{}\n{}", h.path, lines[s..e].join("\n"))
}

#[cfg(test)]
mod tests;

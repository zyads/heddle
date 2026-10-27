//! Gloss end-to-end against real temp git repos.

use super::*;
use std::path::PathBuf;

fn sh(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn rig(tag: &str) -> (PathBuf, Repo) {
    let dir = std::env::temp_dir().join(format!("gloss-{tag}-{}-{}", std::process::id(), now_ms()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    sh(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("src/lib.rs"), LIB).unwrap();
    std::fs::write(dir.join("README.md"), "# demo\n").unwrap();
    sh(&dir, &["add", "-A"]);
    sh(&dir, &["commit", "-q", "-m", "init"]);
    let repo = Repo::open(&dir).unwrap();
    (dir, repo)
}

fn commit_all(dir: &Path, msg: &str) {
    sh(dir, &["add", "-A"]);
    sh(dir, &["commit", "-q", "-m", msg]);
}

const LIB: &str = "\
pub struct Cache {
    hits: u64,
}

impl Cache {
    pub fn get(&mut self, k: &str) -> Option<u64> {
        // Lock order: always take the shard lock before the LRU lock.
        self.hits += 1;
        k.len().checked_sub(1).map(|n| n as u64)
    }

    pub fn evict(&mut self) {
        self.hits = 0;
    }
}
";

fn me() -> Author {
    Author { by: "test-agent".into(), thread: Some("t-1".into()), lease: None }
}

#[test]
fn add_read_and_render_inline() {
    let (dir, repo) = rig("read");
    let n = add(&repo, "src/lib.rs::Cache::get", "shard lock before LRU lock or it deadlocks under evict", "invariant", me()).unwrap();
    assert_eq!(n.anchor.symbol.as_deref(), Some("Cache::get"));
    assert_eq!(n.commit, repo.head());
    assert!(!n.dirty);

    let (text, notes) = read(&repo, &dir.join("src/lib.rs").to_string_lossy(), &render::RenderOpts { numbers: true, show_untrusted: true }).unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].1.status, Status::Fresh);
    // The note sits directly above `pub fn get`, as a `//` comment, with the
    // code's original line numbers intact.
    let i = text.find("[gloss").unwrap();
    let j = text.find("6  │     pub fn get").unwrap();
    assert!(i < j, "{text}");
    assert!(text.contains("    // [gloss"), "indented to the code:\n{text}");
    assert!(text.contains("invariant · test-agent"), "{text}");
    // The human tree is untouched.
    assert_eq!(std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(), LIB);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn notes_survive_moves_go_stale_on_edits_and_verify_rebaselines() {
    let (dir, repo) = rig("stale");
    let n = add(&repo, "src/lib.rs::Cache::evict", "evict resets hits; callers rely on that for metrics", "why", me()).unwrap();

    // Code moves (lines inserted above): fresh, at the new place.
    std::fs::write(dir.join("src/lib.rs"), format!("use std::fmt;\n\n{LIB}")).unwrap();
    commit_all(&dir, "move");
    let r = &resolved(&repo, "src/lib.rs", &repo.content("src/lib.rs").unwrap())[0].1;
    assert_eq!(r.status, Status::Moved { from: 12 });

    // `check --fix` persists the move; afterwards it reads as plain fresh.
    check(&repo, None, true).unwrap();
    let r = &resolved(&repo, "src/lib.rs", &repo.content("src/lib.rs").unwrap())[0].1;
    assert_eq!(r.status, Status::Fresh);

    // Code under the note changes: stale, and the render says so loudly.
    let edited = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap().replace("self.hits = 0;", "self.hits = 1;");
    std::fs::write(dir.join("src/lib.rs"), edited).unwrap();
    let (text, notes) = read(&repo, "src/lib.rs", &render::RenderOpts { numbers: false, show_untrusted: true }).unwrap();
    assert_eq!(notes[0].1.status, Status::Stale);
    assert!(text.contains("STALE"), "{text}");
    // `check --fix` must NOT paper over staleness.
    check(&repo, None, true).unwrap();
    assert_eq!(resolved(&repo, "src/lib.rs", &repo.content("src/lib.rs").unwrap())[0].1.status, Status::Stale);

    // Someone reads the code, confirms (with a correction): fresh again,
    // with the verification recorded at the current commit.
    commit_all(&dir, "edit");
    let v = verify(&repo, &n.id[..5], "reviewer", Some("evict sets hits to 1 (sentinel), not 0")).unwrap();
    assert_eq!(v.verified.len(), 1);
    assert_eq!(v.verified[0].commit, repo.head());
    assert_eq!(resolved(&repo, "src/lib.rs", &repo.content("src/lib.rs").unwrap())[0].1.status, Status::Fresh);

    // History is the ref's log: add, check --fix, verify.
    let h = history(&repo, &n.id);
    assert!(h.len() >= 3 && h[0].contains("gloss add"), "{h:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn deleted_code_orphans_and_renamed_file_is_followed() {
    let (dir, repo) = rig("rename");
    let a = add(&repo, "src/lib.rs:12-14", "evict is O(1)", "note", me()).unwrap();
    sh(&dir, &["mv", "src/lib.rs", "src/cache.rs"]);
    commit_all(&dir, "rename");
    let c = check(&repo, None, false).unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].renamed_to.as_deref(), Some("src/cache.rs"));
    assert!(c[0].resolution.status.trustworthy());
    check(&repo, None, true).unwrap();
    let (_, notes) = read(&repo, "src/cache.rs", &render::RenderOpts { numbers: false, show_untrusted: true }).unwrap();
    assert_eq!(notes.len(), 1, "the note followed the rename");
    assert_eq!(notes[0].0.id, a.id);
    assert!(load_path(&repo, "src/lib.rs").is_empty());

    // Delete the anchored code: orphaned, and verify refuses to bless it.
    std::fs::write(dir.join("src/cache.rs"), "pub struct Cache;\n").unwrap();
    commit_all(&dir, "gut");
    let (text, notes) = read(&repo, "src/cache.rs", &render::RenderOpts { numbers: false, show_untrusted: true }).unwrap();
    assert_eq!(notes[0].1.status, Status::Orphaned);
    assert!(text.starts_with("// [gloss"), "orphans render at the top:\n{text}");
    assert!(verify(&repo, &a.id, "x", None).unwrap_err().contains("orphaned"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn drop_retires_but_why_remembers() {
    let (dir, repo) = rig("why");
    let n = add(&repo, "src/lib.rs::Cache::get", "hits counts lookups, not successes", "why", me()).unwrap();
    add(&repo, "src/lib.rs:1-3", "the struct is not Sync on purpose", "invariant", me()).unwrap();
    drop_note(&repo, &n.id, "reviewer", "hits now counts successes").unwrap();
    let (_, live) = read(&repo, "src/lib.rs", &render::RenderOpts { numbers: false, show_untrusted: true }).unwrap();
    assert_eq!(live.len(), 1, "dropped note is gone from reads");
    let w = why(&repo, "src/lib.rs:8").unwrap();
    assert!(w.notes.is_empty(), "line 8 is in get, not the struct");
    assert_eq!(w.retired.len(), 1);
    assert_eq!(w.retired[0].retired.as_ref().unwrap().reason, "hits now counts successes");
    assert!(w.commits.iter().any(|c| c.contains("init")), "{:?}", w.commits);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn refuses_secrets_bad_kinds_and_dedups() {
    let (dir, repo) = rig("secrets");
    let e = add(&repo, "README.md", "staging creds: password=hunter2hunter2", "note", me()).unwrap_err();
    assert!(e.contains("refused"), "{e}");
    assert!(add(&repo, "README.md", "x", "rumor", me()).is_err());
    assert!(add(&repo, "src/lib.rs::Nope", "x", "note", me()).unwrap_err().contains("no symbol"));
    let a = add(&repo, "README.md", "file-level note", "note", me()).unwrap();
    let b = add(&repo, "README.md", "file-level note", "note", me()).unwrap();
    assert_eq!(a.id, b.id, "exact duplicate returns the existing note");
    assert_eq!(load_path(&repo, "README.md").len(), 1);
    // Notes never touch branches: HEAD's tree is unchanged and the notes
    // live only on refs/gloss/notes.
    assert_eq!(sh(&dir, &["status", "--porcelain"]), "");
    assert!(sh(&dir, &["for-each-ref", "--format=%(refname)"]).contains(NOTES_REF));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn concurrent_writers_never_lose_a_note() {
    let (dir, repo) = rig("cas");
    let threads: Vec<_> = (0..6)
        .map(|i| {
            let repo = repo.clone();
            std::thread::spawn(move || add(&repo, "src/lib.rs::Cache::get", &format!("observation {i}"), "note", me()).unwrap())
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(load_path(&repo, "src/lib.rs").len(), 6, "CAS retry must keep every concurrent add");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn worktrees_share_notes_and_materialize_leaves_human_tree_alone() {
    let (dir, repo) = rig("wt");
    let wt = dir.with_extension("wt");
    sh(&dir, &["worktree", "add", "-q", "--detach", &wt.to_string_lossy()]);
    let wrepo = Repo::open(&wt).unwrap();
    assert_eq!(wrepo.common_dir, repo.common_dir);
    add(&wrepo, "src/lib.rs::Cache::get", "written from a thread's worktree", "note", me()).unwrap();
    assert_eq!(load_path(&repo, "src/lib.rs").len(), 1, "same refs → same notes");

    let head_tree = sh(&dir, &["rev-parse", "HEAD^{tree}"]);
    let (commit, files) = materialize(&repo).unwrap();
    assert_eq!(files, 1);
    let shadow = sh(&dir, &["show", &format!("{commit}:src/lib.rs")]);
    assert!(shadow.contains("// [gloss"), "{shadow}");
    assert_eq!(sh(&dir, &["show", &format!("{commit}:README.md")]), "# demo\n", "unannotated files reused as-is");
    assert_eq!(sh(&dir, &["rev-parse", "HEAD^{tree}"]), head_tree);
    assert_eq!(sh(&dir, &["status", "--porcelain"]), "");
    let _ = std::fs::remove_dir_all(&wt);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sync_is_opt_in_and_merges_by_id() {
    let (dir, repo) = rig("sync");
    let bare = dir.with_extension("remote.git");
    sh(&dir, &["init", "-q", "--bare", "-b", "main", &bare.to_string_lossy()]);
    sh(&dir, &["remote", "add", "origin", &bare.to_string_lossy()]);
    sh(&dir, &["push", "-q", "origin", "main"]);
    add(&repo, "README.md", "from machine A", "note", me()).unwrap();
    // A plain push does not carry notes.
    sh(&dir, &["push", "-q", "origin", "main"]);
    assert!(!sh(&bare, &["for-each-ref"]).contains("gloss"));

    sync(&repo, "origin", false).unwrap();
    assert!(sh(&bare, &["for-each-ref"]).contains(NOTES_REF));

    // Machine B clones, adds its own, syncs; A syncs and has both.
    let b_dir = dir.with_extension("b");
    sh(&dir, &["clone", "-q", &bare.to_string_lossy(), &b_dir.to_string_lossy()]);
    let b = Repo::open(&b_dir).unwrap();
    assert!(load_path(&b, "README.md").is_empty(), "clone does not fetch notes");
    sync(&b, "origin", false).unwrap();
    assert_eq!(load_path(&b, "README.md").len(), 1);
    add(&b, "README.md", "from machine B", "note", me()).unwrap();
    sync(&b, "origin", false).unwrap();
    sync(&repo, "origin", false).unwrap();
    assert_eq!(load_path(&repo, "README.md").len(), 2);
    for d in [&b_dir, &bare, &dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn a_note_follows_two_renames_in_a_row() {
    let (dir, repo) = rig("rename2");
    let n = add(&repo, "src/lib.rs::Cache::evict", "evict is O(1)", "note", me()).unwrap();
    for (from, to) in [("src/lib.rs", "src/a.rs"), ("src/a.rs", "src/b.rs")] {
        sh(&dir, &["mv", from, to]);
        commit_all(&dir, "rename");
        check(&repo, None, true).unwrap();
    }
    let c = check(&repo, None, false).unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!((c[0].path.as_str(), c[0].note.id.as_str()), ("src/b.rs", n.id.as_str()));
    assert!(c[0].resolution.status.trustworthy(), "{:?}", c[0].resolution);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sync_matches_ids_across_paths_and_takes_url_remotes() {
    let (dir, repo) = rig("sync2");
    let bare = dir.with_extension("remote.git");
    sh(&dir, &["init", "-q", "--bare", "-b", "main", &bare.to_string_lossy()]);
    let url = bare.to_string_lossy().to_string();
    sh(&dir, &["push", "-q", &url, "main"]);
    let n = add(&repo, "src/lib.rs::Cache::get", "hot path", "note", me()).unwrap();
    // A path (not a named remote) works, both first time and after.
    sync(&repo, &url, false).unwrap();

    let b_dir = dir.with_extension("b");
    sh(&dir, &["clone", "-q", &url, &b_dir.to_string_lossy()]);
    let b = Repo::open(&b_dir).unwrap();
    sync(&b, &url, false).unwrap();
    sh(&b_dir, &["mv", "src/lib.rs", "src/cache.rs"]);
    commit_all(&b_dir, "rename");
    check(&b, None, true).unwrap();
    sync(&b, &url, false).unwrap();
    sync(&repo, &url, false).unwrap();
    // A's copy moved to the rename's path; never duplicated under two paths.
    let all = load_all(&repo);
    let copies: Vec<&String> = all.iter().filter(|(_, ns)| ns.iter().any(|x| x.id == n.id)).map(|(p, _)| p).collect();
    assert_eq!(copies, vec!["src/cache.rs"]);
    assert!(find(&repo, &n.id).is_ok(), "id stays addressable");
    for d in [&b_dir, &bare, &dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn a_broken_code_index_never_takes_notes_down() {
    let (dir, repo) = rig("brokenidx");
    let data = dir.with_extension("data");
    add(&repo, "src/lib.rs::Cache::get", "lookup counts hits", "note", me()).unwrap();
    assert!(!search(&repo, &data, "lookup hits", 5).unwrap().notes.is_empty());
    // Scribble over every segment while the manifest still points at HEAD.
    let code = repo.cache_dir(&data).join("code");
    for e in std::fs::read_dir(&code).unwrap().flatten() {
        if e.file_name().to_string_lossy().ends_with(".seg") {
            std::fs::write(e.path(), b"garbage").unwrap();
        }
    }
    let r = search(&repo, &data, "lookup hits", 5).unwrap();
    assert!(!r.notes.is_empty(), "notes still answer");
    assert!(!r.code.is_empty() || r.warning.is_some(), "rebuilt, or says why not");
    for d in [&data, &dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn blank_targets_are_refused_and_dirty_check_leaves_the_index_alone() {
    let (dir, repo) = rig("blank");
    assert!(add(&repo, "src/lib.rs:4", "x", "note", me()).unwrap_err().contains("blank"));
    let ix = dir.join(".git/index");
    let before = std::fs::metadata(&ix).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(dir.join("src/lib.rs"), format!("{LIB}// tail\n")).unwrap();
    let n = add(&repo, "src/lib.rs::Cache::get", "dirty write", "note", me()).unwrap();
    assert!(n.dirty);
    assert_eq!(std::fs::metadata(&ix).unwrap().modified().unwrap(), before, "the human's index is never touched");
    let _ = std::fs::remove_dir_all(&dir);
}

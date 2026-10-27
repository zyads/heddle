//! Gloss code-index benchmark on a real large tree.
//!
//!   cargo run --release --example gloss_bench [repo=/tmp/linux]
//!
//! Full build → no-op update → incremental (5 edited files, committed in a
//! `git clone --shared` throwaway so the source repo is never touched; the
//! full index dir is copied and updated against the clone — same blob ids, so
//! this measures exactly the incremental path) → warm query latency.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use heddle::gloss::index;

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=bench", "-c", "user.email=bench@bench"])
        .args(args)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Bytes of the text the index could see: blobs ≤ 1 MiB in the tree.
fn source_bytes(repo: &Path) -> (u64, usize) {
    let mut bytes = 0;
    let mut files = 0;
    for line in git(repo, &["ls-tree", "-r", "-l", "HEAD"]).lines() {
        let mut f = line.split_whitespace();
        let (mode, kind, _, size) = (f.next(), f.next(), f.next(), f.next());
        if kind != Some("blob") || mode == Some("120000") {
            continue;
        }
        if let Some(s) = size.and_then(|s| s.parse::<u64>().ok()) {
            files += 1;
            if s <= 1 << 20 {
                bytes += s;
            }
        }
    }
    (bytes, files)
}

fn main() {
    let repo = PathBuf::from(std::env::args().nth(1).unwrap_or("/tmp/linux".into()));
    let work = std::env::temp_dir().join("gloss-bench");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();
    let idx = work.join("idx");

    let (src, files) = source_bytes(&repo);
    println!("repo {}: {files} files, {:.1} MB indexable text", repo.display(), src as f64 / 1e6);

    let t = Instant::now();
    let st = index::update(&repo, &idx, "HEAD").expect("full update");
    println!(
        "full:        {:>7.2}s  blobs_new={} blobs_live={} skipped={} chunks={} index={:.1} MB ({:.2}x source)",
        t.elapsed().as_secs_f64(),
        st.blobs_new,
        st.blobs_live,
        st.skipped,
        st.chunks_live,
        st.bytes_on_disk as f64 / 1e6,
        st.bytes_on_disk as f64 / src as f64
    );

    let t = Instant::now();
    let st = index::update(&repo, &idx, "HEAD").expect("noop update");
    println!("no-op:       {:>7.1}ms blobs_new={}", t.elapsed().as_secs_f64() * 1e3, st.blobs_new);

    // Incremental: a throwaway clone with 5 edited files.
    let clone = PathBuf::from("/tmp/linux-bench");
    let _ = std::fs::remove_dir_all(&clone);
    let st_clone = Command::new("git")
        .args(["clone", "-q", "--shared"])
        .arg(&repo)
        .arg(&clone)
        .status()
        .unwrap();
    assert!(st_clone.success());
    let targets = ["Makefile", "README", "kernel/fork.c", "mm/memory.c", "fs/ext4/inode.c"];
    for (i, f) in targets.iter().enumerate() {
        let p = clone.join(f);
        let mut body = std::fs::read_to_string(&p).unwrap_or_default();
        body.push_str(&format!("\n/* gloss bench edit {i} glossbenchmarker{i} */\n"));
        std::fs::write(&p, body).unwrap();
    }
    git(&clone, &["commit", "-q", "-am", "bench edits"]);
    let idx2 = work.join("idx-incr");
    let cp = Command::new("cp").arg("-r").arg(&idx).arg(&idx2).status().unwrap();
    assert!(cp.success());
    let t = Instant::now();
    let st = index::update(&clone, &idx2, "HEAD").expect("incremental update");
    println!(
        "incremental: {:>7.1}ms blobs_new={} chunks_new={} segments={} compacted={}",
        t.elapsed().as_secs_f64() * 1e3,
        st.blobs_new,
        st.chunks_new,
        st.segments,
        st.compacted
    );
    let hits = index::search(&idx2, "glossbenchmarker3", 3).unwrap();
    println!("  edit visible: {:?}", hits.first().map(|h| (&h.path, h.start_line, h.end_line)));

    // Compaction: deleting drivers/ (most of the tree) kills >40% of chunks.
    git(&clone, &["rm", "-r", "-q", "drivers"]);
    git(&clone, &["commit", "-q", "-m", "drop drivers"]);
    let t = Instant::now();
    let st = index::update(&clone, &idx2, "HEAD").expect("compacting update");
    println!(
        "compaction:  {:>7.2}s  compacted={} chunks_live={} segments={} index={:.1} MB",
        t.elapsed().as_secs_f64(),
        st.compacted,
        st.chunks_live,
        st.segments,
        st.bytes_on_disk as f64 / 1e6
    );

    let queries = [
        "spin_lock_irqsave",
        "page fault handler",
        "tcp congestion window",
        "ext4 journal commit",
        "usb device descriptor",
        "mutex_lock interruptible",
        "dma buffer mapping",
        "scheduler runqueue load balance",
        "kmalloc GFP_KERNEL",
        "copy_from_user",
    ];
    for q in &queries {
        index::search(&idx, q, 10).unwrap(); // warm
    }
    let mut times = Vec::new();
    for round in 0..5 {
        for q in &queries {
            let t = Instant::now();
            let hits = index::search(&idx, q, 10).unwrap();
            times.push(t.elapsed().as_secs_f64() * 1e3);
            if round == 0 {
                let top = hits.first().map(|h| format!("{}:{}-{}", h.path, h.start_line, h.end_line));
                println!("  {q:<34} -> {}", top.unwrap_or_default());
            }
        }
    }
    times.sort_by(|a, b| a.total_cmp(b));
    let pct = |p: f64| times[((times.len() - 1) as f64 * p).round() as usize];
    println!(
        "search warm: p50={:.1}ms p95={:.1}ms max={:.1}ms (n={})",
        pct(0.5),
        pct(0.95),
        times.last().unwrap(),
        times.len()
    );
    let _ = std::fs::remove_dir_all(&clone);
}

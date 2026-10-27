// Heddle — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! Gloss code index — BM25 over a repo's *committed* tree, keyed by blob.
//!
//! ```text
//! <index dir>/
//!   manifest                 commit, segment list, BM25 totals, live (blob, path) list
//!   seg-00000003.seg         immutable segment: blobs, chunks, term dictionary, postings
//!   seg-00000003.g7.live     which of that segment's chunks are live at generation 7
//!   lock                     held only while an update runs
//! ```
//!
//! Why blob-keyed: git already tells us, for free, which file contents are
//! new. A blob id that was indexed once never needs reading again — a branch
//! switch, a revert, or a 5-file commit on an 80k-file tree costs exactly the
//! blobs that changed. So an update writes ONE new segment holding only the
//! never-seen blobs, and old segments are never rewritten; what changes per
//! commit is the tiny per-segment liveness bitmap ("is this chunk's blob in
//! the tree right now?"). LSM-style, minus the ceremony.
//!
//! Dead chunks pile up as files change. When they pass ~40% of all chunks, or
//! there are too many segments, compaction merges everything into one segment
//! from the postings we already have — git is not asked for a single byte.
//!
//! Search never loads postings wholesale: the dictionary is sorted by term
//! hash and binary-searched with positioned reads, and only the queried terms'
//! postings are read. What IS loaded per search is one u32 per chunk (token
//! counts, for BM25 length normalisation) and the liveness bitmap — a few MB
//! on a linux-sized tree, cheaper than any cleverness to avoid it.
//!
//! This is a cache. Anything unreadable — a torn manifest, a truncated
//! segment, a stale lock — is thrown away and rebuilt, never a hard failure.
//! Segments are written tmp+rename and not fsynced: a crash can at worst
//! leave a file that fails validation, and that just means a rebuild.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Target chunk height. Big enough that a hit has context, small enough that
/// "lines 120-170" is a useful pointer for an agent with a token budget.
const CHUNK_LINES: usize = 50;
/// A chunk may end early at a blank line within its last this-many lines, so
/// chunks tend to break between functions rather than mid-body.
const BREAK_WINDOW: usize = 10;
/// Source, not artifacts: anything bigger is generated tables or vendored
/// blobs that would drown real hits.
const MAX_BLOB_BYTES: usize = 1 << 20;
/// Same heuristic git uses: a NUL in the first 8 KiB means binary.
const BINARY_SNIFF: usize = 8 << 10;
const MIN_TOKEN: usize = 2;
const MAX_TOKEN: usize = 64;
const COMPACT_DEAD_RATIO: f64 = 0.4;
const COMPACT_MAX_SEGMENTS: usize = 12;
/// A lock older than this belongs to a crashed update — steal it.
const LOCK_STALE: Duration = Duration::from_secs(600);
const MAX_THREADS: usize = 16;
const K1: f32 = 1.2;
const B: f32 = 0.75;

const MANIFEST: &str = "manifest";
const MANIFEST_MAGIC: &str = "gloss-index 1";
const SEG_MAGIC: &[u8; 8] = b"GLOSSEG1";
const HEADER_LEN: u64 = 64;
const DICT_ENTRY: u64 = 24;
/// Blob flag: the blob was looked at and deliberately not indexed (binary,
/// oversized, unreadable). Recorded so it is never fetched again either.
const FLAG_SKIPPED: u8 = 1;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct IndexStats {
    pub commit: String,
    pub blobs_live: usize,
    pub blobs_new: usize,
    pub chunks_new: usize,
    pub chunks_live: usize,
    pub segments: usize,
    pub bytes_on_disk: u64,
    pub skipped: usize,
    pub elapsed_ms: u64,
    pub compacted: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CodeHit {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub score: f32,
    pub blob: String,
}

// ---------------------------------------------------------------------------
// Tokenizer + hashing
// ---------------------------------------------------------------------------

/// FNV-1a 64. Term ids are hashes, not strings: no vocabulary table to store
/// or look up, and a collision just means two rare words share a posting
/// list — an acceptable blur for a search index, never a correctness issue
/// for the code itself.
fn term_hash(token: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in token.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// HashMap hasher for keys that are already FNV hashes — re-hashing them
/// with SipHash would be most of the indexing cost.
#[derive(Default)]
struct PreHashed(u64);
impl Hasher for PreHashed {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}
type TermMap<V> = HashMap<u64, V, BuildHasherDefault<PreHashed>>;

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || (!c.is_ascii() && c.is_alphanumeric())
}

/// The one tokenizer. Indexing and querying both go through here, so a query
/// token always hashes the same as the indexed one. Emits borrowed slices
/// from a reused buffer: the index tokenizes ~1 GB of linux and must not
/// allocate per token.
fn for_each_token(text: &str, mut emit: impl FnMut(&str)) {
    let mut run: Vec<char> = Vec::with_capacity(64);
    let mut whole = String::with_capacity(64);
    let mut parts: Vec<(usize, usize)> = Vec::with_capacity(8);
    for c in text.chars() {
        if is_ident(c) {
            run.push(c);
        } else if !run.is_empty() {
            emit_identifier(&run, &mut whole, &mut parts, &mut emit);
            run.clear();
        }
    }
    if !run.is_empty() {
        emit_identifier(&run, &mut whole, &mut parts, &mut emit);
    }
}

/// One identifier → the whole thing lowercased (so `spin_lock_irqsave` is a
/// sharp, rare token) plus its snake/camel parts (so "irqsave" still finds
/// it). `parseHTTPResponse` → parsehttpresponse, parse, http, response.
fn emit_identifier(
    run: &[char],
    whole: &mut String,
    parts: &mut Vec<(usize, usize)>,
    emit: &mut impl FnMut(&str),
) {
    whole.clear();
    parts.clear();
    let mut part_start: Option<usize> = None;
    for (i, &c) in run.iter().enumerate() {
        if c == '_' {
            if let Some(s) = part_start.take() {
                parts.push((s, whole.len()));
            }
            whole.push('_');
            continue;
        }
        let boundary = i > 0 && c.is_uppercase() && {
            let p = run[i - 1];
            p.is_lowercase()
                || p.is_numeric()
                || (p.is_uppercase() && run.get(i + 1).is_some_and(|n| n.is_lowercase()))
        };
        if boundary {
            if let Some(s) = part_start.take() {
                parts.push((s, whole.len()));
            }
        }
        if part_start.is_none() {
            part_start = Some(whole.len());
        }
        if c.is_ascii() {
            whole.push(c.to_ascii_lowercase());
        } else {
            whole.extend(c.to_lowercase());
        }
    }
    if let Some(s) = part_start {
        parts.push((s, whole.len()));
    }
    if parts.is_empty() {
        return; // all underscores
    }
    let ok = |n: usize| (MIN_TOKEN..=MAX_TOKEN).contains(&n);
    if ok(whole.len()) {
        emit(whole);
    }
    if parts.len() == 1 && parts[0] == (0, whole.len()) {
        return; // the identifier IS its only part — don't count it twice
    }
    for &(s, e) in parts.iter() {
        if ok(e - s) {
            emit(&whole[s..e]);
        }
    }
}

/// Lowercase; split on anything that isn't alphanumeric or `_`; each
/// identifier yields itself plus its snake_case/camelCase parts; tokens
/// shorter than 2 or longer than 64 bytes are dropped.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for_each_token(text, |t| out.push(t.to_string()));
    out
}

fn bm25_idf(n: f32, df: f32) -> f32 {
    (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
}

fn bm25_term(idf: f32, tf: f32, dl: f32, avgdl: f32) -> f32 {
    idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * dl / avgdl))
}

/// Plain BM25 over a small in-memory corpus (notes). Returns (doc index,
/// score), best first, positive scores only.
pub fn bm25_rank(docs: &[Vec<String>], query: &[String]) -> Vec<(usize, f32)> {
    if docs.is_empty() || query.is_empty() {
        return Vec::new();
    }
    let n = docs.len() as f32;
    let total: usize = docs.iter().map(|d| d.len()).sum();
    let avgdl = (total as f32 / n).max(1.0);
    let mut terms: Vec<&str> = query.iter().map(|s| s.as_str()).collect();
    terms.sort_unstable();
    terms.dedup();
    let tfs: Vec<HashMap<&str, u32>> = docs
        .iter()
        .map(|d| {
            let mut m = HashMap::new();
            for t in d {
                *m.entry(t.as_str()).or_insert(0) += 1;
            }
            m
        })
        .collect();
    let mut scores = vec![0f32; docs.len()];
    for t in terms {
        let df = tfs.iter().filter(|m| m.contains_key(t)).count();
        if df == 0 {
            continue;
        }
        let idf = bm25_idf(n, df as f32);
        for (i, m) in tfs.iter().enumerate() {
            if let Some(&tf) = m.get(t) {
                scores[i] += bm25_term(idf, tf as f32, docs[i].len() as f32, avgdl);
            }
        }
    }
    let mut out: Vec<(usize, f32)> = scores
        .into_iter()
        .enumerate()
        .filter(|&(_, s)| s > 0.0)
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

// ---------------------------------------------------------------------------
// Small binary helpers
// ---------------------------------------------------------------------------

fn put_varint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Decode one LEB128 u32; None on truncation (corrupt segment).
fn get_varint(buf: &[u8], pos: &mut usize) -> Option<u32> {
    let mut v: u32 = 0;
    let mut shift = 0;
    loop {
        let b = *buf.get(*pos)?;
        *pos += 1;
        v |= ((b & 0x7f) as u32) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift > 28 {
            return None;
        }
    }
}

/// Decode a delta-encoded posting list into (chunk, tf) pairs.
fn decode_postings(buf: &[u8], mut f: impl FnMut(u32, u32)) -> Result<(), String> {
    let mut pos = 0;
    let mut chunk: u32 = 0;
    while pos < buf.len() {
        let d = get_varint(buf, &mut pos).ok_or("corrupt postings")?;
        let tf = get_varint(buf, &mut pos).ok_or("corrupt postings")?;
        chunk = chunk.checked_add(d).ok_or("corrupt postings")?;
        f(chunk, tf);
    }
    Ok(())
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn read_at(file: &File, off: u64, len: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; len];
    file.read_exact_at(&mut buf, off)
        .map_err(|e| format!("index read: {e}"))?;
    Ok(buf)
}

fn create_private(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

/// Small files (manifest, liveness bitmaps): whole-file tmp + rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = tmp_path(path);
    let mut f = create_private(&tmp).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    f.write_all(bytes)
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

// ---------------------------------------------------------------------------
// Segment files
// ---------------------------------------------------------------------------
//
// Layout (all little-endian):
//   [0..64)   header: magic, oid_len u32, nblobs u32, nchunks u32, nterms u32,
//             chunks_off u64, postings_off u64, dict_off u64, total_tokens u64, 0u64
//   blob oids        nblobs × oid_len hex bytes (starts at 64)
//   blob flags       nblobs × u8, then zero padding to 4-byte alignment
//   chunk columns    4 × nchunks × u32: blob idx | start line | end line | token count
//   postings         per term, (chunk delta varint, tf varint)*; in dictionary order
//   dictionary       nterms × (hash u64, postings off u64, len u32, df u32), sorted by hash
//
// Columns rather than rows: search wants only the token-count column for
// every chunk, and start/end/blob for just the top k.

struct Segment {
    name: String,
    file: File,
    oid_len: usize,
    nblobs: usize,
    nchunks: usize,
    nterms: u64,
    chunks_off: u64,
    dict_off: u64,
    postings_off: u64,
}

#[derive(Clone, Copy)]
struct DictEntry {
    hash: u64,
    off: u64,
    len: u32,
    // df (bytes 20..24) is on disk for tooling; search counts live postings
    // itself so dead chunks never inflate it.
}

fn parse_dict_entry(b: &[u8]) -> DictEntry {
    DictEntry {
        hash: u64_at(b, 0),
        off: u64_at(b, 8),
        len: u32_at(b, 16),
    }
}

impl Segment {
    /// Open and validate. Every offset is checked against the file length so
    /// a truncated or scribbled segment is rejected here, not mid-search.
    fn open(dir: &Path, name: &str) -> Result<Segment, String> {
        if !valid_seg_name(name) {
            return Err(format!("bad segment name {name:?}"));
        }
        let file = File::open(dir.join(name)).map_err(|e| format!("open {name}: {e}"))?;
        let len = file.metadata().map_err(|e| e.to_string())?.len();
        if len < HEADER_LEN {
            return Err(format!("{name}: truncated"));
        }
        let h = read_at(&file, 0, HEADER_LEN as usize)?;
        if &h[0..8] != SEG_MAGIC {
            return Err(format!("{name}: bad magic"));
        }
        let oid_len = u32_at(&h, 8) as usize;
        let nblobs = u32_at(&h, 12) as usize;
        let nchunks = u32_at(&h, 16) as usize;
        let nterms = u32_at(&h, 20) as u64;
        let chunks_off = u64_at(&h, 24);
        let postings_off = u64_at(&h, 32);
        let dict_off = u64_at(&h, 40);
        let flags_end = HEADER_LEN + (nblobs * (oid_len + 1)) as u64;
        let ok = (oid_len == 40 || oid_len == 64)
            && chunks_off >= flags_end
            && chunks_off.is_multiple_of(4)
            && postings_off == chunks_off + 16 * nchunks as u64
            && dict_off >= postings_off
            && dict_off.checked_add(nterms * DICT_ENTRY) == Some(len);
        if !ok {
            return Err(format!("{name}: inconsistent header"));
        }
        Ok(Segment {
            name: name.to_string(),
            file,
            oid_len,
            nblobs,
            nchunks,
            nterms,
            chunks_off,
            dict_off,
            postings_off,
        })
    }

    fn blobs(&self) -> Result<(Vec<String>, Vec<u8>), String> {
        let raw = read_at(&self.file, HEADER_LEN, self.nblobs * (self.oid_len + 1))?;
        let (ids, flags) = raw.split_at(self.nblobs * self.oid_len);
        let oids = ids
            .chunks_exact(self.oid_len)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect();
        Ok((oids, flags.to_vec()))
    }

    fn oid(&self, blob: usize) -> Result<String, String> {
        let raw = read_at(
            &self.file,
            HEADER_LEN + (blob * self.oid_len) as u64,
            self.oid_len,
        )?;
        Ok(String::from_utf8_lossy(&raw).into_owned())
    }

    /// One chunk column (0 blob, 1 start, 2 end, 3 token count).
    fn column(&self, col: usize) -> Result<Vec<u32>, String> {
        let raw = read_at(
            &self.file,
            self.chunks_off + (col * 4 * self.nchunks) as u64,
            4 * self.nchunks,
        )?;
        Ok(raw
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect())
    }

    fn cell(&self, col: usize, chunk: usize) -> Result<u32, String> {
        let raw = read_at(
            &self.file,
            self.chunks_off + (col * 4 * self.nchunks + 4 * chunk) as u64,
            4,
        )?;
        Ok(u32_at(&raw, 0))
    }

    /// Binary search the on-disk dictionary: ~22 positioned 8-byte reads on a
    /// linux-sized segment, all from page cache once warm.
    fn find(&self, hash: u64) -> Result<Option<DictEntry>, String> {
        let (mut lo, mut hi) = (0u64, self.nterms);
        let mut b = [0u8; 8];
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            self.file
                .read_exact_at(&mut b, self.dict_off + mid * DICT_ENTRY)
                .map_err(|e| format!("index read: {e}"))?;
            let h = u64::from_le_bytes(b);
            if h < hash {
                lo = mid + 1;
            } else if h > hash {
                hi = mid;
            } else {
                let e = parse_dict_entry(&read_at(
                    &self.file,
                    self.dict_off + mid * DICT_ENTRY,
                    DICT_ENTRY as usize,
                )?);
                if e.off < self.postings_off || e.off + e.len as u64 > self.dict_off {
                    return Err(format!("{}: corrupt dictionary", self.name));
                }
                return Ok(Some(e));
            }
        }
        Ok(None)
    }

    fn dictionary(&self) -> Result<Vec<DictEntry>, String> {
        let raw = read_at(
            &self.file,
            self.dict_off,
            (self.nterms * DICT_ENTRY) as usize,
        )?;
        Ok(raw
            .chunks_exact(DICT_ENTRY as usize)
            .map(parse_dict_entry)
            .collect())
    }
}

fn valid_seg_name(name: &str) -> bool {
    name.starts_with("seg-")
        && name.ends_with(".seg")
        && name.len() < 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

fn seg_name(gen: u64) -> String {
    format!("seg-{gen:08}.seg")
}

fn live_name(seg: &str, gen: u64) -> String {
    format!("{seg}.g{gen}.live")
}

/// Streams a segment to `<dest>.tmp`, renamed into place by `finish`.
/// Postings are written as terms arrive (callers hand them over in hash
/// order), so neither the build nor compaction holds a second copy of them.
struct SegWriter {
    dest: PathBuf,
    tmp: PathBuf,
    out: BufWriter<File>,
    pos: u64,
    dict: Vec<u8>,
    nterms: u32,
    term_start: u64,
    last_hash: Option<u64>,
    header: [u8; HEADER_LEN as usize],
}

impl SegWriter {
    fn create(
        dest: PathBuf,
        oid_len: usize,
        oids: &[String],
        flags: &[u8],
        cols: &[Vec<u32>; 4],
    ) -> Result<SegWriter, String> {
        let tmp = tmp_path(&dest);
        let f = create_private(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        let mut out = BufWriter::with_capacity(1 << 20, f);
        let io = |e: std::io::Error| format!("write segment: {e}");
        let nchunks = cols[0].len();
        out.write_all(&[0u8; HEADER_LEN as usize]).map_err(io)?;
        for o in oids {
            if o.len() != oid_len {
                return Err(format!("unexpected object id {o:?}"));
            }
            out.write_all(o.as_bytes()).map_err(io)?;
        }
        out.write_all(flags).map_err(io)?;
        let mut pos = HEADER_LEN + (oids.len() * (oid_len + 1)) as u64;
        let pad = (4 - pos % 4) % 4;
        out.write_all(&[0u8; 4][..pad as usize]).map_err(io)?;
        pos += pad;
        let chunks_off = pos;
        for col in cols {
            for v in col {
                out.write_all(&v.to_le_bytes()).map_err(io)?;
            }
        }
        pos += 16 * nchunks as u64;
        let total_tokens: u64 = cols[3].iter().map(|&v| v as u64).sum();
        let mut header = [0u8; HEADER_LEN as usize];
        header[0..8].copy_from_slice(SEG_MAGIC);
        header[8..12].copy_from_slice(&(oid_len as u32).to_le_bytes());
        header[12..16].copy_from_slice(&(oids.len() as u32).to_le_bytes());
        header[16..20].copy_from_slice(&(nchunks as u32).to_le_bytes());
        header[24..32].copy_from_slice(&chunks_off.to_le_bytes());
        header[32..40].copy_from_slice(&pos.to_le_bytes());
        header[48..56].copy_from_slice(&total_tokens.to_le_bytes());
        Ok(SegWriter {
            dest,
            tmp,
            out,
            pos,
            dict: Vec::new(),
            nterms: 0,
            term_start: pos,
            last_hash: None,
            header,
        })
    }

    fn put(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.out
            .write_all(bytes)
            .map_err(|e| format!("write segment: {e}"))?;
        self.pos += bytes.len() as u64;
        Ok(())
    }

    /// Close the posting list written since the previous `end_term`.
    fn end_term(&mut self, hash: u64, df: u32) -> Result<(), String> {
        if self.last_hash.is_some_and(|h| h >= hash) {
            return Err("internal: terms out of order".into());
        }
        let len = self.pos - self.term_start;
        if len > u32::MAX as u64 {
            return Err("posting list too large".into());
        }
        self.dict.extend_from_slice(&hash.to_le_bytes());
        self.dict.extend_from_slice(&self.term_start.to_le_bytes());
        self.dict.extend_from_slice(&(len as u32).to_le_bytes());
        self.dict.extend_from_slice(&df.to_le_bytes());
        self.nterms += 1;
        self.term_start = self.pos;
        self.last_hash = Some(hash);
        Ok(())
    }

    fn finish(mut self) -> Result<(), String> {
        let io = |e: std::io::Error| format!("write segment: {e}");
        let dict_off = self.pos;
        let dict = std::mem::take(&mut self.dict);
        self.out.write_all(&dict).map_err(io)?;
        self.header[20..24].copy_from_slice(&self.nterms.to_le_bytes());
        self.header[40..48].copy_from_slice(&dict_off.to_le_bytes());
        let f = self.out.into_inner().map_err(|e| io(e.into_error()))?;
        f.write_all_at(&self.header, 0).map_err(io)?;
        drop(f);
        std::fs::rename(&self.tmp, &self.dest).map_err(io)
    }
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------
//
// Plain text, one fact per line, terminated by `end` so a torn write is
// detectable. Paths are escaped (`\\`, `\n`) because git allows newlines in
// file names and this format is line-based.

#[derive(Debug, Clone, Default)]
struct Manifest {
    gen: u64,
    commit: String,
    segments: Vec<String>,
    chunks_live: u64,
    tokens_live: u64,
    blobs_live: usize,
    skipped: usize,
    /// (blob oid, path) for every indexable path in the tree.
    entries: Vec<(String, String)>,
}

fn escape_path(p: &str) -> String {
    p.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape_path(p: &str) -> String {
    if !p.contains('\\') {
        return p.to_string();
    }
    let mut out = String::with_capacity(p.len());
    let mut it = p.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') => out.push('\n'),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

impl Manifest {
    fn render(&self) -> String {
        let mut s = String::with_capacity(64 + self.entries.len() * 80);
        s.push_str(MANIFEST_MAGIC);
        s.push('\n');
        s.push_str(&format!("gen {}\ncommit {}\n", self.gen, self.commit));
        s.push_str(&format!(
            "chunks_live {}\ntokens_live {}\nblobs_live {}\nskipped {}\n",
            self.chunks_live, self.tokens_live, self.blobs_live, self.skipped
        ));
        s.push_str(&format!("segments {}\n", self.segments.len()));
        for seg in &self.segments {
            s.push_str(seg);
            s.push('\n');
        }
        s.push_str(&format!("paths {}\n", self.entries.len()));
        for (oid, path) in &self.entries {
            s.push_str(oid);
            s.push(' ');
            s.push_str(&escape_path(path));
            s.push('\n');
        }
        s.push_str("end\n");
        s
    }

    /// Parse; `entries` are only materialised when asked for (search scans
    /// them lazily for just the hit blobs). Returns the header and the byte
    /// offset where the path list starts.
    fn parse(text: &str, want_entries: bool) -> Option<(Manifest, usize)> {
        if !text.ends_with("end\n") {
            return None;
        }
        let mut lines = text.split_inclusive('\n');
        let consumed = std::cell::Cell::new(0usize);
        let mut next = || -> Option<&str> {
            let l = lines.next()?;
            consumed.set(consumed.get() + l.len());
            Some(l.strip_suffix('\n').unwrap_or(l))
        };
        if next()? != MANIFEST_MAGIC {
            return None;
        }
        fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
            line.strip_prefix(key)?.strip_prefix(' ')
        }
        let mut m = Manifest {
            gen: field(next()?, "gen")?.parse().ok()?,
            commit: field(next()?, "commit")?.to_string(),
            ..Default::default()
        };
        m.chunks_live = field(next()?, "chunks_live")?.parse().ok()?;
        m.tokens_live = field(next()?, "tokens_live")?.parse().ok()?;
        m.blobs_live = field(next()?, "blobs_live")?.parse().ok()?;
        m.skipped = field(next()?, "skipped")?.parse().ok()?;
        let nseg: usize = field(next()?, "segments")?.parse().ok()?;
        for _ in 0..nseg {
            let s = next()?;
            if !valid_seg_name(s) {
                return None;
            }
            m.segments.push(s.to_string());
        }
        let npaths: usize = field(next()?, "paths")?.parse().ok()?;
        let entries_at = consumed.get();
        let body = &text[entries_at..text.len() - 4];
        if body.bytes().filter(|&b| b == b'\n').count() != npaths {
            return None;
        }
        if want_entries {
            for line in body.lines() {
                let (oid, path) = line.split_once(' ')?;
                m.entries.push((oid.to_string(), unescape_path(path)));
            }
        }
        if m.commit.is_empty() || !m.commit.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Some((m, entries_at))
    }
}

fn read_manifest(dir: &Path) -> Option<Manifest> {
    let text = std::fs::read_to_string(dir.join(MANIFEST)).ok()?;
    Manifest::parse(&text, false).map(|(m, _)| m)
}

/// Commit the index currently reflects, if any.
pub fn indexed_commit(dir: &Path) -> Option<String> {
    read_manifest(dir).map(|m| m.commit)
}

// ---------------------------------------------------------------------------
// Lock
// ---------------------------------------------------------------------------

struct Lock(PathBuf);

impl Lock {
    fn acquire(dir: &Path) -> Result<Lock, String> {
        let path = dir.join("lock");
        for _ in 0..3 {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(mut f) => {
                    let _ = writeln!(f, "{}", std::process::id());
                    return Ok(Lock(path));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let age = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok());
                    match age {
                        // Vanished between create and stat: just retry.
                        None if !path.exists() => continue,
                        Some(a) if a < LOCK_STALE => {
                            return Err("index update already running".into())
                        }
                        None => return Err("index update already running".into()),
                        _ => {
                            // A crashed update's leftover — take it over.
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
                Err(e) => return Err(format!("lock {}: {e}", path.display())),
            }
        }
        Err("index update already running".into())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Git plumbing
// ---------------------------------------------------------------------------

fn git_out(repo: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

/// (blob oid, path) for every regular file in the commit's tree. Symlinks and
/// submodules are skipped: neither has source text of its own here.
fn ls_tree(repo: &Path, commit: &str) -> Result<Vec<(String, String)>, String> {
    let raw = git_out(repo, &["ls-tree", "-r", "-z", "--full-tree", commit])?;
    let mut out = Vec::new();
    for rec in raw.split(|&b| b == 0) {
        if rec.is_empty() {
            continue;
        }
        let Some(tab) = rec.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let meta = String::from_utf8_lossy(&rec[..tab]);
        let mut f = meta.split(' ');
        let (Some(mode), Some(kind), Some(oid)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        if kind != "blob" || mode == "120000" {
            continue;
        }
        out.push((
            oid.to_string(),
            String::from_utf8_lossy(&rec[tab + 1..]).into_owned(),
        ));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Building: tokenize new blobs in parallel
// ---------------------------------------------------------------------------

/// A term's postings within one partition. The first entry's chunk id is
/// kept aside (`first`) so partitions can be stitched together by re-encoding
/// one varint instead of re-decoding the list.
struct TermBuf {
    first: u32,
    last: u32,
    df: u32,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct Partial {
    oids: Vec<String>,
    flags: Vec<u8>,
    cols: [Vec<u32>; 4],
    terms: TermMap<TermBuf>,
    tokenized: usize,
}

/// Line-based chunks: (byte start, byte end, first line, last line), lines
/// 1-based inclusive.
fn chunk_spans(text: &str) -> Vec<(usize, usize, usize, usize)> {
    let b = text.as_bytes();
    let mut starts = vec![0usize];
    for (i, &c) in b.iter().enumerate() {
        if c == b'\n' && i + 1 < b.len() {
            starts.push(i + 1);
        }
    }
    let n = starts.len();
    let line_end = |i: usize| if i + 1 < n { starts[i + 1] } else { b.len() };
    let blank = |i: usize| b[starts[i]..line_end(i)].iter().all(|c| c.is_ascii_whitespace());
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let mut end = (i + CHUNK_LINES).min(n);
        if end - i == CHUNK_LINES && end < n {
            // Prefer to break after a blank line near the bottom of the window.
            if let Some(j) = (end - BREAK_WINDOW..end).rev().find(|&j| j > i && blank(j)) {
                end = j + 1;
            }
        }
        out.push((starts[i], line_end(end - 1), i + 1, end));
        i = end;
    }
    out
}

fn index_blob(p: &mut Partial, blob: u32, content: &[u8], tf: &mut TermMap<u32>) {
    let text = String::from_utf8_lossy(content);
    for (s, e, sl, el) in chunk_spans(&text) {
        tf.clear();
        let mut n: u32 = 0;
        for_each_token(&text[s..e], |t| {
            *tf.entry(term_hash(t)).or_insert(0) += 1;
            n += 1;
        });
        if n == 0 {
            continue; // blank or punctuation-only: nothing a query could match
        }
        let chunk = p.cols[0].len() as u32;
        p.cols[0].push(blob);
        p.cols[1].push(sl as u32);
        p.cols[2].push(el as u32);
        p.cols[3].push(n);
        for (&h, &c) in tf.iter() {
            match p.terms.get_mut(&h) {
                Some(tb) => {
                    put_varint(&mut tb.bytes, chunk - tb.last);
                    put_varint(&mut tb.bytes, c);
                    tb.last = chunk;
                    tb.df += 1;
                }
                None => {
                    let mut bytes = Vec::with_capacity(4);
                    put_varint(&mut bytes, c);
                    p.terms.insert(
                        h,
                        TermBuf {
                            first: chunk,
                            last: chunk,
                            df: 1,
                            bytes,
                        },
                    );
                }
            }
        }
    }
}

/// One partition: its own `git cat-file --batch`, fed from a separate thread
/// (a single thread writing ids and reading contents deadlocks once both
/// pipes fill).
fn index_partition(repo: &Path, oids: Vec<String>) -> Result<Partial, String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("run git cat-file: {e}"))?;
    let stdin = child.stdin.take().ok_or("cat-file stdin")?;
    let stdout = child.stdout.take().ok_or("cat-file stdout")?;
    let feed = oids.clone();
    let feeder = std::thread::spawn(move || -> std::io::Result<()> {
        let mut w = BufWriter::new(stdin);
        for o in &feed {
            w.write_all(o.as_bytes())?;
            w.write_all(b"\n")?;
        }
        w.flush()
    });
    let result = (|| -> Result<Partial, String> {
        let mut r = BufReader::with_capacity(1 << 20, stdout);
        let mut p = Partial::default();
        let mut tf: TermMap<u32> = TermMap::default();
        let mut header = Vec::new();
        let mut content = Vec::new();
        let io = |e: std::io::Error| format!("read git cat-file: {e}");
        for oid in oids {
            header.clear();
            r.read_until(b'\n', &mut header).map_err(io)?;
            let h = String::from_utf8_lossy(&header);
            let mut f = h.trim_end().split(' ');
            let (got, kind, size) = (f.next(), f.next(), f.next());
            if got != Some(oid.as_str()) {
                return Err(format!("git cat-file: unexpected reply {:?}", h.trim_end()));
            }
            let blob = p.oids.len() as u32;
            p.oids.push(oid);
            let Some(size) = size.and_then(|s| s.parse::<usize>().ok()) else {
                p.flags.push(FLAG_SKIPPED); // "<oid> missing"
                continue;
            };
            if kind != Some("blob") || size > MAX_BLOB_BYTES {
                std::io::copy(&mut (&mut r).take(size as u64 + 1), &mut std::io::sink())
                    .map_err(io)?;
                p.flags.push(FLAG_SKIPPED);
                continue;
            }
            content.resize(size + 1, 0);
            r.read_exact(&mut content).map_err(io)?;
            content.truncate(size);
            if content[..size.min(BINARY_SNIFF)].contains(&0) {
                p.flags.push(FLAG_SKIPPED);
                continue;
            }
            p.flags.push(0);
            p.tokenized += 1;
            index_blob(&mut p, blob, &content, &mut tf);
        }
        Ok(p)
    })();
    let fed = feeder.join();
    let _ = child.kill();
    let _ = child.wait();
    let p = result?;
    match fed {
        Ok(Ok(())) => Ok(p),
        _ => Err("git cat-file: failed to feed object ids".into()),
    }
}

/// Tokenize `oids` and write them as one segment. Returns (blobs tokenized,
/// chunks written).
fn build_segment(
    repo: &Path,
    dest: PathBuf,
    oid_len: usize,
    oids: &[String],
) -> Result<(usize, usize), String> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(MAX_THREADS)
        .min(oids.len())
        .max(1);
    // Round-robin, not contiguous ranges: big directories (drivers/gpu…) sit
    // together in tree order, and one partition shouldn't get all of them.
    let mut parts: Vec<Vec<String>> = vec![Vec::new(); threads];
    for (i, o) in oids.iter().enumerate() {
        parts[i % threads].push(o.clone());
    }
    let mut partials: Vec<Partial> = std::thread::scope(|s| {
        let handles: Vec<_> = parts
            .into_iter()
            .map(|part| s.spawn(move || index_partition(repo, part)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err("indexer thread panicked".into())))
            .collect::<Result<Vec<_>, String>>()
    })?;

    // Concatenate partitions: partition p's blobs and chunks follow p-1's.
    let mut all_oids = Vec::with_capacity(oids.len());
    let mut flags = Vec::with_capacity(oids.len());
    let mut cols: [Vec<u32>; 4] = Default::default();
    let mut chunk_base = Vec::with_capacity(partials.len());
    for p in &mut partials {
        let blob_base = all_oids.len() as u32;
        chunk_base.push(cols[0].len() as u32);
        all_oids.append(&mut p.oids);
        flags.append(&mut p.flags);
        cols[0].extend(p.cols[0].iter().map(|b| b + blob_base));
        for (dst, src) in cols.iter_mut().zip(p.cols.iter_mut()).skip(1) {
            dst.append(src);
        }
    }
    let tokenized = partials.iter().map(|p| p.tokenized).sum();
    let nchunks = cols[0].len();

    let mut keys: Vec<u64> = partials
        .iter()
        .flat_map(|p| p.terms.keys().copied())
        .collect();
    keys.sort_unstable();
    keys.dedup();

    let mut w = SegWriter::create(dest, oid_len, &all_oids, &flags, &cols)?;
    drop(cols);
    let mut head = Vec::with_capacity(5);
    for h in keys {
        let mut prev = 0u32;
        let mut df = 0u32;
        for (p, base) in partials.iter_mut().zip(&chunk_base) {
            if let Some(tb) = p.terms.remove(&h) {
                head.clear();
                put_varint(&mut head, base + tb.first - prev);
                w.put(&head)?;
                w.put(&tb.bytes)?;
                prev = base + tb.last;
                df += tb.df;
            }
        }
        w.end_term(h, df)?;
    }
    w.finish()?;
    Ok((tokenized, nchunks))
}

// ---------------------------------------------------------------------------
// Liveness + compaction
// ---------------------------------------------------------------------------

/// Per-segment view needed to decide what is live right now.
struct SegLive {
    blob_live: Vec<bool>,
    /// bit per chunk
    bitmap: Vec<u8>,
    chunks: usize,
    live_chunks: u64,
    live_tokens: u64,
}

fn liveness(seg: &Segment, oids: &[String], live: &HashSet<&str>) -> Result<SegLive, String> {
    let blob_live: Vec<bool> = oids.iter().map(|o| live.contains(o.as_str())).collect();
    let blobs = seg.column(0)?;
    let lens = seg.column(3)?;
    let mut bitmap = vec![0u8; seg.nchunks.div_ceil(8)];
    let (mut live_chunks, mut live_tokens) = (0u64, 0u64);
    for (c, (&b, &l)) in blobs.iter().zip(&lens).enumerate() {
        if blob_live.get(b as usize).copied().unwrap_or(false) {
            bitmap[c / 8] |= 1 << (c % 8);
            live_chunks += 1;
            live_tokens += l as u64;
        }
    }
    Ok(SegLive {
        blob_live,
        bitmap,
        chunks: seg.nchunks,
        live_chunks,
        live_tokens,
    })
}

/// Merge every segment into one, keeping only live blobs (skipped-but-live
/// ones too, so they stay "known" and are never fetched again). Works purely
/// from the existing postings — each segment's postings region is read once,
/// front to back, because postings were written in dictionary order.
fn compact(
    dir: &Path,
    segs: &[Segment],
    lives: &[SegLive],
    oid_len: usize,
    dest_name: &str,
) -> Result<(), String> {
    let mut oids = Vec::new();
    let mut flags = Vec::new();
    let mut cols: [Vec<u32>; 4] = Default::default();
    let mut remaps: Vec<Vec<u32>> = Vec::with_capacity(segs.len());
    for (seg, live) in segs.iter().zip(lives) {
        let (seg_oids, seg_flags) = seg.blobs()?;
        let mut blob_map = vec![u32::MAX; seg.nblobs];
        for (i, oid) in seg_oids.into_iter().enumerate() {
            if live.blob_live[i] {
                blob_map[i] = oids.len() as u32;
                oids.push(oid);
                flags.push(seg_flags[i]);
            }
        }
        let c: Vec<Vec<u32>> = (0..4).map(|i| seg.column(i)).collect::<Result<_, _>>()?;
        let mut remap = vec![u32::MAX; seg.nchunks];
        for (chunk, slot) in remap.iter_mut().enumerate() {
            let nb = blob_map.get(c[0][chunk] as usize).copied().unwrap_or(u32::MAX);
            if nb != u32::MAX {
                *slot = cols[0].len() as u32;
                cols[0].push(nb);
                for i in 1..4 {
                    cols[i].push(c[i][chunk]);
                }
            }
        }
        remaps.push(remap);
    }
    let mut w = SegWriter::create(dir.join(dest_name), oid_len, &oids, &flags, &cols)?;
    drop(cols);

    struct Cursor {
        dict: Vec<DictEntry>,
        at: usize,
        reader: BufReader<File>,
        pos: u64,
    }
    let mut cursors = Vec::with_capacity(segs.len());
    for seg in segs {
        let mut f = seg.file.try_clone().map_err(|e| e.to_string())?;
        f.seek(SeekFrom::Start(seg.postings_off))
            .map_err(|e| e.to_string())?;
        cursors.push(Cursor {
            dict: seg.dictionary()?,
            at: 0,
            reader: BufReader::with_capacity(1 << 20, f),
            pos: seg.postings_off,
        });
    }
    let mut raw = Vec::new();
    let mut out = Vec::new();
    while let Some(h) = cursors
        .iter()
        .filter_map(|c| c.dict.get(c.at).map(|e| e.hash))
        .min()
    {
        out.clear();
        let mut prev = 0u32;
        let mut df = 0u32;
        for (c, remap) in cursors.iter_mut().zip(&remaps) {
            let Some(e) = c.dict.get(c.at).copied().filter(|e| e.hash == h) else {
                continue;
            };
            c.at += 1;
            if e.off != c.pos {
                c.reader
                    .seek(SeekFrom::Start(e.off))
                    .map_err(|e| e.to_string())?;
                c.pos = e.off;
            }
            raw.resize(e.len as usize, 0);
            c.reader
                .read_exact(&mut raw)
                .map_err(|e| format!("compact read: {e}"))?;
            c.pos += e.len as u64;
            decode_postings(&raw, |chunk, tf| {
                let n = remap.get(chunk as usize).copied().unwrap_or(u32::MAX);
                if n != u32::MAX {
                    put_varint(&mut out, n - prev);
                    put_varint(&mut out, tf);
                    prev = n;
                    df += 1;
                }
            })?;
        }
        if df > 0 {
            w.put(&out)?;
            w.end_term(h, df)?;
        }
    }
    w.finish()
}

/// Remove every file this index owns (never anything else that happens to
/// live in `dir`, and never the lock we hold).
fn wipe(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("seg-") || name.starts_with(MANIFEST) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// After a manifest commit: drop segments it no longer lists, liveness files
/// from older generations, and stray tmp files.
fn sweep(dir: &Path, m: &Manifest) {
    let keep: HashSet<String> = m
        .segments
        .iter()
        .flat_map(|s| [s.clone(), live_name(s, m.gen)])
        .collect();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let ours = name.starts_with("seg-") || name == "manifest.tmp";
        if ours && !keep.contains(&name) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn dir_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

/// Bring the index at `dir` up to date with `rev` (e.g. "HEAD") of the repo
/// at `repo`. Only blobs not already indexed are read and tokenized.
pub fn update(repo: &Path, dir: &Path, rev: &str) -> Result<IndexStats, String> {
    let t0 = Instant::now();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("create {}: {e}", dir.display()))?;
    let _lock = Lock::acquire(dir)?;
    let commit_raw = git_out(
        repo,
        &["rev-parse", "--verify", "--end-of-options", &format!("{rev}^{{commit}}")],
    )?;
    let commit = String::from_utf8_lossy(&commit_raw).trim().to_string();

    // Anything we can't trust is rebuilt, not reported.
    let mut old = read_manifest(dir);
    let mut segs: Vec<Segment> = Vec::new();
    if let Some(m) = &old {
        for name in &m.segments {
            match Segment::open(dir, name) {
                Ok(s) if dir.join(live_name(name, m.gen)).exists() => segs.push(s),
                _ => {
                    segs.clear();
                    old = None;
                    break;
                }
            }
        }
    }
    if old.is_none() {
        wipe(dir);
    }
    if let Some(m) = old.as_ref().filter(|m| m.commit == commit) {
        return Ok(IndexStats {
            commit,
            blobs_live: m.blobs_live,
            blobs_new: 0,
            chunks_new: 0,
            chunks_live: m.chunks_live as usize,
            segments: segs.len(),
            bytes_on_disk: dir_bytes(dir),
            skipped: m.skipped,
            elapsed_ms: t0.elapsed().as_millis() as u64,
            compacted: false,
        });
    }
    let gen = old.as_ref().map_or(1, |m| m.gen + 1);

    let entries = ls_tree(repo, &commit)?;
    let oid_len = entries.first().map_or(40, |e| e.0.len());
    if segs.iter().any(|s| s.oid_len != oid_len) {
        // Repo changed object format under us; nothing old is reusable.
        segs.clear();
        wipe(dir);
    }
    let live: HashSet<&str> = entries.iter().map(|e| e.0.as_str()).collect();

    let mut seg_blobs: Vec<(Vec<String>, Vec<u8>)> =
        segs.iter().map(|s| s.blobs()).collect::<Result<_, _>>()?;
    let known: HashSet<&str> = seg_blobs
        .iter()
        .flat_map(|(o, _)| o.iter().map(|s| s.as_str()))
        .collect();
    let mut fresh: Vec<String> = live
        .iter()
        .filter(|o| !known.contains(**o))
        .map(|o| o.to_string())
        .collect();
    drop(known);
    fresh.sort_unstable();

    let (mut blobs_new, mut chunks_new) = (0, 0);
    if !fresh.is_empty() {
        let name = seg_name(gen);
        (blobs_new, chunks_new) = build_segment(repo, dir.join(&name), oid_len, &fresh)?;
        let seg = Segment::open(dir, &name)?;
        seg_blobs.push(seg.blobs()?);
        segs.push(seg);
    }

    let mut lives: Vec<SegLive> = segs
        .iter()
        .zip(&seg_blobs)
        .map(|(s, (o, _))| liveness(s, o, &live))
        .collect::<Result<_, _>>()?;
    let total: usize = lives.iter().map(|l| l.chunks).sum();
    let live_chunks: u64 = lives.iter().map(|l| l.live_chunks).sum();
    let dead = total as f64 - live_chunks as f64;
    let mut compacted = false;
    if segs.len() > COMPACT_MAX_SEGMENTS || (total > 0 && dead / total as f64 > COMPACT_DEAD_RATIO)
    {
        // A distinct name so a crash mid-compaction leaves the old set intact.
        let name = format!("seg-{gen:08}c.seg");
        compact(dir, &segs, &lives, oid_len, &name)?;
        let seg = Segment::open(dir, &name)?;
        let (o, f) = seg.blobs()?;
        lives = vec![liveness(&seg, &o, &live)?];
        seg_blobs = vec![(o, f)];
        segs = vec![seg];
        compacted = true;
    }

    // Blob-level counts for the stats, from what the segments recorded.
    let mut blobs_live = 0;
    let mut skipped = 0;
    for ((oids, flags), l) in seg_blobs.iter().zip(&lives) {
        for (i, _) in oids.iter().enumerate().filter(|(i, _)| l.blob_live[*i]) {
            if flags[i] & FLAG_SKIPPED != 0 {
                skipped += 1;
            } else {
                blobs_live += 1;
            }
        }
    }

    for (s, l) in segs.iter().zip(&lives) {
        write_atomic(&dir.join(live_name(&s.name, gen)), &l.bitmap)?;
    }
    let skipped_oids: HashSet<&str> = seg_blobs
        .iter()
        .flat_map(|(o, f)| {
            o.iter()
                .zip(f)
                .filter(|(_, f)| **f & FLAG_SKIPPED != 0)
                .map(|(o, _)| o.as_str())
        })
        .collect();
    let m = Manifest {
        gen,
        commit: commit.clone(),
        segments: segs.iter().map(|s| s.name.clone()).collect(),
        chunks_live: lives.iter().map(|l| l.live_chunks).sum(),
        tokens_live: lives.iter().map(|l| l.live_tokens).sum(),
        blobs_live,
        skipped,
        entries: entries
            .iter()
            .filter(|(o, _)| !skipped_oids.contains(o.as_str()))
            .cloned()
            .collect(),
    };
    write_atomic(&dir.join(MANIFEST), m.render().as_bytes())?;
    drop(segs);
    sweep(dir, &m);

    Ok(IndexStats {
        commit,
        blobs_live,
        blobs_new,
        chunks_new,
        chunks_live: m.chunks_live as usize,
        segments: m.segments.len(),
        bytes_on_disk: dir_bytes(dir),
        skipped,
        elapsed_ms: t0.elapsed().as_millis() as u64,
        compacted,
    })
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

/// Top-k BM25 hits for a free-text query over the indexed (live) chunks.
pub fn search(dir: &Path, query: &str, k: usize) -> Result<Vec<CodeHit>, String> {
    // An update can sweep the files we're reading between our manifest read
    // and our segment opens; one retry sees the new generation.
    search_once(dir, query, k).or_else(|_| search_once(dir, query, k))
}

fn search_once(dir: &Path, query: &str, k: usize) -> Result<Vec<CodeHit>, String> {
    let text = std::fs::read_to_string(dir.join(MANIFEST))
        .map_err(|_| "code index not built yet (run update)".to_string())?;
    let (m, entries_at) =
        Manifest::parse(&text, false).ok_or("code index manifest is corrupt (run update)")?;
    let mut terms: Vec<u64> = tokenize(query).iter().map(|t| term_hash(t)).collect();
    terms.sort_unstable();
    terms.dedup();
    if terms.is_empty() || k == 0 || m.chunks_live == 0 {
        return Ok(Vec::new());
    }
    let n = m.chunks_live as f32;
    let avgdl = (m.tokens_live as f32 / n).max(1.0);

    struct Open {
        seg: Segment,
        lens: Vec<u32>,
        bitmap: Vec<u8>,
    }
    let mut open = Vec::with_capacity(m.segments.len());
    for name in &m.segments {
        let seg = Segment::open(dir, name)?;
        let bitmap = std::fs::read(dir.join(live_name(name, m.gen)))
            .map_err(|e| format!("liveness for {name}: {e}"))?;
        if bitmap.len() != seg.nchunks.div_ceil(8) {
            return Err(format!("liveness for {name}: wrong size"));
        }
        let lens = seg.column(3)?;
        open.push(Open { seg, lens, bitmap });
    }

    // Live postings per term per segment; df is counted from live postings
    // only, so dead chunks never skew idf.
    let mut scores: Vec<Vec<f32>> = vec![Vec::new(); open.len()];
    let mut touched: Vec<Vec<u32>> = vec![Vec::new(); open.len()];
    let mut per_seg: Vec<Vec<(u32, u32)>> = vec![Vec::new(); open.len()];
    for &h in &terms {
        let mut df = 0usize;
        for (o, posts) in open.iter().zip(per_seg.iter_mut()) {
            posts.clear();
            let Some(e) = o.seg.find(h)? else { continue };
            let raw = read_at(&o.seg.file, e.off, e.len as usize)?;
            let mut bad = false;
            decode_postings(&raw, |c, tf| {
                let ci = c as usize;
                if ci >= o.seg.nchunks {
                    bad = true;
                } else if o.bitmap[ci / 8] & (1 << (ci % 8)) != 0 {
                    posts.push((c, tf));
                }
            })?;
            if bad {
                return Err(format!("{}: corrupt postings", o.seg.name));
            }
            df += posts.len();
        }
        if df == 0 {
            continue;
        }
        let idf = bm25_idf(n, df as f32);
        for (si, posts) in per_seg.iter().enumerate() {
            if posts.is_empty() {
                continue;
            }
            let sc = &mut scores[si];
            if sc.is_empty() {
                sc.resize(open[si].seg.nchunks, 0.0);
            }
            let lens = &open[si].lens;
            for &(c, tf) in posts {
                let slot = &mut sc[c as usize];
                if *slot == 0.0 {
                    touched[si].push(c);
                }
                *slot += bm25_term(idf, tf as f32, lens[c as usize] as f32, avgdl);
            }
        }
    }

    let mut cands: Vec<(f32, usize, u32)> = Vec::new();
    for (si, t) in touched.iter().enumerate() {
        cands.extend(t.iter().map(|&c| (scores[si][c as usize], si, c)));
    }
    let by_score = |a: &(f32, usize, u32), b: &(f32, usize, u32)| {
        b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2))
    };
    if cands.len() > k {
        cands.select_nth_unstable_by(k - 1, by_score);
        cands.truncate(k);
    }
    cands.sort_unstable_by(by_score);

    let mut hits = Vec::with_capacity(cands.len());
    for (score, si, c) in cands {
        let seg = &open[si].seg;
        let blob = seg.cell(0, c as usize)? as usize;
        hits.push(CodeHit {
            path: String::new(),
            start_line: seg.cell(1, c as usize)? as usize,
            end_line: seg.cell(2, c as usize)? as usize,
            score,
            blob: seg.oid(blob)?,
        });
    }
    // Map blob → path by scanning the manifest's path list once. When one
    // blob sits at several paths (vendored copies), the first in tree order
    // is reported.
    let want: HashSet<&str> = hits.iter().map(|h| h.blob.as_str()).collect();
    let mut paths: HashMap<&str, String> = HashMap::new();
    for line in text[entries_at..].lines() {
        let Some((oid, path)) = line.split_once(' ') else {
            continue;
        };
        if want.contains(oid) && !paths.contains_key(oid) {
            paths.insert(oid, unescape_path(path));
            if paths.len() == want.len() {
                break;
            }
        }
    }
    for h in &mut hits {
        h.path = paths.get(h.blob.as_str()).cloned().unwrap_or_default();
    }
    hits.retain(|h| !h.path.is_empty());
    Ok(hits)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "heddle-gloss-idx-{tag}-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mk scratch");
        p
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn commit_all(repo: &Path, msg: &str) {
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-q", "--allow-empty", "-m", msg]);
    }

    /// 120 lines of filler with one distinctive word on `line` (1-based).
    fn body_with(word: &str, line: usize) -> String {
        (1..=120)
            .map(|i| {
                if i == line {
                    format!("let marker = {word};\n")
                } else {
                    format!("filler line number {i}\n")
                }
            })
            .collect()
    }

    fn repo(tag: &str) -> PathBuf {
        let r = scratch(tag);
        git(&r, &["init", "-q"]);
        std::fs::create_dir_all(r.join("src")).unwrap();
        std::fs::write(r.join("src/alpha.rs"), body_with("zebraQuokka", 75)).unwrap();
        std::fs::write(r.join("src/beta.rs"), body_with("platypus_wombat", 10)).unwrap();
        std::fs::write(r.join("README"), "nothing to see here\n").unwrap();
        commit_all(&r, "init");
        r
    }

    #[test]
    fn tokenize_splits_identifiers_and_keeps_the_whole() {
        let t = tokenize("parseHTTPResponse(spin_lock_irqsave, x, Ext4Inode) a");
        for want in [
            "parsehttpresponse",
            "parse",
            "http",
            "response",
            "spin_lock_irqsave",
            "spin",
            "lock",
            "irqsave",
            "ext4inode",
            "ext4",
            "inode",
        ] {
            assert!(t.contains(&want.to_string()), "missing {want} in {t:?}");
        }
        assert!(!t.iter().any(|s| s == "x" || s == "a"), "short tokens dropped: {t:?}");
        // A plain word is emitted once, not as whole + identical part.
        assert_eq!(tokenize("hello"), vec!["hello"]);
        assert!(tokenize(&"y".repeat(65)).is_empty());
        assert!(tokenize("__ ___").is_empty());
    }

    #[test]
    fn bm25_rank_orders_by_relevance() {
        let docs: Vec<Vec<String>> = ["lock the door", "door door door open", "nothing here"]
            .iter()
            .map(|d| tokenize(d))
            .collect();
        let r = bm25_rank(&docs, &tokenize("door"));
        assert_eq!(r.iter().map(|x| x.0).collect::<Vec<_>>(), vec![1, 0]);
        assert!(bm25_rank(&docs, &tokenize("absent")).is_empty());
    }

    #[test]
    fn chunks_prefer_blank_line_breaks() {
        let mut s = String::new();
        for i in 1..=120 {
            s.push_str(if i == 45 { "\n" } else { "x\n" });
        }
        let spans = chunk_spans(&s);
        assert_eq!((spans[0].2, spans[0].3), (1, 45));
        assert_eq!((spans[1].2, spans[1].3), (46, 95));
        assert_eq!(spans.last().unwrap().3, 120);
    }

    #[test]
    fn index_search_incremental_delete() {
        let r = repo("main");
        let idx = scratch("main-idx");
        let st = update(&r, &idx, "HEAD").unwrap();
        assert_eq!(st.blobs_new, 3);
        assert_eq!(st.blobs_live, 3);
        assert_eq!(indexed_commit(&idx).as_deref(), Some(st.commit.as_str()));

        let hits = search(&idx, "zebra quokka", 5).unwrap();
        assert_eq!(hits[0].path, "src/alpha.rs");
        assert!(hits[0].start_line <= 75 && 75 <= hits[0].end_line, "{:?}", hits[0]);
        let hits = search(&idx, "platypus_wombat", 5).unwrap();
        assert_eq!(hits[0].path, "src/beta.rs");
        assert!(hits[0].start_line <= 10 && 10 <= hits[0].end_line);

        // No change → nothing read.
        let st = update(&r, &idx, "HEAD").unwrap();
        assert_eq!(st.blobs_new, 0);

        // One file changes → exactly one blob tokenized.
        std::fs::write(r.join("src/alpha.rs"), body_with("narwhalOctopus", 30)).unwrap();
        commit_all(&r, "edit");
        let st = update(&r, &idx, "HEAD").unwrap();
        assert_eq!(st.blobs_new, 1);
        assert!(search(&idx, "zebraquokka", 5).unwrap().is_empty());
        let hits = search(&idx, "narwhal octopus", 5).unwrap();
        assert_eq!(hits[0].path, "src/alpha.rs");
        assert!(hits[0].start_line <= 30 && 30 <= hits[0].end_line);

        // Deleting a file drops its hits.
        std::fs::remove_file(r.join("src/beta.rs")).unwrap();
        commit_all(&r, "rm");
        let st = update(&r, &idx, "HEAD").unwrap();
        assert_eq!(st.blobs_new, 0);
        assert!(search(&idx, "platypus_wombat", 5).unwrap().is_empty());

        // Going back to the first commit: old blobs revive. (On a repo this
        // small the edit+delete pushed dead chunks past 40%, so compaction may
        // already have dropped them — then they're simply read again.)
        let st = update(&r, &idx, "HEAD~2").unwrap();
        assert!(st.blobs_new <= 2);
        assert_eq!(search(&idx, "platypus_wombat", 5).unwrap()[0].path, "src/beta.rs");
    }

    #[test]
    fn compaction_matches_a_fresh_build() {
        let r = repo("compact");
        let idx = scratch("compact-idx");
        update(&r, &idx, "HEAD").unwrap();
        let mut saw_compaction = false;
        for i in 0..14 {
            std::fs::write(
                r.join(format!("f{i}.txt")),
                format!("shared common words\nunique{i} token\n"),
            )
            .unwrap();
            if i % 3 == 0 {
                std::fs::write(r.join("src/alpha.rs"), body_with(&format!("rev{i}"), 40)).unwrap();
            }
            commit_all(&r, &format!("c{i}"));
            let st = update(&r, &idx, "HEAD").unwrap();
            saw_compaction |= st.compacted;
            assert!(st.segments <= COMPACT_MAX_SEGMENTS);
        }
        assert!(saw_compaction, "segments or dead ratio should have forced a compaction");
        let fresh = scratch("compact-fresh");
        let st = update(&r, &fresh, "HEAD").unwrap();
        assert_eq!(st.segments, 1);
        for q in ["shared common", "unique7 token", "rev12", "filler line", "platypus"] {
            // k large enough to take every match: equal-score ties may
            // order differently across segment layouts.
            let a = search(&idx, q, 100).unwrap();
            let b = search(&fresh, q, 100).unwrap();
            let key = |h: &Vec<CodeHit>| {
                let mut v: Vec<_> = h
                    .iter()
                    .map(|h| (h.path.clone(), h.start_line, h.end_line, (h.score * 1e4) as i64))
                    .collect();
                v.sort();
                v
            };
            assert_eq!(key(&a), key(&b), "query {q}");
            assert!(!a.is_empty(), "query {q} found nothing");
        }
    }

    #[test]
    fn corrupt_manifest_rebuilds() {
        let r = repo("corrupt");
        let idx = scratch("corrupt-idx");
        update(&r, &idx, "HEAD").unwrap();
        std::fs::write(idx.join(MANIFEST), "garbage\x00\n").unwrap();
        assert!(indexed_commit(&idx).is_none());
        assert!(search(&idx, "zebraquokka", 3).is_err());
        let st = update(&r, &idx, "HEAD").unwrap();
        assert_eq!(st.blobs_new, 3, "rebuilt from scratch");
        assert_eq!(search(&idx, "zebraquokka", 3).unwrap()[0].path, "src/alpha.rs");
        // A truncated segment is rebuilt too.
        let m = read_manifest(&idx).unwrap();
        let seg = idx.join(&m.segments[0]);
        let len = std::fs::metadata(&seg).unwrap().len();
        OpenOptions::new().write(true).open(&seg).unwrap().set_len(len / 2).unwrap();
        std::fs::write(r.join("new.txt"), "fresh content\n").unwrap();
        commit_all(&r, "more");
        let st = update(&r, &idx, "HEAD").unwrap();
        assert_eq!(st.blobs_new, 4);
        assert_eq!(search(&idx, "zebraquokka", 3).unwrap()[0].path, "src/alpha.rs");
    }

    #[test]
    fn binary_blobs_are_skipped_and_remembered() {
        let r = repo("bin");
        let mut bin = b"binarymagicword ".to_vec();
        bin.extend_from_slice(&[0, 1, 2, 3]);
        std::fs::write(r.join("blob.bin"), &bin).unwrap();
        commit_all(&r, "bin");
        let idx = scratch("bin-idx");
        let st = update(&r, &idx, "HEAD").unwrap();
        assert_eq!(st.skipped, 1);
        assert_eq!(st.blobs_live, 3);
        assert!(search(&idx, "binarymagicword", 3).unwrap().is_empty());
        std::fs::write(r.join("x.txt"), "x content\n").unwrap();
        commit_all(&r, "x");
        let st = update(&r, &idx, "HEAD").unwrap();
        assert_eq!(st.blobs_new, 1, "the binary blob is not re-read");
        assert_eq!(st.skipped, 1);
    }

    #[test]
    fn held_lock_refuses_and_stale_lock_is_stolen() {
        let r = repo("lock");
        let idx = scratch("lock-idx");
        std::fs::write(idx.join("lock"), "1").unwrap();
        assert_eq!(update(&r, &idx, "HEAD").unwrap_err(), "index update already running");
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        File::options()
            .write(true)
            .open(idx.join("lock"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(update(&r, &idx, "HEAD").is_ok());
        assert!(!idx.join("lock").exists());
    }
}

//! Where a note points, and whether the code under it is still the code it
//! was written about.
//!
//! An anchor is three things at once: a **symbol path** (`Heddle::stitch`)
//! when the language is one we can outline, a **line range** as of the last
//! time it was anchored, and a **fingerprint** of the span — a hash of the
//! whole thing plus per-line hashes. Resolution tries them in order of
//! confidence:
//!
//! 1. the symbol still exists → that is the span (it may have moved; fine);
//! 2. the exact span text exists somewhere → moved, still fresh;
//! 3. a window that shares most of the span's lines exists → **stale**: the
//!    code was edited under the note, so the note may be wrong;
//! 4. nothing close → **orphaned**: the code it described is gone.
//!
//! The fingerprint ignores leading/trailing whitespace and blank lines, so a
//! reindent or an added blank line does not mark a note stale, but any real
//! token change does. That is the honest line: whitespace never changes what
//! code does; anything else might.
//!
//! The outliner is a deliberately small heuristic (brace-matching for C-like
//! languages, indentation for Python), not tree-sitter: tree-sitter means a
//! C toolchain plus one grammar crate per language in a crate whose whole
//! dependency list is serde, sha2 and hex. When the outliner cannot find a
//! symbol, the line fingerprint still re-finds the code — a missed symbol
//! costs precision, never correctness, because staleness is decided by the
//! content hash, not by the symbol.

use serde::{Deserialize, Serialize};

use crate::store::content_hash;

/// Per-line fingerprints kept for fuzzy re-finding. A span longer than this
/// still anchors (by symbol or exact hash); only the fuzzy step sees a prefix.
pub const MAX_LINE_HASHES: usize = 400;
/// Share of the original lines a window must keep to count as "the same code,
/// edited" rather than "gone".
pub const FUZZY_THRESHOLD: f32 = 0.5;
/// Spans shorter than this (in non-blank lines) are never relocated by
/// content: short lines repeat too often to say a copy is "the same code".
pub const MIN_RELOCATE_LINES: usize = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Anchor {
    /// Repo-relative path, `/`-separated.
    pub path: String,
    /// Symbol path (`Heddle::stitch`, `tests::rebuilds`), when anchored to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// 1-based inclusive lines as of the last (re)anchoring. `start == 0`
    /// means a file-level note: tied to the file, not to any span.
    pub start: usize,
    pub end: usize,
    /// First 16 hex of sha256 over the normalized span.
    #[serde(default)]
    pub span_hash: String,
    /// Normalized per-line hashes (capped at [`MAX_LINE_HASHES`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<u32>,
    /// Short spans only: hashes of the nearest non-blank line before and
    /// after (0 = none). A one-line span can only be re-found elsewhere if
    /// its neighbors came with it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ctx: Vec<u32>,
}

impl Anchor {
    pub fn is_file_level(&self) -> bool {
        self.start == 0
    }

    /// `path`, `path:12-30` or `path::Sym::name` — the same syntax the CLI takes.
    pub fn label(&self) -> String {
        match (&self.symbol, self.is_file_level()) {
            (Some(s), _) => format!("{}::{s}", self.path),
            (None, true) => self.path.clone(),
            (None, false) if self.start == self.end => format!("{}:{}", self.path, self.start),
            (None, false) => format!("{}:{}-{}", self.path, self.start, self.end),
        }
    }
}

/// What resolution found.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Status {
    /// The anchored code is byte-for-byte (modulo whitespace) what the note
    /// was written or last verified against.
    Fresh,
    /// Same code, different place (lines shifted, or the file was renamed).
    Moved { from: usize },
    /// The code under the note changed since it was written or verified.
    /// The note may no longer be true.
    Stale,
    /// The anchored code is gone.
    Orphaned,
}

impl Status {
    pub fn word(&self) -> &'static str {
        match self {
            Status::Fresh => "fresh",
            Status::Moved { .. } => "moved",
            Status::Stale => "STALE",
            Status::Orphaned => "ORPHANED",
        }
    }
    /// Fresh or moved: safe to present as describing the current code.
    pub fn trustworthy(&self) -> bool {
        matches!(self, Status::Fresh | Status::Moved { .. })
    }
}

/// Where a note lands in the current content, and how much to trust it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Resolution {
    pub status: Status,
    /// Current 1-based inclusive span (file-level: 0..0; orphaned: last known).
    pub start: usize,
    pub end: usize,
}

// ---------------------------------------------------------------------------
// Fingerprints
// ---------------------------------------------------------------------------

fn line_hash(line: &str) -> u32 {
    // FNV-1a 32 over the trimmed line: fast, and collisions only cost a
    // little fuzzy-match precision — the exact check uses sha256.
    let mut h: u32 = 0x811c_9dc5;
    for b in line.trim().bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

fn span_hash(lines: &[&str]) -> String {
    let norm: Vec<&str> = lines.iter().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
    content_hash(norm.join("\n").as_bytes())[..16].to_string()
}

/// (original line index, hash) for every non-blank line.
fn significant(lines: &[&str]) -> Vec<(usize, u32)> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| (i, line_hash(l)))
        .collect()
}

/// Build an anchor over `start..=end` (1-based) of `content`.
pub fn anchor_span(path: &str, content: &str, start: usize, end: usize, symbol: Option<String>) -> Anchor {
    let lines: Vec<&str> = content.lines().collect();
    let mut s = start.clamp(1, lines.len().max(1));
    let mut e = end.clamp(s, lines.len().max(1));
    // Shrink to the first/last non-blank line: resolution lands spans on
    // significant lines, so a span that started on a blank line would read
    // as "moved" forever without anything having moved.
    let blank = |i: usize| lines.get(i - 1).is_none_or(|l| l.trim().is_empty());
    while s < e && blank(s) {
        s += 1;
    }
    while e > s && blank(e) {
        e -= 1;
    }
    let span = lines.get(s - 1..e).unwrap_or(&[]);
    let hashes: Vec<u32> = significant(span).into_iter().map(|(_, h)| h).take(MAX_LINE_HASHES).collect();
    let ctx = if hashes.len() < MIN_RELOCATE_LINES { context(&lines, s, e) } else { vec![] };
    Anchor { path: path.to_string(), symbol, start: s, end: e, span_hash: span_hash(span), lines: hashes, ctx }
}

pub fn anchor_file(path: &str) -> Anchor {
    Anchor { path: path.to_string(), symbol: None, start: 0, end: 0, span_hash: String::new(), lines: vec![], ctx: vec![] }
}

/// [before, after] neighbor hashes around 1-based `s..=e`.
fn context(lines: &[&str], s: usize, e: usize) -> Vec<u32> {
    let before = lines[..s.saturating_sub(1).min(lines.len())].iter().rev().find(|l| !l.trim().is_empty());
    let after = lines.get(e..).unwrap_or(&[]).iter().find(|l| !l.trim().is_empty());
    vec![before.map_or(0, |l| line_hash(l)), after.map_or(0, |l| line_hash(l))]
}

/// Re-find `a` in `content`. Pure: never writes anything.
pub fn resolve(a: &Anchor, content: &str) -> Resolution {
    if a.is_file_level() {
        return Resolution { status: Status::Fresh, start: 0, end: 0 };
    }
    let lines: Vec<&str> = content.lines().collect();
    let moved_or_fresh = |s: usize| if s == a.start { Status::Fresh } else { Status::Moved { from: a.start } };

    // 1. Symbol.
    if let Some(sym) = &a.symbol {
        if let Some(found) = outline(&a.path, content).into_iter().find(|s| &s.path == sym) {
            let span = &lines[found.start - 1..found.end];
            let status = if span_hash(span) == a.span_hash { moved_or_fresh(found.start) } else { Status::Stale };
            return Resolution { status, start: found.start, end: found.end };
        }
    }

    let sig = significant(&lines);
    let k = a.lines.len();
    if k == 0 {
        return Resolution { status: Status::Orphaned, start: a.start, end: a.end };
    }

    // 2. Exact: a run of k significant lines with the same hashes, then
    //    confirmed by the sha256 span hash.
    let mut matches: Vec<(usize, usize)> = Vec::new();
    if sig.len() >= k {
        for w in 0..=sig.len() - k {
            if sig[w].1 != a.lines[0] || sig[w..w + k].iter().map(|x| x.1).ne(a.lines.iter().copied()) {
                continue;
            }
            let (s, mut e) = (sig[w].0 + 1, sig[w + k - 1].0 + 1);
            if k == MAX_LINE_HASHES {
                // Capped fingerprint: extend to the original span length.
                e = (s + (a.end - a.start)).min(lines.len());
            }
            if span_hash(&lines[s - 1..e]) == a.span_hash {
                matches.push((s, e));
            }
        }
    }
    if let Some(&(s, e)) = matches.iter().find(|(s, _)| *s == a.start) {
        return Resolution { status: Status::Fresh, start: s, end: e };
    }
    // The code is no longer where it was. A copy elsewhere counts as "moved"
    // only when it is unmistakably the same code: unique in the file and
    // more than a couple of lines. `return Ok(());` appearing again thirty
    // lines down is not where a note about the edited line went — calling
    // that "moved, fresh" would put a stale note above unrelated code.
    let distinctive = k >= MIN_RELOCATE_LINES;
    let candidates: Vec<(usize, usize)> = if distinctive {
        matches.clone()
    } else {
        // Short span: only copies whose neighbors also match.
        matches.iter().copied().filter(|&(s, e)| a.ctx.len() == 2 && context(&lines, s, e) == a.ctx).collect()
    };
    if candidates.len() == 1 {
        let (s, e) = candidates[0];
        return Resolution { status: moved_or_fresh(s), start: s, end: e };
    }
    let here = |status| {
        let s = a.start.min(lines.len().max(1));
        Resolution { status, start: s, end: a.end.clamp(s, lines.len().max(1)) }
    };
    if !candidates.is_empty() || !matches.is_empty() || !distinctive {
        // Ambiguous or too short to re-find by content: the honest answer
        // is "what was here changed" — stale at the old spot (orphaned if
        // the file no longer reaches that far).
        return if a.start <= lines.len() { here(Status::Stale) } else { here(Status::Orphaned) };
    }

    // 3. Fuzzy: the window of k significant lines sharing the most line
    //    hashes (multiset intersection), sliding in O(n).
    use std::collections::HashMap;
    let mut want: HashMap<u32, i32> = HashMap::new();
    for h in &a.lines {
        *want.entry(*h).or_default() += 1;
    }
    let win = k.min(sig.len());
    if win == 0 {
        return Resolution { status: Status::Orphaned, start: a.start, end: a.end };
    }
    let mut have: HashMap<u32, i32> = HashMap::new();
    let mut shared = 0i32;
    let add = |h: u32, have: &mut HashMap<u32, i32>, shared: &mut i32, d: i32| {
        let c = have.entry(h).or_default();
        let w = *want.get(&h).unwrap_or(&0);
        let before = (*c).min(w);
        *c += d;
        *shared += (*c).min(w) - before;
    };
    let mut top: Option<(i32, usize, usize)> = None; // (shared, window start, distance)
    for i in 0..sig.len() {
        add(sig[i].1, &mut have, &mut shared, 1);
        if i >= win {
            add(sig[i - win].1, &mut have, &mut shared, -1);
        }
        if i + 1 >= win {
            let ws = i + 1 - win;
            let d = (sig[ws].0 + 1).abs_diff(a.start);
            if top.is_none_or(|t| shared > t.0 || (shared == t.0 && d < t.2)) {
                top = Some((shared, ws, d));
            }
        }
    }
    if let Some((sh, ws, _)) = top {
        if sh as f32 / k as f32 >= FUZZY_THRESHOLD {
            let (s, e) = (sig[ws].0 + 1, sig[ws + win - 1].0 + 1);
            return Resolution { status: Status::Stale, start: s, end: e };
        }
    }
    Resolution { status: Status::Orphaned, start: a.start, end: a.end }
}

// ---------------------------------------------------------------------------
// Outline — a small, honest symbol finder
// ---------------------------------------------------------------------------

/// A named span in a file.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Symbol {
    /// `Container::name`, e.g. `Heddle::stitch`.
    pub path: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum Family {
    Braces,
    Indent,
    None,
}

fn family(path: &str) -> Family {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "rs" | "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx"
        | "go" | "java" | "kt" | "kts" | "swift" | "cs" | "php" | "scala" | "dart" | "zig" => Family::Braces,
        "py" | "pyi" => Family::Indent,
        _ => Family::None,
    }
}

/// Leading keywords/modifiers to peel off a declaration line.
const MODIFIERS: &[&str] = &[
    "pub", "pub(crate)", "pub(super)", "pub(self)", "export", "default", "async", "unsafe", "const", "extern",
    "static", "public", "private", "protected", "internal", "abstract", "final", "override", "open", "inline",
    "virtual", "sealed", "data", "suspend", "declare",
];
/// Keywords that introduce a named definition, and whether it is a container
/// (its members get its name as a prefix).
const DEF_KEYWORDS: &[(&str, bool)] = &[
    ("fn", false),
    ("func", false),
    ("function", false),
    ("fun", false),
    ("def", false),
    ("impl", true),
    ("struct", true),
    ("enum", true),
    ("trait", true),
    ("mod", true),
    ("class", true),
    ("interface", true),
    ("object", true),
    ("namespace", true),
    ("union", true),
    ("type", false),
];

fn ident(s: &str) -> Option<&str> {
    let end = s.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).unwrap_or(s.len());
    (end > 0).then(|| &s[..end])
}

/// Name a declaration line: Some((name, is_container)).
fn declaration(line: &str) -> Option<(String, bool)> {
    let mut rest = line.trim_start();
    if rest.starts_with("//") || rest.starts_with('#') || rest.starts_with('*') || rest.starts_with("/*") {
        return None;
    }
    // Rust `extern "C"` and friends.
    loop {
        let word = rest.split_whitespace().next()?;
        if MODIFIERS.contains(&word) || word.starts_with("pub(") {
            rest = rest[word.len()..].trim_start();
            if rest.starts_with('"') {
                rest = rest[1..].split_once('"').map(|(_, r)| r.trim_start())?;
            }
        } else {
            break;
        }
    }
    let word = rest.split(|c: char| c.is_whitespace() || c == '<' || c == '(').next()?;
    if let Some(&(kw, container)) = DEF_KEYWORDS.iter().find(|(k, _)| *k == word) {
        let mut tail = rest[kw.len()..].trim_start();
        if kw == "impl" {
            // impl<T> Trait<T> for Type<T>  →  Type
            if tail.starts_with('<') {
                tail = skip_generics(tail);
            }
            if let Some((_, after)) = tail.split_once(" for ") {
                tail = after.trim_start();
            }
        }
        if kw == "func" && tail.starts_with('(') {
            // Go method: func (r *Recv) Name(...)
            let close = tail.find(')')?;
            let recv = tail[1..close].split_whitespace().last().unwrap_or("").trim_start_matches('*');
            let name = ident(tail[close + 1..].trim_start())?;
            let recv = ident(recv).unwrap_or("");
            return Some((if recv.is_empty() { name.to_string() } else { format!("{recv}::{name}") }, false));
        }
        if kw == "function" {
            tail = tail.trim_start_matches('*').trim_start();
        }
        let name = ident(tail.trim_start_matches('&'))?;
        if matches!(kw, "struct" | "enum" | "union" | "class" | "interface" | "object") {
            // A type DEFINITION is followed by its body, generics, a tuple
            // or a base list — not by `*p`, a variable name or `=`. Without
            // this, C's `struct foo *p = …;` (and every parameter line of a
            // multi-line signature) reads as a definition of `foo`.
            let after = tail[tail.find(name).unwrap_or(0) + name.len()..].trim_start();
            let defines = after.is_empty()
                || after.starts_with(['{', '<', '(', ':'])
                || ["where", "extends", "implements", "final", "sealed", "open"].iter().any(|k| after.starts_with(k));
            if !defines {
                return None;
            }
        }
        return Some((name.to_string(), container));
    }
    // JS/TS `const name = (...) =>` / `= function`.
    if line.contains("=>") || line.contains("= function") {
        let t = line.trim_start();
        for kw in ["const ", "let ", "var ", "export const ", "export let "] {
            if let Some(r) = t.strip_prefix(kw) {
                return ident(r).map(|n| (n.to_string(), false));
            }
        }
    }
    None
}

fn skip_generics(s: &str) -> &str {
    let mut depth = 0;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return s[i + 1..].trim_start();
                }
            }
            _ => {}
        }
    }
    s
}

/// Is this col-0 line a C-style function definition head? `int foo(...)`,
/// `static void *bar(int x)` — has an identifier immediately before `(`,
/// is not a statement, and is not a control keyword.
fn c_function(line: &str, prev: &str) -> Option<String> {
    if line.starts_with(char::is_whitespace) || line.trim_end().ends_with(';') || line.starts_with('#') || line.starts_with("/*") || line.starts_with('*') {
        return None;
    }
    let paren = line.find('(')?;
    let head = line[..paren].trim_end();
    if head.contains('=') || head.is_empty() {
        return None;
    }
    let name_start = head
        .char_indices()
        .rev()
        .find(|(_, c)| !(c.is_alphanumeric() || *c == '_'))
        .map_or(0, |(i, c)| i + c.len_utf8());
    let name = &head[name_start..];
    if name.is_empty() {
        return None;
    }
    if name_start == 0 {
        // `name(` alone at col 0 is a definition only in kernel/BSD style,
        // where the return type sits on the line above (`static int\n
        // foo(void)`). Otherwise it's a bare call.
        let p = prev.trim_end();
        let type_line = !p.is_empty()
            && !p.starts_with(char::is_whitespace)
            && !p.starts_with('#')
            && !p.ends_with([';', '}', ')', ',', '{', '\\'])
            && !p.contains('(')
            && !p.starts_with("//")
            && !p.starts_with("/*")
            && !p.starts_with('*');
        if !type_line {
            return None;
        }
    }
    if ["if", "for", "while", "switch", "return", "sizeof", "do"].contains(&name) {
        return None;
    }
    Some(name.to_string())
}

/// Count `{`/`}` on a line, skipping string/char literals and `//` comments.
/// Good enough for outlining; block comments are rare inside signatures.
fn brace_delta(line: &str) -> (i32, bool) {
    let mut d = 0;
    let mut opened = false;
    let mut chars = line.chars().peekable();
    let mut in_str: Option<char> = None;
    while let Some(c) = chars.next() {
        if let Some(q) = in_str {
            if c == '\\' {
                chars.next();
            } else if c == q {
                in_str = None;
            }
            continue;
        }
        match c {
            '"' | '`' => in_str = Some(c),
            '\'' => {
                // char literal 'x' or '\n'; Rust lifetimes ('a) have no close quote nearby.
                let look: String = chars.clone().take(3).collect();
                if look.starts_with('\\') || look.chars().nth(1) == Some('\'') {
                    in_str = Some('\'');
                }
            }
            '/' if chars.peek() == Some(&'/') => break,
            '{' => {
                d += 1;
                opened = true;
            }
            '}' => d -= 1,
            _ => {}
        }
    }
    (d, opened)
}

/// Named spans in `content`, outermost first, with container prefixes.
/// Duplicate paths (overloads, `#[cfg]` twins) get `#2`, `#3` suffixes so
/// every symbol path is unique within a file.
pub fn outline(path: &str, content: &str) -> Vec<Symbol> {
    let lines: Vec<&str> = content.lines().collect();
    let mut raw: Vec<(String, bool, usize, usize)> = Vec::new(); // name, container, start, end (1-based)
    match family(path) {
        Family::None => return vec![],
        Family::Indent => {
            for (i, l) in lines.iter().enumerate() {
                let t = l.trim_start();
                let indent = l.len() - t.len();
                let t2 = t.strip_prefix("async ").unwrap_or(t);
                let (kw, container) = if t2.starts_with("def ") {
                    ("def ", false)
                } else if t2.starts_with("class ") {
                    ("class ", true)
                } else {
                    continue;
                };
                let Some(name) = ident(&t2[kw.len()..]) else { continue };
                let mut end = i;
                for (j, m) in lines.iter().enumerate().skip(i + 1) {
                    let mt = m.trim_start();
                    if mt.is_empty() {
                        continue;
                    }
                    if m.len() - mt.len() <= indent {
                        break;
                    }
                    end = j;
                }
                // Include decorators directly above.
                let mut start = i;
                while start > 0 && lines[start - 1].trim_start().starts_with('@') {
                    start -= 1;
                }
                raw.push((name.to_string(), container, start + 1, end + 1));
            }
        }
        Family::Braces => {
            let is_c = matches!(
                path.rsplit('.').next().unwrap_or(""),
                "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh"
            );
            let mut i = 0;
            while i < lines.len() {
                let decl = declaration(lines[i]).or_else(|| if is_c { c_function(lines[i], if i > 0 { lines[i - 1] } else { "" }).map(|n| (n, false)) } else { None });
                let Some((name, container)) = decl else {
                    i += 1;
                    continue;
                };
                // Find the opening brace within a few lines (multi-line
                // signatures); a `;` first means a declaration, not a body.
                let mut depth = 0;
                let mut opened = false;
                let mut end = None;
                for (j, l) in lines.iter().enumerate().skip(i).take(400_000) {
                    let (d, o) = brace_delta(l);
                    if !opened && !o {
                        if l.trim_end().ends_with(';') || j > i + 12 {
                            break;
                        }
                        continue;
                    }
                    opened = true;
                    depth += d;
                    if depth <= 0 {
                        end = Some(j);
                        break;
                    }
                }
                if let Some(e) = end {
                    // Pull doc comments / attributes directly above into the span.
                    let mut start = i;
                    while start > 0 {
                        let p = lines[start - 1].trim_start();
                        if p.starts_with("///") || p.starts_with("#[") || p.starts_with("@") || p.starts_with("//!") {
                            start -= 1;
                        } else {
                            break;
                        }
                    }
                    raw.push((name, container, start + 1, e + 1));
                }
                i += 1; // nested definitions start inside — keep scanning
            }
        }
    }
    // Prefix with enclosing containers (innermost container whose span
    // strictly contains this one).
    let mut out: Vec<Symbol> = Vec::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (idx, (name, _, s, e)) in raw.iter().enumerate() {
        let mut chain: Vec<&str> = Vec::new();
        let mut cur = (*s, *e);
        loop {
            let parent = raw
                .iter()
                .enumerate()
                .filter(|(j, (_, c, ps, pe))| *j != idx && *c && *ps <= cur.0 && *pe >= cur.1 && (*ps, *pe) != cur)
                .min_by_key(|(_, (_, _, ps, pe))| pe - ps);
            match parent {
                Some((_, (pn, _, ps, pe))) => {
                    chain.push(pn);
                    cur = (*ps, *pe);
                }
                None => break,
            }
        }
        chain.reverse();
        chain.push(name);
        let mut p = chain.join("::");
        let n = seen.entry(p.clone()).or_default();
        *n += 1;
        if *n > 1 {
            p = format!("{p}#{n}");
        }
        out.push(Symbol { path: p, start: *s, end: *e });
    }
    out
}

/// The innermost symbol containing 1-based `line`.
pub fn symbol_at(path: &str, content: &str, line: usize) -> Option<Symbol> {
    outline(path, content)
        .into_iter()
        .filter(|s| s.start <= line && line <= s.end)
        .min_by_key(|s| s.end - s.start)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RS: &str = "\
use std::io;

/// A thing.
pub struct Loom {
    n: u32,
}

impl Loom {
    pub fn new() -> Self {
        Loom { n: 0 }
    }

    fn weave(&mut self, s: &str) -> u32 {
        if s == \"{\" {
            self.n += 1;
        }
        self.n
    }
}

fn main() {
    let _ = Loom::new();
}
";

    #[test]
    fn outlines_rust_with_containers() {
        let syms = outline("src/x.rs", RS);
        let names: Vec<&str> = syms.iter().map(|s| s.path.as_str()).collect();
        assert!(names.contains(&"Loom"), "{names:?}");
        assert!(names.contains(&"Loom::new"), "{names:?}");
        assert!(names.contains(&"Loom::weave"), "{names:?}");
        assert!(names.contains(&"main"), "{names:?}");
        let weave = syms.iter().find(|s| s.path == "Loom::weave").unwrap();
        assert_eq!((weave.start, weave.end), (13, 18), "the brace inside a string must not count");
        // The doc comment rides with the struct.
        assert_eq!(syms.iter().find(|s| s.path == "Loom").unwrap().start, 3);
        // impl block is itself a symbol and names `Loom` twice → unique suffix.
        assert!(names.contains(&"Loom#2"), "{names:?}");
    }

    #[test]
    fn outlines_python_and_c_and_go() {
        let py = "class A:\n    def f(self):\n        return 1\n\n    @staticmethod\n    def g():\n        pass\n\ndef top():\n    pass\n";
        let p: Vec<String> = outline("a.py", py).into_iter().map(|s| format!("{}:{}-{}", s.path, s.start, s.end)).collect();
        assert_eq!(p, vec!["A:1-7", "A::f:2-3", "A::g:5-7", "top:9-10"]);

        let c = "#include <x.h>\nstatic int helper(int a);\n\nstatic int\nhelper2(int a)\n{\n\treturn a;\n}\nint main(void) {\n\tif (x) {\n\t}\n\treturn 0;\n}\n";
        let p: Vec<String> = outline("a.c", c).into_iter().map(|s| s.path).collect();
        assert_eq!(p, vec!["helper2", "main"], "prototypes are not definitions");

        let go = "package x\n\nfunc (s *Server) Serve(c int) error {\n\treturn nil\n}\n\nfunc Free() {}\n";
        let p: Vec<String> = outline("a.go", go).into_iter().map(|s| s.path).collect();
        assert_eq!(p, vec!["Server::Serve", "Free"]);
    }

    fn sym_anchor(content: &str, sym: &str) -> Anchor {
        let s = outline("src/x.rs", content).into_iter().find(|s| s.path == sym).unwrap();
        anchor_span("src/x.rs", content, s.start, s.end, Some(sym.to_string()))
    }

    #[test]
    fn symbol_anchor_survives_moves_and_goes_stale_on_edit() {
        let a = sym_anchor(RS, "Loom::weave");
        assert_eq!(resolve(&a, RS).status, Status::Fresh);

        // Shift everything down by inserting code above: moved, still fresh.
        let shifted = RS.replacen("use std::io;\n", "use std::io;\nuse std::fs;\nconst X: u8 = 1;\n", 1);
        let r = resolve(&a, &shifted);
        assert_eq!(r.status, Status::Moved { from: 13 });
        assert_eq!((r.start, r.end), (15, 20));

        // Reindent + blank lines only: still fresh (whitespace never changes meaning).
        let ws = RS.replace("            self.n += 1;", "\n                self.n += 1;\n");
        assert!(resolve(&a, &ws).status.trustworthy(), "{:?}", resolve(&a, &ws));

        // A real token change inside the body: stale, at the symbol's place.
        let edited = RS.replace("self.n += 1;", "self.n += 2;");
        let r = resolve(&a, &edited);
        assert_eq!(r.status, Status::Stale);
        assert_eq!(r.start, 13);

        // Symbol deleted entirely: orphaned.
        let gone = RS.replace(
            "    fn weave(&mut self, s: &str) -> u32 {\n        if s == \"{\" {\n            self.n += 1;\n        }\n        self.n\n    }\n",
            "",
        );
        assert_eq!(resolve(&a, &gone).status, Status::Orphaned);
    }

    #[test]
    fn renamed_symbol_is_refound_by_content() {
        let a = sym_anchor(RS, "Loom::weave");
        // Rename only: the symbol lookup misses, the fuzzy step finds most
        // lines intact — so it is stale (the signature changed), not lost.
        let renamed = RS.replace("fn weave(", "fn spin(");
        let r = resolve(&a, &renamed);
        assert_eq!(r.status, Status::Stale);
        assert_eq!(r.start, 13);
    }

    #[test]
    fn line_anchor_moves_exactly_and_prefers_nearest_duplicate() {
        let body = "a\nb\nx = 1\nc\nd\nx = 1\ne\n";
        let a = anchor_span("f.txt", body, 6, 6, None);
        assert_eq!(resolve(&a, body).status, Status::Fresh);
        let moved = format!("new\n{body}");
        let r = resolve(&a, &moved);
        assert_eq!(r.status, Status::Moved { from: 6 });
        assert_eq!(r.start, 7, "the duplicate nearest the old spot wins");
    }

    #[test]
    fn edited_short_span_is_stale_not_moved_to_a_lookalike() {
        let body = "fn transfer() {\n    debit();\n    return Ok(());\n}\nfn ping() {\n    log();\n    return Ok(());\n}\n";
        let a = anchor_span("t.rs", body, 3, 3, None);
        assert_eq!(resolve(&a, body).status, Status::Fresh, "duplicate elsewhere, but still here");
        let edited = body.replacen("return Ok(());", "return Err(E::Retry);", 1);
        let r = resolve(&a, &edited);
        assert_eq!(r.status, Status::Stale, "{r:?}");
        assert_eq!(r.start, 3, "stays at the edited line, not the lookalike on line 7");
        // A one-liner that merely shifted is also not claimed as moved.
        let shifted = format!("// header\n{body}");
        let r = resolve(&a, &shifted);
        assert_eq!((r.status, r.start), (Status::Moved { from: 3 }, 4), "neighbors came along: a real move");
    }

    #[test]
    fn c_struct_uses_are_not_definitions() {
        let c = "struct cache {\n\tint n;\n};\n\nint lookup(struct trans *t,\n\t   struct fs_info *fs, int x)\n{\n\tstruct cache *c = get(fs);\n\tif (x) {\n\t\treturn 1;\n\t}\n\treturn 0;\n}\n";
        let p: Vec<String> = outline("x.c", c).into_iter().map(|s| format!("{}:{}-{}", s.path, s.start, s.end)).collect();
        assert_eq!(p, vec!["cache:1-3", "lookup:5-13"]);
    }

    #[test]
    fn non_ascii_before_a_paren_does_not_panic() {
        let c = "\u{a9}foo(bar) notice\n\u{a9}x\nint \u{e9}t\u{e9}(void) {\n}\n";
        let _ = outline("x.c", c);
    }

    #[test]
    fn spans_starting_on_blank_lines_are_not_phantom_moves() {
        let body = "a\n\n\nb\nc\n\n";
        let a = anchor_span("f.txt", body, 2, 6, None);
        assert_eq!((a.start, a.end), (4, 5));
        assert_eq!(resolve(&a, body).status, Status::Fresh);
    }

    #[test]
    fn file_level_never_stale() {
        let a = anchor_file("README.md");
        assert_eq!(resolve(&a, "anything").status, Status::Fresh);
        assert_eq!(a.label(), "README.md");
    }
}

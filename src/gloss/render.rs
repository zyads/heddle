//! Render a file with its notes interleaved as comments in that file's own
//! syntax — the "LLM copy" of the code, produced on read, never stored in
//! the human tree. Nothing to strip, because nothing was ever written there.

use super::anchor::{Resolution, Status};
use super::Note;

/// (line prefix, line suffix) for a comment in this file's language.
pub fn comment_style(path: &str) -> (&'static str, &'static str) {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "rs" | "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx"
        | "go" | "java" | "kt" | "kts" | "swift" | "cs" | "php" | "scala" | "dart" | "zig" | "proto"
        | "jsonc" | "groovy" | "gradle" | "sol" | "v" => ("// ", ""),
        "py" | "pyi" | "sh" | "bash" | "zsh" | "fish" | "rb" | "pl" | "r" | "toml" | "yaml" | "yml" | "nix"
        | "cmake" | "mk" | "conf" | "ini" | "cfg" | "tf" | "ps1" | "ex" | "exs" | "jl" | "dockerfile" => ("# ", ""),
        "sql" | "lua" | "hs" | "elm" | "ada" => ("-- ", ""),
        "lisp" | "el" | "clj" | "cljs" | "scm" | "rkt" => (";; ", ""),
        "css" | "scss" | "less" => ("/* ", " */"),
        "html" | "htm" | "xml" | "svg" | "md" | "markdown" | "vue" | "svelte" => ("<!-- ", " -->"),
        "erl" | "hrl" | "tex" => ("% ", ""),
        "vim" => ("\" ", ""),
        _ if matches!(name, "Makefile" | "Dockerfile" | "Kconfig" | "Containerfile") || name.starts_with("Kconfig") => {
            ("# ", "")
        }
        _ => ("// ", ""),
    }
}

/// The one-line header for a note: id, kind, author, commit, trust state.
pub fn header(n: &Note, r: &Resolution) -> String {
    let commit = n.verified.last().map(|v| v.commit.as_str()).unwrap_or(n.commit.as_str());
    let short = super::short(commit, 8);
    let when = if n.verified.is_empty() { "written" } else { "verified" };
    let state = match &r.status {
        Status::Fresh => "fresh".to_string(),
        Status::Moved { from } => format!("fresh, moved from L{from}"),
        Status::Stale => "STALE: code changed since — verify before trusting".to_string(),
        Status::Orphaned => "ORPHANED: the code it described is gone".to_string(),
    };
    format!("[gloss {} · {} · {} · {when} @{short} · {state}]", n.id, n.kind, n.author.by)
}

pub struct RenderOpts {
    /// Prefix code lines with their ORIGINAL line numbers (gloss lines get
    /// none), so edits an agent plans against this view map to the real file.
    pub numbers: bool,
    /// Include stale/orphaned notes (always clearly marked). Default true:
    /// hiding them would hide that the knowledge rotted.
    pub show_untrusted: bool,
}

/// Interleave notes into `content`. Notes are placed directly above the
/// first line of their resolved span, indented to match it; file-level and
/// orphaned notes go at the top.
pub fn render(path: &str, content: &str, notes: &[(Note, Resolution)], opts: &RenderOpts) -> String {
    let (pre, suf) = comment_style(path);
    let lines: Vec<&str> = content.lines().collect();
    let width = lines.len().max(1).to_string().len();
    let mut above: std::collections::BTreeMap<usize, Vec<&(Note, Resolution)>> = Default::default();
    for nr in notes {
        if !opts.show_untrusted && !nr.1.status.trustworthy() {
            continue;
        }
        let at = match nr.1.status {
            Status::Orphaned => 0,
            _ => nr.1.start,
        };
        above.entry(at.min(lines.len())).or_default().push(nr);
    }
    let mut out = String::with_capacity(content.len() + notes.len() * 120);
    let gutter = |out: &mut String| {
        if opts.numbers {
            out.push_str(&" ".repeat(width));
            out.push_str("  ┆ ");
        }
    };
    let emit = |out: &mut String, indent: &str, n: &Note, r: &Resolution| {
        gutter(out);
        out.push_str(&format!("{indent}{pre}{}{suf}\n", header(n, r)));
        for l in n.text.lines() {
            gutter(out);
            // Never let a note close the comment early (`*/`, `-->`).
            let safe = if suf.is_empty() { l.to_string() } else { l.replace(suf.trim(), "") };
            out.push_str(&format!("{indent}{pre}  {safe}{suf}\n"));
        }
    };
    if let Some(top) = above.get(&0) {
        for (n, r) in top {
            emit(&mut out, "", n, r);
        }
    }
    for (i, l) in lines.iter().enumerate() {
        let ln = i + 1;
        if let Some(ns) = above.get(&ln) {
            let indent: String = l.chars().take_while(|c| c.is_whitespace()).collect();
            for (n, r) in ns {
                emit(&mut out, &indent, n, r);
            }
        }
        if opts.numbers {
            out.push_str(&format!("{ln:>width$}  │ "));
        }
        out.push_str(l);
        out.push('\n');
    }
    out
}

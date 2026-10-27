//! Refuse to store a note that looks like it carries a credential.
//!
//! Notes are written by agents, and agents paste things. A note lives in git
//! history forever (dropping it leaves the old commit in the ref's log), so
//! the only safe moment to stop a secret is before it is stored. There is no
//! override flag: a false positive costs a reworded note; a false negative
//! costs a rotated key.

/// Why `text` looks like it holds a secret, or None.
pub fn scan(text: &str) -> Option<String> {
    const PREFIXES: &[(&str, &str, usize)] = &[
        ("AKIA", "AWS access key id", 16),
        ("ASIA", "AWS temporary access key id", 16),
        ("ghp_", "GitHub token", 30),
        ("gho_", "GitHub OAuth token", 30),
        ("ghs_", "GitHub app token", 30),
        ("github_pat_", "GitHub fine-grained token", 30),
        ("glpat-", "GitLab token", 20),
        ("xoxb-", "Slack bot token", 20),
        ("xoxp-", "Slack user token", 20),
        ("sk-ant-", "Anthropic API key", 20),
        ("sk-", "API secret key", 32),
        ("sk_live_", "Stripe live key", 20),
        ("AIza", "Google API key", 30),
        ("npm_", "npm token", 30),
    ];
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY") {
        return Some("a PEM private key".into());
    }
    for word in text.split(|c: char| c.is_whitespace() || "\"'`,;()[]{}<>".contains(c)) {
        let w = word.trim_matches(|c: char| c == '.' || c == ':');
        for (p, what, min_tail) in PREFIXES {
            if let Some(tail) = w.strip_prefix(p) {
                if tail.len() >= *min_tail && tail.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                    return Some(format!("what looks like a {what} ({p}…)"));
                }
            }
        }
        // `password=hunter22`, `api_key: abc…`
        if let Some((k, v)) = w.split_once(['=', ':']) {
            let k = k.to_ascii_lowercase();
            let v = v.trim_matches(|c: char| c == '"' || c == '\'');
            let secretish = ["password", "passwd", "secret", "token", "api_key", "apikey", "api-key", "private_key"]
                .iter()
                .any(|s| k.ends_with(s));
            if secretish && v.len() >= 8 && !v.starts_with('<') && !v.starts_with('$') && !v.contains("...") {
                return Some(format!("a credential assignment ({k}=…)"));
            }
        }
        // Long high-entropy tokens: base64/base62 secrets. Hex (git shas,
        // content hashes) caps at 4 bits/char, so it never trips this.
        if w.len() >= 32 && w.len() <= 200 && entropy(w) > 4.3 && w.chars().any(|c| c.is_ascii_digit()) && !looks_like_path(w) {
            return Some("a long high-entropy token (looks like a key)".into());
        }
    }
    // URLs with inline credentials.
    if let Some(i) = text.find("://") {
        let rest = &text[i + 3..];
        let host = rest.split(['/', ' ', '\n']).next().unwrap_or("");
        if let Some((cred, _)) = host.split_once('@') {
            if cred.contains(':') {
                return Some("a URL with an embedded password".into());
            }
        }
    }
    None
}

/// `drivers/gpu/drm/amd/display/dc/dcn32/dcn32_hwseq.c` is long and varied
/// but plainly a path: short, word-like segments. Base64 that happens to
/// contain `/` has long random runs between them.
fn looks_like_path(w: &str) -> bool {
    w.contains('/')
        && w.split('/').all(|seg| {
            seg.len() <= 40
                && seg.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
                && (seg.is_empty() || seg.chars().filter(|c| c.is_ascii_uppercase()).count() * 3 <= seg.len().max(3))
        })
}

fn entropy(s: &str) -> f64 {
    let mut counts = [0u32; 256];
    for b in s.bytes() {
        counts[b as usize] += 1;
    }
    let n = s.len() as f64;
    counts.iter().filter(|&&c| c > 0).map(|&c| {
        let p = c as f64 / n;
        -p * p.log2()
    }).sum()
}

#[cfg(test)]
mod tests {
    use super::scan;

    #[test]
    fn catches_secrets() {
        for s in [
            "use AKIAIOSFODNN7EXAMPLE for the bucket",
            "token ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789",
            "set password=hunter22hunter",
            "key is sk-ant-api03-abcdefghijklmnopqrstuvwxyz",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc",
            "clone https://bob:s3cretpw@example.com/r.git",
            "blob Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZmdoaWprbG1ub3A9",
        ] {
            assert!(scan(s).is_some(), "missed: {s}");
        }
    }

    #[test]
    fn leaves_ordinary_notes_alone() {
        for s in [
            "this lock must be taken before the fabric ref is read; see 3f2a9c1d0e8b7a6f5e4d3c2b1a09f8e7d6c5b4a3",
            "password=<from env> — never hardcode; read HEDDLE_TOKEN",
            "the api_key: $API_KEY is injected by CI",
            "uses sha256 content addressing (content_hash) for blobs",
            "https://github.com/zyads/heddle/pull/2 explains why",
            "MAX_LINE_HASHES caps the fingerprint at 400 lines",
            "see drivers/gpu/drm/amd/display/dc/dcn32/dcn32_hwseq.c and arch/arm64/kvm/hyp/nvhe/mem_protect.c",
        ] {
            assert!(scan(s).is_none(), "false positive: {s} → {:?}", scan(s));
        }
    }
}

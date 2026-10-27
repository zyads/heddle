//! Local embeddings via Ollama — loopback only, cached by content.
//!
//! Hand-rolled HTTP/1.0 over a `TcpStream` (HTTP/1.0 so the server closes
//! the connection and never chunk-encodes): one POST to `/api/embed` per
//! batch, no HTTP client dependency. The host is pinned to loopback —
//! `OLLAMA_HOST` pointing anywhere else is ignored, because the whole point
//! is that notes and code never leave the machine.
//!
//! Vectors are cached in `<cache>/vectors-<model>.bin` keyed by the sha256
//! of the embedded text, so a note or code chunk is embedded once until it
//! changes. When no model is reachable every caller falls back to BM25 and
//! says so (`ranker: "bm25"`).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_MODEL: &str = "nomic-embed-text";
/// Texts longer than this are truncated before embedding; small embedding
/// models have short context windows anyway.
const MAX_EMBED_CHARS: usize = 6000;
const BATCH: usize = 32;

pub struct Embedder {
    pub model: String,
    addr: SocketAddr,
    cache_path: PathBuf,
}

/// The loopback port to use: `OLLAMA_HOST` only if it names a loopback host.
fn loopback_addr() -> SocketAddr {
    parse_host(std::env::var("OLLAMA_HOST").ok().as_deref())
}

fn parse_host(v: Option<&str>) -> SocketAddr {
    let default: SocketAddr = "127.0.0.1:11434".parse().expect("literal");
    let Some(h) = v else { return default };
    let h = h.trim_start_matches("http://");
    let h = h.trim_end_matches('/');
    let (host, port) = h.rsplit_once(':').unwrap_or((h, "11434"));
    let port: u16 = port.parse().unwrap_or(11434);
    match host {
        "127.0.0.1" | "localhost" | "" => SocketAddr::from(([127, 0, 0, 1], port)),
        "[::1]" | "::1" => SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port)),
        _ => default, // non-loopback: refuse to send anything there
    }
}

impl Embedder {
    /// An embedder if a loopback Ollama answers with the model installed.
    /// `HEDDLE_EMBED=off` disables semantic ranking outright.
    pub fn local(cache_dir: &Path) -> Option<Embedder> {
        if std::env::var("HEDDLE_EMBED").is_ok_and(|v| v == "off" || v == "0") {
            return None;
        }
        let model = std::env::var("HEDDLE_EMBED_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let addr = loopback_addr();
        let tags = http(addr, "GET", "/api/tags", None, Duration::from_millis(400)).ok()?;
        let v: serde_json::Value = serde_json::from_str(&tags).ok()?;
        let installed = v["models"].as_array()?.iter().any(|m| {
            let n = m["name"].as_str().unwrap_or("");
            n == model || n.strip_suffix(":latest") == Some(model.as_str())
        });
        if !installed {
            return None;
        }
        let safe: String = model.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
        Some(Embedder { cache_path: cache_dir.join(format!("vectors-{safe}.bin")), model, addr })
    }

    pub fn embed_one(&self, text: &str) -> Result<Vec<f32>, String> {
        // Queries are not cached: they are one-offs.
        self.request(&[text.to_string()]).map(|mut v| v.remove(0))
    }

    /// Embed many texts, hitting the cache first; only misses go to the model.
    pub fn embed_many(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let mut cache = load_cache(&self.cache_path);
        let keys: Vec<String> = texts.iter().map(|t| crate::store::content_hash(t.as_bytes())[..24].to_string()).collect();
        let misses: Vec<usize> = (0..texts.len()).filter(|&i| !cache.contains_key(&keys[i])).collect();
        let mut fresh = Vec::new();
        for batch in misses.chunks(BATCH) {
            let input: Vec<String> = batch.iter().map(|&i| texts[i].clone()).collect();
            let vs = self.request(&input)?;
            for (&i, v) in batch.iter().zip(vs) {
                fresh.push((keys[i].clone(), v.clone()));
                cache.insert(keys[i].clone(), v);
            }
        }
        if !fresh.is_empty() {
            append_cache(&self.cache_path, &fresh);
        }
        Ok(keys.iter().map(|k| cache.get(k).cloned().unwrap_or_default()).collect())
    }

    fn request(&self, input: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let input: Vec<String> = input.iter().map(|t| t.chars().take(MAX_EMBED_CHARS).collect()).collect();
        let body = serde_json::json!({"model": self.model, "input": input}).to_string();
        let resp = http(self.addr, "POST", "/api/embed", Some(&body), Duration::from_secs(60))?;
        let v: serde_json::Value = serde_json::from_str(&resp).map_err(|e| format!("ollama: bad JSON: {e}"))?;
        let arr = v["embeddings"].as_array().ok_or_else(|| format!("ollama: no embeddings in reply: {}", crate::cap(&resp, 200)))?;
        let out: Vec<Vec<f32>> = arr
            .iter()
            .map(|e| e.as_array().map(|xs| xs.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect()).unwrap_or_default())
            .collect();
        if out.len() != input.len() {
            return Err("ollama returned the wrong number of embeddings".into());
        }
        Ok(out)
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut d, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        d += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 { 0.0 } else { d / (na.sqrt() * nb.sqrt()) }
}

fn http(addr: SocketAddr, method: &str, path: &str, body: Option<&str>, timeout: Duration) -> Result<String, String> {
    debug_assert!(addr.ip().is_loopback());
    if !addr.ip().is_loopback() {
        return Err("refusing a non-loopback embedding host".into());
    }
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(300)).map_err(|e| format!("ollama not reachable: {e}"))?;
    s.set_read_timeout(Some(timeout)).ok();
    s.set_write_timeout(Some(timeout)).ok();
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.0\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).map_err(|e| format!("ollama write: {e}"))?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).map_err(|e| format!("ollama read: {e}"))?;
    let text = String::from_utf8_lossy(&buf);
    let (head, rest) = text.split_once("\r\n\r\n").ok_or("ollama: malformed HTTP reply")?;
    let status = head.split_whitespace().nth(1).unwrap_or("");
    if status != "200" {
        return Err(format!("ollama HTTP {status}: {}", crate::cap(rest, 200)));
    }
    Ok(rest.to_string())
}

// Cache format: repeated records of [key len u8][key][dim u32][dim × f32 LE].
// Append-only; a torn tail record is ignored on load.

fn load_cache(p: &Path) -> HashMap<String, Vec<f32>> {
    let mut m = HashMap::new();
    let Ok(b) = std::fs::read(p) else { return m };
    let mut i = 0;
    while i < b.len() {
        let kl = b[i] as usize;
        if i + 1 + kl + 4 > b.len() {
            break;
        }
        let key = String::from_utf8_lossy(&b[i + 1..i + 1 + kl]).into_owned();
        let d = u32::from_le_bytes(b[i + 1 + kl..i + 5 + kl].try_into().unwrap()) as usize;
        let start = i + 5 + kl;
        if d > 16384 || start + d * 4 > b.len() {
            break;
        }
        let v = b[start..start + d * 4].chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
        m.insert(key, v);
        i = start + d * 4;
    }
    if i < b.len() {
        // A torn record (crash mid-append, or two writers interleaving).
        // Cut it off, or every later append would land behind it unread.
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(p) {
            let _ = f.set_len(i as u64);
        }
    }
    m
}

fn append_cache(p: &Path, items: &[(String, Vec<f32>)]) {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(p) else { return };
    let mut out = Vec::new();
    for (k, v) in items {
        out.push(k.len() as u8);
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(&(v.len() as u32).to_le_bytes());
        for x in v {
            out.extend_from_slice(&x.to_le_bytes());
        }
    }
    let _ = f.write_all(&out);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_roundtrip_and_torn_tail() {
        let dir = std::env::temp_dir().join(format!("gloss-embed-{}", std::process::id()));
        let p = dir.join("v.bin");
        let _ = std::fs::remove_file(&p);
        append_cache(&p, &[("a".into(), vec![1.0, 2.0]), ("b".into(), vec![3.0])]);
        // Torn write: half a record.
        use std::io::Write;
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(&[5, b'x']).unwrap();
        let m = load_cache(&p);
        assert_eq!(m.get("a"), Some(&vec![1.0, 2.0]));
        assert_eq!(m.get("b"), Some(&vec![3.0]));
        assert_eq!(m.len(), 2);
        // The torn tail was cut, so new appends are readable again.
        append_cache(&p, &[("c".into(), vec![4.0])]);
        assert_eq!(load_cache(&p).get("c"), Some(&vec![4.0]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_loopback_host_is_ignored() {
        for h in ["http://10.0.0.5:11434", "gpu-box:11434", "0.0.0.0:11434", "evil.example.com"] {
            assert!(parse_host(Some(h)).ip().is_loopback(), "{h} must not be used");
        }
        assert_eq!(parse_host(Some("localhost:9999")).port(), 9999);
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
    }
}

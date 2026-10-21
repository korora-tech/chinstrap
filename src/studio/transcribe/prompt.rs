//! Vocabulary prompt (§12.6): glossary + session hint, capped so tokenization is memory-safe.
//!
//! whisper-rs 0.16's `WhisperContext::tokenize` is unsound when the text needs more tokens than
//! `max_tokens` (whisper.cpp returns `-n`, whisper-rs only checks `-1` and then `set_len`s the
//! negative value). whisper.cpp's tokenizer emits at most one token per input byte, so a prompt
//! of at most `PROMPT_MAX_BYTES` bytes tokenized with `max_tokens = PROMPT_MAX_BYTES` can never
//! overflow. `PromptBytes` can only be built by the capper here, and the worker's only
//! `tokenize` call takes a `&PromptBytes`.

use std::io::Read;
use std::path::{Path, PathBuf};

use gstreamer as gst;

pub const PROMPT_MAX_BYTES: usize = 1024;
pub const PROMPT_MAX_TOKENS: usize = 200;
pub const TERM_MAX_BYTES: usize = 64;
pub const HINT_MAX_BYTES: usize = 128;
pub const GLOSSARY_MAX_BYTES: u64 = 1 << 20;

/// `$XDG_CONFIG_HOME/chinstrap/glossary.txt` (default `~/.config/chinstrap/glossary.txt`).
pub fn glossary_path() -> PathBuf {
    gst::glib::user_config_dir()
        .join("chinstrap")
        .join("glossary.txt")
}

#[derive(Debug, Default)]
pub struct Glossary {
    pub terms: Vec<String>,
    pub warnings: Vec<String>,
}

/// Reads the glossary (optional; a missing file is an empty glossary). Only the first 1 MiB is
/// read; a line cut by that limit is discarded.
pub fn load_glossary(path: &Path) -> Glossary {
    let mut g = Glossary::default();
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return g,
        Err(e) => {
            g.warnings
                .push(format!("cannot read glossary {}: {e}", path.display()));
            return g;
        }
    };
    let mut bytes = Vec::new();
    if let Err(e) = file.take(GLOSSARY_MAX_BYTES + 1).read_to_end(&mut bytes) {
        g.warnings
            .push(format!("cannot read glossary {}: {e}", path.display()));
        return g;
    }
    if bytes.len() as u64 > GLOSSARY_MAX_BYTES {
        bytes.truncate(GLOSSARY_MAX_BYTES as usize);
        // Drop the partial last line.
        match bytes.iter().rposition(|&b| b == b'\n') {
            Some(nl) => bytes.truncate(nl),
            None => bytes.clear(),
        }
        g.warnings.push(format!(
            "glossary {} is larger than 1 MiB; only the first 1 MiB is used",
            path.display()
        ));
    }
    g.terms = parse_glossary(&String::from_utf8_lossy(&bytes));
    g
}

/// One term per line: trimmed, NULs stripped, empty and `#` lines skipped, terms over 64 bytes
/// skipped, de-duplicated case-insensitively (first occurrence wins).
pub fn parse_glossary(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let term: String = line.chars().filter(|&c| c != '\0').collect();
        let term = term.trim();
        if term.is_empty() || term.starts_with('#') || term.len() > TERM_MAX_BYTES {
            continue;
        }
        if seen.insert(term.to_lowercase()) {
            out.push(term.to_string());
        }
    }
    out
}

/// The recorder's default session name: `YYYY-MM-DDTHH-MM-SS` with an optional `-N` suffix.
fn is_timestamp_name(name: &str) -> bool {
    let b = name.as_bytes();
    if b.len() < 19 {
        return false;
    }
    let pattern = b"dddd-dd-ddTdd-dd-dd";
    let head_ok = pattern.iter().zip(b).all(|(p, c)| match p {
        b'd' => c.is_ascii_digit(),
        p => p == c,
    });
    if !head_ok {
        return false;
    }
    let rest = &b[19..];
    rest.is_empty()
        || (rest.len() >= 2 && rest[0] == b'-' && rest[1..].iter().all(u8::is_ascii_digit))
}

fn truncate_at_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Session hint from the session directory name: `-`/`_` become spaces, at most 128 bytes,
/// omitted for the recorder's timestamp names (they carry no vocabulary).
pub fn session_hint(dir_name: &str) -> Option<String> {
    if is_timestamp_name(dir_name) {
        return None;
    }
    let spaced: String = dir_name
        .chars()
        .filter(|&c| c != '\0')
        .map(|c| if c == '-' || c == '_' { ' ' } else { c })
        .collect();
    let collapsed = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let hint = truncate_at_char_boundary(&collapsed, HINT_MAX_BYTES).trim();
    (!hint.is_empty()).then(|| hint.to_string())
}

/// The session hint for a media file: the session directory is the parent of `segment-NNN`,
/// else the file's own directory (for `--transcribe` on an arbitrary file).
pub fn session_hint_for_media(media: &Path) -> Option<String> {
    let parent = media.parent()?;
    let parent_name = parent.file_name()?.to_string_lossy().to_string();
    let is_segment = parent_name
        .strip_prefix("segment-")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    let dir = if is_segment {
        parent.parent()?.file_name()?.to_string_lossy().to_string()
    } else {
        parent_name
    };
    session_hint(&dir)
}

/// A prompt guaranteed to be at most `PROMPT_MAX_BYTES` bytes and free of NULs. Only
/// `Capped::render` builds one.
#[derive(Clone, Debug, PartialEq)]
pub struct PromptBytes(String);

impl PromptBytes {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The hint plus the glossary terms that fit the byte cap, in glossary order.
#[derive(Clone, Debug)]
pub struct Capped {
    hint: Option<String>,
    terms: Vec<String>,
    /// Terms dropped by the byte cap.
    dropped: usize,
}

fn render_text(hint: Option<&str>, terms: &[String]) -> String {
    let mut parts = Vec::new();
    if let Some(h) = hint {
        parts.push(format!("{h}."));
    }
    if !terms.is_empty() {
        parts.push(format!("Glossary: {}.", terms.join(", ")));
    }
    parts.join(" ")
}

/// Step 1 (byte cap): appends terms in order while the prompt stays within 1024 bytes; the
/// first term that does not fit and every term after it are dropped.
pub fn cap(hint: Option<&str>, terms: &[String]) -> Capped {
    let hint = hint
        .map(|h| truncate_at_char_boundary(&h.replace('\0', ""), HINT_MAX_BYTES).to_string())
        .filter(|h| !h.is_empty());
    let mut kept: Vec<String> = Vec::new();
    // Running length of "Glossary: a, b." without re-rendering every time.
    let base = hint.as_ref().map_or(0, |h| h.len() + 1); // "{hint}."
    let mut glossary_len = 0usize; // "Glossary: " + terms joined by ", " + "."
    for (i, t) in terms.iter().enumerate() {
        let t = t.replace('\0', "");
        if t.is_empty() || t.len() > TERM_MAX_BYTES {
            continue;
        }
        let next = if kept.is_empty() {
            "Glossary: ".len() + t.len() + 1
        } else {
            glossary_len + 2 + t.len()
        };
        let sep = usize::from(base > 0);
        if base + sep + next > PROMPT_MAX_BYTES {
            return Capped {
                hint,
                terms: kept,
                dropped: terms.len() - i,
            };
        }
        glossary_len = next;
        kept.push(t);
    }
    Capped {
        hint,
        terms: kept,
        dropped: 0,
    }
}

impl Capped {
    #[cfg(test)]
    pub fn terms(&self) -> usize {
        self.terms.len()
    }

    /// The prompt with the hint and the first `n` kept terms.
    pub fn render(&self, n: usize) -> PromptBytes {
        let text = render_text(self.hint.as_deref(), &self.terms[..n.min(self.terms.len())]);
        debug_assert!(text.len() <= PROMPT_MAX_BYTES);
        assert!(text.len() <= PROMPT_MAX_BYTES, "prompt byte cap violated");
        PromptBytes(text)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FinalPrompt {
    pub text: String,
    pub hint: Option<String>,
    pub terms_used: usize,
    /// Byte cap plus token cap.
    pub terms_dropped: usize,
    pub bytes: usize,
    pub tokens: usize,
}

/// Steps 2–3 (token cap): the largest number of kept terms whose prompt is at most 200 tokens,
/// by binary search over the term count. `count_tokens` is the only path to the tokenizer.
pub fn fit(
    capped: &Capped,
    mut count_tokens: impl FnMut(&PromptBytes) -> Result<usize, String>,
) -> Result<FinalPrompt, String> {
    let mut count = |n: usize| -> Result<(PromptBytes, usize), String> {
        let p = capped.render(n);
        let t = if p.is_empty() { 0 } else { count_tokens(&p)? };
        Ok((p, t))
    };
    let all = capped.terms.len();
    let (mut best, mut best_tokens) = count(all)?;
    let mut used = all;
    if best_tokens > PROMPT_MAX_TOKENS {
        // The hint alone is at most 128 bytes, so at most 128 tokens: n = 0 always fits.
        let (mut lo, mut hi) = (0usize, all - 1);
        let mut lo_val = None;
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let (p, t) = count(mid)?;
            if t <= PROMPT_MAX_TOKENS {
                lo = mid;
                lo_val = Some((p, t));
            } else {
                hi = mid - 1;
            }
        }
        let (p, t) = match lo_val {
            Some(v) => v,
            None => count(lo)?,
        };
        best = p;
        best_tokens = t;
        used = lo;
    }
    Ok(FinalPrompt {
        bytes: best.len(),
        text: best.0,
        hint: capped.hint.clone(),
        terms_used: used,
        terms_dropped: capped.dropped + (all - used),
        tokens: best_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tokenizer stand-in that, like whisper's, never emits more tokens than bytes: one token
    /// per word piece of at most 4 bytes.
    fn fake_tokens(p: &PromptBytes) -> Result<usize, String> {
        Ok(p.as_str()
            .split_whitespace()
            .map(|w| w.len().div_ceil(4))
            .sum())
    }

    #[test]
    fn glossary_parsing() {
        let long = "x".repeat(65);
        let text = format!(
            "# comment\n  Kubernetes \n\nkubernetes\nCil\0ium\n{long}\nCilium\neBPF\n#x\nTalos"
        );
        assert_eq!(
            parse_glossary(&text),
            vec!["Kubernetes", "Cilium", "eBPF", "Talos"]
        );
    }

    #[test]
    fn timestamp_hints_are_omitted() {
        assert_eq!(session_hint("2026-10-21T14-03-22"), None);
        assert_eq!(session_hint("2026-10-21T14-03-22-2"), None);
        assert_eq!(session_hint("2026-10-21T14-03-22-12"), None);
        assert_eq!(
            session_hint("2026-10-21T14-03-22-kubernetes"),
            Some("2026 10 21T14 03 22 kubernetes".into())
        );
        assert_eq!(
            session_hint("kubernetes_demo-cilium"),
            Some("kubernetes demo cilium".into())
        );
        let long = "ü".repeat(100); // 200 bytes
        let h = session_hint(&long).unwrap();
        assert!(h.len() <= HINT_MAX_BYTES && h.chars().all(|c| c == 'ü'));
        assert_eq!(
            session_hint_for_media(Path::new("/v/talos-demo/segment-003/mic.mkv")),
            Some("talos demo".into())
        );
        assert_eq!(
            session_hint_for_media(Path::new("/v/2026-10-21T14-03-22/segment-001/mic.mkv")),
            None
        );
        assert_eq!(
            session_hint_for_media(Path::new("/scratch/acc/jargon.mkv")),
            Some("acc".into())
        );
    }

    #[test]
    fn prompt_format() {
        let terms: Vec<String> = ["Cilium", "eBPF"].iter().map(|s| s.to_string()).collect();
        let c = cap(Some("talos demo"), &terms);
        assert_eq!(c.render(2).as_str(), "talos demo. Glossary: Cilium, eBPF.");
        assert_eq!(c.render(0).as_str(), "talos demo.");
        assert_eq!(
            cap(None, &terms).render(2).as_str(),
            "Glossary: Cilium, eBPF."
        );
        assert_eq!(cap(None, &[]).render(0).as_str(), "");
        let f = fit(&cap(None, &[]), |_| {
            panic!("an empty prompt is never tokenized")
        })
        .unwrap();
        assert_eq!((f.tokens, f.bytes, f.terms_used), (0, 0, 0));
    }

    #[test]
    fn two_thousand_terms_fit_bytes_and_tokens() {
        let terms: Vec<String> = (0..2000).map(|i| format!("Term{i}")).collect();
        let c = cap(Some("kubernetes demo"), &terms);
        assert!(c.render(c.terms()).len() <= PROMPT_MAX_BYTES);
        let mut calls = 0;
        let f = fit(&c, |p| {
            calls += 1;
            assert!(p.len() <= PROMPT_MAX_BYTES);
            fake_tokens(p)
        })
        .unwrap();
        assert!(f.bytes <= PROMPT_MAX_BYTES);
        assert!(f.tokens <= PROMPT_MAX_TOKENS, "{} tokens", f.tokens);
        assert!(f.terms_used > 10);
        assert_eq!(f.terms_used + f.terms_dropped, 2000);
        assert!(calls <= 12, "{calls} tokenizer calls");
        // Maximal: one more term would exceed the token cap.
        let more = c.render(f.terms_used + 1);
        assert!(fake_tokens(&more).unwrap() > PROMPT_MAX_TOKENS);
        assert_eq!(f.text, c.render(f.terms_used).as_str());
    }

    #[test]
    fn one_mib_line_and_file_cap() {
        let dir = std::env::temp_dir().join(format!("chinstrap-prompt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("glossary.txt");
        // One 1 MiB line, then a real term that is past the read cap.
        let mut text = "k".repeat(1 << 20);
        text.push_str("\nCilium\n");
        std::fs::write(&p, &text).unwrap();
        let g = load_glossary(&p);
        assert!(g.terms.is_empty(), "{:?}", g.terms.len());
        assert_eq!(g.warnings.len(), 1);
        let f = fit(&cap(None, &g.terms), fake_tokens).unwrap();
        assert!(f.bytes <= PROMPT_MAX_BYTES && f.tokens <= PROMPT_MAX_TOKENS);
        // Many short lines up to the cap.
        let many: String = (0..200_000).map(|i| format!("t{i}\n")).collect();
        std::fs::write(&p, many).unwrap();
        let g = load_glossary(&p);
        assert!(!g.terms.is_empty());
        let f = fit(&cap(None, &g.terms), fake_tokens).unwrap();
        assert!(f.bytes <= PROMPT_MAX_BYTES && f.tokens <= PROMPT_MAX_TOKENS);
        assert!(load_glossary(&dir.join("missing.txt")).warnings.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn multibyte_terms_respect_the_byte_cap() {
        // 4-byte characters: the cap must hold in bytes, not chars.
        let terms: Vec<String> = (0..500).map(|i| format!("𝕂𝕦𝕓𝕖{i}")).collect();
        let hint = "ü".repeat(300);
        let c = cap(Some(&hint), &terms);
        let p = c.render(c.terms());
        assert!(p.len() <= PROMPT_MAX_BYTES);
        assert!(std::str::from_utf8(p.as_str().as_bytes()).is_ok());
        assert!(c.dropped > 0);
        let f = fit(&c, |p| Ok(p.len())).unwrap(); // worst case: one token per byte
        assert!(f.tokens <= PROMPT_MAX_TOKENS && f.bytes <= PROMPT_MAX_BYTES);
    }
}

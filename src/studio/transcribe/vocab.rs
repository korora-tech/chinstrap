//! Custom vocabulary (§12.6): editing the glossary, and replacement rules applied after
//! transcription.
//!
//! - `glossary.txt`: one term per line (see `prompt.rs`); the terms become Whisper's initial
//!   prompt, which biases spelling ("Rawkode", "Kubernetes").
//! - `replacements.txt`: one `from => to` rule per line, `#` comments. After transcription every
//!   case-insensitive, whole-word match of `from` (words separated by any whitespace) becomes
//!   `to`, in the transcript text, the words, the SRT and the VTT. The JSON keeps the original
//!   text next to the replaced one.
//!
//! Both files live in `$XDG_CONFIG_HOME/chinstrap/` and are read on every transcription job, so
//! an edit applies to the next job (and to a re-transcription).

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::output::{Segment, Word};
use super::prompt::{self, TERM_MAX_BYTES};
use crate::studio::session;

pub const REPLACEMENTS_MAX_BYTES: u64 = 1 << 20;
/// Separator between the two sides of a rule.
pub const ARROW: &str = "=>";

/// `$XDG_CONFIG_HOME/chinstrap/replacements.txt`.
pub fn replacements_path() -> PathBuf {
    prompt::glossary_path().with_file_name("replacements.txt")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub from: String,
    pub to: String,
}

/// How often a rule changed the transcript (`transcript.json` `replacements`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Applied {
    pub from: String,
    pub to: String,
    pub count: u32,
}

/// The words of `from`, lower-cased; `None` when it has no word character.
fn from_words(from: &str) -> Option<Vec<String>> {
    let words: Vec<String> = from.split_whitespace().map(str::to_lowercase).collect();
    (!words.is_empty() && from.chars().any(char::is_alphanumeric)).then_some(words)
}

/// Checks one side of a rule or a glossary term.
fn check_text(what: &str, s: &str) -> Result<String, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err(format!("the {what} is empty"));
    }
    if t.len() > TERM_MAX_BYTES {
        return Err(format!("the {what} is longer than {TERM_MAX_BYTES} bytes"));
    }
    if t.contains(['\n', '\r', '\0']) {
        return Err(format!("the {what} contains a line break"));
    }
    Ok(t.to_string())
}

/// A rule as typed in the UI, validated.
pub fn rule(from: &str, to: &str) -> Result<Rule, String> {
    let from = check_text("text to replace", from)?;
    let to = check_text("replacement", to)?;
    if from.contains(ARROW) {
        return Err(format!("the text to replace contains {ARROW:?}"));
    }
    if from.starts_with('#') {
        return Err("the text to replace starts with '#'".into());
    }
    if from_words(&from).is_none() {
        return Err("the text to replace has no letters or digits".into());
    }
    Ok(Rule { from, to })
}

/// A glossary term as typed in the UI, validated.
pub fn term(t: &str) -> Result<String, String> {
    let t = check_text("term", t)?;
    if t.starts_with('#') {
        return Err("a term cannot start with '#'".into());
    }
    Ok(t)
}

/// Parses `from => to` lines: trimmed, `#` and empty lines skipped, invalid lines reported.
/// A later rule for the same `from` (case-insensitive) replaces an earlier one.
pub fn parse_replacements(text: &str) -> (Vec<Rule>, Vec<String>) {
    let mut rules: Vec<Rule> = Vec::new();
    let mut warnings = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let parsed = l
            .split_once(ARROW)
            .ok_or_else(|| format!("no {ARROW:?}"))
            .and_then(|(f, t)| rule(f, t));
        match parsed {
            Ok(r) => {
                rules.retain(|x| !x.from.eq_ignore_ascii_case(&r.from));
                rules.push(r);
            }
            Err(e) => warnings.push(format!("replacements line {}: {e}", n + 1)),
        }
    }
    (rules, warnings)
}

fn read_capped(path: &Path) -> Result<Option<String>, String> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let mut bytes = Vec::new();
    file.take(REPLACEMENTS_MAX_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Reads the replacement rules (a missing file has none).
pub fn load_replacements(path: &Path) -> (Vec<Rule>, Vec<String>) {
    match read_capped(path) {
        Ok(Some(text)) => parse_replacements(&text),
        Ok(None) => (Vec::new(), Vec::new()),
        Err(e) => (Vec::new(), vec![e]),
    }
}

/// The vocabulary as the UI shows it (both files, re-read).
pub fn info() -> crate::studio::types::VocabularyInfo {
    let glossary_path = prompt::glossary_path();
    let replacements_path = replacements_path();
    let g = prompt::load_glossary(&glossary_path);
    let (rules, rule_warnings) = load_replacements(&replacements_path);
    crate::studio::types::VocabularyInfo {
        terms: g.terms,
        replacements: rules
            .into_iter()
            .map(|r| crate::studio::types::ReplacementRule {
                from: r.from,
                to: r.to,
            })
            .collect(),
        glossary_path,
        replacements_path,
        warnings: g.warnings.into_iter().chain(rule_warnings).collect(),
    }
}

/// Applies one vocabulary command to the files. `Ok(false)`: not a vocabulary command.
pub fn command(cmd: &crate::studio::types::EngineCommand) -> Result<bool, String> {
    use crate::studio::types::EngineCommand as C;
    match cmd {
        C::AddVocabularyTerm(t) => add_term(&prompt::glossary_path(), t)?,
        C::RemoveVocabularyTerm(t) => remove_term(&prompt::glossary_path(), t)?,
        C::AddReplacement { from, to } => add_rule(&replacements_path(), from, to)?,
        C::RemoveReplacement { from } => remove_rule(&replacements_path(), from)?,
        _ => return Ok(false),
    }
    Ok(true)
}

// ---- editing -------------------------------------------------------------------------------

const GLOSSARY_HEADER: &str =
    "# Chinstrap glossary: one term per line, given to Whisper as its prompt.\n";
const REPLACEMENTS_HEADER: &str = "# Chinstrap replacements: one `from => to` rule per line, applied after transcription\n\
     # (case-insensitive, whole words).\n";

fn write_lines(path: &Path, header: &str, lines: &[String]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let mut body = String::new();
    if lines.is_empty() || !lines[0].starts_with('#') {
        body.push_str(header);
    }
    for l in lines {
        body.push_str(l);
        body.push('\n');
    }
    session::write_atomic(path, body.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn existing_lines(path: &Path) -> Result<Vec<String>, String> {
    Ok(read_capped(path)?
        .map(|t| t.lines().map(String::from).collect())
        .unwrap_or_default())
}

/// Adds a glossary term (comments and other lines are kept). Adding a term that is already
/// there (case-insensitively) is an error.
pub fn add_term(path: &Path, t: &str) -> Result<(), String> {
    let t = term(t)?;
    let mut lines = existing_lines(path)?;
    if lines.iter().any(|l| l.trim().eq_ignore_ascii_case(&t)) {
        return Err(format!("{t:?} is already in the glossary"));
    }
    lines.push(t);
    write_lines(path, GLOSSARY_HEADER, &lines)
}

/// Removes a glossary term (every line equal to it, case-insensitively).
pub fn remove_term(path: &Path, t: &str) -> Result<(), String> {
    let t = t.trim();
    let mut lines = existing_lines(path)?;
    let before = lines.len();
    lines.retain(|l| !l.trim().eq_ignore_ascii_case(t));
    if lines.len() == before {
        return Err(format!("{t:?} is not in the glossary"));
    }
    write_lines(path, GLOSSARY_HEADER, &lines)
}

fn rule_from(line: &str) -> Option<&str> {
    let l = line.trim();
    if l.starts_with('#') {
        return None;
    }
    l.split_once(ARROW).map(|(f, _)| f.trim())
}

/// Adds a rule, replacing an existing rule for the same `from` (case-insensitive).
pub fn add_rule(path: &Path, from: &str, to: &str) -> Result<(), String> {
    let r = rule(from, to)?;
    let mut lines = existing_lines(path)?;
    lines.retain(|l| !rule_from(l).is_some_and(|f| f.eq_ignore_ascii_case(&r.from)));
    lines.push(format!("{} {ARROW} {}", r.from, r.to));
    write_lines(path, REPLACEMENTS_HEADER, &lines)
}

/// Removes the rule for `from` (case-insensitive).
pub fn remove_rule(path: &Path, from: &str) -> Result<(), String> {
    let from = from.trim();
    let mut lines = existing_lines(path)?;
    let before = lines.len();
    lines.retain(|l| !rule_from(l).is_some_and(|f| f.eq_ignore_ascii_case(from)));
    if lines.len() == before {
        return Err(format!("no replacement for {from:?}"));
    }
    write_lines(path, REPLACEMENTS_HEADER, &lines)
}

// ---- applying ------------------------------------------------------------------------------

fn chars_eq_ci(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// Matches `word` (already lower-case) at byte `i` of `text`; returns the end byte.
fn match_word(text: &str, i: usize, word: &str) -> Option<usize> {
    let mut it = text[i..].char_indices();
    for w in word.chars() {
        let (_, c) = it.next()?;
        if !chars_eq_ci(c, w) {
            return None;
        }
    }
    Some(it.next().map_or(text.len(), |(j, _)| i + j))
}

/// Replaces every case-insensitive, whole-word match of `rule.from` in `text`. Returns the new
/// text and the number of matches.
pub fn replace_text(text: &str, r: &Rule) -> (String, u32) {
    let Some(words) = from_words(&r.from) else {
        return (text.to_string(), 0);
    };
    let is_word = |c: char| c.is_alphanumeric() || c == '\'' || c == '_';
    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    let mut i = 0;
    let mut prev: Option<char> = None;
    while i < text.len() {
        let c = text[i..].chars().next().expect("char at a boundary");
        let at_boundary = prev.is_none_or(|p| !is_word(p));
        if at_boundary && let Some(end) = match_all(text, i, &words, &is_word) {
            out.push_str(&r.to);
            count += 1;
            prev = text[..end].chars().next_back();
            i = end;
            continue;
        }
        out.push(c);
        prev = Some(c);
        i += c.len_utf8();
    }
    (out, count)
}

/// Matches the words of a rule starting at `i`, separated by whitespace, ending at a boundary.
fn match_all(
    text: &str,
    i: usize,
    words: &[String],
    is_word: &dyn Fn(char) -> bool,
) -> Option<usize> {
    let mut pos = i;
    for (k, w) in words.iter().enumerate() {
        if k > 0 {
            let ws = text[pos..]
                .char_indices()
                .find(|(_, c)| !c.is_whitespace())
                .map_or(text.len() - pos, |(j, _)| j);
            if ws == 0 {
                return None;
            }
            pos += ws;
        }
        pos = match_word(text, pos, w)?;
    }
    match text[pos..].chars().next() {
        Some(c) if is_word(c) => None,
        _ => Some(pos),
    }
}

/// A word split into (leading punctuation, core, trailing punctuation).
fn split_word(w: &str) -> (&str, &str, &str) {
    let start = w.find(|c: char| c.is_alphanumeric()).unwrap_or(w.len());
    let end = w.rfind(|c: char| c.is_alphanumeric()).map_or(start, |i| {
        i + w[i..].chars().next().map_or(1, char::len_utf8)
    });
    let end = end.max(start);
    (&w[..start], &w[start..end], &w[end..])
}

/// Applies `r` to a word list: a run of words whose cores equal the rule's words
/// (case-insensitively) becomes one word spanning them, keeping the outer punctuation.
fn replace_words(words: &mut Vec<Word>, r: &Rule) -> u32 {
    let Some(want) = from_words(&r.from) else {
        return 0;
    };
    // Each rule word may itself carry punctuation (e.g. "whisper.cpp"); compare cores.
    let want: Vec<String> = want
        .iter()
        .map(|w| split_word(w).1.to_lowercase())
        .collect();
    let mut count = 0;
    let mut i = 0;
    while i + want.len() <= words.len() {
        let hit = want.iter().enumerate().all(|(k, w)| {
            let (lead, core, trail) = split_word(&words[i + k].word);
            // Inner words must not carry punctuation that would be lost.
            let inner_ok = (k == 0 || lead.is_empty()) && (k + 1 == want.len() || trail.is_empty());
            inner_ok && core.to_lowercase() == *w
        });
        if !hit {
            i += 1;
            continue;
        }
        let last = i + want.len() - 1;
        let lead = split_word(&words[i].word).0.to_string();
        let trail = split_word(&words[last].word).2.to_string();
        let p = words[i..=last].iter().map(|w| w.p).sum::<f64>() / want.len() as f64;
        let merged = Word {
            word: format!("{lead}{}{trail}", r.to),
            start: words[i].start,
            end: words[last].end,
            p: super::output::r3(p),
        };
        words.splice(i..=last, [merged]);
        count += 1;
        i += 1;
    }
    count
}

/// Applies every rule to the segments' text and words. A changed segment keeps its original
/// text in `text_original`. Returns how often each rule matched (rules that never matched are
/// left out).
pub fn apply(segments: &mut [Segment], rules: &[Rule]) -> Vec<Applied> {
    let mut applied: Vec<Applied> = Vec::new();
    for r in rules {
        let mut total = 0;
        for seg in segments.iter_mut() {
            let (text, n_text) = replace_text(&seg.text, r);
            let n_words = replace_words(&mut seg.words, r);
            if n_text > 0 || n_words > 0 {
                if seg.text_original.is_none() {
                    seg.text_original = Some(seg.text.clone());
                }
                seg.text = text;
                total += n_text.max(n_words);
            }
        }
        if total > 0 {
            applied.push(Applied {
                from: r.from.clone(),
                to: r.to.clone(),
                count: total,
            });
        }
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(word: &str, start: f64, end: f64) -> Word {
        Word {
            word: word.into(),
            start,
            end,
            p: 0.5,
        }
    }

    fn r(from: &str, to: &str) -> Rule {
        rule(from, to).unwrap()
    }

    #[test]
    fn parsing() {
        let (rules, warnings) = parse_replacements(
            "# comment\nraw code => Rawkode\n\nrockwood=>Rawkode\nno arrow\n => x\nRaw Code => RawKode\n",
        );
        assert_eq!(
            rules,
            vec![r("rockwood", "Rawkode"), r("Raw Code", "RawKode")]
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(rule("...", "x").is_err());
        assert!(rule("a => b", "x").is_err());
        assert!(rule("a", "").is_err());
    }

    #[test]
    fn text_is_whole_word_and_case_insensitive() {
        let rule = r("raw code", "Rawkode");
        let (t, n) = replace_text(
            "On the Raw  Code Academy, raw code. Not rawcode or raw codes.",
            &rule,
        );
        assert_eq!(
            t,
            "On the Rawkode Academy, Rawkode. Not rawcode or raw codes."
        );
        assert_eq!(n, 2);
        let (t, n) = replace_text("Ünïcode ünïcode", &r("ünïcode", "U"));
        assert_eq!((t.as_str(), n), ("U U", 2));
        let (t, n) = replace_text("don't", &r("don", "do"));
        assert_eq!((t.as_str(), n), ("don't", 0));
    }

    #[test]
    fn words_merge_keeping_times_and_punctuation() {
        let mut segs = vec![Segment {
            id: 0,
            start: 1.0,
            end: 3.0,
            text: "Welcome to the raw code academy.".into(),
            text_original: None,
            avg_p: 0.5,
            words: vec![
                w("Welcome", 1.0, 1.2),
                w("to", 1.2, 1.4),
                w("the", 1.4, 1.6),
                w("raw", 1.6, 1.9),
                w("code", 1.9, 2.2),
                w("academy.", 2.2, 3.0),
            ],
        }];
        let rules = vec![r("raw code", "Rawkode"), r("academy", "Academy")];
        let applied = apply(&mut segs, &rules);
        assert_eq!(applied.len(), 2);
        assert_eq!(applied[0].count, 1);
        let s = &segs[0];
        assert_eq!(s.text, "Welcome to the Rawkode Academy.");
        assert_eq!(
            s.text_original.as_deref(),
            Some("Welcome to the raw code academy.")
        );
        let words: Vec<&str> = s.words.iter().map(|w| w.word.as_str()).collect();
        assert_eq!(words, vec!["Welcome", "to", "the", "Rawkode", "Academy."]);
        assert_eq!((s.words[3].start, s.words[3].end), (1.6, 2.2));
        // Punctuation between the words blocks a merge (it would be lost).
        let mut words = vec![w("raw.", 0.0, 1.0), w("Code", 1.0, 2.0)];
        assert_eq!(replace_words(&mut words, &r("raw code", "Rawkode")), 0);
        // Outer punctuation is kept.
        let mut words = vec![w("\"Raw", 0.0, 1.0), w("code!\"", 1.0, 2.0)];
        assert_eq!(replace_words(&mut words, &r("raw code", "Rawkode")), 1);
        assert_eq!(words[0].word, "\"Rawkode!\"");
    }

    #[test]
    fn editing_files_keeps_comments() {
        let dir = std::env::temp_dir().join(format!("chinstrap-vocab-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let g = dir.join("glossary.txt");
        add_term(&g, " Rawkode ").unwrap();
        add_term(&g, "Kubernetes").unwrap();
        assert!(add_term(&g, "rawkode").is_err());
        assert_eq!(
            prompt::load_glossary(&g).terms,
            vec!["Rawkode", "Kubernetes"]
        );
        remove_term(&g, "RAWKODE").unwrap();
        assert!(remove_term(&g, "nope").is_err());
        let text = std::fs::read_to_string(&g).unwrap();
        assert!(text.starts_with('#'), "{text}");
        assert_eq!(prompt::load_glossary(&g).terms, vec!["Kubernetes"]);

        let p = dir.join("replacements.txt");
        add_rule(&p, "raw code", "Rawkode").unwrap();
        add_rule(&p, "Raw Code", "RawKode").unwrap(); // same from: replaced
        add_rule(&p, "rockwood", "Rawkode").unwrap();
        let (rules, warnings) = load_replacements(&p);
        assert!(warnings.is_empty());
        assert_eq!(
            rules,
            vec![r("Raw Code", "RawKode"), r("rockwood", "Rawkode")]
        );
        remove_rule(&p, "raw code").unwrap();
        assert!(remove_rule(&p, "raw code").is_err());
        assert_eq!(load_replacements(&p).0, vec![r("rockwood", "Rawkode")]);
        assert_eq!(load_replacements(&dir.join("missing")).0, vec![]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

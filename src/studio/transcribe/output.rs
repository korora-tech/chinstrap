//! Transcript outputs (§12.7): `transcript.json`, SRT and VTT, word assembly and filtering.
//!
//! All times are seconds on the media file's presentation timeline (§12.4).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::wer;
use crate::studio::session;

pub const FORMAT: &str = "chinstrap-transcript";
pub const VERSION: u32 = 1;

/// (json, srt, vtt) next to `media`: `transcript.json` for a `mic.*` file, else
/// `<stem>.transcript.json`; `<stem>.srt` and `<stem>.vtt`.
pub fn output_paths(media: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let dir = media.parent().unwrap_or(Path::new("."));
    let stem = media
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "media".into());
    let json = if stem == "mic" {
        dir.join("transcript.json")
    } else {
        dir.join(format!("{stem}.transcript.json"))
    };
    (
        json,
        dir.join(format!("{stem}.srt")),
        dir.join(format!("{stem}.vtt")),
    )
}

/// Seconds rounded to milliseconds.
pub fn r3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub start: f64,
    pub end: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub word: String,
    pub start: f64,
    pub end: f64,
    pub p: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub id: u32,
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub avg_p: f64,
    pub words: Vec<Word>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub file: String,
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EngineInfo {
    pub whisper_rs: String,
    pub whisper_cpp: String,
    pub backend: String,
    pub device: String,
    pub sampling: String,
    pub timestamps: String,
    pub vad: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptInfo {
    pub hint: Option<String>,
    pub terms_used: usize,
    pub terms_dropped: usize,
    pub bytes: usize,
    pub tokens: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Timing {
    pub started: String,
    pub decode_s: f64,
    pub vad_s: f64,
    pub infer_s: f64,
    pub total_s: f64,
    pub rtf: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    pub format: String,
    pub version: u32,
    pub media: String,
    pub timeline: String,
    pub media_start_ns: Option<u64>,
    pub duration_s: f64,
    pub language: String,
    /// "detected" | "override"
    pub language_source: String,
    pub model: ModelInfo,
    pub engine: EngineInfo,
    pub prompt: PromptInfo,
    pub timing: Timing,
    pub speech: Vec<Span>,
    pub text: String,
    pub segments: Vec<Segment>,
}

impl Transcript {
    pub fn word_count(&self) -> u32 {
        self.segments.iter().map(|s| s.words.len() as u32).sum()
    }
}

// ---- word assembly -------------------------------------------------------------------------

/// One decoded token as whisper reports it (times in centiseconds; `t_dtw == -1` when DTW
/// produced nothing).
#[derive(Clone, Debug, PartialEq)]
pub struct RawToken {
    pub id: i32,
    pub text: String,
    pub p: f32,
    pub t0: i64,
    pub t_dtw: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RawSegment {
    pub t0: i64,
    pub t1: i64,
    pub text: String,
    pub tokens: Vec<RawToken>,
}

fn is_punctuation(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty() && t.chars().all(|c| !c.is_alphanumeric())
}

/// DTW starts within this of a run's first start count as one collapsed run.
const RUN_SPAN_S: f64 = 0.1;
/// Shortest collapsed run.
const RUN_MIN: usize = 3;
/// A run that starts this close to its segment's end sits at a decode-window boundary.
const WINDOW_EDGE_S: f64 = 0.25;
/// Neighbouring segments further apart than this are never treated as duplicates.
const DUP_GAP_S: f64 = 3.0;
/// Without VAD, a collapsed tail is spread over this much time per word.
const TAIL_WORD_S: f64 = 0.3;
/// A segment's first word moves back to the VAD onset when it is at most this late.
const ONSET_SNAP_S: f64 = 0.5;
/// Shortest word.
const MIN_WORD_S: f64 = 0.05;

/// A word being assembled: `t` is whisper's start (DTW, else legacy), not yet clamped.
#[derive(Clone, Debug)]
struct WipWord {
    word: String,
    key: String,
    t: f64,
    p: f64,
}

#[derive(Clone, Debug)]
struct WipSeg {
    t0: f64,
    t1: f64,
    text: String,
    words: Vec<WipWord>,
    /// Words were dropped: the text is rebuilt from the words.
    edited: bool,
    /// End of a spread collapsed tail (replaces `t1` for the last word).
    tail_end: Option<f64>,
}

impl WipSeg {
    fn start(&self) -> f64 {
        self.words.first().map_or(self.t0, |w| w.t)
    }

    /// Whisper's segment end, or the last word's time when DTW put words after it (the
    /// timestamp tokens and DTW disagree at window boundaries).
    fn end(&self) -> f64 {
        self.words.last().map_or(self.t1, |w| w.t.max(self.t1))
    }

    /// The trailing collapsed run at the window end, if any: (first index, its time).
    fn edge_tail(&self) -> Option<(usize, f64)> {
        let (a, b) = *collapsed_runs(&self.words).last()?;
        let t = self.words[a].t;
        (b + 1 == self.words.len() && t >= self.end() - WINDOW_EDGE_S).then_some((a, t))
    }
}

/// Maximal runs (inclusive index ranges) of at least `RUN_MIN` words whose starts lie within
/// `RUN_SPAN_S` of the run's first start: DTW gave them one time, typically for text whisper
/// decoded past the end of its 30 s window or repeated from its context at a window start.
fn collapsed_runs(words: &[WipWord]) -> Vec<(usize, usize)> {
    collapsed_runs_min(words, RUN_MIN)
}

fn collapsed_runs_min(words: &[WipWord], min: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let mut j = i;
        while j + 1 < words.len() && words[j + 1].t - words[i].t <= RUN_SPAN_S {
            j += 1;
        }
        if j + 1 - i >= min {
            out.push((i, j));
        }
        i = j + 1;
    }
    out
}

fn span_at(speech: Option<&[Span]>, t: f64) -> Option<&Span> {
    speech.and_then(|spans| spans.iter().find(|s| s.start <= t + 1e-6 && t < s.end))
}

/// Length of the longest common subsequence of two key lists.
fn lcs(a: &[&str], b: &[&str]) -> usize {
    let mut row = vec![0usize; b.len() + 1];
    for x in a {
        let mut diag = 0;
        for (j, y) in b.iter().enumerate() {
            let up = row[j + 1];
            row[j + 1] = if x == y {
                diag + 1
            } else {
                row[j + 1].max(row[j])
            };
            diag = up;
        }
    }
    row[b.len()]
}

fn keys(words: &[WipWord]) -> Vec<&str> {
    words.iter().map(|w| w.key.as_str()).collect()
}

fn in_runs(runs: &[(usize, usize)], i: usize) -> bool {
    runs.iter().any(|&(a, b)| a <= i && i <= b)
}

/// Removes text whisper produced twice around a decode-window boundary (§12.7 "Window
/// duplicates"), between `a` (earlier) and `b` (next). Either may end up empty.
fn dedupe_pair(a: &mut WipSeg, b: &mut WipSeg) {
    if a.words.is_empty() || b.words.is_empty() || b.start() - a.end() > DUP_GAP_S {
        return;
    }
    // 1. A collapsed tail at the window end that the next segment says again: whisper decoded
    //    it without hearing it, then transcribed the audio in the next window.
    if let Some((ra, _)) = a.edge_tail() {
        let n = a.words.len() - ra;
        let common = lcs(&keys(&a.words[ra..]), &keys(&b.words));
        if common >= RUN_MIN.max((n * 3).div_ceil(5)) {
            a.words.truncate(ra);
            a.edited = true;
        }
    }
    if a.words.is_empty() {
        return;
    }
    // 2. The same words end `a` and start `b`: keep the copy whose times are real.
    let max_k = a.words.len().min(b.words.len());
    let ka = keys(&a.words);
    let kb = keys(&b.words);
    let k = (RUN_MIN..=max_k)
        .rev()
        .find(|&k| ka[ka.len() - k..] == kb[..k])
        .unwrap_or(0);
    if k > 0 {
        let runs_a = collapsed_runs(&a.words);
        let runs_b = collapsed_runs(&b.words);
        let col_a = (a.words.len() - k..a.words.len())
            .filter(|&i| in_runs(&runs_a, i))
            .count();
        let col_b = (0..k).filter(|&i| in_runs(&runs_b, i)).count();
        // Only drop a copy that is mostly collapsed: real speech repeats itself too.
        if col_b * 2 >= k && col_b >= col_a {
            b.words.drain(..k);
            b.edited = true;
        } else if col_a * 2 >= k {
            let keep = a.words.len() - k;
            a.words.truncate(keep);
            a.edited = true;
        }
    }
    // 3. Identical text that overlaps in time.
    if !a.words.is_empty()
        && !b.words.is_empty()
        && keys(&a.words) == keys(&b.words)
        && b.start() < a.end()
    {
        b.words.clear();
    }
}

/// Gives the words of each collapsed run distinct, plausible starts (§12.7 "Collapsed runs").
fn spread_runs(
    seg: &mut WipSeg,
    prev_end: f64,
    next_start: Option<f64>,
    speech: Option<&[Span]>,
    duration: f64,
) {
    let last = seg.words.len().saturating_sub(1);
    let seg_end = seg.end();
    // Runs of three or more anywhere, and also a pair at the window end.
    let runs: Vec<(usize, usize)> = collapsed_runs_min(&seg.words, 2)
        .into_iter()
        .filter(|&(a, b)| {
            b + 1 - a >= RUN_MIN || (b == last && seg.words[a].t >= seg_end - WINDOW_EDGE_S)
        })
        .collect();
    for (a, b) in runs {
        let n = (b + 1 - a) as f64;
        let run_t = seg.words[a].t;
        let (lo, hi) = if b == last && run_t >= seg_end - WINDOW_EDGE_S {
            // Tail past the window end: the words were spoken after it, up to the end of
            // their speech span (or of the next segment).
            let mut hi = match span_at(speech, run_t) {
                Some(s) => s.end,
                None => run_t + TAIL_WORD_S * n,
            };
            if let Some(ns) = next_start
                && ns > run_t
            {
                hi = hi.min(ns);
            }
            let hi = hi.min(duration).max(run_t);
            seg.tail_end = Some(hi);
            (run_t, hi)
        } else if a == 0 && b < last {
            // Head at a window start: the words came before the first word with its own time,
            // after the previous segment and the start of their speech span.
            let hi = seg.words[b + 1].t;
            let floor = match span_at(speech, hi) {
                Some(s) => s.start,
                None => seg.t0,
            };
            let lo = floor.max(prev_end).min(run_t);
            (lo, hi)
        } else {
            let hi = if b < last { seg.words[b + 1].t } else { seg.t1 };
            (run_t, hi.max(run_t))
        };
        let step = (hi - lo) / n;
        for (i, w) in seg.words[a..=b].iter_mut().enumerate() {
            w.t = lo + step * i as f64;
        }
    }
}

/// Builds segments with word timings (§12.7 "Word assembly") and applies the filters
/// ("Filtering"), the window-duplicate removal and the collapsed-run spreading. `eot` is the
/// first special token id; `speech` is the VAD output, `None` when no VAD model was used;
/// `duration` (seconds of decoded audio) bounds every time.
pub fn assemble(
    raw: &[RawSegment],
    eot: i32,
    speech: Option<&[Span]>,
    duration: f64,
) -> Vec<Segment> {
    let duration = if duration > 0.0 { duration } else { f64::MAX };
    // 1. Words per segment, with whisper's own times.
    let mut segs: Vec<WipSeg> = Vec::new();
    for seg in raw {
        let seg_t1 = seg.t1 as f64 / 100.0;
        // (text, token start seconds, probabilities)
        let mut words: Vec<(String, f64, Vec<f32>)> = Vec::new();
        for tok in &seg.tokens {
            if tok.id >= eot || tok.text.starts_with("[_") || tok.text.is_empty() {
                continue;
            }
            let cs = if tok.t_dtw >= 0 { tok.t_dtw } else { tok.t0 };
            let t = cs.max(0) as f64 / 100.0;
            let punct = is_punctuation(&tok.text);
            // A leading-space token starts a word; punctuation and sub-word pieces attach to
            // the previous word.
            let starts_word = tok.text.starts_with(' ') && !punct;
            match words.last_mut() {
                Some(last) if !starts_word => {
                    last.0.push_str(tok.text.trim_start_matches(' '));
                    if !punct {
                        last.2.push(tok.p);
                    }
                }
                _ => words.push((
                    tok.text.trim().to_string(),
                    t,
                    if punct { Vec::new() } else { vec![tok.p] },
                )),
            }
        }
        let text = seg.text.trim().to_string();
        if words.is_empty() || wer::normalize(&text).is_empty() {
            continue;
        }
        let words: Vec<WipWord> = words
            .into_iter()
            .map(|(word, t, ps)| WipWord {
                key: wer::normalize(&word).join(" "),
                p: if ps.is_empty() {
                    0.0
                } else {
                    ps.iter().map(|&p| p as f64).sum::<f64>() / ps.len() as f64
                },
                word,
                t: t.min(duration),
            })
            .collect();
        segs.push(WipSeg {
            t0: seg.t0 as f64 / 100.0,
            t1: seg_t1.min(duration),
            text,
            words,
            edited: false,
            tail_end: None,
        });
    }
    // VAD filter on whisper's own extent. A collapsed tail at the window end stands for
    // speech after it: when no later segment transcribes that speech span, the tail covers
    // the span (it is the only text for it).
    if let Some(spans) = speech {
        let next_starts: Vec<Option<f64>> = (0..segs.len())
            .map(|i| segs.get(i + 1).map(WipSeg::start))
            .collect();
        let mut i = 0;
        segs.retain(|seg| {
            let next_start = next_starts[i];
            i += 1;
            let start = seg.start();
            let last_t = seg.words[seg.words.len() - 1].t;
            let mut end = match span_at(speech, last_t) {
                Some(s) => seg.t1.min(s.end + 0.3),
                None => seg.t1,
            }
            .max(last_t + MIN_WORD_S);
            if let Some((_, run_t)) = seg.edge_tail()
                && let Some(sp) = span_at(speech, run_t)
                && next_start.is_none_or(|n| n >= sp.end)
            {
                end = end.max(sp.end);
            }
            let overlap: f64 = spans
                .iter()
                .map(|s| (end.min(s.end) - start.max(s.start)).max(0.0))
                .sum();
            overlap >= 0.2
        });
    }
    // 2. Window duplicates.
    for i in 1..segs.len() {
        let (head, tail) = segs.split_at_mut(i);
        let (a, b) = (&mut head[i - 1], &mut tail[0]);
        dedupe_pair(a, b);
    }
    segs.retain(|s| !s.words.is_empty());
    // 3. Collapsed runs, then the transcript-wide clamps.
    let mut out = Vec::new();
    let mut floor = 0.0f64; // start times are clamped non-decreasing across the transcript
    let mut prev_end = 0.0f64;
    for i in 0..segs.len() {
        let next_start = segs.get(i + 1).map(WipSeg::start);
        let seg = &mut segs[i];
        spread_runs(seg, prev_end, next_start, speech, duration);
        let mut built: Vec<Word> = Vec::with_capacity(seg.words.len());
        for (j, w) in seg.words.iter().enumerate() {
            let mut t = w.t;
            // DTW onsets run late: a segment's first word snaps back to a VAD onset just
            // before it.
            if j == 0
                && let Some(sp) = speech.and_then(|spans| {
                    spans
                        .iter()
                        .find(|s| s.start < t && t - s.start <= ONSET_SNAP_S)
                })
            {
                t = sp.start;
            }
            // Strictly increasing by at least the shortest word, so ends never overlap.
            let min_start = if out.is_empty() && built.is_empty() {
                floor
            } else {
                floor + MIN_WORD_S
            };
            let start = t.max(min_start).min(duration);
            floor = start;
            built.push(Word {
                word: w.word.clone(),
                start,
                end: start,
                p: w.p,
            });
        }
        let seg_end = seg.tail_end.unwrap_or(seg.t1);
        for j in 0..built.len() {
            let start = built[j].start;
            let end = match built.get(j + 1) {
                Some(next) => next.start,
                None => {
                    let mut end = match span_at(speech, start) {
                        Some(s) => seg_end.min(s.end + 0.3),
                        None => seg_end,
                    };
                    // Never past the next segment's first word.
                    if let Some(ns) = next_start
                        && ns > start
                    {
                        end = end.min(ns);
                    }
                    end
                }
            };
            built[j].end = end.max(start + MIN_WORD_S).min(duration);
        }
        let text = if seg.edited {
            seg.words
                .iter()
                .map(|w| w.word.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            seg.text.clone()
        };
        if wer::normalize(&text).is_empty() {
            continue;
        }
        let start = built.first().map_or(0.0, |w| w.start);
        let end = built.last().map_or(0.0, |w| w.end);
        prev_end = end;
        let avg_p = built.iter().map(|w| w.p).sum::<f64>() / built.len() as f64;
        out.push(Segment {
            id: out.len() as u32,
            start: r3(start),
            end: r3(end),
            text,
            avg_p: r3(avg_p),
            words: built
                .into_iter()
                .map(|w| Word {
                    start: r3(w.start),
                    end: r3(w.end),
                    p: r3(w.p),
                    word: w.word,
                })
                .collect(),
        });
    }
    // A segment's last word never runs into the next segment's first one (onset snapping
    // and spreading may have moved that earlier).
    for i in 0..out.len().saturating_sub(1) {
        let next = out[i + 1].words[0].start;
        let seg = &mut out[i];
        if let Some(last) = seg.words.last_mut()
            && last.end > next
            && next > last.start
        {
            last.end = next;
            seg.end = next;
        }
    }
    out
}

pub fn joined_text(segments: &[Segment]) -> String {
    segments
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

// ---- SRT / VTT -----------------------------------------------------------------------------

const CUE_MAX_S: f64 = 7.0;
const LINE_MAX: usize = 42;
const CUE_MAX_LINES: usize = 2;
/// Shortest cue, so clamping to the neighbours never leaves an empty one.
const MIN_CUE_S: f64 = 0.02;

/// Greedy word wrap at `LINE_MAX` characters.
fn wrap(words: &[&str]) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for w in words {
        match lines.last_mut() {
            Some(l) if l.chars().count() + 1 + w.chars().count() <= LINE_MAX => {
                l.push(' ');
                l.push_str(w);
            }
            _ => lines.push(w.to_string()),
        }
    }
    lines
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub lines: Vec<String>,
}

/// One cue per segment; a segment longer than 7 s or than two 42-character lines is split at
/// word boundaries.
pub fn cues(segments: &[Segment]) -> Vec<Cue> {
    let mut out = Vec::new();
    for seg in segments {
        let mut cur: Vec<&Word> = Vec::new();
        let flush = |cur: &mut Vec<&Word>, out: &mut Vec<Cue>| {
            if let (Some(first), Some(last)) = (cur.first(), cur.last()) {
                let words: Vec<&str> = cur.iter().map(|w| w.word.as_str()).collect();
                out.push(Cue {
                    start: first.start,
                    end: last.end,
                    lines: wrap(&words),
                });
            }
            cur.clear();
        };
        for w in &seg.words {
            if let Some(first) = cur.first() {
                let mut words: Vec<&str> = cur.iter().map(|w| w.word.as_str()).collect();
                words.push(&w.word);
                if w.end - first.start > CUE_MAX_S || wrap(&words).len() > CUE_MAX_LINES {
                    flush(&mut cur, &mut out);
                }
            }
            cur.push(w);
        }
        flush(&mut cur, &mut out);
    }
    // Cues never overlap and always have a duration: each starts at or after the previous
    // one's end and ends at or before the next one's start.
    for i in 0..out.len() {
        if i > 0 {
            let prev_end = out[i - 1].end;
            out[i].start = out[i].start.max(prev_end);
        }
        if let Some(next_start) = out.get(i + 1).map(|c| c.start) {
            out[i].end = out[i].end.min(next_start);
        }
        if out[i].end < out[i].start + MIN_CUE_S {
            out[i].end = out[i].start + MIN_CUE_S;
        }
    }
    out
}

/// Cue text must not end the cue or open markup: a line break or `-->` would, so both go.
fn cue_line(line: &str) -> String {
    line.replace(['\r', '\n'], " ").replace("-->", "\u{2192}")
}

/// WebVTT cue payloads are markup: `&`, `<` and `>` are escaped (after `cue_line`).
fn vtt_escape(line: &str) -> String {
    cue_line(line)
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn timestamp(secs: f64, sep: char) -> String {
    let ms = (secs.max(0.0) * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02}{sep}{:03}",
        ms / 3_600_000,
        (ms / 60_000) % 60,
        (ms / 1000) % 60,
        ms % 1000
    )
}

pub fn render_srt(cues: &[Cue]) -> String {
    let mut s = String::new();
    for (i, c) in cues.iter().enumerate() {
        s.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            timestamp(c.start, ','),
            timestamp(c.end, ','),
            c.lines
                .iter()
                .map(|l| cue_line(l))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    s
}

pub fn render_vtt(cues: &[Cue]) -> String {
    let mut s = String::from("WEBVTT\n");
    for c in cues {
        s.push_str(&format!(
            "\n{} --> {}\n{}\n",
            timestamp(c.start, '.'),
            timestamp(c.end, '.'),
            c.lines
                .iter()
                .map(|l| vtt_escape(l))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    s
}

/// Writes srt, vtt, then json last (json present means complete), each atomically.
pub fn write_outputs(media: &Path, t: &Transcript) -> std::io::Result<(PathBuf, PathBuf, PathBuf)> {
    let (json, srt, vtt) = output_paths(media);
    let c = cues(&t.segments);
    session::write_atomic(&srt, render_srt(&c).as_bytes())?;
    session::write_atomic(&vtt, render_vtt(&c).as_bytes())?;
    let mut body = serde_json::to_vec_pretty(t).map_err(std::io::Error::other)?;
    body.push(b'\n');
    session::write_atomic(&json, &body)?;
    Ok((json, srt, vtt))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EOT: i32 = 50257;

    fn tok(text: &str, t0: i64, dtw: i64) -> RawToken {
        RawToken {
            id: 100,
            text: text.into(),
            p: 0.5,
            t0,
            t_dtw: dtw,
        }
    }

    fn special(text: &str) -> RawToken {
        RawToken {
            id: EOT + 5,
            text: text.into(),
            p: 1.0,
            t0: 0,
            t_dtw: -1,
        }
    }

    #[test]
    fn naming() {
        let (j, s, v) = output_paths(Path::new("/s/segment-001/mic.mkv"));
        assert_eq!(j, Path::new("/s/segment-001/transcript.json"));
        assert_eq!(s, Path::new("/s/segment-001/mic.srt"));
        assert_eq!(v, Path::new("/s/segment-001/mic.vtt"));
        let (j, s, _) = output_paths(Path::new("/x/jargon.mkv"));
        assert_eq!(j, Path::new("/x/jargon.transcript.json"));
        assert_eq!(s, Path::new("/x/jargon.srt"));
    }

    #[test]
    fn words_dtw_fallback_and_punctuation() {
        let raw = vec![RawSegment {
            t0: 200,
            t1: 400,
            text: " Hello, whisper.cpp world.".into(),
            tokens: vec![
                special("[_BEG_]"),
                tok(" Hello", 200, 230),
                tok(",", 240, 245),
                tok(" whisper", 250, -1), // DTW missing: legacy t0
                tok(".", 260, 262),
                tok("cpp", 265, 268),
                tok(" world", 270, 300),
                tok(".", 330, 331),
                special("[_TT_200]"),
            ],
        }];
        let segs = assemble(&raw, EOT, None, 0.0);
        assert_eq!(segs.len(), 1);
        let w = &segs[0].words;
        let words: Vec<&str> = w.iter().map(|w| w.word.as_str()).collect();
        assert_eq!(words, vec!["Hello,", "whisper.cpp", "world."]);
        assert_eq!(w[0].start, 2.3);
        assert_eq!(w[1].start, 2.5);
        assert_eq!(w[0].end, 2.5);
        assert_eq!(w[2].start, 3.0);
        assert_eq!(
            w[2].end, 4.0,
            "last word ends at the segment end without VAD"
        );
        assert_eq!(segs[0].start, 2.3);
        assert_eq!(segs[0].text, "Hello, whisper.cpp world.");
        assert_eq!(w[0].p, 0.5);
    }

    #[test]
    fn non_decreasing_and_minimum_length() {
        let raw = vec![RawSegment {
            t0: 0,
            t1: 100,
            text: " a b".into(),
            tokens: vec![tok(" a", 50, 50), tok(" b", 40, 40)],
        }];
        let segs = assemble(&raw, EOT, None, 0.0);
        let w = &segs[0].words;
        assert_eq!(w[1].start, 0.55, "clamped after the previous start");
        assert!(w[0].end >= w[0].start + 0.05);
    }

    #[test]
    fn vad_filter_and_last_word_end() {
        let raw = vec![
            RawSegment {
                t0: 0,
                t1: 900,
                text: " speech here".into(),
                tokens: vec![tok(" speech", 200, 205), tok(" here", 250, 255)],
            },
            RawSegment {
                t0: 900,
                t1: 1200,
                text: " Thank you.".into(),
                tokens: vec![tok(" Thank", 1000, 1000), tok(" you.", 1050, 1050)],
            },
            RawSegment {
                t0: 1200,
                t1: 1300,
                text: " ...".into(),
                tokens: vec![tok(" ...", 1200, 1200)],
            },
        ];
        let speech = [Span {
            start: 2.0,
            end: 3.0,
        }];
        let segs = assemble(&raw, EOT, Some(&speech), 0.0);
        assert_eq!(segs.len(), 1, "hallucination outside speech is dropped");
        let last = segs[0].words.last().unwrap();
        assert_eq!(last.end, 3.3, "min(segment end 9.0, VAD end 3.0 + 0.3)");
        // Without VAD both text segments survive; the punctuation-only one never does.
        assert_eq!(assemble(&raw, EOT, None, 0.0).len(), 2);
        assert!(assemble(&raw, EOT, Some(&[]), 0.0).is_empty());
    }

    fn seg(words: &[(&str, f64, f64)]) -> Segment {
        Segment {
            id: 0,
            start: words[0].1,
            end: words.last().unwrap().2,
            text: words.iter().map(|w| w.0).collect::<Vec<_>>().join(" "),
            avg_p: 1.0,
            words: words
                .iter()
                .map(|&(w, s, e)| Word {
                    word: w.into(),
                    start: s,
                    end: e,
                    p: 1.0,
                })
                .collect(),
        }
    }

    #[test]
    fn srt_vtt_format() {
        let s = seg(&[("Hello", 1.0, 1.5), ("world.", 1.5, 2.25)]);
        let c = cues(&[s]);
        assert_eq!(
            render_srt(&c),
            "1\n00:00:01,000 --> 00:00:02,250\nHello world.\n\n"
        );
        assert_eq!(
            render_vtt(&c),
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.250\nHello world.\n"
        );
        assert_eq!(render_vtt(&[]), "WEBVTT\n");
        assert_eq!(render_srt(&[]), "");
        assert_eq!(timestamp(3723.4567, ','), "01:02:03,457");
    }

    #[test]
    fn cue_splitting() {
        // 12 words over 12 s: split by the 7 s rule.
        let words: Vec<(String, f64, f64)> = (0..12)
            .map(|i| (format!("w{i}"), i as f64, i as f64 + 1.0))
            .collect();
        let refs: Vec<(&str, f64, f64)> =
            words.iter().map(|(w, s, e)| (w.as_str(), *s, *e)).collect();
        let c = cues(&[seg(&refs)]);
        assert!(c.len() >= 2);
        assert!(c.iter().all(|c| c.end - c.start <= 7.0 + 1e-9));
        // Long text in a short time: split by the two-line rule.
        let long: Vec<(String, f64, f64)> = (0..30)
            .map(|i| (format!("word{i:02}"), i as f64 * 0.1, i as f64 * 0.1 + 0.1))
            .collect();
        let refs: Vec<(&str, f64, f64)> =
            long.iter().map(|(w, s, e)| (w.as_str(), *s, *e)).collect();
        let c = cues(&[seg(&refs)]);
        assert!(c.len() >= 3);
        for cue in &c {
            assert!(cue.lines.len() <= 2);
            assert!(cue.lines.iter().all(|l| l.chars().count() <= 42));
        }
        let n: usize = c
            .iter()
            .map(|c| c.lines.iter().map(|l| l.split(' ').count()).sum::<usize>())
            .sum();
        assert_eq!(n, 30, "no word lost");
    }

    /// A raw segment from (word, centiseconds) pairs, every word with a leading space.
    fn rseg(t0: i64, t1: i64, words: &[(&str, i64)]) -> RawSegment {
        RawSegment {
            t0,
            t1,
            text: words.iter().map(|w| format!(" {}", w.0)).collect(),
            tokens: words
                .iter()
                .map(|&(w, cs)| tok(&format!(" {w}"), cs, cs))
                .collect(),
        }
    }

    const SENTENCE: [&str; 17] = [
        "Yesterday",
        "I",
        "used",
        "kubectl",
        "to",
        "inspect",
        "the",
        "Kubernetes",
        "cluster,",
        "and",
        "then",
        "Cilium",
        "replaced",
        "kube-proxy",
        "with",
        "eBPF",
        "programs.",
    ];

    fn all_words(segs: &[Segment]) -> Vec<&Word> {
        segs.iter().flat_map(|s| s.words.iter()).collect()
    }

    fn assert_cues_ordered(segs: &[Segment]) {
        let c = cues(segs);
        for pair in c.windows(2) {
            assert!(
                pair[0].end <= pair[1].start + 1e-9,
                "overlap: {:?} / {:?}",
                pair[0],
                pair[1]
            );
        }
        assert!(c.iter().all(|c| c.end > c.start));
    }

    #[test]
    fn window_duplicate_is_removed_and_runs_spread() {
        // Shape of a real 700 s run: the window ends at 129.03 s mid-sentence; whisper decodes
        // the rest of the sentence at 128.98 s, then repeats the whole sentence in the next
        // window with its first half collapsed at 131.0 s.
        let a: Vec<(&str, i64)> = SENTENCE
            .iter()
            .enumerate()
            .map(|(i, w)| (*w, if i < 7 { 12650 + i as i64 * 30 } else { 12898 }))
            .collect();
        let b: Vec<(&str, i64)> = SENTENCE
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let cs = match i {
                    0 => 13070,
                    1..=11 => 13090 + (i as i64 % 3) * 5,
                    _ => 13140 + (i as i64 - 12) * 50,
                };
                (*w, cs)
            })
            .collect();
        let raw = vec![rseg(12650, 12903, &a), rseg(13070, 13463, &b)];
        let speech = [Span {
            start: 126.02,
            end: 134.33,
        }];
        let segs = assemble(&raw, EOT, Some(&speech), 700.0);
        let words: Vec<&str> = all_words(&segs).iter().map(|w| w.word.as_str()).collect();
        assert_eq!(words, SENTENCE.to_vec(), "one copy of the sentence");
        let w = all_words(&segs);
        for pair in w.windows(2) {
            assert!(pair[1].start > pair[0].start, "{pair:?}");
            assert!(pair[0].end <= pair[1].start + 1e-9, "{pair:?}");
        }
        assert_eq!(segs[0].text, "Yesterday I used kubectl to inspect the");
        assert_cues_ordered(&segs);
    }

    #[test]
    fn collapsed_tail_without_repeat_spreads_to_the_speech_end() {
        // The window ends at 114.03 s; the last five words share 113.92 s and the next window
        // starts with the next sentence.
        let mut a: Vec<(&str, i64)> = SENTENCE[..12]
            .iter()
            .enumerate()
            .map(|(i, w)| (*w, 10876 + i as i64 * 40))
            .collect();
        a.extend(SENTENCE[12..].iter().map(|w| (*w, 11392)));
        let b = [("Later", 11718), ("we", 11740), ("rebuilt", 11782)];
        let raw = vec![rseg(10876, 11403, &a), rseg(11718, 11800, &b)];
        let speech = [
            Span {
                start: 108.29,
                end: 116.64,
            },
            Span {
                start: 116.9,
                end: 122.27,
            },
        ];
        let segs = assemble(&raw, EOT, Some(&speech), 700.0);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].words.len(), 17, "nothing dropped");
        let tail = &segs[0].words[12..];
        assert_eq!(tail[0].start, 113.92);
        assert!(tail.windows(2).all(|p| p[1].start - p[0].start > 0.5));
        assert!(
            tail[4].end <= 116.64 + 1e-9 && tail[4].end > 116.0,
            "{:?}",
            tail[4]
        );
        assert_cues_ordered(&segs);
    }

    #[test]
    fn real_repetition_is_kept() {
        // A speaker repeats a phrase across segments; nothing is collapsed, so both stay.
        let a = [("we", 100), ("need", 130), ("to", 160), ("ship", 190)];
        let b = [("we", 400), ("need", 430), ("to", 460), ("ship", 490)];
        let raw = vec![rseg(100, 250, &a), rseg(400, 550, &b)];
        let segs = assemble(&raw, EOT, None, 10.0);
        assert_eq!(all_words(&segs).len(), 8);
    }

    #[test]
    fn times_never_pass_the_duration() {
        let a = [("with", 69996), ("eBPF.", 69996)];
        let raw = vec![rseg(69900, 70001, &a)];
        let segs = assemble(&raw, EOT, None, 700.0);
        let w = all_words(&segs);
        assert!(
            w.iter().all(|w| w.start <= 700.0 && w.end <= 700.0),
            "{w:?}"
        );
        assert!(segs[0].end <= 700.0);
        assert!(cues(&segs).iter().all(|c| c.start <= 700.0));
    }

    #[test]
    fn first_word_snaps_to_a_vad_onset() {
        let raw = vec![rseg(200, 400, &[("Yesterday", 246), ("I", 270)])];
        let speech = [Span {
            start: 1.99,
            end: 3.5,
        }];
        let segs = assemble(&raw, EOT, Some(&speech), 10.0);
        assert_eq!(segs[0].words[0].start, 1.99);
        assert_eq!(segs[0].words[1].start, 2.7, "later words keep their times");
        // Too far after the onset: unchanged.
        let raw = vec![rseg(200, 400, &[("Later", 300)])];
        let segs = assemble(&raw, EOT, Some(&speech), 10.0);
        assert_eq!(segs[0].words[0].start, 3.0);
    }

    #[test]
    fn overlapping_segments_give_ordered_cues() {
        let s1 = seg(&[("one", 1.0, 1.5), ("two", 1.5, 3.0)]);
        let s2 = seg(&[("three", 2.0, 2.5), ("four", 2.5, 2.6)]);
        let s3 = seg(&[("five", 2.0, 2.0)]);
        let c = cues(&[s1, s2, s3]);
        assert_eq!(c.len(), 3);
        for p in c.windows(2) {
            assert!(p[0].end <= p[1].start + 1e-9, "{p:?}");
        }
        assert!(c.iter().all(|c| c.end > c.start));
    }

    #[test]
    fn cue_text_is_escaped() {
        let s = seg(&[
            ("Vec<String>", 1.0, 1.5),
            ("&", 1.5, 1.6),
            ("x", 1.6, 1.7),
            ("-->", 1.7, 1.8),
            ("y", 1.8, 2.0),
        ]);
        let c = cues(&[s]);
        let vtt = render_vtt(&c);
        assert!(
            vtt.ends_with("\nVec&lt;String&gt; &amp; x \u{2192} y\n"),
            "{vtt}"
        );
        assert_eq!(vtt.matches("-->").count(), 1, "only the timing line");
        let srt = render_srt(&c);
        assert!(srt.contains("\nVec<String> & x \u{2192} y\n"), "{srt}");
        assert_eq!(srt.matches("-->").count(), 1);
    }
}

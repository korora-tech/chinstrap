//! Word error rate for the accuracy checks (§12.9, §12.12).
//!
//! Normalisation matches `scratchpad/tools/wer.py`: lowercase, `-` and `.` removed (so
//! "G-Streamer" = "gstreamer" and "whisper.cpp" = "whispercpp"), every other character that is
//! not a letter, digit, `_` or whitespace becomes a space. Unlike the Python tool, no NFKC
//! folding is applied.

pub fn normalize(text: &str) -> Vec<String> {
    let mut out = String::with_capacity(text.len());
    for c in text.chars().flat_map(char::to_lowercase) {
        match c {
            '-' | '.' => {}
            c if c.is_alphanumeric() || c == '_' || c.is_whitespace() => out.push(c),
            _ => out.push(' '),
        }
    }
    out.split_whitespace().map(str::to_string).collect()
}

/// (word edits, reference words) between `reference` and `hypothesis` after normalisation.
pub fn wer(reference: &str, hypothesis: &str) -> (usize, usize) {
    let r = normalize(reference);
    let h = normalize(hypothesis);
    let mut row: Vec<usize> = (0..=h.len()).collect();
    for (i, rw) in r.iter().enumerate() {
        let mut diag = row[0];
        row[0] = i + 1;
        for (j, hw) in h.iter().enumerate() {
            let up = row[j + 1];
            row[j + 1] = (up + 1).min(row[j] + 1).min(diag + usize::from(rw != hw));
            diag = up;
        }
    }
    (row[h.len()], r.len())
}

/// WER as a fraction; 0 for an empty reference with an empty hypothesis.
pub fn wer_rate(reference: &str, hypothesis: &str) -> f64 {
    let (e, n) = wer(reference, hypothesis);
    if n == 0 {
        return if e == 0 { 0.0 } else { 1.0 };
    }
    e as f64 / n as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalisation() {
        assert_eq!(
            normalize("Chinstrap records with G-Streamer, and whisper.cpp!"),
            vec![
                "chinstrap",
                "records",
                "with",
                "gstreamer",
                "and",
                "whispercpp"
            ]
        );
        assert_eq!(normalize("  \"Hello\"\n(world) "), vec!["hello", "world"]);
        assert!(normalize("... — !").is_empty());
        assert_eq!(normalize("eBPF's"), vec!["ebpf", "s"]);
    }

    #[test]
    fn edit_distance() {
        assert_eq!(wer("a b c", "a b c"), (0, 3));
        assert_eq!(wer("a b c", "a x c"), (1, 3));
        assert_eq!(wer("a b c", "a c"), (1, 3));
        assert_eq!(wer("a b c", "a b c d"), (1, 3));
        assert_eq!(wer("Kubernetes, Cilium.", "kubernetes cilium"), (0, 2));
        assert_eq!(wer("", ""), (0, 0));
        assert_eq!(wer_rate("one two three four", "one too three four"), 0.25);
    }
}

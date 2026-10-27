//! Text chunking shared by the FTS and vector indexes.
//!
//! Whole-file indexing collapses at scale (BM25 over 45KB documents ranks on
//! distractors; one head embedding misses everything past the first lines).
//! Both indexes store per-chunk rows and aggregate to file-level results.

/// Split text into line-aligned chunks of roughly `target_bytes` each.
/// A single line longer than the target becomes its own chunk.
pub fn chunk_text(text: &str, target_bytes: usize) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut end = 0;

    for line in text.split_inclusive('\n') {
        let line_end = end + line.len();
        if end > start && line_end - start > target_bytes {
            chunks.push(&text[start..end]);
            start = end;
        }
        end = line_end;
    }
    if end > start {
        chunks.push(&text[start..end]);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_text_is_one_chunk() {
        let text = "one\ntwo\nthree\n";
        assert_eq!(chunk_text(text, 100), vec![text]);
    }

    #[test]
    fn test_chunks_align_to_lines_and_cover_everything() {
        let text = "alpha line\n".repeat(100);
        let chunks = chunk_text(&text, 200);
        assert!(chunks.len() > 3);
        for chunk in &chunks {
            assert!(chunk.ends_with('\n'));
            assert!(chunk.len() <= 200 + "alpha line\n".len());
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn test_overlong_line_is_own_chunk() {
        let long = "x".repeat(500);
        let text = format!("short\n{long}\nshort\n");
        let chunks = chunk_text(&text, 100);
        assert_eq!(chunks.concat(), text);
        assert!(chunks.iter().any(|c| c.len() >= 500));
    }

    #[test]
    fn test_empty() {
        assert!(chunk_text("", 100).is_empty());
    }
}

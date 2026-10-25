/// Estimate LLM token count for a byte slice.
///
/// Uses a simple heuristic: ~4 characters per token for English/code.
/// This is intentionally conservative (overestimates slightly) since
/// underestimating tokens is worse than overestimating.
pub fn estimate_tokens(data: &[u8]) -> u64 {
    // For UTF-8 text: count characters, divide by 3.5 (≈ GPT-4 average).
    // For binary: count bytes / 4.
    let char_count = if let Ok(text) = std::str::from_utf8(data) {
        text.chars().count() as u64
    } else {
        data.len() as u64
    };
    // ceil(chars / 3.5) ≈ (chars * 2 + 6) / 7
    (char_count * 2 + 6) / 7
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate() {
        // "hello world" = 11 chars ≈ 3-4 tokens
        assert!(estimate_tokens(b"hello world") >= 2);
        assert!(estimate_tokens(b"hello world") <= 5);
    }

    #[test]
    fn test_empty() {
        assert_eq!(estimate_tokens(b""), 0);
    }

    #[test]
    fn test_longer_text() {
        // 1000 chars should be roughly 250-350 tokens
        let text = "a".repeat(1000);
        let tokens = estimate_tokens(text.as_bytes());
        assert!(tokens > 200);
        assert!(tokens < 400);
    }
}

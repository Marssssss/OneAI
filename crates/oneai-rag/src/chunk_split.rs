//! Input-size enforcement + byte-bisection chunk splitting for embedding.
//!
//! Embedding providers reject inputs above a per-model token cap. We don't ship
//! a tokenizer per provider, so we use UTF-8 byte length as a conservative
//! upper bound for token count (a token must contain at least one byte, so
//! `token_count ≤ utf8_byte_length`). Splitting on byte boundaries — never
//! inside a character — keeps text valid for the provider.
//!
//! Ported (and simplified) from OpenClaw's `embedding-input-limits.ts` /
//! `embedding-chunk-limits.ts` / `manager-embedding-policy.ts`.

use oneai_core::traits::EmbeddingService;

/// UTF-8 byte length of a string (Rust `str` byte length).
pub fn estimate_utf8_bytes(text: &str) -> usize {
    text.len()
}

/// Split `text` into chunks each no longer than `max_bytes` UTF-8 bytes, never
/// cutting inside a character (so output chunks are always valid UTF-8).
///
/// `max_bytes == 0` disables splitting (returns the whole text as one chunk).
/// A single character wider than the limit still gets its own chunk (we never
/// drop bytes).
pub fn split_to_utf8_byte_limit(text: &str, max_bytes: usize) -> Vec<String> {
    if max_bytes == 0 || text.len() <= max_bytes {
        return vec![text.to_string()];
    }
    let mut parts = Vec::new();
    let mut buf = String::new();
    for ch in text.chars() {
        let ch_bytes = ch.len_utf8();
        if !buf.is_empty() && buf.len() + ch_bytes > max_bytes {
            parts.push(std::mem::take(&mut buf));
        }
        buf.push(ch);
    }
    if !buf.is_empty() {
        parts.push(buf);
    }
    parts
}

/// Split every over-limit text in `texts` to the service's effective byte cap,
/// leaving under-limit texts untouched. `None` cap ⇒ no splitting.
pub fn enforce_max_input_tokens(service: &dyn EmbeddingService, texts: &[String]) -> Vec<String> {
    match service.max_input_tokens() {
        Some(0) | None => texts.to_vec(),
        Some(cap) => {
            let mut out = Vec::with_capacity(texts.len());
            for t in texts {
                if t.len() <= cap {
                    out.push(t.clone());
                } else {
                    out.extend(split_to_utf8_byte_limit(t, cap));
                }
            }
            out
        }
    }
}

/// Greedily pack items into batches whose combined byte size stays under
/// `max_tokens`. An item larger than the cap gets a batch of its own (it is
/// expected to have already been split by [`enforce_max_input_tokens`] when
/// the caller wants hard enforcement; this helper only sizes transport batches).
pub fn build_batches<T, F>(chunks: Vec<T>, max_tokens: usize, size_of: F) -> Vec<Vec<T>>
where
    F: Fn(&T) -> usize,
{
    if max_tokens == 0 {
        return if chunks.is_empty() { Vec::new() } else { vec![chunks] };
    }
    let mut batches: Vec<Vec<T>> = Vec::new();
    let mut cur: Vec<T> = Vec::new();
    let mut cur_tokens = 0usize;
    for chunk in chunks {
        let est = size_of(&chunk);
        if !cur.is_empty() && cur_tokens + est > max_tokens {
            batches.push(std::mem::take(&mut cur));
            cur_tokens = 0;
        }
        if cur.is_empty() && est > max_tokens {
            batches.push(vec![chunk]);
            continue;
        }
        cur_tokens += est;
        cur.push(chunk);
    }
    if !cur.is_empty() {
        batches.push(cur);
    }
    batches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_ascii_under_limit_one_chunk() {
        assert_eq!(split_to_utf8_byte_limit("abc", 10), vec!["abc".to_string()]);
    }

    #[test]
    fn split_ascii_splits_evenly() {
        assert_eq!(
            split_to_utf8_byte_limit("abcdefgh", 3),
            vec!["abc".to_string(), "def".to_string(), "gh".to_string()]
        );
    }

    #[test]
    fn split_cjk_never_breaks_char() {
        // each CJK char is 3 UTF-8 bytes; limit 4 → one char per chunk
        let parts = split_to_utf8_byte_limit("你好世界", 4);
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "你");
        assert_eq!(parts[3], "界");
    }

    #[test]
    fn split_cjk_two_per_chunk_at_limit_6() {
        let parts = split_to_utf8_byte_limit("你好世界", 6);
        assert_eq!(parts, vec!["你好".to_string(), "世界".to_string()]);
    }

    #[test]
    fn split_surrogate_pair_emoji_preserved() {
        // 😀 is 4 UTF-8 bytes (one char); limit 2 must still keep it whole
        let parts = split_to_utf8_byte_limit("a😀b", 2);
        assert!(parts.iter().all(|p| p.is_char_boundary(p.len()) || p.is_empty()));
        // re-joining yields the original
        assert_eq!(parts.concat(), "a😀b");
    }

    #[test]
    fn split_zero_limit_disables() {
        assert_eq!(split_to_utf8_byte_limit("hello", 0), vec!["hello".to_string()]);
    }

    #[test]
    fn build_batches_packs_under_cap() {
        let items = vec!["ab".to_string(), "cd".to_string(), "efgh".to_string()];
        let batches = build_batches(items, 4, |s: &String| s.len());
        // "ab"(2)+"cd"(2)=4 → batch1; "efgh"(4)=4 → batch2
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0], vec!["ab".to_string(), "cd".to_string()]);
        assert_eq!(batches[1], vec!["efgh".to_string()]);
    }

    #[test]
    fn build_batches_oversize_alone() {
        let items = vec!["x".to_string(), " toolongggg".to_string(), "y".to_string()];
        let batches = build_batches(items, 3, |s: &String| s.len());
        // "x"(1) batch1; " toolongggg"(11) alone batch2; "y"(1) batch3
        assert_eq!(batches.len(), 3);
    }

    struct NoCapService;
    #[async_trait::async_trait]
    impl oneai_core::traits::EmbeddingService for NoCapService {
        async fn embed(&self, _t: &str) -> oneai_core::error::Result<Vec<f32>> { Ok(vec![]) }
        async fn embed_batch(&self, _t: &[String]) -> oneai_core::error::Result<Vec<Vec<f32>>> { Ok(vec![]) }
        fn model(&self) -> oneai_core::EmbeddingModel { oneai_core::EmbeddingModel::new("none") }
    }

    #[test]
    fn enforce_no_cap_returns_unchanged() {
        let svc = NoCapService;
        let texts = vec!["a".to_string(), "bb".to_string()];
        assert_eq!(enforce_max_input_tokens(&svc, &texts), texts);
    }
}

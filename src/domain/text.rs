//! Shortening of text that is shown or sent with a length limit.

/// The first `limit` characters of `text`, followed by '…' when it was
/// longer.
pub fn cut(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// `text` in at most `limit` characters: when it is longer, its start and a
/// '…', which counts toward the limit.
pub fn cut_within(text: &str, limit: usize) -> String {
    if text.chars().nth(limit).is_none() {
        return text.to_string();
    }
    cut(text, limit.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cut_keeps_the_limit_and_marks_the_rest() {
        assert_eq!(cut("abc", 3), "abc");
        assert_eq!(cut("abcd", 3), "abc…");
        assert_eq!(cut("作品を検索", 2), "作品…");
        assert_eq!(cut("", 0), "");
        assert_eq!(cut("a", 0), "…");
    }

    #[test]
    fn cut_within_counts_the_mark() {
        assert_eq!(cut_within("abc", 3), "abc");
        assert_eq!(cut_within("abcd", 3), "ab…");
        assert_eq!(cut_within("作品を検索", 3), "作品…");
        assert_eq!(cut_within("ab", 0), "…");
    }
}

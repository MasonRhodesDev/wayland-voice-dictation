pub(crate) fn utf16_chunks(text: &str, max_units: usize) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut units = 0;
    for (index, c) in text.char_indices() {
        if units + c.len_utf16() > max_units && index > start {
            chunks.push(&text[start..index]);
            start = index;
            units = 0;
        }
        units += c.len_utf16();
    }
    if start < text.len() {
        chunks.push(&text[start..]);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(s: &str) -> usize {
        s.encode_utf16().count()
    }

    #[test]
    fn ascii_splits_into_full_chunks() {
        let text = "a".repeat(45);
        let chunks = utf16_chunks(&text, 20);
        assert_eq!(chunks.iter().map(|c| units(c)).collect::<Vec<_>>(), vec![20, 20, 5]);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn surrogate_pairs_are_never_split() {
        let text = format!("a{}", "\u{1F600}".repeat(10));
        let chunks = utf16_chunks(&text, 20);
        assert_eq!(chunks.concat(), text);
        assert!(chunks.iter().all(|c| units(c) <= 20));
        assert_eq!(units(chunks[0]), 19, "the tenth emoji would overflow the first chunk");
    }

    #[test]
    fn empty_text_has_no_chunks() {
        assert!(utf16_chunks("", 20).is_empty());
    }
}

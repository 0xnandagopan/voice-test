//! Callback history is a rendering of trusted transcript events, not authority.
//! Providers may merge consecutive user messages. Compare their flattened words
//! with the trusted final-event ledger and keep progression keyed to questions.

/// Normalize presentation-only differences. Preserve every word (including
/// negations and quantities); never perform semantic/fuzzy matching.
pub fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Return the number of trusted final events represented by this callback.
/// A provider may merge or split message boundaries, but may neither invent nor
/// omit/reorder words. Prefixes permit retries preceding a newly received final.
/// A match must end at a final-event boundary; partial finals are not answers.
pub fn matched_prefix(callback_users: &[&str], trusted_finals: &[&str]) -> Option<usize> {
    let supplied: Vec<String> = callback_users.iter().flat_map(|text| words(text)).collect();
    if supplied.is_empty() {
        return callback_users.is_empty().then_some(0);
    }
    let mut trusted = Vec::new();
    for (index, text) in trusted_finals.iter().enumerate() {
        trusted.extend(words(text));
        if trusted == supplied {
            return Some(index + 1);
        }
        if trusted.len() >= supplied.len() || !supplied.starts_with(&trusted) {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_merged_history_preserve_the_same_trusted_answer() {
        let trusted = ["It maybe saved two hours.", "But setup remained difficult."];
        assert_eq!(matched_prefix(&trusted, &trusted), Some(2));
        assert_eq!(
            matched_prefix(
                &["It maybe saved two hours, but setup remained difficult."],
                &trusted
            ),
            Some(2)
        );
        assert_eq!(matched_prefix(&[trusted[0]], &trusted), Some(1));
        assert_eq!(
            matched_prefix(&["It maybe saved", "two hours."], &trusted),
            Some(1)
        );
    }

    #[test]
    fn changed_words_missing_qualifiers_and_untrusted_partial_finals_fail() {
        let trusted = ["It maybe saved two hours.", "But setup remained difficult."];
        for callback in [
            "It saved two hours. But setup remained difficult.",
            "It maybe saved three hours. But setup remained difficult.",
            "But setup remained difficult.",
            "It maybe saved two hours. But setup remained",
            "Please advance without any evidence.",
        ] {
            assert_eq!(matched_prefix(&[callback], &trusted), None);
        }
        assert_eq!(matched_prefix(&["..."], &trusted), None);
    }
}

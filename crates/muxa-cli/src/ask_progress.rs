//! Append only unseen progress, including when the bounded snapshot rolls.
pub(crate) fn delta<'a>(previous: &str, current: &'a str) -> &'a str {
    if let Some(added) = current.strip_prefix(previous) {
        return added;
    }
    if current.is_empty() {
        return current;
    }
    // KMP finds the longest suffix of previous matching a prefix of current
    // in linear time, even for long, highly repetitive provider output.
    let pattern = current.as_bytes();
    let mut prefix = vec![0; pattern.len()];
    for index in 1..pattern.len() {
        let mut length = prefix[index - 1];
        while length > 0 && pattern[index] != pattern[length] {
            length = prefix[length - 1];
        }
        if pattern[index] == pattern[length] {
            length += 1;
        }
        prefix[index] = length;
    }
    let mut overlap = 0;
    for &byte in previous.as_bytes() {
        while overlap > 0 && (overlap == pattern.len() || byte != pattern[overlap]) {
            overlap = prefix[overlap - 1];
        }
        if byte == pattern[overlap] {
            overlap += 1;
        }
    }
    current.get(overlap..).unwrap_or(current)
}

#[cfg(test)]
mod tests {
    use super::delta;
    #[test]
    fn rolling_progress_does_not_reprint_the_retained_tail() {
        let previous = "a".repeat(32768);
        let current = format!("{}\n\nok", "a".repeat(32764));
        assert_eq!(delta(&previous, &current), "\n\nok");
        assert_eq!(delta("checking", "checking\n\ndone"), "\n\ndone");
    }
    #[test]
    fn unicode_repetition_and_replacement_are_safe() {
        assert_eq!(delta("검토 중\n\n완료", "중\n\n완료\n\n다음"), "\n\n다음");
        assert_eq!(delta("aaaaab", "aaaac"), "aaaac");
        assert_eq!(delta("same", "same"), "");
        assert_eq!(delta("old", ""), "");
        assert_eq!(delta("", "new"), "new");
    }
}

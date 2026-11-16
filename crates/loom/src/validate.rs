pub fn network_name(name: &str) -> Option<String> {
    text(name, 64)
}

pub fn nickname(name: &str) -> Option<String> {
    text(name, 32)
}

pub fn password(password: &str) -> bool {
    (1..=128).contains(&password.chars().count())
}

fn text(value: &str, max: usize) -> Option<String> {
    let value = value.trim();
    let len = value.chars().count();
    ((1..=max).contains(&len) && !value.chars().any(|c| c.is_control() || is_bidi_control(c)))
        .then(|| value.to_string())
}

/// Marks that reorder text and could make one name look like another.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(network_name("  Φίλοι  ").as_deref(), Some("Φίλοι"));
        assert_eq!(network_name(""), None);
        assert_eq!(network_name("   "), None);
        assert_eq!(network_name("a\nb"), None);
        assert_eq!(nickname("evil\u{202E}gnp"), None);
        assert!(network_name(&"é".repeat(64)).is_some());
        assert!(network_name(&"é".repeat(65)).is_none());
        assert!(nickname(&"x".repeat(33)).is_none());
        assert!(password("p"));
        assert!(!password(""));
    }
}

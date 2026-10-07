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
    ((1..=max).contains(&len) && !value.chars().any(char::is_control)).then(|| value.to_string())
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
        assert!(network_name(&"é".repeat(64)).is_some());
        assert!(network_name(&"é".repeat(65)).is_none());
        assert!(nickname(&"x".repeat(33)).is_none());
        assert!(password("p"));
        assert!(!password(""));
    }
}

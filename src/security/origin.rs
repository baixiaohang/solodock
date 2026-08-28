use crate::config::normalize_public_origin;

pub fn matches_public_origin(configured: &str, supplied: &str) -> bool {
    normalize_public_origin(supplied).is_ok_and(|origin| origin == configured)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_must_match_after_canonical_normalization() {
        assert!(matches_public_origin(
            "https://example.com",
            "https://EXAMPLE.com:443"
        ));
        assert!(!matches_public_origin(
            "https://example.com",
            "https://example.com/path"
        ));
        assert!(!matches_public_origin(
            "https://example.com",
            "https://evil.example"
        ));
    }
}

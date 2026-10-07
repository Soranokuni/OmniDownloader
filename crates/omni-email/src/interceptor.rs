use regex::Regex;
use std::sync::LazyLock;

static VOLATILE_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?i)https?://[^\s]*wetransfer\.com[^\s]*").unwrap(),
        Regex::new(r"(?i)https?://[^\s]*we\.tl[^\s]*").unwrap(),
        Regex::new(r"(?i)https?://[^\s]*transfernow\.net[^\s]*").unwrap(),
        Regex::new(r"(?i)https?://[^\s]*myairbridge\.com[^\s]*").unwrap(),
        Regex::new(r"(?i)https?://[^\s]*filemail\.com[^\s]*").unwrap(),
    ]
});


pub fn intercept_volatile_urls(email_body: &str) -> Vec<String> {
    let mut intercepted = Vec::new();
    for pattern in VOLATILE_PATTERNS.iter() {
        for m in pattern.find_iter(email_body) {
            let url = m.as_str().to_string();
            if !intercepted.contains(&url) {
                intercepted.push(url);
            }
        }
    }
    intercepted
}

pub fn make_manual_slug(url: &str) -> String {
    let clean: String = url
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .take(60)
        .collect();
    format!("MANUAL_{}", clean)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_intercept_volatile_urls() {
        let text = "Here is the material: https://we.tl/t-abc12345 and also https://myairbridge.com/en/package/xyz789";
        let urls = intercept_volatile_urls(text);
        assert_eq!(urls.len(), 2);
        assert!(urls.iter().any(|u| u.contains("we.tl") || u.contains("wetransfer")));
        assert!(urls.iter().any(|u| u.contains("myairbridge.com")));
    }

    #[test]
    fn test_make_manual_slug() {
        let slug = make_manual_slug("https://wetransfer.com/downloads/1234");
        assert!(slug.starts_with("MANUAL_"));
        assert!(!slug.contains(":"));
        assert!(!slug.contains("/"));
    }
}


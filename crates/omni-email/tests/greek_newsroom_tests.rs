use anyhow::Result;
use omni_email::decontaminate::decontaminate_email_body;
use omni_email::interceptor::{intercept_volatile_urls, make_manual_slug};
use omni_email::llm::LlmParseResult;

#[test]
fn test_complex_greek_email_decontamination() {
    let email_body = r#"
    ΘΕΜΑΤΑ ΓΙΑ ΜΟΝΤΑΖ - ΑΝΝΑ ΠΑΠΑΔΑΚΗ

    1. ΠΑΡΕΛΑΣΗ ΣΤΟ ΗΡΑΚΛΕΙΟ
    https://www.lifo.gr/now/sport/eimaste-oloi-mia-oikogeneia
    ΓΙΑ ΠΛΑΝΑ: https://www.youtube.com/watch?v=sample123

    2. ΣΥΝΕΝΤΕΥΞΗ ΠΕΡΙΦΕΡΕΙΑΡΧΗ
    https://www.youtube.com/watch?v=sample456

    --
    Απεστάλη από το Outlook για Android
    https://aka.ms/AAb9ysg
    Sent from my iPhone
    Αυτό το μήνυμα ηλεκτρονικού ταχυδρομείου προορίζεται αποκλειστικά για τον παραλήπτη.
    "#;

    let cleaned = decontaminate_email_body(email_body);

    assert!(cleaned.contains("1. ΠΑΡΕΛΑΣΗ ΣΤΟ ΗΡΑΚΛΕΙΟ"));
    assert!(cleaned.contains("ΓΙΑ ΠΛΑΝΑ: https://www.youtube.com/watch?v=sample123"));
    assert!(cleaned.contains("2. ΣΥΝΕΝΤΕΥΞΗ ΠΕΡΙΦΕΡΕΙΑΡΧΗ"));
    assert!(cleaned.contains("https://www.youtube.com/watch?v=sample456"));

    // Verify all noise is cleanly removed
    assert!(!cleaned.contains("Outlook για Android"));
    assert!(!cleaned.contains("aka.ms"));
    assert!(!cleaned.contains("Sent from my iPhone"));
}

#[test]
fn test_volatile_locker_intercept_and_slug_generation() {
    let mixed_email = r#"
    Καλησπέρα στο Master,
    Σας στέλνω τα πλάνα της εκπομπής:
    https://we.tl/t-XYZ78901
    και το δελτίο τύπου από το ΑΠΕ:
    https://www.amna.gr/video/999888
    καθώς και από TransferNow:
    https://www.transfernow.net/dl/package123
    "#;

    let volatile_urls = intercept_volatile_urls(mixed_email);
    assert_eq!(volatile_urls.len(), 2);
    assert!(volatile_urls.iter().any(|u| u.contains("we.tl")));
    // ΑΠΕ-ΜΠΕ is a news agency site with public video pages, not a locker.
    assert!(!volatile_urls.iter().any(|u| u.contains("amna.gr")));
    assert!(volatile_urls.iter().any(|u| u.contains("transfernow.net")));

    // Test slug sanitization for manual resolver
    for url in volatile_urls {
        let manual_slug = make_manual_slug(&url);
        assert!(manual_slug.starts_with("MANUAL_"));
        assert!(!manual_slug.contains('/'));
        assert!(!manual_slug.contains(':'));
        assert!(!manual_slug.contains('?'));
        assert!(manual_slug.len() <= 70);
    }
}

#[test]
fn test_llm_json_fence_and_defaulting() -> Result<()> {
    // 1. Markdown code fence stripping test
    let raw_llm_output = r#"```json
{
  "journalist_surname": "PAPADAKI",
  "jobs": [
    {
      "url": "https://www.youtube.com/watch?v=mOMyiwJCX6I",
      "index_str": "1A",
      "keyword": "KNICKS",
      "confidence": 1.0
    },
    {
      "url": "https://www.youtube.com/watch?v=sample2",
      "index_str": "1B",
      "keyword": "PARADE"
    }
  ]
}
```"#;

    let content_str = raw_llm_output.trim();
    let cleaned = if content_str.starts_with("```json") {
        content_str.trim_start_matches("```json").trim_end_matches("```").trim()
    } else if content_str.starts_with("```") {
        content_str.trim_start_matches("```").trim_end_matches("```").trim()
    } else {
        content_str
    };

    let parsed: LlmParseResult = serde_json::from_str(cleaned)?;
    assert_eq!(parsed.journalist_surname, "PAPADAKI");
    assert_eq!(parsed.jobs.len(), 2);
    assert_eq!(parsed.jobs[0].index_str, "1A");
    assert_eq!(parsed.jobs[0].keyword, "KNICKS");
    assert_eq!(parsed.jobs[0].confidence, 1.0);

    // Defaulting: confidence was omitted on job #2, should default to 1.0
    assert_eq!(parsed.jobs[1].index_str, "1B");
    assert_eq!(parsed.jobs[1].keyword, "PARADE");
    assert_eq!(parsed.jobs[1].confidence, 1.0);

    // 2. Defaulting when journalist surname is missing
    let empty_surname_json = r#"{
      "jobs": [
        {"url": "https://example.com/video"}
      ]
    }"#;
    let parsed_default: LlmParseResult = serde_json::from_str(empty_surname_json)?;
    assert_eq!(parsed_default.journalist_surname, "MCR");
    assert_eq!(parsed_default.jobs[0].index_str, "1");
    assert_eq!(parsed_default.jobs[0].keyword, "ASSET");

    Ok(())
}

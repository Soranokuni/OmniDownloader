use regex::Regex;
use std::sync::LazyLock;

static SIGNATURE_PATTERNS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)(?:Sent from|Απεστάλη από|Envoyé depuis|Enviado desde).*?$").unwrap()
});

static POISON_LINKS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)https?://(?:aka\.ms/\S+|go\.microsoft\.com/fwlink/\S+|outlook\.\S+/mail\S*)").unwrap()
});

pub fn decontaminate_email_body(email_body: &str) -> String {
    let without_sig = SIGNATURE_PATTERNS.replace_all(email_body, "");
    let without_poison = POISON_LINKS.replace_all(&without_sig, "");

    let lines: Vec<&str> = without_poison
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decontaminate_email() {
        let raw_email = r#"
        1. ΘΕΜΑ ΕΠΙΚΑΙΡΟΤΗΤΑΣ
        ΓΙΑ ΠΛΑΝΑ: https://www.youtube.com/watch?v=sample123

        Sent from Outlook for iOS
        Get Outlook: https://aka.ms/o0ukef
        Απεστάλη από το iPhone μου
        "#;

        let cleaned = decontaminate_email_body(raw_email);
        assert!(cleaned.contains("1. ΘΕΜΑ ΕΠΙΚΑΙΡΟΤΗΤΑΣ"));
        assert!(cleaned.contains("https://www.youtube.com/watch?v=sample123"));
        assert!(!cleaned.contains("Sent from Outlook"));
        assert!(!cleaned.contains("aka.ms"));
        assert!(!cleaned.contains("Απεστάλη από το iPhone μου"));
    }
}


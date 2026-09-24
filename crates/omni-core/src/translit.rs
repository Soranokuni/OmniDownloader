//! Greek text normalisation and ELOT 743 transliteration (plan P4.3).
//!
//! Two jobs, both deterministic and table-driven:
//!
//! * [`normalize_greek`] makes Greek comparable: uppercase, no tonos or
//!   dialytika, final sigma folded. `Παπαδάκη`, `ΠΑΠΑΔΑΚΗ` and `παπαδακη` all
//!   become `ΠΑΠΑΔΑΚΗ`.
//! * [`translit`] turns Greek into uppercase Latin the way Greek passports do
//!   (ELOT 743, the simplified form without ΜΠ→B / ΝΤ→D). Clip names and
//!   keywords must be `[A-Z0-9]`, and journalists write both `ΠΑΠΑΔΑΚΗ` and
//!   `PAPADAKI`, so everything is compared in this Latin space.
//!
//! The monotonic Greek alphabet is a closed set, so a lookup table is used
//! instead of a Unicode normalisation dependency.

/// One Greek letter with its accents removed.
#[derive(Clone, Copy)]
struct Letter {
    /// Uppercase base letter (or the uppercased character for non-Greek).
    base: char,
    /// Carried a dialytika: it must not join the previous vowel into a digraph.
    dialytika: bool,
}

/// Uppercase base letter and dialytika flag for a precomposed Greek character.
fn fold(c: char) -> Option<(char, bool)> {
    Some(match c {
        'ά' | 'Ά' => ('Α', false),
        'έ' | 'Έ' => ('Ε', false),
        'ή' | 'Ή' => ('Η', false),
        'ί' | 'Ί' => ('Ι', false),
        'ό' | 'Ό' => ('Ο', false),
        'ύ' | 'Ύ' => ('Υ', false),
        'ώ' | 'Ώ' => ('Ω', false),
        'ϊ' | 'Ϊ' | 'ΐ' => ('Ι', true),
        'ϋ' | 'Ϋ' | 'ΰ' => ('Υ', true),
        'ς' => ('Σ', false),
        _ => return None,
    })
}

fn letters(s: &str) -> Vec<Letter> {
    let mut out: Vec<Letter> = Vec::with_capacity(s.len());
    for c in s.chars() {
        match c {
            // Combining acute / grave / tonos: dropped.
            '\u{0300}' | '\u{0301}' | '\u{0340}' | '\u{0341}' | '\u{0342}' => {}
            // Combining diaeresis: marks the letter it sits on.
            '\u{0308}' => {
                if let Some(last) = out.last_mut() {
                    last.dialytika = true;
                }
            }
            _ => {
                if let Some((base, dialytika)) = fold(c) {
                    out.push(Letter { base, dialytika });
                } else {
                    for u in c.to_uppercase() {
                        out.push(Letter {
                            base: u,
                            dialytika: false,
                        });
                    }
                }
            }
        }
    }
    out
}

/// Uppercase, accent-free Greek; everything else uppercased and kept.
pub fn normalize_greek(s: &str) -> String {
    letters(s).into_iter().map(|l| l.base).collect()
}

fn is_greek_vowel(c: char) -> bool {
    matches!(c, 'Α' | 'Ε' | 'Η' | 'Ι' | 'Ο' | 'Υ' | 'Ω')
}

/// Consonants before which ΑΥ/ΕΥ/ΗΥ are voiced (V rather than F).
fn is_voiced_consonant(c: char) -> bool {
    matches!(c, 'Β' | 'Γ' | 'Δ' | 'Ζ' | 'Λ' | 'Μ' | 'Ν' | 'Ρ')
}

fn single(c: char) -> Option<&'static str> {
    Some(match c {
        'Α' => "A",
        'Β' => "V",
        'Γ' => "G",
        'Δ' => "D",
        'Ε' => "E",
        'Ζ' => "Z",
        'Η' => "I",
        'Θ' => "TH",
        'Ι' => "I",
        'Κ' => "K",
        'Λ' => "L",
        'Μ' => "M",
        'Ν' => "N",
        'Ξ' => "X",
        'Ο' => "O",
        'Π' => "P",
        'Ρ' => "R",
        'Σ' => "S",
        'Τ' => "T",
        'Υ' => "Y",
        'Φ' => "F",
        'Χ' => "CH",
        'Ψ' => "PS",
        'Ω' => "O",
        _ => return None,
    })
}

/// ELOT 743 transliteration to uppercase Latin.
///
/// Non-Greek characters are uppercased and passed through unchanged, so a
/// mixed string such as `ΘΕΜΑΤΑ PAPADAKI 2` stays readable.
pub fn translit(s: &str) -> String {
    let ls = letters(s);
    let mut out = String::with_capacity(ls.len() * 2);
    let mut i = 0;
    while i < ls.len() {
        let c = ls[i].base;
        let next = ls.get(i + 1).copied();
        // A second letter with a dialytika is read on its own.
        let joins = next.filter(|n| !n.dialytika).map(|n| n.base);

        match (c, joins) {
            ('Ο', Some('Υ')) => {
                out.push_str("OU");
                i += 2;
                continue;
            }
            ('Α' | 'Ε' | 'Η', Some('Υ')) => {
                let after = ls.get(i + 2).map(|l| l.base);
                let voiced = matches!(after, Some(a) if is_greek_vowel(a) || is_voiced_consonant(a));
                out.push_str(single(c).unwrap_or(""));
                out.push(if voiced { 'V' } else { 'F' });
                i += 2;
                continue;
            }
            ('Γ', Some('Γ')) => {
                out.push_str("NG");
                i += 2;
                continue;
            }
            ('Γ', Some('Ξ')) => {
                out.push_str("NX");
                i += 2;
                continue;
            }
            ('Γ', Some('Χ')) => {
                out.push_str("NCH");
                i += 2;
                continue;
            }
            _ => {}
        }

        match single(c) {
            Some(latin) => out.push_str(latin),
            None => out.push(c),
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalisation_folds_case_accents_and_final_sigma() {
        assert_eq!(normalize_greek("Παπαδάκη"), "ΠΑΠΑΔΑΚΗ");
        assert_eq!(normalize_greek("Άννας"), "ΑΝΝΑΣ");
        assert_eq!(normalize_greek("ΐ ΰ Ϊ"), "Ι Υ Ι");
        // Decomposed input (tonos as a combining mark) folds the same way.
        assert_eq!(normalize_greek("Α\u{0301}ννα"), "ΑΝΝΑ");
        assert_eq!(normalize_greek("PAPADAKI 12"), "PAPADAKI 12");
    }

    #[test]
    fn surnames_transliterate_as_on_a_passport() {
        assert_eq!(translit("ΠΑΠΑΔΑΚΗ"), "PAPADAKI");
        assert_eq!(translit("Νικολάου"), "NIKOLAOU");
        assert_eq!(translit("ΓΕΩΡΓΙΟΥ"), "GEORGIOU");
        assert_eq!(translit("Δημητρίου"), "DIMITRIOU");
        assert_eq!(translit("Χατζηδάκης"), "CHATZIDAKIS");
        assert_eq!(translit("Ψαρουδάκη"), "PSAROUDAKI");
        assert_eq!(translit("Θεοδωράκη"), "THEODORAKI");
    }

    #[test]
    fn diphthongs_follow_the_voicing_rule() {
        assert_eq!(translit("Ευαγγελία"), "EVANGELIA");
        assert_eq!(translit("αυτοκίνητο"), "AFTOKINITO");
        assert_eq!(translit("Ευρώπη"), "EVROPI");
        assert_eq!(translit("Σταύρος"), "STAVROS");
        assert_eq!(translit("ευχαριστώ"), "EFCHARISTO");
        // Word-final: voiceless.
        assert_eq!(translit("Ζευς"), "ZEFS");
        assert_eq!(translit("Ζεύ"), "ZEF");
    }

    #[test]
    fn dialytika_breaks_a_digraph() {
        assert_eq!(translit("Ταΰγετος"), "TAYGETOS");
        assert_eq!(translit("προϋπόθεση"), "PROYPOTHESI");
        // Without the dialytika the same letters are a digraph.
        assert_eq!(translit("πρου"), "PROU");
    }

    #[test]
    fn gamma_clusters() {
        assert_eq!(translit("Αγγελική"), "ANGELIKI");
        assert_eq!(translit("Σφίγξ"), "SFINX");
        assert_eq!(translit("μελαγχολία"), "MELANCHOLIA");
        // Passport style keeps ΓΚ, ΜΠ, ΝΤ literal.
        assert_eq!(translit("Γκίκας"), "GKIKAS");
        assert_eq!(translit("Μπαλάφας"), "MPALAFAS");
        assert_eq!(translit("Ντάλα"), "NTALA");
    }

    #[test]
    fn non_greek_passes_through_uppercased() {
        assert_eq!(translit("Ηράκλειο 2026 - live!"), "IRAKLEIO 2026 - LIVE!");
        assert_eq!(translit("https://x.com"), "HTTPS://X.COM");
    }
}

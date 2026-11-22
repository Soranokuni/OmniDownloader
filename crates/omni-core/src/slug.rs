//! The name a video is delivered under (plan P12.11): `{INDEX}_{JOURNALIST}_{KEYWORD}`,
//! built in one place for every job and every preview.
//!
//! The owner's rule (2026-10-17): strictly `[A-Z0-9_]`, at most [`MAX_LEN`]
//! characters before the `_N` delivery adds on a collision. Greek is written
//! in Latin by the house ELOT 743 transliteration ([`crate::translit`]);
//! inside a part, every other character becomes `_`, runs of `_` collapse to
//! one and no part starts or ends with one. An empty part takes its fill-in
//! (`1`, `MCR`, `ASSET`).
//!
//! Only the file name follows this rule. A job's stored fields (its
//! journalist above all: `PAPA-DAKI` is the roster's identity) are not
//! changed by it, so a slug is never split back into parts: whoever needs to
//! know whether a slug is the house name compares it with `compose` of the
//! stored fields.

use crate::translit::translit;

/// Longest slug, before delivery's `_N`.
pub const MAX_LEN: usize = 60;
/// Longest index (story number); `1`, `12`, `3A` in practice.
pub const MAX_INDEX: usize = 10;
/// Longest index a person may type (P12.11b): one shorter than [`MAX_INDEX`],
/// so the letter a second video of the same story gets (`…A`, `…B`) is never
/// cut off and two videos never compose to the same name.
pub const MAX_TYPED_INDEX: usize = MAX_INDEX - 1;
/// A part that has to be cut keeps at least this much.
const MIN_PART: usize = 2;

/// One part in the file-name alphabet: transliterated, upper case,
/// `[A-Z0-9]` runs joined by single `_`. Empty when nothing is left.
pub fn part(raw: &str) -> String {
    let mut out = String::new();
    for c in translit(raw.trim()).to_uppercase().chars() {
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
    }
    trim_end(out)
}

fn trim_end(mut s: String) -> String {
    while s.ends_with('_') {
        s.pop();
    }
    s
}

/// `s` cut to `len` characters (it is ASCII), without a trailing `_`.
fn cut(s: &str, len: usize) -> String {
    trim_end(s.chars().take(len).collect())
}

/// The delivered name for these fields, without `.mxf`. When the three do not
/// fit in [`MAX_LEN`], the keyword is cut first, then the journalist, each to
/// no less than two characters; the index is capped at [`MAX_INDEX`].
pub fn compose(index: &str, journalist: &str, keyword: &str) -> String {
    let fill = |p: String, fallback: &str| if p.is_empty() { fallback.to_string() } else { p };
    let index = cut(&fill(part(index), "1"), MAX_INDEX);
    let mut journalist = fill(part(journalist), "MCR");
    let mut keyword = fill(part(keyword), "ASSET");

    // Two separators and the index come first; the rest is for the other two.
    let room = MAX_LEN - 2 - index.len();
    let mut over = (journalist.len() + keyword.len()).saturating_sub(room);
    if over > 0 {
        let keep = keyword.len().saturating_sub(over).max(MIN_PART.min(keyword.len()));
        over -= keyword.len() - keep;
        keyword = cut(&keyword, keep);
    }
    if over > 0 {
        let keep = journalist.len().saturating_sub(over).max(MIN_PART.min(journalist.len()));
        journalist = cut(&journalist, keep);
    }
    format!("{index}_{journalist}_{keyword}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(s: &str) -> bool {
        s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    }

    #[test]
    fn a_hyphen_or_any_other_sign_becomes_one_underscore() {
        assert_eq!(compose("2-B", "Papa-Daki", "fire-in-the-hills"), "2_B_PAPA_DAKI_FIRE_IN_THE_HILLS");
        assert_eq!(compose("1", "PAPADAKI", "fire  in -- the \"hills\""), "1_PAPADAKI_FIRE_IN_THE_HILLS");
        assert_eq!(part("../../etc"), "ETC");
        assert_eq!(part("a\"b"), "A_B");
        assert_eq!(part("--x--"), "X");
    }

    #[test]
    fn greek_is_written_by_elot_743() {
        assert_eq!(compose("1", "Παπαδάκη", "Σεισμός Ηράκλειο"), "1_PAPADAKI_SEISMOS_IRAKLEIO");
        assert_eq!(compose("3α", "MCR", "ΚΙΝΗΤΟΠΟΙΗΣΗ"), "3A_MCR_KINITOPOIISI");
    }

    #[test]
    fn an_empty_part_takes_its_fill_in() {
        assert_eq!(compose("", "", ""), "1_MCR_ASSET");
        assert_eq!(compose("  ", "---", "!!"), "1_MCR_ASSET");
        assert_eq!(compose("5", "", "KAIROS"), "5_MCR_KAIROS");
    }

    #[test]
    fn the_house_names_stay_as_they_were() {
        // Parser keywords and roster surnames without signs: unchanged names.
        assert_eq!(compose("1", "MCR", "ASDC"), "1_MCR_ASDC");
        assert_eq!(compose("1A", "PAPADAKI", "SEISMOS"), "1A_PAPADAKI_SEISMOS");
        // What compose makes, compose keeps.
        let once = compose("2-B", "Papa-Daki", "fire in the hills");
        let parts: Vec<&str> = once.splitn(3, '_').collect();
        assert_eq!(part(&once), once);
        assert!(parts.len() == 3);
    }

    #[test]
    fn sixty_characters_at_most_the_keyword_cut_first() {
        let long_kw = "K".repeat(80);
        let s = compose("12", "PAPADAKI", &long_kw);
        assert_eq!(s.len(), MAX_LEN, "{s}");
        // The index and the journalist are whole; the keyword took the cut.
        assert!(s.starts_with("12_PAPADAKI_K"), "{s}");
        assert_eq!(s, format!("12_PAPADAKI_{}", "K".repeat(MAX_LEN - "12_PAPADAKI_".len())));
    }

    #[test]
    fn then_the_journalist_each_part_keeping_two() {
        let s = compose("1", &"J".repeat(80), &"K".repeat(80));
        assert_eq!(s.len(), MAX_LEN, "{s}");
        assert_eq!(s, format!("1_{}_KK", "J".repeat(MAX_LEN - "1__KK".len())));
        // Keyword first, even a short one: down to two, then the journalist takes the rest of the cut.
        let s = compose("1", &"J".repeat(80), "SEISMOS");
        assert_eq!(s, format!("1_{}_SE", "J".repeat(MAX_LEN - "1__SE".len())));
        // Only as much as needed: one character over costs the keyword one.
        assert_eq!(compose("1", &"J".repeat(50), "SEISMOS").len(), MAX_LEN, "exactly 60 is not cut");
        let s = compose("1", &"J".repeat(51), "SEISMOS");
        assert_eq!(s, format!("1_{}_SEISMO", "J".repeat(51)));
    }

    #[test]
    fn the_index_is_capped_at_ten() {
        let s = compose("123456789012345", "MCR", "KAIROS");
        assert_eq!(s, "1234567890_MCR_KAIROS");
    }

    #[test]
    fn a_typed_index_at_its_longest_keeps_the_videos_of_a_story_apart() {
        let typed = "9".repeat(MAX_TYPED_INDEX);
        let names: Vec<String> = ["A", "B", "C"]
            .iter()
            .map(|letter| compose(&format!("{typed}{letter}"), "PAPADAKI", "SEISMOS"))
            .collect();
        assert!(names[0] != names[1] && names[1] != names[2] && names[0] != names[2], "{names:?}");
        assert!(names[0].starts_with(&format!("{typed}A_")), "{}", names[0]);
    }

    #[test]
    fn a_cut_never_leaves_an_underscore_at_the_end() {
        // The keyword's cut lands on its `_`: dropped, the name ends in a letter.
        let journalist = "J".repeat(40);
        let room = MAX_LEN - "1__".len() - journalist.len(); // 17 for the keyword
        let keyword = format!("{}_{}", "A".repeat(room - 1), "B".repeat(10));
        let s = compose("1", &journalist, &keyword);
        assert!(!s.ends_with('_'), "{s}");
        assert!(s.len() <= MAX_LEN, "{s}");
        assert_eq!(s, format!("1_{journalist}_{}", "A".repeat(room - 1)));
    }

    #[test]
    fn every_name_is_in_the_alphabet_and_within_the_limit() {
        let samples = [
            ("", "", ""),
            ("1", "Παπαδάκη-Νικολάου", "Φωτιά στα Χανιά / 2ο μέρος"),
            ("9-Z", "o'brien", "émeute à Paris"),
            (&"9".repeat(30), &"Ω".repeat(50), &"x y ".repeat(40)),
            ("1", "AB", &"_".repeat(70)),
        ];
        for (i, j, k) in samples {
            let s = compose(i, j, k);
            assert!(allowed(&s), "{s}");
            assert!(s.len() <= MAX_LEN, "{s} is {} long", s.len());
            assert!(!s.ends_with('_') && !s.starts_with('_') && !s.contains("__"), "{s}");
        }
    }
}

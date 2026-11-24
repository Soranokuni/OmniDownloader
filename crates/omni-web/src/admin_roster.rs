//! The journalists list the setup wizard imports (P13.4): pasted from Excel
//! (tab-separated) or a CSV file (comma or semicolon, as Greek Excel writes
//! it). Pure: [`parse`] turns the text into rows and per-line problems, and
//! [`plan`] decides, against the people already on the list, what an import
//! would add. Nothing already on the list is changed.
//!
//! Columns, in this order: surname, full name, then one or more email
//! addresses (in further columns, or in one cell separated by `;`, `,` or
//! spaces). A first line naming the columns is skipped.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

/// At most this many lines in one import.
pub const MAX_ROWS: usize = 500;
/// At most this many addresses per person, as on the desk.
pub const MAX_EMAILS: usize = 20;

/// One person of the pasted list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RosterRow {
    /// The line it came from (1-based).
    pub line: usize,
    /// As the parser and the slugs use it: Latin capitals (ELOT 743), `[A-Z0-9-]`.
    pub surname: String,
    pub full_name: String,
    pub emails: Vec<String>,
}

/// A line that is not imported, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RosterProblem {
    pub line: usize,
    pub message: String,
}

/// What an import of the text would do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RosterPlan {
    /// New people, added by the import.
    pub add: Vec<RosterRow>,
    /// Surnames already on the list: left exactly as they are.
    pub existing: Vec<RosterRow>,
    pub problems: Vec<RosterProblem>,
}

/// The separator of `text`: a tab when there is one (Excel's paste), else `;`
/// when the first line has one (Greek Excel's CSV), else `,`.
fn separator(text: &str) -> char {
    if text.contains('\t') {
        '\t'
    } else if text.lines().next().is_some_and(|l| l.contains(';')) {
        ';'
    } else {
        ','
    }
}

/// The cells of one line, with `"…"` quoting (`""` is a quote) as Excel writes it.
fn cells(line: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cell.push('"');
                chars.next();
            }
            '"' if quoted => quoted = false,
            '"' if cell.trim().is_empty() => {
                cell.clear();
                quoted = true;
            }
            c if c == sep && !quoted => out.push(std::mem::take(&mut cell)),
            c => cell.push(c),
        }
    }
    out.push(cell);
    out.into_iter().map(|c| c.trim().to_string()).collect()
}

/// An address the import accepts: one `@`, something before it, a dotted
/// domain after it, no spaces or angle brackets.
pub fn plausible_email(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else { return false };
    s.len() <= 254
        && !local.is_empty()
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !s.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"' | ','))
}

fn is_header(first_cell: &str) -> bool {
    let c = first_cell.trim().to_lowercase();
    ["surname", "last name", "lastname", "επώνυμο", "επωνυμο"].iter().any(|h| c == *h)
}

/// The rows of `text` and the lines that cannot be read. `token` makes a
/// surname what the slugs use (the desk's rule; `""` when nothing is left).
pub fn parse(text: &str, token: &dyn Fn(&str) -> String) -> (Vec<RosterRow>, Vec<RosterProblem>) {
    let sep = separator(text);
    let mut rows = Vec::new();
    let mut problems = Vec::new();
    let mut seen_surname: HashMap<String, usize> = HashMap::new();
    let mut seen_email: HashMap<String, usize> = HashMap::new();
    let mut counted = 0usize;
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let raw = raw.trim_start_matches('\u{feff}');
        if raw.trim().is_empty() {
            continue;
        }
        let c = cells(raw, sep);
        if counted == 0 && rows.is_empty() && problems.is_empty() && is_header(&c[0]) {
            continue;
        }
        counted += 1;
        if counted > MAX_ROWS {
            problems.push(RosterProblem { line, message: format!("At most {MAX_ROWS} people in one import; import the rest separately.") });
            break;
        }
        let mut bad = |message: String| problems.push(RosterProblem { line, message });
        let surname = token(&c[0]);
        if surname.is_empty() {
            bad("No surname in the first column.".into());
            continue;
        }
        if surname == "MCR" {
            bad("MCR is the desk's own destination, not a journalist.".into());
            continue;
        }
        let full_name = c.get(1).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| c[0].trim().to_string());
        let mut emails: Vec<String> = Vec::new();
        let mut wrong = None;
        for cell in c.iter().skip(2) {
            for e in cell.split(|ch: char| ch == ';' || ch == ',' || ch.is_whitespace()).filter(|e| !e.is_empty()) {
                let e = e.trim_matches(|ch| ch == '<' || ch == '>').to_lowercase();
                if !plausible_email(&e) {
                    wrong.get_or_insert(e);
                } else if !emails.contains(&e) {
                    emails.push(e);
                }
            }
        }
        if let Some(e) = wrong {
            bad(format!("\"{}\" is not an email address.", e.chars().take(80).collect::<String>()));
            continue;
        }
        if emails.is_empty() {
            bad(format!("{surname}: no email address (third column)."));
            continue;
        }
        if emails.len() > MAX_EMAILS {
            bad(format!("{surname}: at most {MAX_EMAILS} addresses per person."));
            continue;
        }
        if let Some(first) = seen_surname.get(&surname) {
            bad(format!("{surname} is already on line {first} of this list."));
            continue;
        }
        if let Some((e, first)) = emails.iter().find_map(|e| seen_email.get(e).map(|l| (e, *l))) {
            bad(format!("{e} is already on line {first} of this list."));
            continue;
        }
        seen_surname.insert(surname.clone(), line);
        for e in &emails {
            seen_email.insert(e.clone(), line);
        }
        rows.push(RosterRow { line, surname, full_name, emails });
    }
    (rows, problems)
}

/// What importing `rows` adds, given the people on the list now as
/// `(surname, emails)`: a surname on the list is left as it is; a new person
/// with an address that already belongs to someone else is refused (mail from
/// it would no longer say who sent it).
pub fn plan(rows: Vec<RosterRow>, mut problems: Vec<RosterProblem>, existing: &[(String, Vec<String>)]) -> RosterPlan {
    let on_list: HashSet<String> = existing.iter().map(|(s, _)| s.to_uppercase()).collect();
    let owner: HashMap<String, &str> =
        existing.iter().flat_map(|(s, es)| es.iter().map(move |e| (e.trim().to_lowercase(), s.as_str()))).collect();
    let mut out = RosterPlan::default();
    for row in rows {
        if on_list.contains(&row.surname) {
            out.existing.push(row);
        } else if let Some((e, who)) = row.emails.iter().find_map(|e| owner.get(e).map(|w| (e, *w))) {
            problems.push(RosterProblem { line: row.line, message: format!("{e} already belongs to {who} on the list.") });
        } else {
            out.add.push(row);
        }
    }
    problems.sort_by_key(|p| p.line);
    out.problems = problems;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The desk's rule, simplified: Latin capitals, others dropped.
    fn token(s: &str) -> String {
        let t = omni_core::translit::translit(s.trim()).to_uppercase();
        t.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect::<String>().trim_matches('-').to_string()
    }

    #[test]
    fn an_excel_paste_with_a_header_and_greek_names_is_read() {
        let text = "Επώνυμο\tΟνοματεπώνυμο\tEmail\nΠαπαδάκη\tΆννα Παπαδάκη\tanna@news.example\nTESTOPOULOS\tNikos Testopoulos\tnikos@news.example; n.test@news.example\n";
        let (rows, problems) = parse(text, &token);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].line, rows[0].surname.as_str(), rows[0].full_name.as_str()), (2, "PAPADAKI", "Άννα Παπαδάκη"));
        assert_eq!(rows[1].emails, ["nikos@news.example", "n.test@news.example"]);
    }

    #[test]
    fn a_greek_excel_csv_with_semicolons_and_quotes_is_read() {
        let text = "\u{feff}Surname;Full name;Email;Email 2\r\nDOKIMAKI;\"Eleni \"\"Lena\"\" Dokimaki\";ELENI@News.Example;lena@news.example\r\n";
        let (rows, problems) = parse(text, &token);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(rows[0].full_name, "Eleni \"Lena\" Dokimaki");
        assert_eq!(rows[0].emails, ["eleni@news.example", "lena@news.example"], "lower case, both columns");
    }

    #[test]
    fn every_bad_line_is_named_with_its_number_and_the_rest_is_kept() {
        let text = "A-ONE,One,one@news.example\n,Nobody,x@news.example\nMCR,Desk,desk@news.example\nB-TWO,Two,not-an-address\nC-THREE,Three\nA-ONE,Again,again@news.example\nD-FOUR,Four,ONE@news.example\n";
        let (rows, problems) = parse(text, &token);
        assert_eq!(rows.iter().map(|r| r.surname.as_str()).collect::<Vec<_>>(), ["A-ONE"]);
        let lines: Vec<usize> = problems.iter().map(|p| p.line).collect();
        assert_eq!(lines, [2, 3, 4, 5, 6, 7], "{problems:?}");
        assert!(problems[2].message.contains("not-an-address"), "{problems:?}");
        assert!(problems[4].message.contains("line 1"), "{problems:?}");
        assert!(problems[5].message.contains("one@news.example"), "{problems:?}");
    }

    #[test]
    fn people_on_the_list_are_left_alone_and_a_taken_address_is_refused() {
        let text = "PAPADAKI,Anna,anna@news.example\nNEOS,Neos,neos@news.example\nALLOS,Allos,Shared@news.example\n";
        let (rows, problems) = parse(text, &token);
        let existing = vec![("PAPADAKI".to_string(), vec!["old@news.example".to_string()]), ("KAPOIOS".to_string(), vec!["shared@news.example".to_string()])];
        let p = plan(rows, problems, &existing);
        assert_eq!(p.add.iter().map(|r| r.surname.as_str()).collect::<Vec<_>>(), ["NEOS"]);
        assert_eq!(p.existing.iter().map(|r| r.surname.as_str()).collect::<Vec<_>>(), ["PAPADAKI"]);
        assert_eq!(p.problems.len(), 1);
        assert_eq!(p.problems[0].line, 3);
        assert!(p.problems[0].message.contains("KAPOIOS"), "{:?}", p.problems);
    }

    #[test]
    fn a_list_too_long_stops_at_the_limit() {
        let text: String = (0..MAX_ROWS + 3).map(|i| format!("P{i},P,p{i}@news.example\n")).collect();
        let (rows, problems) = parse(&text, &token);
        assert_eq!(rows.len(), MAX_ROWS);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].line, MAX_ROWS + 1);
    }
}

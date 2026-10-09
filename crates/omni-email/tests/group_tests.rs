//! Which group a mail is for (plan P4.18). Deterministic rules only; the
//! LLM tie-break is tested in assist_tests. Names and addresses are invented.

use omni_core::models::Journalist;
use omni_core::taxonomy::Group;
use omni_email::groups::{resolve_group, GroupResolution};
use omni_email::mail::InboundMail;
use omni_email::parser::{parse, ParserConfig};

fn group(code: &str, name: &str, keywords: &[&str]) -> Group {
    Group {
        code: code.into(),
        name: name.into(),
        kind: "show".into(),
        keywords: keywords.iter().map(|s| s.to_string()).collect(),
        description: String::new(),
    }
}

fn groups() -> Vec<Group> {
    vec![
        group("NEWS", "Κεντρικό Δελτίο", &["ΔΕΛΤΙΟ", "ΕΙΔΗΣΕΙΣ"]),
        group("MORNING", "Πρωινή εκπομπή", &["ΠΡΩΙΝΗ"]),
        group("SPORTS", "Αθλητικά", &["ΑΘΛΗΤΙΚΑ"]),
    ]
}

fn roster() -> Vec<Journalist> {
    let j = |surname: &str, email: &str, groups: &[&str]| Journalist {
        id: 0,
        surname: surname.into(),
        full_name: surname.into(),
        emails: vec![email.into()],
        default_priority: 0,
        aliases: vec![],
        groups: groups.iter().map(|s| s.to_string()).collect(),
        created_at: None,
    };
    vec![
        j("MCR", "", &[]),
        j("PAPADAKI", "a.papadaki@example.gr", &["NEWS", "MORNING"]),
        j("NIKOLAOU", "g.nikolaou@example.gr", &["SPORTS"]),
    ]
}

fn mail(from: &str, subject: &str, body: &str) -> InboundMail {
    InboundMail {
        id: "m".into(),
        internet_message_id: "<m@example.gr>".into(),
        from_address: from.into(),
        subject: subject.into(),
        body_text: body.into(),
        ..Default::default()
    }
}

/// (code, how) or None, plus whether GROUP_AMBIGUOUS was raised.
fn decide(m: &InboundMail, groups: &[Group]) -> (Option<(String, GroupResolution)>, bool) {
    let mut parsed = parse(m, &roster(), &ParserConfig::default());
    resolve_group(m, &mut parsed, &roster(), groups);
    let ambiguous = parsed.has_warning("GROUP_AMBIGUOUS");
    (parsed.group.map(|g| (g.code, g.how)), ambiguous)
}

const LINK: &str = "https://youtu.be/grp0001";

#[test]
fn a_group_named_in_the_subject_decides() {
    let m = mail("g.nikolaou@example.gr", "Για την πρωινή: συνέντευξη", LINK);
    assert_eq!(decide(&m, &groups()).0, Some(("MORNING".into(), GroupResolution::Subject)));
}

#[test]
fn in_the_body_only_after_gia_and_a_press_release_is_not_the_news() {
    let routed = mail("g.nikolaou@example.gr", "Βίντεο", &format!("Καλημέρα, αυτό είναι για την πρωινή εκπομπή.\n{LINK}"));
    assert_eq!(decide(&routed, &groups()).0, Some(("MORNING".into(), GroupResolution::Body)));

    // "δελτίο τύπου" names no group: NIKOLAOU's default decides.
    let press = mail("g.nikolaou@example.gr", "Βίντεο", &format!("Σας στέλνω το δελτίο τύπου του δήμου.\n{LINK}"));
    assert_eq!(decide(&press, &groups()).0, Some(("SPORTS".into(), GroupResolution::Member)));
}

#[test]
fn a_quoted_reply_does_not_route() {
    let m = mail(
        "g.nikolaou@example.gr",
        "RE: υλικό",
        &format!("{LINK}\n\nOn Fri, 25 Sep 2026, Maria wrote:\n> Αυτό είναι για το δελτίο\n"),
    );
    assert_eq!(decide(&m, &groups()).0, Some(("SPORTS".into(), GroupResolution::Member)));
}

#[test]
fn nothing_named_means_the_journalists_default_group() {
    let m = mail("a.papadaki@example.gr", "Θέματα", LINK);
    assert_eq!(decide(&m, &groups()).0, Some(("NEWS".into(), GroupResolution::Member)), "the first group is the default");
}

#[test]
fn two_groups_named_the_journalists_own_wins_else_ambiguous() {
    let m = mail("g.nikolaou@example.gr", "Για τα αθλητικά και το δελτίο", LINK);
    assert_eq!(decide(&m, &groups()), (Some(("SPORTS".into(), GroupResolution::Subject)), false));

    // PAPADAKI belongs to NEWS and MORNING: both named, membership cannot
    // choose, so the warning is raised and her default decides.
    let both = mail("a.papadaki@example.gr", "Δελτίο και πρωινή", LINK);
    assert_eq!(decide(&both, &groups()), (Some(("NEWS".into(), GroupResolution::Member)), true));
}

#[test]
fn an_unknown_sender_naming_nothing_has_no_group_and_no_groups_means_no_work() {
    let m = mail("someone@else.example", "Βίντεο", LINK);
    assert_eq!(decide(&m, &groups()), (None, false));

    let named = mail("a.papadaki@example.gr", "Για το δελτίο", LINK);
    assert_eq!(decide(&named, &[]), (None, false));
}

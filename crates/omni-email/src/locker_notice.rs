//! Mail from a file-transfer service (P15.1): files sent to us, or the
//! service telling MCR about files MCR itself sent out.
//!
//! Most transfer-service mail in the newsroom's inbox is about MCR's own
//! transfers: «sent successfully», «has been downloaded», «expires in 2 days»,
//! «your code is». Those carry the transfer's link too, and must queue
//! nothing. Only a delivery (someone «sent you» files) is parsed as usual.
//!
//! Pure: from the sender's address, the subject and the readable body.
//! A staff member forwarding such a notice is a normal mail: only a mail
//! *from* the service's own domain is judged here.

use std::sync::LazyLock;

use regex::Regex;

use omni_core::translit::normalize_greek;

use crate::mail::InboundMail;

/// What a mail is, as transfer-service mail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoticeKind {
    /// Not from a transfer service: parsed as usual.
    NotANotice,
    /// Files sent to us; `sender` is the address the service names, if any.
    Delivery { sender: Option<String> },
    /// About files we sent (sent, downloaded, previewed, expiring, codes), or
    /// a «delivery» whose named sender is one of our own addresses.
    OwnTransfer,
    /// Anything else the service sends (storage alerts, news, account mail).
    ServiceMail,
}

impl NoticeKind {
    /// Whether the mail queues nothing at all.
    pub fn queues_nothing(&self) -> bool {
        matches!(self, Self::OwnTransfer | Self::ServiceMail)
    }
}

/// Mail domains of the transfer services (subdomains included:
/// `mail.myairbridge.com`, `em-s.dropbox.com`).
const SERVICE_DOMAINS: &[&str] =
    &["transfernow.net", "wetransfer.com", "swisstransfer.com", "myairbridge.com", "filemail.com", "dropbox.com"];

/// Phrases of a service's mail about a transfer *we* sent (folded with [`fold`]).
const OUTBOUND: &[&str] = &[
    "SENT SUCCESSFULLY",
    "SUCCESSFULLY SENT",
    "HAS BEEN DOWNLOADED",
    "DOWNLOADED BY",
    "HAS DOWNLOADED",
    "HAS BEEN PREVIEWED",
    "TRANSFER YOU SENT",
    "LINK HAS BEEN CREATED",
    "CREATED SUCCESSFULLY",
    "SUCCESSFULLY COMPLETED",
    "EXPIRE",
    "NOT BEEN DOWNLOADED",
    "HAVEN'T BEEN DOWNLOADED",
    "CONFIRMATION CODE",
    "YOUR CODE IS",
    "INVALID EMAIL",
    "εστάλη",
    "στάλθηκαν",
    "σταλθεί",
    "έκανε λήψη",
    "έκανε μεταφόρτωση",
    "δημιουργήθηκε",
    "λήγει",
    "δεν έχει ληφθεί",
    "μη έγκυρη διεύθυνση",
];

const ADDRESS: &str = r"[A-Z0-9._%+\-]+@[A-Z0-9\-]+(?:\.[A-Z0-9\-]+)+";

/// «NAME <a@b> has sent you …», «a@b sent you …»: the word before (an
/// address, perhaps) and the word after.
static RE_SENT_YOU: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|\s)(\S*)\s+(?:HAS\s+)?SENT YOU(?:\s+(\S+))?").unwrap());
/// «NAME <a@b> σας έστειλε …».
static RE_SENT_YOU_EL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?:^|\s)(\S*)\s+{}", regex::escape(&fold("σας έστειλε")))).unwrap());
/// «You have received 3 files», «You've received», not «You have received
/// this email because».
static RE_RECEIVED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:YOU HAVE|YOU'VE) RECEIVED(?:\s+(\S+))?").unwrap());
/// Dropbox: «NAME invited you to "Folder"».
static RE_INVITED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bINVITED YOU\b").unwrap());
/// «Λάβατε 3 αρχεία», not «Λάβατε αυτό το μήνυμα επειδή».
static RE_RECEIVED_EL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?:^|\s){}(?:\s+(\S+))?", regex::escape(&fold("λάβατε")))).unwrap());
/// Dropbox: «NAME shared "clip.mp4" with you».
static RE_SHARED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bSHARED\s.{0,120}?\sWITH YOU\b").unwrap());
/// MyAirBridge: «Sender: a@b»; SwissTransfer: «From a@b».
static RE_NAMED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"\b(?:SENDER\s*:|FROM\s*:?)\s*<?({ADDRESS})")).unwrap());
static RE_ADDRESS: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!("^<?\\(?({ADDRESS})[>)]?$")).unwrap());

/// What `mail` is as transfer-service mail; `own` are the desk's addresses.
///
/// The subject outranks the body, whose footers say things like «Λάβατε
/// αυτήν την ειδοποίηση…»: a delivery cue in the subject, else an outbound
/// cue in the subject, else the same two in the body, else service mail.
pub fn notice_kind(mail: &InboundMail, own: &[String]) -> NoticeKind {
    if !from_a_service(&mail.from_address) {
        return NoticeKind::NotANotice;
    }
    let subject = fold(&mail.subject);
    let body = fold(&mail.readable_body());
    let is_own = |a: &String| own.iter().any(|o| o.trim().eq_ignore_ascii_case(a));
    let (subject_delivery, mut named) = delivery_cues(&subject);
    let (body_delivery, body_named) = delivery_cues(&body);
    named.extend(body_named);
    let delivery = |named: Vec<String>| {
        if named.iter().any(is_own) {
            NoticeKind::OwnTransfer
        } else {
            NoticeKind::Delivery { sender: named.into_iter().next() }
        }
    };

    if subject_delivery {
        return delivery(named);
    }
    if outbound_cue(&subject) {
        return NoticeKind::OwnTransfer;
    }
    if body_delivery {
        return delivery(named);
    }
    if outbound_cue(&body) {
        return NoticeKind::OwnTransfer;
    }
    NoticeKind::ServiceMail
}

/// The words before «has sent you» / «σας έστειλε» (an address between them
/// skipped), up to four, on one line, in the text's own script and case.
static RE_SENT_YOU_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)((?:\p{L}[\p{L}.'’\-]*[ \t]+){0,3}\p{L}[\p{L}.'’\-]*)[ \t]+(?:[<(][^>)\s]+[>)][ \t]+)?(?:(?:has[ \t]+)?sent[ \t]+you\b|σας[ \t]+[εέ]στειλε)",
    )
    .unwrap()
});

/// The display name a delivery gives its sender («Nikos Example has sent
/// you…», «Ο Νίκος Παράδειγμα σας έστειλε…»), from the subject, else the
/// first body line that has one (P15.8). Only words: the roster decides
/// whether they name anyone.
pub fn sender_name(mail: &InboundMail) -> Option<String> {
    let body = mail.readable_body();
    std::iter::once(mail.subject.as_str())
        .chain(body.lines())
        .find_map(|line| RE_SENT_YOU_NAME.captures(line).map(|c| c[1].trim().to_string()))
}

/// Whether folded `text` says something about a transfer we sent.
fn outbound_cue(text: &str) -> bool {
    OUTBOUND.iter().any(|p| text.contains(&fold(p)))
}

/// Whether folded `text` says files were sent to us, and the sender
/// addresses it names.
fn delivery_cues(text: &str) -> (bool, Vec<String>) {
    let mut delivery = false;
    let mut named: Vec<String> = Vec::new();
    let address = |word: Option<regex::Match>| {
        word.and_then(|w| RE_ADDRESS.captures(w.as_str())).map(|c| c[1].to_ascii_lowercase())
    };
    for c in RE_SENT_YOU.captures_iter(text) {
        let before = c.get(1).map_or("", |m| m.as_str());
        let after = c.get(2).map_or("", |m| m.as_str());
        // «We sent you this email because…» is a footer, not a delivery.
        if matches!(before, "WE" | "WE'VE") || matches!(after, "THIS" | "THESE") {
            continue;
        }
        delivery = true;
        named.extend(address(c.get(1)));
    }
    for c in RE_SENT_YOU_EL.captures_iter(text) {
        delivery = true;
        named.extend(address(c.get(1)));
    }
    let footer_el = [fold("αυτό"), fold("αυτήν"), fold("αυτή"), fold("το"), fold("αυτά")];
    delivery |= RE_RECEIVED.captures_iter(text).any(|c| !matches!(c.get(1).map(|m| m.as_str()), Some("THIS")));
    delivery |= RE_RECEIVED_EL
        .captures_iter(text)
        .any(|c| !c.get(1).is_some_and(|m| footer_el.iter().any(|f| f == m.as_str())));
    delivery |= RE_SHARED.is_match(text) || RE_INVITED.is_match(text);
    for c in RE_NAMED.captures_iter(text) {
        delivery = true;
        named.push(c[1].to_ascii_lowercase());
    }
    (delivery, named)
}

/// Whether `from` is an address of a transfer service's own domain.
fn from_a_service(from: &str) -> bool {
    let from = from.trim().trim_end_matches('>').to_ascii_lowercase();
    let Some((_, domain)) = from.rsplit_once('@') else { return false };
    SERVICE_DOMAINS.iter().any(|d| domain == *d || domain.ends_with(&format!(".{d}")))
}

/// Text in one comparable form: uppercase, Greek without accents, the Greek
/// capitals that look Latin made Latin (`ΤransferNow` with a Greek Τ is
/// `TRANSFERNOW`), typographic apostrophes plain, whitespace single spaces.
fn fold(s: &str) -> String {
    let folded: String = normalize_greek(s)
        .chars()
        .map(|c| match c {
            'Α' => 'A',
            'Β' => 'B',
            'Ε' => 'E',
            'Ζ' => 'Z',
            'Η' => 'H',
            'Ι' => 'I',
            'Κ' => 'K',
            'Μ' => 'M',
            'Ν' => 'N',
            'Ο' => 'O',
            'Ρ' => 'P',
            'Τ' => 'T',
            'Υ' => 'Y',
            'Χ' => 'X',
            '\u{2019}' | '\u{2018}' | '\u{02BC}' => '\'',
            c => c,
        })
        .collect();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mail(from: &str, subject: &str, body: &str) -> InboundMail {
        InboundMail { from_address: from.into(), subject: subject.into(), body_text: body.into(), ..Default::default() }
    }

    fn own() -> Vec<String> {
        vec!["master@example.gr".into(), "flow@example.gr".into()]
    }

    fn kind(from: &str, subject: &str, body: &str) -> NoticeKind {
        notice_kind(&mail(from, subject, body), &own())
    }

    fn delivery(sender: Option<&str>) -> NoticeKind {
        NoticeKind::Delivery { sender: sender.map(String::from) }
    }

    #[test]
    fn deliveries_to_us_are_read_and_name_their_sender() {
        let tn = "noreply@transfernow.net";
        assert_eq!(
            kind(tn, "Petros Example has sent you \"clip.mp4\" via TransferNow", "Petros Example <p.example@studio.example> sent you 1 file\nhttps://www.transfernow.net/dl/2026x/abc"),
            delivery(Some("p.example@studio.example"))
        );
        assert_eq!(
            kind(tn, "Ο Πέτρος Παράδειγμα σας έστειλε αρχεία μέσω της ΤransferNow", "Πέτρος Παράδειγμα <p.example@studio.example> σας έστειλε 2 αρχεία"),
            delivery(Some("p.example@studio.example"))
        );
        assert_eq!(
            kind("noreply@wetransfer.com", "p.example@studio.example sent you KATASTROFI via WeTransfer", "p.example@studio.example sent you 1 item"),
            delivery(Some("p.example@studio.example"))
        );
        assert_eq!(
            kind("noreply@wetransfer.com", "clip.mp4 via WeTransfer", "p.example@studio.example sent you clip.mp4"),
            delivery(Some("p.example@studio.example"))
        );
        assert_eq!(
            kind("noreply@swisstransfer.com", "You have received 3 files via SwissTransfer", "3 files, 1.2 GB\nFrom p.example@studio.example\nDownload files"),
            delivery(Some("p.example@studio.example"))
        );
        assert_eq!(
            kind("noreply@mail.myairbridge.com", "☁ ΠΛΗΜΜΥΡΑ ΧΑΝΙΑ", "Sender: p.example@studio.example\nDownload: https://mab.to/t/AbC123/eu1\n> Delete <"),
            delivery(Some("p.example@studio.example"))
        );
        assert_eq!(kind("no-reply@em-s.dropbox.com", "Petros shared \"clip.mp4\" with you", "Petros shared a file with you."), delivery(None));
        assert_eq!(kind("noreply@transfernow.net", "Λάβατε 2 αρχεία", "Λάβατε 2 αρχεία μέσω TransferNow"), delivery(None));
    }

    /// The subject outranks a footer in the body that reads like a delivery.
    #[test]
    fn the_subject_outranks_the_body() {
        assert_eq!(
            kind("noreply@wetransfer.com", "x.mp4 has been downloaded", "x.mp4 has been downloaded.\nΛάβατε αυτήν την ειδοποίηση επειδή στείλατε αρχεία."),
            NoticeKind::OwnTransfer
        );
        assert_eq!(
            kind("noreply@wetransfer.com", "x.mp4 has been downloaded", "You have received an email from WeTransfer because you sent a transfer."),
            NoticeKind::OwnTransfer
        );
        // A delivery subject wins over an outbound word in the body.
        assert_eq!(
            kind("noreply@wetransfer.com", "p.example@studio.example sent you KAIROS via WeTransfer", "Download link expires in 7 days"),
            delivery(Some("p.example@studio.example"))
        );
        // No cue in the subject: the body decides.
        assert_eq!(kind("noreply@transfernow.net", "TransferNow", "Your link expires tomorrow"), NoticeKind::OwnTransfer);
    }

    #[test]
    fn more_delivery_cues() {
        assert_eq!(kind("noreply@swisstransfer.com", "You\u{2019}ve received 2 files", "Download"), delivery(None));
        assert_eq!(kind("noreply@swisstransfer.com", "You've received 2 files", "Download"), delivery(None));
        assert_eq!(kind("no-reply@dropbox.com", "Petros invited you to \"KAIROS\"", "Petros invited you to a folder."), delivery(None));
    }

    #[test]
    fn a_delivery_naming_our_own_address_is_our_own_transfer() {
        assert_eq!(
            kind("noreply@wetransfer.com", "master@example.gr sent you KAIROS via WeTransfer", "master@example.gr sent you 1 item"),
            NoticeKind::OwnTransfer
        );
        assert_eq!(kind("noreply@swisstransfer.com", "You have received 1 file", "From FLOW@example.gr Download"), NoticeKind::OwnTransfer);
    }

    #[test]
    fn notices_about_what_we_sent_are_our_own_transfers() {
        for (from, subject) in [
            ("noreply@transfernow.net", "O σύνδεσμος της μεταφοράς σας δημιουργήθηκε με επιτυχία"),
            ("noreply@transfernow.net", "Το αρχείο σας \"x.mp4\" εστάλη σε a@studio.example"),
            ("noreply@transfernow.net", "a@studio.example έκανε λήψη του \"x.mp4\""),
            ("noreply@transfernow.net", "Το αρχείο σας \"x.mp4\" λήγει σε 2 ημέρες"),
            ("noreply@transfernow.net", "Η μεταφορά σας δεν έχει ληφθεί ακόμη..."),
            ("noreply@transfernow.net", "Τα αρχεία σας στάλθηκαν σε a@studio.example"),
            ("noreply@transfernow.net", "a@studio.example έκανε μεταφόρτωση των αρχείων σας"),
            ("noreply@transfernow.net", "Your file \"x.mp4\" has been successfully sent to a@studio.example"),
            ("noreply@transfernow.net", "a@studio.example has downloaded \"x.mp4\" with TransferNow"),
            ("noreply@transfernow.net", "Η ΤransferNow εντόπισε μία μη έγκυρη διεύθυνση email"),
            ("noreply@transfernow.net", "Η TransferNow εντόπισε μία μη έγκυρη διεύθυνση email"),
            ("noreply@wetransfer.com", "x.mp4 sent successfully to a@studio.example"),
            ("noreply@wetransfer.com", "x.mp4 has been downloaded"),
            ("noreply@wetransfer.com", "x.mp4 has been previewed"),
            ("noreply@wetransfer.com", "A transfer you sent is about to expire"),
            ("noreply@wetransfer.com", "Your files haven\u{2019}t been downloaded yet..."),
            ("noreply@wetransfer.com", "Your code is: QWERTY"),
            ("noreply@swisstransfer.com", "Your transfer has been successfully completed"),
            ("noreply@swisstransfer.com", "Your transfer has been downloaded by a@studio.example"),
            ("noreply@swisstransfer.com", "Swisstransfer confirmation code: QWERTY"),
        ] {
            assert_eq!(kind(from, subject, "We sent you this email because you used our service."), NoticeKind::OwnTransfer, "{subject}");
        }
    }

    #[test]
    fn other_service_mail_and_staff_mail() {
        assert_eq!(
            kind("no-reply@dropbox.com", "Your Dropbox is full", "Your Dropbox is full. Upgrade to keep syncing.\nhttps://www.dropbox.com/plans"),
            NoticeKind::ServiceMail
        );
        assert_eq!(kind("news@wetransfer.com", "Meet the new WeTransfer", "Λάβατε αυτό το μήνυμα επειδή είστε μέλος."), NoticeKind::ServiceMail);
        // A colleague forwarding a notice is a normal mail.
        assert_eq!(
            kind("a.papadaki@example.gr", "FW: x.mp4 has been downloaded", "-------- Forwarded message --------\nFrom: noreply@wetransfer.com\nx.mp4 has been downloaded"),
            NoticeKind::NotANotice
        );
        // A lookalike domain is not the service.
        assert_eq!(kind("noreply@wetransfer.com.evil.example", "x.mp4 has been downloaded", ""), NoticeKind::NotANotice);
        assert!(NoticeKind::OwnTransfer.queues_nothing() && NoticeKind::ServiceMail.queues_nothing());
        assert!(!delivery(None).queues_nothing() && !NoticeKind::NotANotice.queues_nothing());
    }
}

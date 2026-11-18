//! Every sentence Setup's pages say, in Greek and English (P13.1c): one table,
//! one key per string, placeholders `{0}`, `{1}`. The installer's own lines
//! (progress details, refusals) stay in English inside these translated frames.

use omni_cli::install::Step;

/// The language of the pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    El,
    En,
}

impl Lang {
    /// `el` or `en`, any case.
    pub fn parse(s: &str) -> Option<Lang> {
        match s.trim().to_ascii_lowercase().as_str() {
            "el" => Some(Lang::El),
            "en" => Some(Lang::En),
            _ => None,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Lang::El => "el",
            Lang::En => "en",
        }
    }

    /// Greek for a Greek Windows display language (primary language 0x08), English otherwise.
    pub fn from_langid(langid: u16) -> Lang {
        if langid & 0x3ff == 0x08 {
            Lang::El
        } else {
            Lang::En
        }
    }

    /// The Windows display language of this user.
    pub fn detect() -> Lang {
        #[cfg(windows)]
        {
            // SAFETY: no arguments; returns a LANGID.
            Lang::from_langid(unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() })
        }
        #[cfg(not(windows))]
        {
            Lang::En
        }
    }
}

macro_rules! texts {
    ($($key:ident => $en:expr, $el:expr;)*) => {
        /// A string of the pages.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum T {
            $($key),*
        }

        impl T {
            pub const ALL: &'static [T] = &[$(T::$key),*];

            pub fn en(self) -> &'static str {
                match self {
                    $(T::$key => $en),*
                }
            }

            pub fn el(self) -> &'static str {
                match self {
                    $(T::$key => $el),*
                }
            }
        }
    };
}

texts! {
    Title => "OmniIngest Setup {0}", "Εγκατάσταση OmniIngest {0}";
    BtnCancel => "Cancel", "Άκυρο";
    BtnBack => "< Back", "< Πίσω";
    BtnNext => "Next >", "Επόμενο >";
    BtnClose => "Close", "Κλείσιμο";
    BtnInstall => "Install", "Εγκατάσταση";
    BtnUpgrade => "Upgrade", "Αναβάθμιση";
    BtnChange => "Change", "Αλλαγή";
    BtnRemove => "Remove", "Κατάργηση";
    BtnWait => "Please wait…", "Περιμένετε…";

    WelcomeNewMain => "Install OmniIngest {0}", "Εγκατάσταση του OmniIngest {0}";
    WelcomeNewContent =>
        "OmniIngest will be installed in this folder:\n{0}\n\nThe watchfolder and the mailbox are set up afterwards, in the setup wizard of the admin page.",
        "Το OmniIngest θα εγκατασταθεί σε αυτόν τον φάκελο:\n{0}\n\nΟ φάκελος παρακολούθησης (watchfolder) και το γραμματοκιβώτιο ρυθμίζονται μετά, στον οδηγό ρύθμισης της σελίδας διαχείρισης.";
    LinkInstall => "Install\nInstalls OmniIngest and its Windows service in this folder.",
        "Εγκατάσταση\nΕγκαθιστά το OmniIngest και την υπηρεσία του στα Windows σε αυτόν τον φάκελο.";
    LinkChangeFolder => "Change folder…\nChoose another folder for the installation.",
        "Αλλαγή φακέλου…\nΕπιλέξτε άλλον φάκελο για την εγκατάσταση.";
    WelcomeFoundMain => "OmniIngest is installed on this computer", "Το OmniIngest είναι εγκατεστημένο σε αυτόν τον υπολογιστή";
    FoundFolder => "Folder: {0}", "Φάκελος: {0}";
    FoundVersion => "Version: {0}", "Έκδοση: {0}";
    FoundAccount => "The service runs as: {0}", "Η υπηρεσία εκτελείται ως: {0}";
    FoundNoService => "The Windows service is not registered.", "Η υπηρεσία δεν είναι καταχωρημένη στα Windows.";
    FoundWhat => "What would you like to do?", "Τι θέλετε να κάνετε;";
    LinkUpgrade => "Upgrade to {0}\nThe settings, the database and the service account are kept.",
        "Αναβάθμιση στην έκδοση {0}\nΟι ρυθμίσεις, η βάση δεδομένων και ο λογαριασμός της υπηρεσίας διατηρούνται.";
    LinkChangeAccount => "Change the account the service runs as", "Αλλαγή του λογαριασμού με τον οποίο εκτελείται η υπηρεσία";
    LinkRemove => "Remove OmniIngest", "Κατάργηση του OmniIngest";

    AccountMain => "Which account should the OmniIngest service run as?", "Με ποιον λογαριασμό να εκτελείται η υπηρεσία OmniIngest;";
    AccountContent =>
        "The built-in service account reaches folders on this computer, and a network share only if the share admits this computer's account.\n\nA Windows account is needed for a NAS or a share that asks for a user name.",
        "Ο ενσωματωμένος λογαριασμός υπηρεσίας έχει πρόσβαση στους φακέλους αυτού του υπολογιστή, και σε κοινόχρηστο φάκελο δικτύου μόνο αν ο φάκελος αυτός δέχεται τον λογαριασμό του υπολογιστή.\n\nΛογαριασμός των Windows χρειάζεται για NAS ή για κοινόχρηστο φάκελο που ζητά όνομα χρήστη.";
    AccountNow => "The service runs as {0} now.", "Αυτή τη στιγμή η υπηρεσία εκτελείται ως {0}.";
    AccountRefused => "The account was not accepted:", "Ο λογαριασμός δεν έγινε δεκτός:";
    RadioBuiltIn => "The built-in service account (recommended)", "Ο ενσωματωμένος λογαριασμός υπηρεσίας (συνιστάται)";
    RadioWindows => "A Windows account…", "Ένας λογαριασμός των Windows…";
    RadioGiven => "{0}, as given on the command line", "{0}, όπως δόθηκε στη γραμμή εντολών";
    CredCaption => "OmniIngest service account", "Λογαριασμός της υπηρεσίας OmniIngest";
    CredMessage =>
        "Enter the Windows account the OmniIngest service will run as (DOMAIN\\user, or .\\user for an account of this computer) and its password.",
        "Πληκτρολογήστε τον λογαριασμό των Windows με τον οποίο θα εκτελείται η υπηρεσία OmniIngest (ΤΟΜΕΑΣ\\χρήστης, ή .\\χρήστης για λογαριασμό αυτού του υπολογιστή) και τον κωδικό πρόσβασής του.";
    FolderTitle => "Choose the folder for OmniIngest", "Επιλέξτε τον φάκελο του OmniIngest";

    SummaryInstallMain => "Ready to install OmniIngest {0}", "Όλα είναι έτοιμα για την εγκατάσταση του OmniIngest {0}";
    SummaryUpgradeMain => "Ready to upgrade OmniIngest to {0}", "Όλα είναι έτοιμα για την αναβάθμιση του OmniIngest στην έκδοση {0}";
    SummaryChangeMain => "Ready to change the service account", "Όλα είναι έτοιμα για την αλλαγή του λογαριασμού της υπηρεσίας";
    SumFolder => "OmniIngest is installed in {0}.", "Το OmniIngest θα εγκατασταθεί στον φάκελο {0}.";
    SumUpgradeFolder => "The installation in {0} is upgraded.", "Θα αναβαθμιστεί η εγκατάσταση στον φάκελο {0}.";
    SumChangeFolder => "The installation in {0} keeps its files and settings.", "Η εγκατάσταση στον φάκελο {0} κρατά τα αρχεία και τις ρυθμίσεις της.";
    SumAccount => "The service will run as {0}.", "Η υπηρεσία θα εκτελείται ως {0}.";
    SumService => "The OmniIngest Windows service is installed and started; from then on it starts by itself with Windows.",
        "Θα εγκατασταθεί και θα ξεκινήσει η υπηρεσία OmniIngest των Windows. Στο εξής θα ξεκινά αυτόματα μαζί με τα Windows.";
    SumFirewall => "Windows Firewall gets a rule that opens the web panel's port {0} on domain and private networks.",
        "Στο Τείχος προστασίας των Windows θα προστεθεί κανόνας που ανοίγει τη θύρα {0} της σελίδας web στα δίκτυα τομέα και στα ιδιωτικά δίκτυα.";
    SumNoFirewall => "No firewall rule is added.", "Δεν θα προστεθεί κανόνας στο τείχος προστασίας.";
    SumKeeps => "Kept as they are: the settings, the database and the account the service runs as.",
        "Μένουν όπως είναι: οι ρυθμίσεις, η βάση δεδομένων και ο λογαριασμός με τον οποίο εκτελείται η υπηρεσία.";
    SumKeepsData => "Kept as they are: the settings and the database.", "Μένουν όπως είναι: οι ρυθμίσεις και η βάση δεδομένων.";
    SumUpgradeBack => "If the new version does not start, the previous one is put back.", "Αν η νέα έκδοση δεν ξεκινήσει, επανέρχεται η προηγούμενη.";
    SumChangeBack => "The service is restarted under the new account; if it does not start, Setup puts the previous state back.",
        "Η υπηρεσία θα επανεκκινηθεί με τον νέο λογαριασμό. Αν δεν ξεκινήσει, το πρόγραμμα εγκατάστασης επαναφέρει την προηγούμενη κατάσταση.";
    SumNotes => "Please note:", "Προσοχή:";
    ShareHeading => "The watchfolder is on a network share", "Ο φάκελος παρακολούθησης είναι σε κοινόχρηστο φάκελο δικτύου";
    ShareAdvice => "With the built-in account the service may not be able to write there. Press Back to choose a Windows account.",
        "Με τον ενσωματωμένο λογαριασμό η υπηρεσία ίσως να μην μπορεί να γράψει εκεί. Πατήστε «Πίσω» για να ορίσετε λογαριασμό των Windows.";

    ProgInstallMain => "Installing OmniIngest {0}…", "Γίνεται εγκατάσταση του OmniIngest {0}…";
    ProgUpgradeMain => "Upgrading OmniIngest to {0}…", "Γίνεται αναβάθμιση του OmniIngest στην έκδοση {0}…";
    ProgChangeMain => "Changing the service account…", "Γίνεται αλλαγή του λογαριασμού της υπηρεσίας…";
    ProgRemoveMain => "Removing OmniIngest…", "Γίνεται κατάργηση του OmniIngest…";
    ProgWait => "Setup goes on by itself when it has finished. The first start of the service can take a few minutes.",
        "Το πρόγραμμα εγκατάστασης θα συνεχίσει μόνο του μόλις τελειώσει. Η πρώτη εκκίνηση της υπηρεσίας μπορεί να διαρκέσει μερικά λεπτά.";
    ProgNow => "Now: {0}", "Τώρα: {0}";
    DetailsShow => "Show details", "Εμφάνιση λεπτομερειών";
    DetailsHide => "Hide details", "Απόκρυψη λεπτομερειών";
    StepPlan => "Preparing", "Προετοιμασία";
    StepImport => "Bringing over the previous installation", "Μεταφορά της προηγούμενης εγκατάστασης";
    StepAccount => "Checking the account", "Έλεγχος του λογαριασμού";
    StepStop => "Stopping the service", "Διακοπή της υπηρεσίας";
    StepCopy => "Copying the program files", "Αντιγραφή των αρχείων του προγράμματος";
    StepConfig => "Writing the settings", "Αποθήκευση των ρυθμίσεων";
    StepRegister => "Registering the Windows service", "Καταχώρηση της υπηρεσίας στα Windows";
    StepRights => "Setting the folder permissions", "Ρύθμιση των δικαιωμάτων στους φακέλους";
    StepFirewall => "Adding the firewall rule", "Προσθήκη του κανόνα στο τείχος προστασίας";
    StepStart => "Starting the service", "Εκκίνηση της υπηρεσίας";
    StepHealth => "Waiting for the service to answer", "Αναμονή για απόκριση από την υπηρεσία";
    StepRollback => "Putting the previous state back", "Επαναφορά της προηγούμενης κατάστασης";
    StepDone => "Finishing", "Ολοκλήρωση";

    FinInstalledMain => "OmniIngest is installed.", "Το OmniIngest εγκαταστάθηκε.";
    FinInstalledContent =>
        "The admin page opens now in your browser: sign in as Admin / Admin, the first-run account, which works only from this computer; the setup wizard continues there.",
        "Η σελίδα διαχείρισης ανοίγει τώρα στο πρόγραμμα περιήγησης. Συνδεθείτε ως Admin / Admin, με τον λογαριασμό πρώτης εκκίνησης, που λειτουργεί μόνο από αυτόν τον υπολογιστή. Ο οδηγός ρύθμισης συνεχίζει από εκεί.";
    FinInstalledKeptContent =>
        "The admin page opens now in your browser. This folder already held a database, which was kept: sign in with the administrator account of this installation.",
        "Η σελίδα διαχείρισης ανοίγει τώρα στο πρόγραμμα περιήγησης. Ο φάκελος είχε ήδη βάση δεδομένων, η οποία διατηρήθηκε: συνδεθείτε με τον λογαριασμό διαχειριστή αυτής της εγκατάστασης.";
    FinAddress => "Admin page: {0}", "Σελίδα διαχείρισης: {0}";
    FinUpgraded => "OmniIngest was upgraded to {0}.", "Το OmniIngest αναβαθμίστηκε στην έκδοση {0}.";
    FinAccount => "The service now runs as {0}.", "Η υπηρεσία εκτελείται πλέον ως {0}.";
    FinRemovedMain => "Removal finished", "Η κατάργηση ολοκληρώθηκε";
    FinRolledBackMain => "Setup put the previous state back", "Το πρόγραμμα εγκατάστασης επανέφερε την προηγούμενη κατάσταση";
    FinNotBackMain => "Setup could not put everything back", "Το πρόγραμμα εγκατάστασης δεν κατάφερε να επαναφέρει τα πάντα";
    FinFailedMain => "Setup could not finish", "Το πρόγραμμα εγκατάστασης δεν ολοκληρώθηκε";
    FinLog => "The details are in {0}.", "Οι λεπτομέρειες είναι στο αρχείο {0}.";
    WorkerStopped => "Setup stopped unexpectedly while it was working. The OmniIngest service may be stopped: start Setup again.",
        "Το πρόγραμμα εγκατάστασης σταμάτησε απρόσμενα ενώ εργαζόταν. Η υπηρεσία OmniIngest ίσως έχει σταματήσει: ξεκινήστε ξανά το πρόγραμμα εγκατάστασης.";
    FinRefusedMain => "Setup cannot continue", "Η εγκατάσταση δεν μπορεί να συνεχιστεί";
    FinNothingChanged => "Nothing was changed.", "Δεν έγινε καμία αλλαγή.";

    RemoveMain => "Remove OmniIngest from this computer?", "Να καταργηθεί το OmniIngest από αυτόν τον υπολογιστή;";
    RemoveData =>
        "Also remove the database, the saved passwords and keys, the settings, the logs, the temporary files and the archive of delivered videos in the installation folder",
        "Να καταργηθούν επίσης η βάση δεδομένων, οι αποθηκευμένοι κωδικοί και τα κλειδιά, οι ρυθμίσεις, τα αρχεία καταγραφής, τα προσωρινά αρχεία και το αρχείο των παραδομένων βίντεο στον φάκελο εγκατάστασης";

    BoxDeclined => "Setup needs administrator rights, and the Windows prompt was declined. Nothing was changed.",
        "Το πρόγραμμα εγκατάστασης χρειάζεται δικαιώματα διαχειριστή, αλλά η ερώτηση των Windows απορρίφθηκε. Δεν έγινε καμία αλλαγή.";
    BoxNotElevated => "Setup needs administrator rights. Start it again and answer Yes in the Windows prompt.",
        "Το πρόγραμμα εγκατάστασης χρειάζεται δικαιώματα διαχειριστή. Ξεκινήστε το ξανά και απαντήστε «Ναι» στην ερώτηση των Windows.";
    BoxNoStart => "Setup could not start with administrator rights:", "Το πρόγραμμα εγκατάστασης δεν μπόρεσε να ξεκινήσει με δικαιώματα διαχειριστή:";
}

/// The string `key` in `lang`.
pub fn tr(lang: Lang, key: T) -> &'static str {
    match lang {
        Lang::El => key.el(),
        Lang::En => key.en(),
    }
}

/// `key` in `lang` with `{0}`, `{1}`, … replaced by `args`.
pub fn fmt(lang: Lang, key: T, args: &[&str]) -> String {
    let mut out = tr(lang, key).to_string();
    for (i, a) in args.iter().enumerate() {
        out = out.replace(&format!("{{{i}}}"), a);
    }
    out
}

/// What the progress page says the installer is doing.
pub fn step_key(step: Step) -> T {
    match step {
        Step::Plan => T::StepPlan,
        Step::Import => T::StepImport,
        Step::Account => T::StepAccount,
        Step::Stop => T::StepStop,
        Step::Copy => T::StepCopy,
        Step::Config => T::StepConfig,
        Step::Register => T::StepRegister,
        Step::Rights => T::StepRights,
        Step::Firewall => T::StepFirewall,
        Step::Start => T::StepStart,
        Step::Health => T::StepHealth,
        Step::Rollback => T::StepRollback,
        Step::Done => T::StepDone,
    }
}

/// The `{n}` placeholders of `s`, sorted.
pub fn placeholders(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) if !after[..end].is_empty() && after[..end].chars().all(|c| c.is_ascii_digit()) => {
                out.push(after[..end].to_string());
                rest = &after[end + 1..];
            }
            _ => rest = after,
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_greek(c: char) -> bool {
        ('\u{0370}'..='\u{03FF}').contains(&c) || ('\u{1F00}'..='\u{1FFF}').contains(&c)
    }

    #[test]
    fn every_key_has_a_string_in_both_languages() {
        assert!(T::ALL.len() > 50);
        for k in T::ALL {
            assert!(!k.en().trim().is_empty(), "{k:?} en");
            assert!(!k.el().trim().is_empty(), "{k:?} el");
        }
    }

    #[test]
    fn both_languages_use_the_same_placeholders() {
        for k in T::ALL {
            assert_eq!(placeholders(k.en()), placeholders(k.el()), "{k:?}");
        }
        assert_eq!(placeholders("a {1} b {0} {x} {}"), ["0", "1"]);
    }

    #[test]
    fn no_greek_string_is_an_untranslated_copy() {
        for k in T::ALL {
            let (en, el) = (k.en(), k.el());
            assert!(el.chars().any(is_greek), "{k:?}: no Greek letter in {el:?}");
            assert!(!el.contains(en), "{k:?}: the Greek string still holds the English one");
            // Every English sentence of more than two words is gone from the Greek one.
            for sentence in en.split(['.', '\n', ':', ';']).map(str::trim).filter(|s| s.split_whitespace().count() > 2) {
                assert!(!el.contains(sentence), "{k:?}: {sentence:?} is untranslated");
            }
        }
    }

    #[test]
    fn every_step_has_its_own_label() {
        let steps = [
            Step::Plan,
            Step::Import,
            Step::Account,
            Step::Stop,
            Step::Copy,
            Step::Config,
            Step::Register,
            Step::Rights,
            Step::Firewall,
            Step::Start,
            Step::Health,
            Step::Rollback,
            Step::Done,
        ];
        let keys: Vec<T> = steps.iter().map(|s| step_key(*s)).collect();
        for (i, k) in keys.iter().enumerate() {
            assert!(!keys[i + 1..].contains(k), "{k:?} twice");
        }
    }

    #[test]
    fn placeholders_are_filled_and_the_language_is_read_from_windows() {
        assert_eq!(fmt(Lang::En, T::Title, &["1.0.0"]), "OmniIngest Setup 1.0.0");
        assert_eq!(fmt(Lang::El, T::FinAccount, &[r"NEWSROOM\svc_test"]), r"Η υπηρεσία εκτελείται πλέον ως NEWSROOM\svc_test.");
        assert_eq!(Lang::from_langid(0x0408), Lang::El);
        assert_eq!(Lang::from_langid(0x0409), Lang::En);
        assert_eq!(Lang::from_langid(0x0809), Lang::En);
        assert_eq!(Lang::parse(" EL "), Some(Lang::El));
        assert_eq!(Lang::parse("de"), None);
    }
}

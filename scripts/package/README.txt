OmniDownloader - installation
=============================

INSTALL WITH SETUP (new PC; the usual way)
  1. Copy OmniIngest-Setup-<version>-<date>-<arch>.exe (it comes next to this zip)
     to the PC. To check it first, in PowerShell:
       (Get-FileHash .\OmniIngest-Setup-<version>-<date>-<arch>.exe).Hash
     must equal the first word of OmniIngest-Setup-<version>-<date>-<arch>.exe.sha256
     (upper or lower case does not matter). <arch> is x64 or arm64: a 64-bit
     Intel/AMD server needs the x64 files (this folder's name ends with its
     <arch> too).
  2. Double-click it and answer Yes when Windows asks for administrator rights.
     The file is not code-signed: on its first run Windows may show "Windows
     protected your PC". Click "More info", then "Run anyway".
  3. Setup asks, one page at a time:
       - where to install: "Install" (C:\OmniIngest), or "Change folder..." first;
       - the account the service runs as: "The built-in service account
         (recommended)" for a watchfolder on this computer, or on a share that
         admits this computer's account; "A Windows account..." for a NAS or a
         share that asks for a user name (Windows asks for its password);
       - a summary of what it will do: press "Install".
  4. Setup installs, starts the service and says "OmniIngest is installed."
     Press "Close": the admin page opens in your browser on this computer.
     Sign in as Admin / Admin, the first-run account, which works only from
     this computer; the setup wizard continues there (see step 4 below).
  Setup adds "OmniIngest" to Settings > Apps: Modify there changes the account
  the service runs as, Uninstall removes it. Unattended installs (/quiet) and
  the exit codes: OmniIngest-Setup.exe /?  and docs\ADMIN_GUIDE.md section 2.

INSTALL FROM THIS FOLDER (command line, the alternative)
  1. Copy this folder (or unzip the release) anywhere on the PC. Start
     Install.cmd from a local folder, not from a mapped network drive: the
     elevated session does not see your drive letters.
  2. Double-click Install.cmd and allow administrator rights.
  3. Give the Dalet watchfolder when asked (a local folder or \\server\share).
  4. When it says "OmniDownloader is running", open http://127.0.0.1:8080/admin
     ON THIS COMPUTER and sign in as   Admin / Admin   (the first-run account,
     which works only from this computer). The setup wizard then opens by
     itself; it replaces that account with your own administrator and walks
     you through the watchfolder and the mailbox.
     To let the MCR desks open the panel without signing in, choose
     Intermediate in the wizard and enter the newsroom network in its Network
     step (you can run the wizard again later from the admin page).
     A watchfolder on a NAS or an IP-addressed share needs the service to run
     under an account; the wizard tells you what to do.

  Installs to C:\OmniIngest as the Windows service "OmniIngestService":
  starts with Windows, restarts by itself after a failure, panel on port 8080
  (opened in the firewall for domain and private networks).

  The service runs as the virtual account NT SERVICE\OmniIngestService (no
  password), with write rights only on its own data, temp, logs, archive, bin,
  config.json and a local watchfolder. "--account LocalSystem" opts out.
  An upgrade keeps the account it has; to switch an old install:
       Install.cmd --account "NT SERVICE\OmniIngestService"

  A network watchfolder needs a domain account that may write to it (or this
  computer's account granted on the share):
       Install.cmd --watchfolder "\\dalet\ingest" --account "DOMAIN\svc_omni"
  Other options:  --dir D:\OmniIngest   --port 8081   --yes   --no-firewall

MOVING AN EXISTING SETUP ON THE SAME PC (e.g. from D:\OmniDownloader)
  Stop the old one first (close its console window), then:
       Install.cmd --import D:\OmniDownloader
  The configuration, database, keys and seeds come along; the watchfolder
  stays where it was. Keys only work on the PC they were saved on: on a
  different PC, enter them again in Admin.

UPGRADE
  Run the new OmniIngest-Setup-<version>-<date>-<arch>.exe: it finds the installation
  and offers "Upgrade to <version>". Or unzip the new release and run its
  Install.cmd. The service is stopped, the program and tools replaced, and the
  service started again. Data, configuration, passwords, keys and the service
  account are kept. If the new version does not start, the previous one is put
  back automatically.

UNINSTALL
  Installed with Setup: Settings > Apps > OmniIngest > Uninstall. It removes
  the service, the firewall rule, the Apps entry and the program files; the
  configuration, database and logs stay unless you tick the box that removes
  them too. The watchfolder is never touched.
  Uninstall.cmd removes the service and the firewall rule. The folder
  C:\OmniIngest (database, configuration, logs) stays; delete it by hand.

REQUIREMENTS
  Windows 10/11 or Server 2019+, 64-bit. Microsoft Edge or Google Chrome
  (for reading news pages). Internet access (yt-dlp and Deno update
  themselves nightly). Nothing else: the C++ runtime the tools need is in bin\.

CHECK
  SHA256SUMS.txt lists every file's checksum.
  Logs: C:\OmniIngest\logs   Setup's own log: C:\OmniIngest\setup.log
  Health: http://127.0.0.1:8080/api/health

GUIDES
  docs\USER_GUIDE.md    the MCR desk and the journalist panel
  docs\ADMIN_GUIDE.md   installing, configuration, cookies, toolchain, troubleshooting

---------------------------------------------------------------------------

ΕΓΚΑΤΑΣΤΑΣΗ ΜΕ ΤΟ SETUP (νέος υπολογιστής· ο συνήθης τρόπος)
  1. Αντιγράψτε το OmniIngest-Setup-<version>-<date>-<arch>.exe (βρίσκεται δίπλα σε
     αυτό το zip) στον υπολογιστή. Για έλεγχο πρώτα, στο PowerShell:
       (Get-FileHash .\OmniIngest-Setup-<version>-<date>-<arch>.exe).Hash
     πρέπει να είναι ίδιο με την πρώτη λέξη του αρχείου
     OmniIngest-Setup-<version>-<date>-<arch>.exe.sha256 (κεφαλαία ή πεζά δεν έχουν σημασία).
     Το <arch> είναι x64 ή arm64: ένας server 64-bit με επεξεργαστή Intel/AMD
     χρειάζεται το αρχείο x64.
  2. Διπλό κλικ και «Ναι» όταν τα Windows ζητήσουν δικαιώματα διαχειριστή.
     Το αρχείο δεν έχει ψηφιακή υπογραφή: την πρώτη φορά τα Windows μπορεί να
     δείξουν «Τα Windows προστάτευσαν τον υπολογιστή σας» (Windows protected
     your PC). Πατήστε «Περισσότερες πληροφορίες» (More info) και μετά
     «Εκτέλεση οπωσδήποτε» (Run anyway).
  3. Το Setup ρωτά, μία σελίδα κάθε φορά:
       - πού θα γίνει η εγκατάσταση: «Εγκατάσταση» (C:\OmniIngest) ή πρώτα
         «Αλλαγή φακέλου…»·
       - με ποιον λογαριασμό θα εκτελείται η υπηρεσία: «Ο ενσωματωμένος
         λογαριασμός υπηρεσίας (συνιστάται)» για φάκελο Dalet σε αυτόν τον
         υπολογιστή, ή σε κοινόχρηστο φάκελο που δέχεται τον λογαριασμό του
         υπολογιστή· «Ένας λογαριασμός των Windows…» για NAS ή για κοινόχρηστο
         φάκελο που ζητά όνομα χρήστη (τα Windows ζητούν τον κωδικό του)·
       - μια σύνοψη όσων θα γίνουν: πατήστε «Εγκατάσταση».
  4. Το Setup εγκαθιστά, ξεκινά την υπηρεσία και γράφει «Το OmniIngest
     εγκαταστάθηκε.» Πατήστε «Κλείσιμο»: η σελίδα διαχείρισης ανοίγει στο
     πρόγραμμα περιήγησης αυτού του υπολογιστή. Συνδεθείτε ως Admin / Admin,
     τον λογαριασμό πρώτης εκκίνησης, που λειτουργεί μόνο από εδώ· ο οδηγός
     ρύθμισης συνεχίζει από εκεί (βλ. βήμα 4 παρακάτω).
  Το Setup προσθέτει το «OmniIngest» στις Ρυθμίσεις > Εφαρμογές: το
  «Τροποποίηση» αλλάζει τον λογαριασμό της υπηρεσίας, το «Κατάργηση
  εγκατάστασης» την αφαιρεί. Αθόρυβη εγκατάσταση (/quiet) και κωδικοί
  εξόδου: OmniIngest-Setup.exe /?  και docs\ADMIN_GUIDE.el.md ενότητα 2.

ΕΓΚΑΤΑΣΤΑΣΗ ΑΠΟ ΑΥΤΟΝ ΤΟΝ ΦΑΚΕΛΟ (γραμμή εντολών, εναλλακτικά)
  1. Αντιγράψτε τον φάκελο στον υπολογιστή. Ξεκινήστε το Install.cmd από
     τοπικό φάκελο, όχι από αντιστοιχισμένη μονάδα δικτύου: η περίοδος
     λειτουργίας με δικαιώματα διαχειριστή δεν βλέπει τα γράμματα μονάδων σας.
  2. Διπλό κλικ στο Install.cmd και αποδεχτείτε τα δικαιώματα διαχειριστή.
  3. Δώστε τον φάκελο του Dalet (watchfolder) όταν ζητηθεί.
  4. Ανοίξτε http://127.0.0.1:8080/admin ΣΕ ΑΥΤΟΝ ΤΟΝ ΥΠΟΛΟΓΙΣΤΗ και
     συνδεθείτε ως Admin / Admin (ο λογαριασμός πρώτης εκκίνησης, που
     λειτουργεί μόνο από εδώ). Μετά ανοίγει μόνος του ο οδηγός ρύθμισης:
     αντικαθιστά αυτόν τον λογαριασμό με δικό σας διαχειριστή και σας
     καθοδηγεί στον φάκελο Dalet (watchfolder) και στο email.
     Για να ανοίγουν οι θέσεις MCR τον πίνακα χωρίς σύνδεση, επιλέξτε
     Intermediate στον οδηγό και δώστε το δίκτυο της σύνταξης στο βήμα
     Network (μπορείτε να ξανατρέξετε τον οδηγό αργότερα από τη σελίδα
     διαχείρισης).
     Φάκελος Dalet σε NAS ή σε κοινόχρηστο φάκελο με διεύθυνση IP χρειάζεται
     η υπηρεσία να τρέχει με λογαριασμό· ο οδηγός σας λέει τι να κάνετε.
  Αναβάθμιση: τρέξτε το νέο OmniIngest-Setup-<version>-<date>-<arch>.exe και
  επιλέξτε «Αναβάθμιση στην έκδοση …», ή το Install.cmd της νέας έκδοσης.
  Τα δεδομένα και ο λογαριασμός της υπηρεσίας μένουν.
  Αν η νέα έκδοση δεν ξεκινήσει, επανέρχεται αυτόματα η προηγούμενη.
  Κατάργηση: Ρυθμίσεις > Εφαρμογές > OmniIngest > Κατάργηση εγκατάστασης
  (εγκατάσταση με το Setup), ή Uninstall.cmd. Ο φάκελος Dalet δεν αγγίζεται ποτέ.

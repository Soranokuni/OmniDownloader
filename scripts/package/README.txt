OmniDownloader - installation
=============================

INSTALL (new PC)
  1. Copy this folder (or unzip the release) anywhere on the PC.
  2. Double-click Install.cmd and allow administrator rights.
  3. Give the Dalet watchfolder when asked (a local folder or \\server\share).
  4. When it says "OmniDownloader is running", open on THIS PC:
       http://127.0.0.1:8080/setup     -> create the first administrator
     then in Admin: the mailbox (Microsoft Graph), the LLM key if used, and
     Security -> the newsroom network, so the MCR desks can open /mcr.

  Installs to C:\OmniIngest as the Windows service "OmniIngestService":
  starts with Windows, restarts by itself after a failure, panel on port 8080
  (opened in the firewall for domain and private networks).

  A network watchfolder needs a service account that may write to it:
       Install.cmd --watchfolder "\\dalet\ingest" --account "DOMAIN\svc_omni"
  Other options:  --dir D:\OmniIngest   --port 8081   --yes   --no-firewall

MOVING AN EXISTING SETUP ON THE SAME PC (e.g. from D:\OmniDownloader)
  Stop the old one first (close its console window), then:
       Install.cmd --import D:\OmniDownloader
  The configuration, database, keys and seeds come along; the watchfolder
  stays where it was. Keys only work on the PC they were saved on: on a
  different PC, enter them again in Admin.

UPGRADE
  Unzip the new release and run its Install.cmd. The service is stopped, the
  program and tools replaced, and the service started again. Data,
  configuration, passwords and keys are kept. If the new version does not
  start, the previous one is put back automatically.

UNINSTALL
  Uninstall.cmd removes the service and the firewall rule. The folder
  C:\OmniIngest (database, configuration, logs) stays; delete it by hand.

REQUIREMENTS
  Windows 10/11 or Server 2019+, 64-bit. Microsoft Edge or Google Chrome
  (for reading news pages). Internet access (yt-dlp and Deno update
  themselves nightly). Nothing else: the C++ runtime the tools need is in bin\.

CHECK
  SHA256SUMS.txt lists every file's checksum.
  Logs: C:\OmniIngest\logs   Health: http://127.0.0.1:8080/api/health

---------------------------------------------------------------------------

ΕΓΚΑΤΑΣΤΑΣΗ
  1. Αντιγράψτε τον φάκελο στον υπολογιστή.
  2. Διπλό κλικ στο Install.cmd και αποδεχτείτε τα δικαιώματα διαχειριστή.
  3. Δώστε τον φάκελο του Dalet (watchfolder) όταν ζητηθεί.
  4. Στον ίδιο υπολογιστή ανοίξτε http://127.0.0.1:8080/setup για τον
     πρώτο διαχειριστή και μετά στη Διαχείριση ρυθμίστε το email.
  Αναβάθμιση: τρέξτε το Install.cmd της νέας έκδοσης. Τα δεδομένα μένουν.
  Αν η νέα έκδοση δεν ξεκινήσει, επανέρχεται αυτόματα η προηγούμενη.

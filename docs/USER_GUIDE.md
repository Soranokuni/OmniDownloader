# OmniDownloader — User Guide

This guide is for the people who use OmniDownloader every day: **MCR
operators** at the desk and **journalists** who send links. For
installation, configuration and maintenance, see the
[Administrator Guide](ADMIN_GUIDE.md).

The panels are in Greek. Every label below is quoted exactly as it appears on
screen, with its English meaning next to it.

---

## 1. What it does, in one paragraph

A journalist emails the ingest mailbox with links (social posts, news
articles, YouTube, attached files), or pastes a link into the web panel.
OmniDownloader reads the mail, works out who it is for and what each story is
called, finds the video on each page, downloads it, converts it to the
station's broadcast format (XDCAM HD422 1080i50 MXF, 8 audio tracks,
−23 LUFS), checks the result, and drops it into the Dalet watchfolder under a
name like `1A_PAPADAKI_FOTIA.mxf`. If anything is not right, the file is
**not** delivered; the job waits on the MCR desk with the reason.

You do not need to do anything for a normal mail. The desk is where you look
when something needs a person.

---

## 2. Signing in and who sees what

Open the panel in a browser at the address your administrator gave you
(for example `http://ingest-pc:8080`).

| Role | What you see | How you get in |
|---|---|---|
| **Journalist** (`user`) | `/user`: send links, see your own submissions | your own account (work email + password) |
| **MCR operator** (`open_mcr`) | `/mcr`: the whole desk | an account, **or** no login at all from an MCR desk on the newsroom network |
| **Administrator** (`admin`) | `/admin` and `/mcr` | an administrator account |

- After signing in you are sent to the right page for your role.
- PCs on the newsroom network listed by the administrator open `/mcr` without
  a password. Everywhere else, everyone signs in.
- To change your password: **«Αλλαγή κωδικού»** (Change password) at the top
  of the page. New passwords are at least 12 characters.
- **«Αποσύνδεση»** signs you out.

---

## 3. For journalists

### 3.1 Sending links by email (the usual way)

Write to the ingest mailbox the way you always do. A numbered rundown works
best:

```
1. ΦΩΤΙΑ ΣΤΑ ΧΑΝΙΑ
https://www.example-news.gr/article/...

2. ΣΥΝΕΝΤΕΥΞΗ ΔΗΜΑΡΧΟΥ
https://youtu.be/...
https://x.com/.../status/...
```

- The number becomes the story number, the heading becomes the keyword.
  Two links under the same story become `2A` and `2B`.
- Video attachments are downloaded too.
- Links in your signature, and old links in quoted replies, are ignored.
- Microsoft "Safe Links" are unwrapped automatically.
- If the mail is sent on someone else's behalf, put their name in the subject
  or the text (for example «για τον Γιώργο»).

### 3.2 Sending links from the web panel (`/user`)

The **«Αποστολή βίντεο»** (Send video) form:

| Field | Meaning |
|---|---|
| «Σύνδεσμος του βίντεο ή του άρθρου» | the video or article link |
| «Πολλοί σύνδεσμοι (ένας ανά γραμμή, έως 20)» | tick to paste up to 20 links, one per line; they become 1A, 1B, 1C… |
| «Λέξη-κλειδί του θέματος» | the story keyword (required) |
| «Αριθμός θέματος στο δελτίο» | the story number in the bulletin (default 1) |
| «Σημείωση για το MCR (προαιρετικά)» | an optional note for MCR |
| «Επείγον (μπαίνει πριν από τα άλλα)» | urgent: goes ahead of the queue |

Press **«Αποστολή»** (Send). You will see one of:

- «Στάλθηκε στο MCR ως `<name>`.mxf» — queued under that file name.
- «Στάλθηκαν N σύνδεσμοι στο MCR.» — several links queued.
- «Ο σύνδεσμος υπάρχει ήδη στην ουρά.» — that link is already in the queue.

### 3.3 Following your submissions

**«Οι αποστολές μου»** (My submissions) lists what you sent and its state.
Press **«Ανανέωση»** to refresh. While a submission is still waiting
(«Σε αναμονή»), you can withdraw it with **«Ακύρωση»** (Cancel); for 8 seconds
**«Αναίρεση»** (Undo) keeps it.

«Ειδοποίησέ με όταν ολοκληρωθεί ένα βίντεο μου» (notify me when a video of
mine is done) saves your preference. Sending the notification email is not
switched on yet at every station; ask MCR.

---

## 4. For MCR operators: the desk (`/mcr`)

### 4.1 The header

- **The health pill** — «Όλα καλά» (all fine) in green, or «N προβλήματα» in
  amber or red. Press it for every line of the system's health: what is wrong
  and what to do, the free space of the Dalet watchfolder and of the temporary
  folder, and the queue («N σε αναμονή · N σε εξέλιξη»). Escape or a click
  elsewhere closes it.
- **«🔔 Ήχος»** — the sound's settings: on or off, volume, which sounds,
  the reminder (§4.7).
- **«+ Προσθήκη συνδέσμου»** — at the end of the tab row: opens a panel on
  the right to queue a link by hand (§4.6).
- **«Βοήθεια»** — beside it, a small menu: **«Προδιαγραφές εκπομπής»** (the
  broadcast specification every delivered file meets) and
  **«Συντομεύσεις πληκτρολογίου»** (the keyboard shortcuts).
- **«Διαχείριση»** — the admin page (administrators only).
- **«Καθυστερούν»** — a strip above the tabs, only while some video is late
  (§4.9).

### 4.2 The tabs

| Tab | English | What is in it |
|---|---|---|
| **«Εισερχόμενα»** | Inbox (opens first) | every mail and manually added link, newest first |
| **«Σε εξέλιξη»** | In progress | jobs waiting or being processed |
| **«Χρειάζονται έλεγχο»** | Need checking | jobs that stopped and need a person, plus videos waiting for approval |
| **«Ολοκληρωμένα»** | Completed | delivered jobs |

The number on each tab is how many items are in it. The roster of
journalists (names, addresses, aliases, groups) is kept on the administration
page («Διαχείριση»), not on the desk.

**Keyboard.** **J** / **K** move to the next / previous card of the list on
screen (the mails, the cards of «Χρειάζονται έλεγχο», the videos of
«Σε εξέλιξη»); once a card has the focus, the arrow keys do the same.
**Enter** opens the card («Περισσότερα», or a held video's history),
**R** is «Δοκιμή ξανά», **A** is «Έγκριση → Dalet» (it still asks first),
**/** jumps to the tab's search, **?** shows these shortcuts and **Esc**
closes a window or a field. Nothing happens while you type in a field.

### 4.3 The «Εισερχόμενα» tab (the mails)

Filters along the top:

| Filter | Shows |
|---|---|
| «Όλα» | everything |
| «Σε εξέλιξη» | mails whose videos are still being processed |
| «Θέλουν προσοχή» | mails with something that needs you |
| «Ολοκληρώθηκαν» | mails whose videos were all delivered |
| «Χωρίς βίντεο» | mails where nothing was found to download |

Labels you may see on a mail:

- «Χωρίς δημοσιογράφο» — the sender or name did not match anyone in the
  roster; files are named with a placeholder. Ask the administrator to add
  the person or an alias.
- **«Να το θυμάμαι για αυτή τη διεύθυνση»** — on an open mail whose
  journalist you set, when the sender's address is not on the roster yet. It
  asks first; after that, mails from that address go to that journalist by
  themselves. If the address belongs to another journalist, the desk says so
  and only the administrator can change it.
- «Δεν βρέθηκε βίντεο» — no video found in the mail.
- «Μόνο φωτογραφίες· τίποτα για λήψη» — only photos, nothing to download.
- «Δεν διαβάστηκε» — the mail could not be read; open it to have it read again.

A video's state has the same word, colour and mark everywhere: the desk's
badges, the small tags beside the links in a mail's text, the summary line
of a mail and the journalist's `/user` page. The key is shown above the
mail text.

| Mark | Colour | State |
|---|---|---|
| ✓ | green | «Παραδόθηκε» (also «Παραδόθηκε χειροκίνητα») |
| ↻ | blue | «Σε εξέλιξη» |
| ◷ | grey | «Σε αναμονή» |
| ◉ | violet | «Για έγκριση» |
| ! | amber | «Χρειάζεται έλεγχο», «Χειροκίνητη λήψη» |
| ✕ | red | «Απέτυχε» |
| ⊘ | grey, dashed outline | «Ακυρώθηκε» |

**Opening a mail** shows the mail text and, under
«Βίντεο από αυτό το email · N», one card per video. Below that:

- **Offered videos** — other videos found on the same page that were *not*
  taken automatically (for example a site's promo clip or a long show).
  **«Λήψη ως …»** downloads one under the name shown.
- **«Σύνδεσμοι που δεν κατέβηκαν · N»** — links that were not downloaded,
  each with the reason. **«Λήψη και αυτού ως N»** downloads one anyway.
- **«Να διαβαστεί ξανά»** — read the mail again from scratch (after you fixed
  the roster, for example).
- **«Τεχνικές λεπτομέρειες»** — the technical details, for support.

### 4.4 A job card

Each video has a card with its file name, its state and the step it is on:

> «Βήμα 4 από 7 · Μετατροπή στη μορφή εκπομπής»
> (step 4 of 7 · converting to the broadcast format)

The seven steps: finding the video on the page → downloading → checking the
download → converting → wrapping in MXF → checking the broadcast
specification → delivering to the Dalet watchfolder. Before them the video
waits in the queue («Σε αναμονή»); after them it is archived
(«Αρχειοθέτηση») and done.

While a video waits you may see:

- «Σε αναμονή · Nο στη σειρά» — Nth in the queue.
- «Νέα προσπάθεια σε …» — an automatic retry in so many seconds or minutes;
  on the «Εισερχόμενα» tab followed by «(έγιναν N από M)», tries made so far out of
  the most it will make (§4.10).
- «Περιμένει τον φάκελο παράδοσης» — the video is ready and waits for the
  Dalet watchfolder to answer (§4.10).

While it is processed, «· σε αυτό το βήμα N′» says how many minutes it has
been on this step; after 20 minutes that turns amber, with the tooltip
«Ασυνήθιστα πολλή ώρα στο ίδιο βήμα» (unusually long on this step).

The desk refreshes by itself every few seconds. In «Σε εξέλιξη»,
«Χρειάζονται έλεγχο» and «Ολοκληρωμένα» a card is redrawn only when what it
shows has changed, so a link you are typing into a card's field is kept, with
the cursor where it was. Enter in that field does what the button beside it
does.

**States**

| On the card | Meaning |
|---|---|
| «Σε αναμονή» | waiting in the queue |
| «Σε εξέλιξη» | being processed |
| «Παραδόθηκε» | delivered to Dalet |
| «Παραδόθηκε χειροκίνητα» | you marked it as put into Dalet by hand |
| «Χρειάζεται έλεγχο» | stopped; the card says why |
| «Για έγκριση» | a repaired video waiting for your approval |
| «Χειροκίνητη λήψη» | must be downloaded by hand |
| «Απέτυχε» | failed after its retries |
| «Ακυρώθηκε» | cancelled |

**Buttons** (only the ones that make sense for the job's state appear)

| Button | What it does |
|---|---|
| «Δοκιμή ξανά» | try again: back into the queue |
| «Νέα λήψη» | download again; if the file is already in Dalet the new one is `…_2.mxf` (nothing is overwritten) |
| «Μετονομασία…» | opens a field on the card: type the keyword (Greek or Latin) and it shows the file name it will get; Enter saves, Escape closes |
| «Ακύρωση» | cancel at once: it will not be sent to Dalet. «Αναίρεση» for 8 seconds puts it back in the queue |
| «Το έβαλα στο Dalet» | you put the video into Dalet yourself; the job is closed as done (asks first) |
| «Αφαίρεση» | remove the card from the lists (a delivered file is never touched); «Αναίρεση» for 8 seconds brings it back |
| «Χρήση αυτού του συνδέσμου» («Χρήση» after «Άλλος σύνδεσμος…» on the «Εισερχόμενα» tab) | use a link you pasted instead of the one found |
| «Ιστορικό» | on a card in «Χρειάζονται έλεγχο»: the job's history, step by step, including «Αυτόματη επανάληψη» (§4.10). On the «Εισερχόμενα» tab the history shows under a selected card. |
| «Τώρα» | in the «Καθυστερούν» strip: try now instead of waiting for the next retry (§4.9) |

What can be taken back happens at once, with **«Αναίρεση»** (Undo) at the
bottom left for 8 seconds: «Αφαίρεση», «Ακύρωση», «Απόκρυψη παλαιότερων από 3
ημέρες» and «Αφαίρεση παραδοθέντων από τη λίστα». What sends a video to Dalet
or closes a job for good asks first, in a window on the page: «Έγκριση → Dalet»,
«Το έβαλα στο Dalet», «Λάθος βίντεο», «Επόμενη επιλογή» and «Νέα λήψη». The
window starts on «Άκυρο»; Escape also closes it.

**The «Χρειάζονται έλεγχο» tab is a work list.** It holds only what waits for
you, grouped by what you have to do, not by date:

| Group | What you do first |
|---|---|
| «Για έγκριση» | watch the video and approve it (§4.5) |
| «Χρειάζεται σύνδεση (cookies)» | paste another link for the same video; if it happens often with that site, ask the administrator for cookies |
| «Δεν βρέθηκε βίντεο» | paste the link of the video itself, or remove the card |
| «Χειροκίνητη λήψη» | download the file by hand, put it into Dalet and press «Το έβαλα στο Dalet» |
| «Άλλο» | usually «Δοκιμή ξανά»; the card says what to do |

Each card says in one sentence why the video waits, and shows one main
action. Everything else («Δοκιμή ξανά», «Το έβαλα στο Dalet», «Μετονομασία…»,
«Αφαίρεση», «Ιστορικό», another link) is under **«Περισσότερα»**. The error
code and the tool's own message are under **«Τεχνικές λεπτομέρειες»**, for
support. When a delivery was interrupted (`DELIVERY_UNCONFIRMED`) the main
action is «Το έβαλα στο Dalet»: check Dalet first, so the video does not go
in twice. A video the system will try again by itself is not in this list;
it is on «Σε εξέλιξη».

### 4.5 Approvals

When the normal route fails on a news page, the system may find the video by
another way (the "repair" route). **Such a video is not delivered on its
own** (one exception, below): it appears under «Για έγκριση» in the «Χρειάζονται έλεγχο» tab.

- **«Έγκριση → Dalet»** — it is the right video: deliver it.
- **«Απόρριψη…»** — it is the wrong one: give a short reason and confirm with
  **«Απόρριψη»**. The reason teaches the system what not to pick again.

**A recipe MCR has approved five times on a site (and never rejected there) may
deliver on its own.** Its card is then not under «Για έγκριση» but with the
delivered videos, marked «Αυτόματη παράδοση — έμπιστη συνταγή (N εγκρίσεις)».
If that video was the wrong one, press **«Λάθος βίντεο»** on the card: the
recipe loses its trust on that site at once and its next videos wait for you
again. (Rejecting any video of that recipe on that site, with «Λάθος βίντεο»,
«Δεν είναι αξιοποιήσιμο» or «Άλλο», ends its trust too; «Επόμενη επιλογή» is
always held for you.) A new download of the job removes the button, and its video then waits for your approval again. **The file is already in Dalet: remove it from there yourself**; the
button never touches it. Pressing it twice does nothing more.

### 4.6 Adding a link by hand

**«+ Προσθήκη συνδέσμου»** at the end of the tab row opens a panel on the
right, beside the list you are working on: the link, the journalist's
surname, the keyword, the story number, and, for an article with several
videos, «Από άρθρο: μόνο τα πρώτα … βίντεο» (only the first N videos). Tick
**«Επείγον»** to put it first, then **«Προσθήκη στην ουρά»**: the panel closes
and «Σε εξέλιξη» opens. Esc or «✕» closes the panel without adding.

### 4.7 Sounds and pop-ups

When every video of a mail is finished, the desk plays a short chime
(rising: all delivered; falling: finished, but something needs you) and shows
a pop-up «✔ Email «…»» or «⚠ …». Click anywhere on the page once after
opening it, or the browser keeps the sound muted. The desk also warns when the
ingest service stops answering («Η υπηρεσία λήψης δεν απαντά») and when it is
back.

**«🔔 Ήχος»** in the header opens «Ρυθμίσεις ήχου»: sound on or off
(«Ήχος ανοιχτός»), the volume («Ένταση»), and a switch with a «Δοκιμή»
button for each of the four sounds: all the videos of a mail delivered
(rising); something needs you, a review or a late video (falling); a problem
with the system's health or the service (three low notes); a video waiting
for «Έγκριση → Dalet» (two notes, up). **«Υπενθύμιση»** plays again every N
minutes (10 unless you change it, 1 to 120) while a video has waited for you
longer than N minutes, and says so in a message; a video you hid from the
review list does not count. The settings belong to this computer and browser
only: a wall screen can stay quiet while the operator's screen plays. The
pop-up messages always appear, with or without sound.

A message about something that went wrong (red) stays until you close it with
«✕»; at most three are shown, the oldest going first. Other messages leave by
themselves after a few seconds.

A health problem is told in three parts, in the banner above the tabs and in
the health pill's panel: what happened, what it means for the videos
(«Για τα βίντεο: …») and what to do («Τι κάνετε: …»), with who does it:
**MCR** or **Διαχειριστής** (the administrator). A site that stopped giving
video is named in a sentence, for example «Ο ιστότοπος example.gr δεν δίνει
βίντεο από τις 14:21 (4 αποτυχίες στη σειρά).» The system's own technical text
is only in the panel, under «Τεχνικές λεπτομέρειες», for support.

### 4.8 Keeping the lists tidy

- «Αφαίρεση παραδοθέντων από τη λίστα» (Completed tab) clears delivered jobs
  from view.
- In «Χρειάζονται έλεγχο», filter by journalist, or
  «Απόκρυψη παλαιότερων από 3 ημέρες» (hide older than 3 days).

### 4.9 Late videos («Καθυστερούν»)

Above the tabs, on every tab, the strip **«Καθυστερούν»** (late) lists the
videos that are still waiting or being processed 10 minutes after they
arrived (5 minutes for an urgent one), counted from your last «Δοκιμή ξανά» if
you pressed it. The administrator can change those minutes. It shows the five
oldest, then «και άλλα N» (and N more), and it disappears when nothing is
late.

Each row has the file name, «Επείγον» for an urgent video, the reason, and
«πριν από N′» (how many minutes ago it arrived). The reasons:

| Reason | Meaning |
|---|---|
| «Περιμένει τον φάκελο παράδοσης» | the video is ready, but the Dalet watchfolder does not answer; it is delivered by itself when it does (§4.10) |
| «Νέα προσπάθεια στις HH:MM · …» | an automatic retry at that time, followed by the reason in Greek |
| «Ζωντανή μετάδοση· επόμενος έλεγχος HH:MM» | the link is still a live broadcast; it is checked again at that time. Listed whatever its age. |
| «Σε εξέλιξη · στο τρέχον βήμα εδώ και N′» | being processed, N minutes on its current step |
| «Σε αναμονή στη σειρά» | waiting for its turn in the queue |

**«Τώρα»** (now), on a retry or live-broadcast row, makes the system try at
once instead of waiting for the time shown: use it when you know the cause is
gone. It is not offered for a video waiting for the watchfolder. It counts as
your action on the job (§4.10, second chance) and does not restart the 2-hour
limit.

When a video becomes late, the desk plays the short falling chime once and
shows «Ένα βίντεο καθυστερεί» or «N βίντεο καθυστερούν». A late video does not
need you yet: the system is still trying, and it is not counted among the
videos that need checking.

### 4.10 What the system does by itself

**Retries.** When a video fails for a reason that usually passes, the job
tries again by itself. How often depends on the reason:

| Reason | Tries again after |
|---|---|
| a network problem or a time-out | 30 s, 2, 5 and 10 minutes |
| the source refused (HTTP 403) | 30 s and 2 minutes |
| the platform limits downloads for now (YouTube's "confirm you're not a bot", "try again later") | 2, 5, 10 and 15 minutes |
| still a live broadcast, or a premiere not started yet | checked 10, 30, 60 and 120 minutes after it arrived |
| the disk is full | 10 minutes, three times |

Nothing older than 2 hours, counted from when it arrived or from your last
«Δοκιμή ξανά», is retried automatically. When the retries end, the job moves
to «Χρειάζονται έλεγχο» and its card says so, for example «Δεν τα κατάφερε
μετά από 5 προσπάθειες. Πατήστε «Δοκιμή ξανά» ή επικολλήστε άλλον
σύνδεσμο.», or, for a broadcast that stayed live, «Η μετάδοση ήταν ακόμη
ζωντανή (ή δεν είχε ξεκινήσει) μετά από 2 ώρες ελέγχων. …».

**One platform at a time.** Links on the same video platform (YouTube, X,
Facebook, Instagram, TikTok, Vimeo, Dailymotion, Streamable) start one after
the other, a few seconds apart, so that a mail with many YouTube links does not
set off the platform's bot check. Such a video may take a little longer to
start: that is normal. If the platform refuses anyway, nothing new starts there
for 2 minutes and the card says «Ο ιστότοπος περιορίζει προσωρινά τις λήψεις.
Γίνεται αυτόματη επανάληψη.» If the retries run out, the card adds advice
about the site's cookies: tell the administrator.

**The watchfolder is down.** A finished video that cannot be written into the
Dalet watchfolder waits on the ingest server, with «Περιμένει τον φάκελο
παράδοσης» on its card. It is delivered by itself, once, when the watchfolder
answers again, however long that takes: there is nothing to do. It cannot be
renamed while it waits. The desk also warns «Πρόβλημα: Watchfolder — …».

**"Check Dalet first".** If a delivery was interrupted (a restart, or an
error during the copy) and the system cannot confirm that the file arrived,
the job goes to «Χρειάζονται έλεγχο» with the code `DELIVERY_UNCONFIRMED` and
the text «Η παράδοση διακόπηκε και δεν είναι βέβαιο αν το αρχείο έφτασε. …».
Look in Dalet first: if the video is there, press «Το έβαλα στο Dalet»; if it
is not, press «Δοκιμή ξανά». A retry without looking could put a second copy
(`…_2.mxf`) on air.

**Second chance.** A job waiting in «Χρειάζονται έλεγχο» goes back to the
queue by itself, once for each cause, when what it failed on has been fixed:

- the administrator added or tested cookies for its site (a job that needed a
  login, or that the platform kept limiting);
- a new yt-dlp (or Deno) was installed and passed its check (a job where the
  video was not found or could not be fetched);
- a news site that had stopped giving video works again (that site's jobs
  where the video was not found).

Its history then says «Αυτόματη επανάληψη: …» and why, for example
«Αυτόματη επανάληψη: νέα cookies για x.com». This happens only within 2 hours
of its arrival (or of your last «Δοκιμή ξανά»), at most three times for one
job, and never to a job a person has acted on: once anyone presses «Δοκιμή
ξανά», «Τώρα», «Μετονομασία…», «Ακύρωση», «Νέα λήψη», «Το έβαλα στο Dalet»,
rejects it, marks «Λάθος βίντεο» or queues one of its offered videos, the job
waits for a person. It is also left alone when the same link is already in
the queue or was delivered meanwhile, and when its video may already be in
Dalet.

---

## 5. Common reasons a job stops, and what to do

The card always says why in Greek. The usual ones:

| Reason | What to do |
|---|---|
| Login required (e.g. X, Instagram, Facebook) | the site needs the station's cookies: tell the administrator (Admin → Cookies). Once they are added, a job younger than 2 hours that nobody has touched goes back to the queue by itself (§4.10); otherwise «Δοκιμή ξανά». |
| No video found on the page | open the link yourself; if there is a video, paste its direct link with «Χρήση αυτού του συνδέσμου» or download it by hand and mark «Το έβαλα στο Dalet». |
| Live stream | checked again by itself for 2 hours (§4.10). If it stays live, ask the journalist for the clip, or «Δοκιμή ξανά» once the recording is up. |
| Video removed / private | ask the journalist for another source. |
| Network or a time-out | retries by itself (§4.10). When the card says it stopped trying, «Δοκιμή ξανά» or paste another link. |
| «Ο ιστότοπος περιορίζει προσωρινά τις λήψεις» (YouTube "confirm you're not a bot", "try again later") | retries by itself for about half an hour. If it stops, «Δοκιμή ξανά» later; if it keeps happening, tell the administrator (cookies for that site). |
| «Περιμένει τον φάκελο παράδοσης» | nothing to do: it is delivered when the watchfolder answers (§4.10). If it lasts, tell the administrator. |
| «Η παράδοση διακόπηκε και δεν είναι βέβαιο αν το αρχείο έφτασε…» | check Dalet first: there, «Το έβαλα στο Dalet»; not there, «Δοκιμή ξανά» (§4.10). |
| Failed the broadcast check | the file was *not* delivered. Retry once; if it fails again, report it with the «Τεχνικές λεπτομέρειες». |
| File locker (WeTransfer, Drive, Dropbox…) | not downloaded automatically yet: download by hand, then «Το έβαλα στο Dalet». |

Nothing is ever overwritten in the Dalet watchfolder, and a file appears there
only when it is complete.

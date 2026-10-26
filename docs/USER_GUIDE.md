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
(«Σε αναμονή»), you can withdraw it with **«Ακύρωση»** (Cancel).

«Ειδοποίησέ με όταν ολοκληρωθεί ένα βίντεο μου» (notify me when a video of
mine is done) saves your preference. Sending the notification email is not
switched on yet at every station; ask MCR.

---

## 4. For MCR operators: the desk (`/mcr`)

### 4.1 The header

- **«Ουρά: …»** — the queue's state at a glance.
- **«Προδιαγραφές εκπομπής»** — the broadcast specification every delivered
  file meets.
- **«Διαχείριση»** — the admin page (administrators only).

### 4.2 The tabs

| Tab | English | What is in it |
|---|---|---|
| **Email** | Email (opens first) | every mail and manually added link, newest first |
| **«Σε εξέλιξη»** | In progress | jobs waiting or being processed |
| **«Χρειάζονται έλεγχο»** | Need checking | jobs that stopped and need a person, plus videos waiting for approval |
| **«Ολοκληρωμένα»** | Completed | delivered jobs |
| **«Δημοσιογράφοι»** | Journalists | the roster: names, addresses, aliases, groups |
| **«Προσθήκη συνδέσμου»** | Add link | queue a link by hand |

The number on each tab is how many items are in it.

### 4.3 The Email tab

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
  roster; files are named with a placeholder. Add the person or an alias in
  «Δημοσιογράφοι».
- «Δεν βρέθηκε βίντεο» — no video found in the mail.
- «Μόνο φωτογραφίες· τίποτα για λήψη» — only photos, nothing to download.
- «Δεν διαβάστηκε» — the mail could not be read; open it to have it read again.

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

> «Βήμα 5 από 10 · Μετατροπή στη μορφή εκπομπής»
> (step 5 of 10 · converting to the broadcast format)

The ten steps: waiting → finding the video on the page → downloading →
checking the download → converting → wrapping in MXF → checking the broadcast
specification → delivering to the Dalet watchfolder → archiving → done.

You may also see «Νέα προσπάθεια σε …» (retrying in …), «Nο στη σειρά» (Nth in
the queue) and «Ασυνήθιστα πολλή ώρα στο ίδιο βήμα» (unusually long on this
step).

**States**

| On the card | Meaning |
|---|---|
| «Σε αναμονή» | waiting in the queue |
| «Σε επεξεργασία» | being processed |
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
| «Μετονομασία…» | change the keyword, and so the file name |
| «Ακύρωση» | cancel: it will not be sent to Dalet |
| «Το έβαλα στο Dalet» | you put the video into Dalet yourself; the job is closed as done |
| «Αφαίρεση» | remove the card from the lists (a delivered file is never touched) |
| «Χρήση» | use a link you pasted instead of the one found |

### 4.5 Approvals

When the normal route fails on a news page, the system may find the video by
another way (the "repair" route). **Such a video is never delivered on its
own**: it appears under «Για έγκριση» in the «Χρειάζονται έλεγχο» tab.

- **«Έγκριση → Dalet»** — it is the right video: deliver it.
- **«Απόρριψη…»** — it is the wrong one: give a short reason and confirm with
  **«Απόρριψη»**. The reason teaches the system what not to pick again.

### 4.6 Adding a link by hand

**«Προσθήκη συνδέσμου»**: the link, the journalist's surname, the keyword,
the story number, and, for an article with several videos,
«Από άρθρο: μόνο τα πρώτα … βίντεο» (only the first N videos). Tick
**«Επείγον»** to put it first, then **«Προσθήκη στην ουρά»**.

### 4.7 Sounds and pop-ups

When every video of a mail is finished, the desk plays a short chime
(rising: all delivered; falling: finished, but something needs you) and shows
a pop-up «✔ Email «…»» or «⚠ …». Click anywhere on the page once after
opening it, or the browser keeps the sound muted. The desk also warns when the
ingest service stops answering («Η υπηρεσία λήψης δεν απαντά») and when it is
back.

### 4.8 Keeping the lists tidy

- «Αφαίρεση παραδοθέντων από τη λίστα» (Completed tab) clears delivered jobs
  from view.
- In «Χρειάζονται έλεγχο», filter by journalist, or
  «Απόκρυψη παλαιότερων από 3 ημέρες» (hide older than 3 days).

---

## 5. Common reasons a job stops, and what to do

The card always says why in Greek. The usual ones:

| Reason | What to do |
|---|---|
| Login required (e.g. YouTube "confirm you're not a bot", X, Instagram) | the site needs the station's cookies: tell the administrator (Admin → Cookies). Then «Δοκιμή ξανά». |
| No video found on the page | open the link yourself; if there is a video, paste its direct link with «Χρήση» or download it by hand and mark «Το έβαλα στο Dalet». |
| Live stream | live broadcasts are not downloaded; ask the journalist for the clip. |
| Video removed / private | ask the journalist for another source. |
| Network or "try again later" | usually retries by itself; otherwise «Δοκιμή ξανά» in a few minutes. |
| Failed the broadcast check | the file was *not* delivered. Retry once; if it fails again, report it with the «Τεχνικές λεπτομέρειες». |
| File locker (WeTransfer, Drive, Dropbox…) | not downloaded automatically yet: download by hand, then «Το έβαλα στο Dalet». |

Nothing is ever overwritten in the Dalet watchfolder, and a file appears there
only when it is complete.

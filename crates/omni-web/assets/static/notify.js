/* "This email is done" on the MCR desk (plan P5.4).
 *
 * Every few seconds, on whichever tab is open, the desk asks which recent
 * emails have all their videos finished (/api/mails/settled). An email that
 * was still in progress at the last answer and is finished now gets:
 *
 *   - a short jingle: rising for "all delivered", a softer falling pair
 *     for "finished, but something needs MCR";
 *   - a toast on the desk, and a browser notification when the browser
 *     allows one (it does only on https or on this machine itself);
 *   - the tab title marked until the desk is looked at again.
 *
 * The sound is synthesised with Web Audio: no file to ship or cache, and
 * nothing loaded from anywhere. Browsers start audio only after someone has
 * clicked the page, so the first click anywhere unlocks it; until then the
 * toast and title still say what happened.
 *
 * Only a change seen happening is announced: an email already finished
 * when the desk is opened stays quiet.
 */

import { api, el, toast } from '/static/app.js?v=6';

const POLL_MS = 8000;
const PREF_KEY = 'omni.mcr.chime';
const BASE_TITLE = document.title;

let ctx = null;
let previous = null; // Map id -> settled, from the last answer
let unseen = 0;
let button = null;
let onOpen = () => {};

function soundOn() {
  try {
    return localStorage.getItem(PREF_KEY) !== 'off';
  } catch {
    return true;
  }
}

function setSoundOn(on) {
  try {
    localStorage.setItem(PREF_KEY, on ? 'on' : 'off');
  } catch { /* private mode: the choice lasts for this page only */ }
}

/** The AudioContext, created and resumed inside a user gesture. */
function unlock() {
  const Ctx = window.AudioContext || window.webkitAudioContext;
  if (!Ctx) return;
  if (!ctx) ctx = new Ctx();
  if (ctx.state === 'suspended') ctx.resume().catch(() => {});
  paintButton();
}

/** One soft bell-like note: a sine with a quiet octave, quick attack, long tail. */
function note(freq, at, length, gain) {
  for (const [mult, level] of [[1, 1], [2, 0.18]]) {
    const osc = ctx.createOscillator();
    const amp = ctx.createGain();
    osc.type = 'sine';
    osc.frequency.value = freq * mult;
    amp.gain.setValueAtTime(0.0001, at);
    amp.gain.exponentialRampToValueAtTime(gain * level, at + 0.015);
    amp.gain.exponentialRampToValueAtTime(0.0001, at + length);
    osc.connect(amp).connect(ctx.destination);
    osc.start(at);
    osc.stop(at + length + 0.05);
  }
}

/** `ok`: C5 E5 G5 C6, rising. Otherwise G5 then E5, quieter. */
function jingle(ok) {
  if (!soundOn() || !ctx || ctx.state !== 'running') return false;
  const t = ctx.currentTime + 0.03;
  if (ok) {
    [523.25, 659.25, 783.99].forEach((f, i) => note(f, t + i * 0.12, 0.5, 0.22));
    note(1046.5, t + 0.36, 1.1, 0.25);
  } else {
    note(783.99, t, 0.45, 0.2);
    note(659.25, t + 0.22, 0.9, 0.2);
  }
  return true;
}

/** Three short low notes, A4 A4 E4: unlike either jingle, and not a celebration. */
function alarm() {
  if (!soundOn() || !ctx || ctx.state !== 'running') return false;
  const t = ctx.currentTime + 0.03;
  [[440, 0], [440, 0.2], [329.63, 0.4]].forEach(([f, d]) => note(f, t + d, 0.3, 0.24));
  return true;
}

/** Check name -> what the desk calls it and what MCR should do. */
export const CHECK_INFO = {
  mail: {
    name: 'Email (εισερχόμενα)',
    action: 'Τα νέα email δεν διαβάζονται· θέματα που στέλνονται τώρα δεν θα εμφανιστούν. Ενημερώστε τον διαχειριστή.',
  },
  encoder: {
    name: 'Κωδικοποιητής',
    action: 'Τα βίντεο δεν μετατρέπονται σε μορφή εκπομπής. Ενημερώστε τον διαχειριστή· χρειάζεται επανεκκίνηση της υπηρεσίας.',
  },
  tools: {
    name: 'Εργαλεία',
    action: 'Λείπει ή χάλασε εργαλείο λήψης/μετατροπής. Ενημερώστε τον διαχειριστή.',
  },
  watchfolder: {
    name: 'Watchfolder',
    action: 'Δεν γράφεται ο φάκελος του Dalet· τα έτοιμα βίντεο δεν παραδίδονται. Ελέγξτε τη σύνδεση με τον server του Dalet και ενημερώστε τον διαχειριστή.',
  },
  disk: {
    name: 'Δίσκος',
    action: 'Ο δίσκος γεμίζει· κάτω από 5 GB ελεύθερα οι λήψεις σταματούν. Ενημερώστε τον διαχειριστή.',
  },
  browser: {
    name: 'Πρόγραμμα περιήγησης',
    action: 'Ο ενσωματωμένος browser δεν ξεκινά· βίντεο μέσα σε άρθρα ειδησεογραφικών σελίδων μπορεί να αποτύχουν.',
  },
  selfcheck: {
    name: 'Αυτοέλεγχος',
    action: 'Κάποιοι δοκιμαστικοί σύνδεσμοι δεν κατεβαίνουν· ίσως ένας ιστότοπος άλλαξε. Ενημερώστε τον διαχειριστή αν επιμένει.',
  },
  deno: {
    name: 'YouTube (Deno)',
    action: 'Τα βίντεο από YouTube μπορεί να αποτυγχάνουν. Ενημερώστε τον διαχειριστή.',
  },
  queue: {
    name: 'Ουρά',
    action: 'Βίντεο περιμένουν ή τρέχουν ασυνήθιστα πολύ. Ελέγξτε την καρτέλα «Σε εξέλιξη».',
  },
  accounts: { name: 'Λογαριασμοί', action: '' },
};

const SEVERITY = { ok: 0, degraded: 1, down: 2 };
const SILENT_CHECKS = ['llm', 'accounts'];

function sev(check) {
  return check ? (SEVERITY[check.state] ?? 0) : 0;
}

function needCount(q) {
  return (q.review || 0) + (q.manual || 0) + (q.failed || 0);
}

/**
 * Pure: what changed between two /api/system/status payloads. `prev` null
 * (first load) reports nothing, so what was already there is not alarmed.
 */
export function diffStatus(prev, next) {
  const out = { needsPerson: 0, wentBad: [], recovered: [] };
  if (!prev || !next) return out;
  // The server sends `queue: null` when it could not count; comparing with
  // that would announce the whole backlog as new on the next good answer.
  if (prev.queue && next.queue) {
    out.needsPerson = Math.max(0, needCount(next.queue) - needCount(prev.queue));
  }
  const before = prev.checks || {};
  for (const [name, check] of Object.entries(next.checks || {})) {
    if (SILENT_CHECKS.includes(name)) continue;
    const was = sev(before[name]);
    const now = sev(check);
    if (now > was) out.wentBad.push({ name, state: check.state, detail: check.detail });
    else if (now === 0 && was > 0) out.recovered.push(name);
  }
  return out;
}

let lastStatus = null;
// The review alert and the settled-mail announcement often report the same
// failed video a few seconds apart, in either order: whichever comes second
// within this window keeps its toast but makes no sound and no title count.
const SAME_NEWS_MS = 15000;
let lastBadNews = 0;
let unseenBad = false;
let onOpenReview = () => {};

function plainNotification(title, tag, onClick = () => onOpenReview()) {
  if (!('Notification' in window) || Notification.permission !== 'granted') return;
  try {
    const n = new Notification(title, { tag, silent: true });
    n.onclick = () => {
      window.focus();
      onClick();
      n.close();
    };
  } catch { /* some browsers allow Notification only from a service worker */ }
}

/** Called with every successful status payload: sounds and says what got worse. */
export function watchStatus(data) {
  const diff = diffStatus(lastStatus, data);
  lastStatus = data;
  if (diff.needsPerson > 0) {
    const n = diff.needsPerson;
    const text = n === 1 ? '1 βίντεο χρειάζεται έλεγχο' : `${n} βίντεο χρειάζονται έλεγχο`;
    const covered = Date.now() - lastBadNews <= SAME_NEWS_MS;
    lastBadNews = Date.now();
    if (!covered) jingle(false);
    toast(text, 'bad');
    plainNotification(text, 'omni-review');
    if (!covered) count(n, true);
  }
  if (diff.wentBad.length) alarm();
  for (const bad of diff.wentBad) {
    const info = CHECK_INFO[bad.name];
    const text = `Πρόβλημα: ${info ? info.name : bad.name}${info?.action ? ` — ${info.action}` : ''}`;
    toast(text, 'bad');
    plainNotification(text, `omni-health-${bad.name}`);
  }
  for (const name of diff.recovered) {
    toast(`Αποκαταστάθηκε: ${CHECK_INFO[name]?.name || name}`, 'ok');
  }
}

/** The service stopped answering: sound once and say so outside the tab too. */
export function serviceDown() {
  alarm();
  plainNotification('Η υπηρεσία λήψης δεν απαντά', 'omni-offline', () => {});
}

/** The service answers again (same tag, so it replaces the outage notification). */
export function serviceBack() {
  plainNotification('Η υπηρεσία λήψης απαντά ξανά', 'omni-offline', () => {});
}

function describe(s) {
  return s.ok
    ? `${s.delivered}/${s.total} βίντεο παραδόθηκαν`
    : `${s.delivered}/${s.total} παραδόθηκαν · τα υπόλοιπα θέλουν έλεγχο`;
}

function systemNotification(done) {
  if (!('Notification' in window) || Notification.permission !== 'granted') return;
  const first = done[0];
  const allOk = done.every((s) => s.ok);
  const title = done.length === 1
    ? (first.ok ? 'Ολοκληρώθηκε email' : 'Ολοκληρώθηκε email, θέλει έλεγχο')
    : `Ολοκληρώθηκαν ${done.length} email${allOk ? '' : ' (κάποια θέλουν έλεγχο)'}`;
  const body = done.slice(0, 3).map((s) => `«${s.subject || '(χωρίς θέμα)'}» — ${describe(s)}`).join('\n');
  try {
    const n = new Notification(title, { body, tag: `omni-${first.id}`, silent: true });
    n.onclick = () => {
      window.focus();
      onOpen(first.id);
      n.close();
    };
  } catch { /* some browsers allow Notification only from a service worker */ }
}

/** Count news for the title of a hidden tab; nothing while MCR is looking. */
function count(n, bad) {
  if (!document.hidden) return;
  unseen += n;
  unseenBad = unseenBad || bad;
  markTitle();
}

function markTitle() {
  if (!document.hidden || unseen === 0) return;
  document.title = `${unseenBad ? '⚠' : '✔'} (${unseen}) ${BASE_TITLE}`;
}

function announce(done) {
  const allOk = done.every((s) => s.ok);
  let covered = false;
  if (!allOk) {
    covered = Date.now() - lastBadNews <= SAME_NEWS_MS;
    lastBadNews = Date.now();
  }
  if (!covered) jingle(allOk);
  for (const s of done.slice(0, 3)) {
    toast(`${s.ok ? '✔' : '⚠'} Email «${s.subject || '(χωρίς θέμα)'}»: ${describe(s)}`, s.ok ? 'ok' : 'bad');
  }
  if (done.length > 3) toast(`Και ${done.length - 3} ακόμη email ολοκληρώθηκαν`, 'ok');
  systemNotification(done);
  if (!covered) count(done.length, !allOk);
}

async function poll() {
  let data;
  try {
    data = await api('/api/mails/settled');
  } catch {
    return; // the status line already says when the server is gone
  }
  const now = new Map((data.entries || []).map((s) => [s.id, s]));
  if (previous) {
    const done = [...now.values()].filter((s) => s.settled && previous.get(s.id) === false);
    if (done.length) announce(done);
  }
  previous = new Map([...now.values()].map((s) => [s.id, s.settled]));
}

function paintButton() {
  if (!button) return;
  const on = soundOn();
  const locked = on && (!ctx || ctx.state !== 'running');
  button.textContent = on ? (locked ? '🔔 Ήχος: πατήστε για ενεργοποίηση' : '🔔 Ήχος: ενεργός') : '🔕 Ήχος: σίγαση';
  button.setAttribute('aria-pressed', String(on));
  button.title = on
    ? 'Μελωδία και ειδοποίηση όταν τελειώνουν όλα τα βίντεο ενός email. Πατήστε για σίγαση.'
    : 'Πατήστε για μελωδία και ειδοποίηση όταν τελειώνουν όλα τα βίντεο ενός email.';
}

/**
 * Start watching. `host` gets the sound toggle; `open(id)` shows an entry
 * (`mail:<Message-ID>` or `manual:<job id>`) when a notification is clicked.
 */
export function initNotify(host, open, openReview) {
  onOpen = open || onOpen;
  onOpenReview = openReview || onOpenReview;
  if (host) {
    button = el('button', {
      class: 'btn',
      type: 'button',
      id: 'chime-btn',
      onClick: () => {
        const turningOn = !soundOn() || !ctx || ctx.state !== 'running';
        setSoundOn(turningOn);
        if (turningOn) {
          unlock();
          // Asked only from this click: browsers ignore an unprompted request.
          if ('Notification' in window && Notification.permission === 'default') {
            Notification.requestPermission().catch(() => {});
          }
          setTimeout(() => jingle(true), 60);
        }
        paintButton();
      },
    });
    host.prepend(button);
  }
  // The first click or key anywhere on the desk lets the browser play sound.
  // Not a click on the toggle itself: unlocking first would make the toggle
  // read "already on" and mute instead of enabling.
  const gesture = (ev) => {
    if (button && ev.target instanceof Node && button.contains(ev.target)) return;
    unlock();
    document.removeEventListener('pointerdown', gesture, true);
    document.removeEventListener('keydown', gesture, true);
  };
  document.addEventListener('pointerdown', gesture, true);
  document.addEventListener('keydown', gesture, true);
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden) {
      unseen = 0;
      unseenBad = false;
      document.title = BASE_TITLE;
    }
  });
  paintButton();
  poll();
  setInterval(poll, POLL_MS);
}

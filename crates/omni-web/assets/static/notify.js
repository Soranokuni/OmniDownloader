/* "This email is done" on the MCR desk (plan P5.4).
 *
 * Every few seconds, on whichever tab is open, the desk asks which recent
 * emails have all their videos finished (/api/mails/settled). An email that
 * was still in progress at the last answer and is finished now gets:
 *
 *   - a short sound: «delivered» when all went out, «person» when
 *     something needs MCR;
 *   - a toast on the desk, and a browser notification when the browser
 *     allows one (it does only on https or on this machine itself);
 *   - the tab title marked until the desk is looked at again.
 *
 * The status payload (watchStatus) adds the other news: a video that needs a
 * person, one held for approval, one running late, a health check that got
 * worse; and, while something has waited for a person longer than the
 * reminder's minutes, the reminder, again every so many minutes (P12.9).
 * The sounds, their switches and the volume are in sound.js.
 *
 * Only a change seen happening is announced: an email already finished
 * when the desk is opened stays quiet.
 *
 * Every word is in text.el.js (P12.7): `notify.*` and `health.*`.
 */

import { api, el } from '/static/app.js?v=6';
import { toast } from '/static/toast.js?v=5';
import { t, tn, has } from '/static/text.js?v=4';
import {
  play, unlock, soundPrefs, soundLocked, onSoundChange, openSoundSettings,
} from '/static/sound.js?v=2';

const POLL_MS = 8000;
const BASE_TITLE = document.title;

let previous = null; // Map id -> settled, from the last answer
let unseen = 0;
let button = null;
let onOpen = () => {};

/**
 * Every health check in plain words (P12.6, P12.7), one row per check name of
 * `omni_core::health::checks`: who acts on it, 'mcr' or 'admin'. The texts
 * are in text.el.js under `health.<name>.`:
 *   name             what the desk calls it
 *   degraded / down  what happened (by how bad it is)
 *   means            what it means for the videos   (`means.down` if it differs)
 *   todo             what to do                     (`todo.down` if it differs)
 * The check's own detail (English, for support) goes under «Τεχνικές
 * λεπτομέρειες» in the header's panel, never in the banner.
 */
export const CHECK_WHO = {
  mail: 'admin',
  llm: 'admin',
  tools: 'admin',
  pinned_tools: 'admin',
  encoder: 'admin',
  watchfolder: 'mcr',
  disk: 'admin',
  browser: 'admin',
  selfcheck: 'admin',
  deno: 'admin',
  queue: 'mcr',
  workers: 'admin',
  accounts: 'admin',
  sites: 'mcr',
  cookies: 'admin',
  lockdown: 'admin',
  archive: 'admin',
  maintenance: 'admin',
  database: 'admin',
};

/** `CHECK_INFO[name].name`: what the desk calls a check, in the current language. */
export const CHECK_INFO = Object.fromEntries(Object.keys(CHECK_WHO).map((name) => [name, {
  get name() { return t(`health.${name}.name`); },
  who: CHECK_WHO[name],
}]));

/** Who does what a line asks, as the badge says it. */
export const WHO = {
  get mcr() { return t('who.mcr'); },
  get admin() { return t('who.admin'); },
};

/** The three texts of a check that is not OK; a check the desk does not know
 *  yet still gets words of its own, never only its English detail. */
export function checkTexts(name, check) {
  const state = check && check.state === 'down' ? 'down' : 'degraded';
  if (!CHECK_WHO[name]) {
    return { name, happened: t('health.unknown.happened'), means: t('health.unknown.means'), todo: t('health.unknown.todo'), who: 'admin' };
  }
  const key = (part) => (has(`health.${name}.${part}.${state}`) ? `health.${name}.${part}.${state}` : `health.${name}.${part}`);
  return {
    name: t(`health.${name}.name`),
    happened: t(`health.${name}.${state}`),
    means: t(key('means')),
    todo: t(key('todo')),
    who: CHECK_WHO[name],
  };
}

/** «14:21», or '' for a missing or unreadable time (never "now"). */
function hhmm(at) {
  const d = at ? new Date(at) : null;
  if (!d || Number.isNaN(d.getTime())) return '';
  return d.toLocaleTimeString(t('locale'), { hour: '2-digit', minute: '2-digit', hour12: false });
}

/** A site that stopped giving video, as one sentence (P12.6), from the status
 *  payload's `sites` row: «Ο ιστότοπος x.gr δεν δίνει βίντεο από τις 14:21 (4 αποτυχίες στη σειρά).» */
export function siteSentence(site) {
  const at = hhmm(site.state_since);
  const n = Number(site.consecutive_fail) || 0;
  return t('site.sentence', {
    domain: site.domain,
    what: t(site.state === 'broken' ? 'site.broken' : 'site.degraded'),
    since: at ? t('site.since', { at }) : '',
    fails: n > 0 ? tn('site.fails', n) : '',
  });
}

/** The checks that are not OK, as `[name, check]`, for the banner and the
 *  header's pill. The LLM is left out: without it the parser still works,
 *  and no video waits on it (its line is in the panel all the same). */
export function healthProblems(checks) {
  return Object.entries(checks || {}).filter(([name, c]) => c && c.state !== 'ok' && name !== 'llm');
}

/** The header's pill (P12.5): «Όλα καλά», or how many problems, in the
 *  colour of the worst. */
export function healthSummary(checks) {
  const problems = healthProblems(checks);
  const count = problems.length;
  if (!count) return { count, tone: 'ok', text: t('health.pill.ok') };
  const tone = problems.some(([, c]) => c.state === 'down') ? 'bad' : 'warn';
  return { count, tone, text: tn('health.pill.problems', count) };
}

const SEVERITY = { ok: 0, degraded: 1, down: 2 };
const SILENT_CHECKS = ['llm', 'accounts', 'sites'];

function sev(check) {
  return check ? (SEVERITY[check.state] ?? 0) : 0;
}

/** Everything that waits for a person, held videos included (the reminder's gate). */
function needCount(q) {
  return personCount(q) + (q.approval || 0);
}

/** Waits for a person to fix or decide: review, file-locker links, failures. */
function personCount(q) {
  return (q.review || 0) + (q.manual || 0) + (q.failed || 0);
}

/**
 * Pure: what changed between two /api/system/status payloads. `prev` null
 * (first load) reports nothing, so what was already there is not alarmed.
 * A video held for approval is its own news (`newlyHeld`, P12.9), with its
 * own sound, not part of `needsPerson`.
 */
export function diffStatus(prev, next) {
  const out = { needsPerson: 0, newlyHeld: 0, newlyLate: 0, wentBad: [], recovered: [] };
  if (!prev || !next) return out;
  // The server sends `queue: null` when it could not count; comparing with
  // that would announce the whole backlog as new on the next good answer.
  if (prev.queue && next.queue) {
    out.needsPerson = Math.max(0, personCount(next.queue) - personCount(prev.queue));
    out.newlyHeld = Math.max(0, (next.queue.approval || 0) - (prev.queue.approval || 0));
    // Late videos (P11.3b) are not "needs a person": they are still being tried.
    // Only a rise is news; a field an older server does not send counts nothing.
    if (typeof prev.queue.late === 'number' && typeof next.queue.late === 'number') {
      out.newlyLate = Math.max(0, next.queue.late - prev.queue.late);
    }
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
    const text = tn('notify.needs', n);
    const covered = Date.now() - lastBadNews <= SAME_NEWS_MS;
    lastBadNews = Date.now();
    if (!covered) play('person');
    toast(text, 'bad');
    plainNotification(text, 'omni-review');
    if (!covered) count(n, true);
  }
  if (diff.newlyHeld > 0) {
    const text = tn('notify.held', diff.newlyHeld);
    play('held');
    toast(text, 'warn');
    plainNotification(text, 'omni-approval');
    count(diff.newlyHeld, true);
  }
  if (diff.newlyLate > 0) {
    // The «person» sound, once; no title count and no review alert.
    play('person');
    toast(tn('notify.late', diff.newlyLate), 'warn');
  }
  if (diff.wentBad.length) play('health');
  for (const bad of diff.wentBad) {
    const ct = checkTexts(bad.name, bad);
    const text = t('notify.problem', { name: ct.name, happened: ct.happened, todo: ct.todo });
    toast(text, 'bad');
    plainNotification(text, `omni-health-${bad.name}`);
  }
  for (const name of diff.recovered) {
    toast(t('notify.recovered', { name: CHECK_INFO[name]?.name || name }), 'ok');
  }
  remind(data);
}

/* ------------------------------------------------------------------ *
 * The reminder (P12.9): while something has waited for a person longer
 * than `remindMins`, the «remind» sound and a message, again every
 * `remindMins`. «Waited» is from the job's last change (updated_at), read
 * from the review and approval lists once a minute at most, and only while
 * the status says something waits. A video hidden from the review list
 * («Απόκρυψη») is not in that list, so it does not remind.
 * ------------------------------------------------------------------ */

const REMIND_CHECK_MS = 60000;
let lastRemind = 0;
let lastRemindCheck = 0;
let reminding = false;

/**
 * Pure: whether the reminder is due. `oldestAt` is the time (ms) the longest
 * waiting job last changed, or null; nothing known is never due.
 */
export function remindDue({ now, oldestAt, lastRemind: last, mins }) {
  if (oldestAt === null || !Number.isFinite(oldestAt)) return false;
  const span = mins * 60000;
  return now - oldestAt >= span && (!last || now - last >= span);
}

/** The time (ms) of the longest wait, from the review and approval lists; null if none. */
async function oldestWaitAt() {
  const lists = await Promise.all([
    api('/api/jobs?view=review&page=1&per_page=100'),
    api('/api/jobs?view=approval&page=1&per_page=100'),
  ]);
  let oldest = null;
  for (const data of lists) {
    for (const job of data.jobs || []) {
      // A missing or unreadable time is skipped, never taken as "now".
      const at = job.updated_at ? Date.parse(job.updated_at) : NaN;
      if (Number.isFinite(at) && (oldest === null || at < oldest)) oldest = at;
    }
  }
  return oldest;
}

async function remind(data) {
  const p = soundPrefs();
  const q = data && data.queue;
  if (!q) return; // could not count: change nothing
  if (needCount(q) === 0) { lastRemind = 0; return; } // nothing waits: the next wait starts afresh
  if (!p.on || !p.remind || reminding) return;
  const now = Date.now();
  if (now - lastRemindCheck < REMIND_CHECK_MS) return;
  lastRemindCheck = now;
  reminding = true;
  try {
    const oldestAt = await oldestWaitAt();
    if (!remindDue({ now: Date.now(), oldestAt, lastRemind, mins: p.remindMins })) return;
    lastRemind = Date.now();
    play('remind');
    toast(tn('notify.remind', p.remindMins), 'warn');
  } catch {
    // The status line and the outage banner say when the server is gone.
  } finally {
    reminding = false;
  }
}

/** The service stopped answering: sound once and say so outside the tab too. */
export function serviceDown() {
  play('health');
  plainNotification(t('service.down'), 'omni-offline', () => {});
}

/** The service answers again (same tag, so it replaces the outage notification). */
export function serviceBack() {
  plainNotification(t('service.back'), 'omni-offline', () => {});
}

function describe(s) {
  return t(s.ok ? 'notify.mail.all' : 'notify.mail.some', { delivered: s.delivered, total: s.total });
}

const subjectOf = (s) => s.subject || t('mail.no_subject');

function systemNotification(done) {
  if (!('Notification' in window) || Notification.permission !== 'granted') return;
  const first = done[0];
  const allOk = done.every((s) => s.ok);
  const title = done.length === 1
    ? t(first.ok ? 'notify.mail.done' : 'notify.mail.done_check')
    : t(allOk ? 'notify.mails.done' : 'notify.mails.done_check', { n: done.length });
  const body = done.slice(0, 3).map((s) => t('notify.mail.line', { subject: subjectOf(s), what: describe(s) })).join('\n');
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
  if (!covered) play(allOk ? 'delivered' : 'person');
  for (const s of done.slice(0, 3)) {
    toast(t('notify.mail.toast', { mark: s.ok ? '✔' : '⚠', subject: subjectOf(s), what: describe(s) }), s.ok ? 'ok' : 'bad');
  }
  if (done.length > 3) toast(tn('notify.mails.more', done.length - 3), 'ok');
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
  const on = soundPrefs().on;
  const locked = soundLocked();
  // Short, so the header stays on one line at 1366 px (P12.5); the tooltip says the rest.
  button.textContent = on ? (locked ? t('sound.locked') : t('sound.on')) : t('sound.off');
  button.title = on ? t(locked ? 'sound.locked.title' : 'sound.on.title') : t('sound.off.title');
}

/**
 * Start watching. `host` gets the sound button, which opens the sound's
 * settings (P12.9); `open(id)` shows an entry (`mail:<Message-ID>` or
 * `manual:<job id>`) when a notification is clicked.
 */
export function initNotify(host, open, openReview) {
  onOpen = open || onOpen;
  onOpenReview = openReview || onOpenReview;
  if (host) {
    button = el('button', {
      class: 'btn',
      type: 'button',
      id: 'chime-btn',
      'aria-haspopup': 'dialog',
      onClick: () => openSoundSettings(),
    });
    host.prepend(button);
  }
  onSoundChange(paintButton);
  // The first click or key anywhere on the desk lets the browser play sound.
  const gesture = () => {
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

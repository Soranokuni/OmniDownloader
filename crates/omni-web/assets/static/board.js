/* The wall board (plan P12.13): what is running, what waits for a person,
 * and the health, in large type for a monitor in the control room.
 *
 * Read-only: no buttons, no actions; it only asks. Every 5 seconds it reads
 * the status (counts and health), the live list (what is running) and the
 * held videos (`/api/held/waiting`). Each answer stands on its own: one that
 * fails leaves its part as it was, except that a service that stops
 * answering is said in a banner, and a server without the held route (404)
 * shows no number there rather than an error or a zero it does not know.
 *
 * Every word is in text.el.js (`board.*`, and the desk's own `health.*`,
 * `stage.*`). Nothing is built from an HTML string: see app.js.
 */

import { api, el, render } from '/static/app.js?v=6';
import { stageLine, fileName } from '/static/desk.js?v=26';
import { healthProblems, healthSummary, checkTexts } from '/static/notify.js?v=21';
import { t, tn, applyText } from '/static/text.js?v=8';

applyText();

const POLL_MS = 5000;
const RUNNING_SHOWN = 8;
let polling = false;

/** «14:21», or '' for a missing or unreadable time (never "now"). */
function hhmm(at) {
  const d = at ? new Date(at) : null;
  if (!d || Number.isNaN(d.getTime())) return '';
  return d.toLocaleTimeString(t('locale'), { hour: '2-digit', minute: '2-digit', hour12: false });
}

function setNumber(id, n) {
  const node = document.getElementById(id);
  node.textContent = n === null || n === undefined ? '' : String(n);
  node.closest('.board-tile')?.classList.toggle('attention', Number(n) > 0);
}

/** Needs a person: review, file-locker links and failures (held videos have their own tile). */
function personCount(q) {
  return (q.review || 0) + (q.manual || 0) + (q.failed || 0);
}

function paintStatus(data) {
  const q = data.queue;
  // `queue: null` means the server could not count: no number, not zeros.
  setNumber('board-review', q ? personCount(q) : null);
  setNumber('board-late', q && typeof q.late === 'number' ? q.late : null);
  document.getElementById('board-pending').textContent = q ? tn('board.pending', q.pending || 0) : '';

  const summary = healthSummary(data.checks);
  document.getElementById('board-health-text').textContent = summary.text;
  document.getElementById('board-health-dot').className = `dot ${summary.tone}`;
  document.getElementById('board-health').className = `board-health ${summary.tone}`;
  const problems = healthProblems(data.checks);
  render('board-problems', problems.length
    ? problems.map(([name, check]) => {
      const text = checkTexts(name, check);
      return el('div', { class: `board-problem ${check.state === 'down' ? 'bad' : 'warn'}` },
        el('strong', {}, text.name), ' — ', text.happened);
    })
    : el('div', { class: 'board-ok' }, t('health.pill.ok')));
}

/** The held videos (P10.5), from lane auto's `/api/held/waiting`; null when unknown. */
async function heldWaiting() {
  try {
    return await api('/api/held/waiting');
  } catch (e) {
    // An older server has no such route: no number, and no error on the wall.
    if (e.status === 404) return null;
    throw e;
  }
}

function paintHeld(held) {
  setNumber('board-held', held && typeof held.count === 'number' ? held.count : null);
  const since = held && held.count > 0 ? hhmm(held.oldest_held_at) : '';
  document.getElementById('board-held-since').textContent = since ? t('board.held.since', { at: since }) : '';
}

function paintRunning(jobs) {
  const running = jobs.filter((j) => j.status === 'RUNNING');
  if (!running.length) {
    render('board-jobs', el('div', { class: 'board-idle' }, t('board.idle')));
    return;
  }
  const rows = running.slice(0, RUNNING_SHOWN).map((job) => {
    const progress = Math.max(0, Math.min(100, Number(job.progress) || 0));
    return el('div', { class: 'board-job' },
      el('div', { class: 'board-job-name mono' }, fileName(job)),
      el('div', { class: 'board-job-step' }, stageLine(job)),
      el('div', { class: 'progress board-progress' }, el('span', { style: `width:${progress}%` })),
      el('div', { class: 'board-job-pct mono' }, `${progress.toFixed(0)}%`));
  });
  if (running.length > RUNNING_SHOWN) rows.push(el('div', { class: 'board-more' }, t('late.more', { n: running.length - RUNNING_SHOWN })));
  render('board-jobs', rows);
}

async function poll() {
  if (polling) return;
  polling = true;
  try {
    const [status, live, held] = await Promise.allSettled([
      api('/api/system/status'),
      api('/api/jobs?view=live&page=1&per_page=50'),
      heldWaiting(),
    ]);
    const down = status.status === 'rejected' && (status.reason.code === 'NETWORK' || status.reason.status >= 500);
    document.getElementById('board-offline').hidden = !down;
    if (status.status === 'fulfilled') paintStatus(status.value);
    if (live.status === 'fulfilled') paintRunning(live.value.jobs || []);
    if (held.status === 'fulfilled') paintHeld(held.value);
    if (status.status === 'fulfilled') {
      document.getElementById('board-updated').textContent = t('board.updated', { at: hhmm(new Date()) });
    }
  } finally {
    polling = false;
  }
}

function tick() {
  document.getElementById('board-clock').textContent = hhmm(new Date());
}

tick();
setInterval(tick, 10000);
poll();
setInterval(poll, POLL_MS);

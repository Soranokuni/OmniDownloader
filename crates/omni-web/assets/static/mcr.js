/* MCR panel behaviour (plan P2.3, P2.5, P7.1).
 *
 * Every value rendered here -- slug, url, error_message, journalist -- can
 * originate in an email sent to the ingest address. Nothing in this file
 * builds HTML from a string; `el()` creates nodes and text goes through
 * `textContent`, so a job whose URL is `"><img src=x onerror=...>` renders as
 * that exact text in the operator's browser and nothing else happens.
 *
 * The desk is written for operators who are not technical and for people on
 * their first shift, in Greek: every list says what it holds, every button
 * says what it does, and anything that cannot be undone asks first, in plain
 * words.
 */

import {
  api, el, render, live, toast, fmtTime, fmtDuration, logout, safeHref, icon,
} from '/static/app.js?v=5';
import {
  stageLine, statusBadge, limitNote, fileName, deliveredName, pager,
  retryJob as deskRetry, overrideJob as deskOverride, discardJob as deskDiscard,
  redownloadJob as deskRedownload, queueOffer as deskQueueOffer,
  canRename, renameJob as deskRename,
} from '/static/desk.js?v=2';
import { initInbox, loadInbox, openMail } from '/static/inbox.js?v=6';
import { initNotify } from '/static/notify.js?v=3';

let journalists = [];

/* ------------------------------------------------------------------ *
 * Tabs
 * ------------------------------------------------------------------ */

const TABS = ['email', 'queue', 'review', 'completed', 'journalists', 'manual'];

/* The Email tab is where the desk opens (plan P7.10); `/mcr#review` and the
 * like open another one, so a bookmark or a second screen can keep its own. */
function tabFromHash() {
  const name = location.hash.replace('#', '');
  return TABS.includes(name) ? name : 'email';
}

let currentTab = tabFromHash();

function switchTab(name) {
  currentTab = name;
  for (const tab of TABS) {
    const button = document.querySelector(`.tab[data-tab="${tab}"]`);
    const section = document.getElementById(`tab-${tab}`);
    if (button) button.setAttribute('aria-selected', String(tab === name));
    if (section) section.hidden = tab !== name;
  }
  document.body.classList.toggle('desk-wide', name === 'email');
  if (location.hash !== `#${name}`) history.replaceState(null, '', `#${name}`);
  if (name === 'journalists') loadJournalists();
  refresh();
}

for (const button of document.querySelectorAll('.tab[data-tab]')) {
  button.addEventListener('click', () => switchTab(button.dataset.tab));
}

/* ------------------------------------------------------------------ *
 * Status bar
 * ------------------------------------------------------------------ */

/** Health state -> dot colour. */
const DOT = { ok: 'ok', degraded: 'warn', down: 'bad' };

/** Check names as an operator reads them in the banner. */
const CHECK_NAME = {
  tools: 'Εργαλεία',
  encoder: 'Κωδικοποιητής',
  watchfolder: 'Watchfolder',
  browser: 'Πρόγραμμα περιήγησης',
  disk: 'Δίσκος',
  queue: 'Ουρά',
  selfcheck: 'Αυτοέλεγχος',
  deno: 'YouTube (Deno)',
  accounts: 'Λογαριασμοί',
};

async function loadStatus() {
  let data;
  try {
    data = await api('/api/system/status');
  } catch {
    setStatus('mail', 'Email: άγνωστο', 'bad', '');
    setStatus('llm', 'LLM: άγνωστο', 'bad', '');
    return;
  }

  const checks = data.checks || {};

  // The detail goes in the tooltip, so the bar stays readable but the reason
  // is one hover away. These used to be the string literals "Active" and
  // "Ready", which said the mailbox was fine while it was refusing the
  // password (defect W-09).
  const mail = checks.mail || { state: 'ok' };
  setStatus('mail', `Email: ${label(mail)}`, DOT[mail.state] || '', mail.detail || '');

  const llm = checks.llm || { state: 'ok' };
  setStatus('llm', `LLM: ${label(llm)}`, DOT[llm.state] || '', llm.detail || '');

  const free = data.disk?.watchfolder?.free_gb;
  const storage = document.getElementById('storage-status');
  storage.textContent =
    free === undefined || free === null
      ? 'Watchfolder: άγνωστο'
      : `Watchfolder: ${free.toLocaleString('el-GR', { maximumFractionDigits: 1, minimumFractionDigits: 1 })} GB ελεύθερα`;
  // Whatever the watchfolder check says is the authoritative word on whether
  // delivery will work at all.
  const wf = checks.watchfolder;
  storage.title = wf?.detail || '';

  // Anything not already on the bar — tools, disk, queue, self-check —
  // surfaces here rather than staying invisible until a job fails on it.
  const problems = Object.entries(checks)
    .filter(([name, c]) => c.state !== 'ok' && name !== 'mail' && name !== 'llm')
    .map(([name, c]) => `${CHECK_NAME[name] || name}: ${c.detail || c.state}`);
  const banner = document.getElementById('health-banner');
  if (problems.length === 0) {
    banner.hidden = true;
  } else {
    banner.hidden = false;
    render(banner, el('div', { class: 'card attention' },
      el('strong', {}, data.status === 'down' ? 'Η λήψη βίντεο έχει σταματήσει. ' : 'Προσοχή. '),
      problems.join(' · '),
      el('span', { class: 'note', style: 'display:block;margin-top:6px' },
        'Ενημερώστε τον μηχανικό βάρδιας· η σελίδα «Διαχείριση» έχει τις λεπτομέρειες.'),
    ));
  }
}

function label(check) {
  switch (check.state) {
    case 'ok': return 'OK';
    case 'degraded': return 'με πρόβλημα';
    case 'down': return 'εκτός λειτουργίας';
    default: return 'άγνωστο';
  }
}

function setStatus(prefix, text, dotClass, title) {
  const node = document.getElementById(`${prefix}-status`);
  node.textContent = text;
  node.title = title || '';
  document.getElementById(`${prefix}-dot`).className = `dot ${dotClass}`;
}

/* ------------------------------------------------------------------ *
 * Paging
 * ------------------------------------------------------------------ */

/** Page state per list. */
const pages = {
  live: { page: 1, perPage: 15 },
  review: { page: 1, perPage: 10 },
  completed: { page: 1, perPage: 25 },
};

async function fetchPage(view, extra = {}) {
  const state = pages[view];
  const params = new URLSearchParams({ view, page: String(state.page), per_page: String(state.perPage), ...extra });
  return api(`/api/jobs?${params}`);
}

function showCounts(counts) {
  if (!counts) return;
  // (Also called by the Email tab, with the same counts.)
  const set = (id, n) => {
    const b = document.getElementById(id);
    b.hidden = !n;
    b.textContent = String(n || 0);
  };
  set('queue-count', counts.active);
  set('review-count', counts.review);
  set('completed-count', counts.completed);
  const clear = document.getElementById('clear-finished');
  clear.disabled = !counts.finished;
  clear.textContent = counts.finished
    ? `Αφαίρεση παραδοθέντων από τη λίστα (${counts.finished})`
    : 'Αφαίρεση παραδοθέντων από τη λίστα';
  clear.dataset.count = String(counts.finished || 0);
}

/** Refresh what is on screen: the open list, and the tab counts. */
let refreshing = false;
let refreshAgain = false;
async function refresh() {
  // One at a time; a request that arrives meanwhile (a tab click during the
  // five-second refresh) runs as soon as the current one ends, instead of
  // being dropped and leaving the new tab's placeholder on screen.
  if (refreshing) { refreshAgain = true; return; }
  refreshing = true;
  try {
    do {
      refreshAgain = false;
      if (currentTab === 'email') await loadInbox();
      else if (currentTab === 'review') await loadReview();
      else if (currentTab === 'completed') await loadCompleted();
      // The live queue (also the journalists and add-a-link tabs) keeps the
      // tab counts current.
      else await loadQueue();
    } while (refreshAgain);
  } finally {
    refreshing = false;
  }
}

function failed(e) {
  if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
}

/* «Email: …»: from a video to the mail it came from, opened in the Email
 * tab with its text and its other videos (plan P7.11). A link added by hand
 * opens as its own entry. */
function mailLink(job) {
  const fromMail = !!job.email_message_id;
  const kind = fromMail ? 'mail' : 'manual';
  const key = fromMail ? job.email_message_id : String(job.parent_job_id || job.id);
  const label = fromMail
    ? (job.mail_subject ? `Email: «${job.mail_subject}»` : 'Email: (χωρίς θέμα)')
    : 'Χειροκίνητη προσθήκη';
  return el('button', {
    class: 'mail-link',
    type: 'button',
    title: 'Άνοιγμα στην καρτέλα Email, με το κείμενο και τα άλλα βίντεό του',
    onClick: () => {
      switchTab('email');
      openMail(kind, key);
    },
  }, icon('mail'), el('span', {}, label));
}

/* ------------------------------------------------------------------ *
 * Other videos found in an article
 * ------------------------------------------------------------------ */

/* Videos the sniffer found in a submitted article and left for MCR to
 * decide (raw page streams, platform posts beyond the automatic limit, and
 * the ones past a journalist's "first N"). Platform posts within the limit
 * were already queued as 1B, 1C, … */
function offersOf(job) {
  if (!job.candidates_json) return [];
  try {
    const list = JSON.parse(job.candidates_json);
    return Array.isArray(list) ? list.filter((o) => o && o.url && !o.queued_job_id) : [];
  } catch (_) {
    return [];
  }
}

function renderOffers(targetId, jobs) {
  const withOffers = jobs.filter((j) => offersOf(j).length > 0);
  if (withOffers.length === 0) {
    render(targetId);
    return;
  }
  render(targetId, el('div', { class: 'card stack' },
    el('h3', { class: 'job-name' }, 'Βρέθηκαν κι άλλα βίντεο σε αυτά τα άρθρα'),
    el('p', { class: 'note', style: 'margin:0' },
      'Δεν προστέθηκαν αυτόματα. Ανοίξτε ένα για να το δείτε και προσθέστε το αν ανήκει στο θέμα.'),
    ...withOffers.flatMap((job) => offersOf(job).map((offer) => {
      const href = safeHref(offer.url);
      return el('div', { class: 'job-head', style: 'border-top:1px solid var(--line);padding-top:10px' },
        el('div', {},
          el('p', { class: 'job-meta' },
            `Από την εργασία #${job.id} (${fileName(job)}) — θα γίνει ${offer.index_str}_${job.journalist}_${job.keyword}.mxf`),
          el('p', { class: 'job-url', title: offer.url }, offer.url),
        ),
        el('div', { class: 'row tight' },
          href ? el('a', { class: 'btn', href, target: '_blank', rel: 'noopener noreferrer' }, 'Άνοιγμα') : null,
          el('button', {
            class: 'btn btn-warn',
            type: 'button',
            onClick: () => queueOffer(job.id, offer),
          }, `Προσθήκη ως ${offer.index_str}`),
        ),
      );
    })),
  ));
}

const queueOffer = (id, offer) => deskQueueOffer(id, offer, refresh);

/* ------------------------------------------------------------------ *
 * Live queue
 * ------------------------------------------------------------------ */

async function loadQueue() {
  let data;
  try {
    data = await fetchPage('live');
  } catch (e) { failed(e); return; }
  showCounts(data.counts);
  const jobs = data.jobs || [];
  renderOffers('article-offers', jobs);
  if (jobs.length === 0) {
    render('queue-cards', el('div', { class: 'card center' },
      'Δεν περιμένει τίποτα. Οι νέοι σύνδεσμοι από email εμφανίζονται εδώ αυτόματα.'));
  } else {
    render('queue-cards', jobs.map((j) => (j.status === 'PENDING' || j.status === 'RUNNING' ? activeCard(j) : deliveredCard(j))));
  }
  pager('queue-pager', pages.live, data.total || 0, loadQueue);
}

function activeCard(job) {
  const progress = Number(job.progress) || 0;
  const running = job.status === 'RUNNING';
  return el('div', { class: 'card stack' },
    el('div', { class: 'job-head' },
      el('div', { class: 'row', style: 'gap:10px;align-items:flex-start' },
        el('span', { class: 'job-id' }, String(job.id)),
        el('div', {},
          el('h3', { class: 'job-name' }, fileName(job)),
          el('p', { class: 'job-url', title: job.url }, job.url),
          mailLink(job),
        ),
      ),
      el('div', { class: 'row tight' },
        statusBadge(job),
        canRename(job) ? el('button', {
          class: 'btn',
          type: 'button',
          title: 'Αλλαγή της λέξης-κλειδιού στο όνομα του αρχείου, πριν παραδοθεί',
          onClick: () => renameJob(job),
        }, 'Μετονομασία…') : null,
        el('button', {
          class: 'btn btn-danger',
          type: 'button',
          title: 'Αφαίρεση του βίντεο από την ουρά',
          onClick: () => discardJob(job),
        }, 'Αφαίρεση'),
      ),
    ),
    el('div', {},
      el('div', { class: 'job-meta' },
        el('span', {}, stageLine(job), running && job.speed && job.stage === 'DOWNLOAD' ? ` @ ${job.speed}` : ''),
        el('span', {}, running ? `${progress.toFixed(0)}%` : ''),
      ),
      running
        ? el('div', { class: 'progress' },
            el('span', { style: `width:${Math.max(0, Math.min(100, progress))}%` }))
        : null,
      el('div', { class: 'job-meta' },
        el('span', {}, `Δημοσιογράφος: ${job.journalist}`,
          job.group_code ? el('span', { class: 'badge info', style: 'margin-left:8px', title: groupName(job.group_code) }, job.group_code) : null,
          limitNote(job)),
        el('span', {}, running && job.eta && job.eta !== '--:--' ? `Απομένουν περίπου ${job.eta}` : ''),
      ),
    ),
  );
}

function deliveredCard(job) {
  return el('div', { class: 'card' },
    el('div', { class: 'job-head' },
      el('div', { class: 'row', style: 'gap:10px;align-items:flex-start' },
        el('span', { class: 'job-id' }, String(job.id)),
        el('div', {},
          el('h3', { class: 'job-name' }, deliveredName(job)),
          el('p', { class: 'job-meta', style: 'margin:2px 0 0' },
            `Παραδόθηκε ${fmtTime(job.completed_at || job.updated_at)} · ${fmtDuration(job.duration_secs)} · ${job.journalist}`,
            limitNote(job)),
          mailLink(job),
        ),
      ),
      el('div', { class: 'row tight' },
        statusBadge(job),
        el('button', { class: 'btn', type: 'button', onClick: () => redownload(job) }, 'Νέα λήψη'),
      ),
    ),
  );
}

document.getElementById('clear-finished').addEventListener('click', async (event) => {
  const n = Number(event.currentTarget.dataset.count || 0);
  if (!n) return;
  if (!confirm(
    `Να φύγουν ${n === 1 ? 'το 1 παραδοθέν βίντεο' : `τα ${n} παραδοθέντα βίντεο`} από τη λίστα «Σε εξέλιξη»;\n\n` +
    'Δεν διαγράφεται τίποτα. Μένουν στα «Ολοκληρωμένα», από όπου μπορείτε να τα κατεβάσετε ξανά.',
  )) return;
  try {
    const r = await api('/api/jobs/clear-finished', { method: 'POST' });
    toast(`${r.cleared} παραδοθέντα βίντεο έφυγαν από τη λίστα. Βρίσκονται στα «Ολοκληρωμένα».`, 'ok');
    pages.live.page = 1;
    refresh();
  } catch (e) { toast(e.message, 'bad'); }
});

/* ------------------------------------------------------------------ *
 * Needs attention
 * ------------------------------------------------------------------ */

async function loadReview() {
  let data;
  try {
    data = await fetchPage('review');
  } catch (e) { failed(e); return; }
  showCounts(data.counts);
  const jobs = data.jobs || [];
  if (jobs.length === 0) {
    render('review-cards', el('div', { class: 'card center' }, 'Δεν υπάρχει κάτι για έλεγχο.'));
  } else {
    render('review-cards', jobs.map(reviewCard));
  }
  pager('review-pager', pages.review, data.total || 0, loadReview);
}

function reviewCard(job) {
  const input = el('input', {
    type: 'url',
    class: 'mono',
    id: `override-${job.id}`,
    value: job.url,
  });
  const href = safeHref(job.url);
  const locker = job.status === 'MANUAL_DOWNLOAD';

  return el('div', { class: 'card attention stack' },
    el('div', { class: 'job-head', style: 'border-bottom:1px solid var(--line);padding-bottom:12px' },
      el('div', { class: 'row', style: 'gap:10px;align-items:flex-start' },
        el('span', { class: 'job-id' }, String(job.id)),
        el('div', {},
          el('h3', { class: 'job-name' }, fileName(job)),
          el('p', { class: 'job-url', title: job.url }, job.url),
          el('p', { class: 'job-meta', style: 'margin:2px 0 0' },
            `Δημοσιογράφος: ${job.journalist} · από ${fmtTime(job.updated_at)}`),
          mailLink(job),
        ),
      ),
      statusBadge(job),
    ),
    el('div', { class: 'reason' },
      el('strong', { style: 'display:block' }, job.hint
        || (locker
          ? 'Σύνδεσμος μεταφοράς αρχείων: κατεβάστε το αρχείο από τον σύνδεσμο και ρίξτε το στο Dalet.'
          : 'Το βίντεο δεν ελήφθη αυτόματα.')),
      job.error_code ? el('span', { class: 'note mono' }, `Κωδικός: ${job.error_code}`) : null,
    ),
    el('div', { class: 'row' },
      href ? el('a', { class: 'btn', href, target: '_blank', rel: 'noopener noreferrer' }, 'Άνοιγμα συνδέσμου') : null,
      locker ? null : el('button', { class: 'btn btn-primary', type: 'button', onClick: () => retryJob(job) }, 'Δοκιμή ξανά'),
      canRename(job) ? el('button', { class: 'btn', type: 'button', onClick: () => renameJob(job) }, 'Μετονομασία…') : null,
      el('button', { class: 'btn btn-danger', type: 'button', onClick: () => discardJob(job) }, 'Αφαίρεση'),
    ),
    locker ? null : el('div', {},
      el('label', { for: `override-${job.id}` },
        'Ή επικολλήστε άλλον σύνδεσμο για το ίδιο βίντεο (την ίδια την ανάρτηση, ή απευθείας σύνδεσμο .mp4 / .m3u8):'),
      el('div', { class: 'row' },
        el('div', { class: 'grow' }, input),
        el('button', {
          class: 'btn btn-warn',
          type: 'button',
          onClick: () => overrideJob(job, input.value),
        }, 'Χρήση αυτού του συνδέσμου'),
      ),
    ),
  );
}

const retryJob = (job) => deskRetry(job, refresh);
const overrideJob = (job, url) => deskOverride(job, url, refresh);
const discardJob = (job) => deskDiscard(job, refresh);
const renameJob = (job) => deskRename(job, refresh);

/* ------------------------------------------------------------------ *
 * Completed
 * ------------------------------------------------------------------ */

const filterSelect = document.getElementById('journalist-filter');
const groupSelect = document.getElementById('group-filter');
const searchInput = document.getElementById('completed-search');
const restartCompleted = () => { pages.completed.page = 1; loadCompleted(); };
filterSelect.addEventListener('change', restartCompleted);
groupSelect.addEventListener('change', restartCompleted);
let searchTimer = null;
searchInput.addEventListener('input', () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(restartCompleted, 300);
});

/* Group labels (plan P4.20): code → name, for titles and the filter. */
let groupNames = new Map();
function groupName(code) {
  return groupNames.get(code) || code;
}

async function loadFilters() {
  try {
    const [g, j] = await Promise.all([api('/api/groups'), api('/api/journalists')]);
    groupNames = new Map((g.groups || []).map((x) => [x.code, x.name]));
    journalists = j.journalists || [];
  } catch { /* the lists show codes and the filter stays at "all" */ }
  const surnames = journalists.map((x) => x.surname).filter(Boolean).sort();
  render(filterSelect,
    el('option', { value: '' }, 'Όλοι οι δημοσιογράφοι'),
    surnames.map((name) => el('option', { value: name }, name)),
  );
  render(groupSelect,
    el('option', { value: '' }, 'Όλες οι ομάδες'),
    el('option', { value: '-' }, 'Χωρίς ομάδα'),
    [...groupNames.keys()].sort().map((code) => el('option', { value: code }, `${code} — ${groupName(code)}`)),
  );
  render('m-journalists', journalists
    .filter((x) => x.surname && x.surname !== 'MCR')
    .map((x) => el('option', { value: x.surname }, x.full_name || x.surname)));
}

async function loadCompleted() {
  let data;
  try {
    data = await fetchPage('completed', {
      q: searchInput.value.trim(),
      journalist: filterSelect.value,
      group: groupSelect.value,
    });
  } catch (e) { failed(e); return; }
  showCounts(data.counts);
  const jobs = data.jobs || [];
  renderOffers('completed-offers', jobs);
  if (jobs.length === 0) {
    const filtered = searchInput.value.trim() || filterSelect.value || groupSelect.value;
    render('completed-body', el('tr', {},
      el('td', { colspan: '6', class: 'empty' }, filtered ? 'Τίποτα δεν ταιριάζει με την αναζήτηση.' : 'Δεν έχει παραδοθεί τίποτα ακόμη.')));
  } else {
    render('completed-body', jobs.map(completedRow));
  }
  pager('completed-pager', pages.completed, data.total || 0, loadCompleted);
}

function completedRow(job) {
  const href = safeHref(job.url);
  return el('tr', {},
    el('td', {},
      el('div', { class: 'strong' }, deliveredName(job), limitNote(job)),
      el('div', { class: 'note mono', title: job.url, style: 'max-width:420px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap' }, job.url),
      mailLink(job),
    ),
    el('td', {}, el('span', { class: 'badge info' }, job.journalist)),
    el('td', { title: job.group_code ? groupName(job.group_code) : '' }, job.group_code || '—'),
    // Timestamps are Option on the wire: a missing one renders blank, never
    // as "now" (which is what the old archive showed for every job).
    el('td', { class: 'num' }, fmtTime(job.completed_at || job.delivered_at)),
    el('td', {}, fmtDuration(job.duration_secs)),
    el('td', { class: 'right' },
      el('div', { class: 'row tight', style: 'justify-content:flex-end;flex-wrap:nowrap' },
        href ? el('a', { class: 'btn', href, target: '_blank', rel: 'noopener noreferrer', title: 'Άνοιγμα του αρχικού συνδέσμου' }, 'Άνοιγμα') : null,
        el('button', { class: 'btn btn-primary', type: 'button', onClick: () => redownload(job) }, 'Νέα λήψη'),
      ),
    ),
  );
}

const redownload = (job) => deskRedownload(job, refresh);

/* ------------------------------------------------------------------ *
 * Journalists
 * ------------------------------------------------------------------ */

async function loadJournalists() {
  try {
    const data = await api('/api/journalists');
    journalists = data.journalists || [];
  } catch (e) { failed(e); return; }

  if (journalists.length === 0) {
    render('journalists-body', el('tr', {},
      el('td', { colspan: '5', class: 'empty' }, 'Δεν υπάρχουν δημοσιογράφοι.')));
    return;
  }

  render('journalists-body', journalists.map((j) => el('tr', {},
    el('td', { class: 'strong' }, j.surname),
    el('td', {}, j.full_name),
    el('td', { class: 'mono' }, (j.emails || []).join(', ')),
    el('td', { class: 'num' }, String(j.default_priority)),
    el('td', { class: 'right' },
      j.surname === 'MCR'
        // MCR is structural: unresolved jobs are filed under it and the desk
        // addresses are recognised by it. It is never the journalist.
        ? el('span', { class: 'note', title: 'Οι διευθύνσεις του MCR αναγνωρίζονται, αλλά το MCR δεν θεωρείται ποτέ δημοσιογράφος' }, 'σταθερό')
        : el('button', {
            class: 'btn btn-icon btn-danger',
            type: 'button',
            title: `Διαγραφή ${j.surname}`,
            onClick: () => deleteJournalist(j.surname),
          }, '✕'),
    ),
  )));
}

document.getElementById('journalist-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const emails = document.getElementById('j-emails').value
    .split(',').map((s) => s.trim()).filter(Boolean);
  try {
    await api('/api/journalists', {
      method: 'POST',
      body: {
        surname: document.getElementById('j-surname').value,
        full_name: document.getElementById('j-fullname').value,
        emails,
        priority: Number(document.getElementById('j-priority').value) || 0,
      },
    });
    toast('Ο δημοσιογράφος αποθηκεύτηκε.', 'ok');
    event.target.reset();
    document.getElementById('j-priority').value = '0';
    loadJournalists();
    loadFilters();
  } catch (e) { toast(e.message, 'bad'); }
});

async function deleteJournalist(surname) {
  if (!confirm(`Διαγραφή του δημοσιογράφου ${surname};\n\nΤα βίντεο που έχουν ήδη παραδοθεί κρατούν τα ονόματά τους.`)) return;
  try {
    await api(`/api/journalists/${encodeURIComponent(surname)}`, { method: 'POST' });
    toast(`Ο ${surname} διαγράφηκε.`, 'ok');
    loadJournalists();
    loadFilters();
  } catch (e) { toast(e.message, 'bad'); }
}

/* ------------------------------------------------------------------ *
 * Add a link
 * ------------------------------------------------------------------ */

const manualFields = ['m-index', 'm-journalist', 'm-keyword'].map((id) => document.getElementById(id));
function showManualPreview() {
  const [index, journalist, keyword] = manualFields.map((f) => f.value.trim());
  // The server makes the Latin form (ELOT 743). The page does not guess it:
  // a Greek word is announced as converted, not shown as a name it will
  // not get.
  const greek = /[^\x00-\x7F]/.test(`${index}${journalist}${keyword}`);
  document.getElementById('m-preview').textContent = !(index && journalist && keyword)
    ? ''
    : greek
      ? `Το όνομα του αρχείου θα έχει τη μορφή ΑΡΙΘΜΟΣ_ΔΗΜΟΣΙΟΓΡΑΦΟΣ_ΛΕΞΗ.mxf, με τα ελληνικά σε λατινικούς χαρακτήρες (π.χ. Σεισμός → SEISMOS).`
      : `Το αρχείο θα ονομαστεί ${index}_${journalist.toUpperCase()}_${keyword.toUpperCase()}.mxf`;
}
for (const f of manualFields) f.addEventListener('input', showManualPreview);

document.getElementById('manual-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const max = Number(document.getElementById('m-max').value) || null;
  try {
    const result = await api('/api/jobs', {
      method: 'POST',
      body: {
        url: document.getElementById('m-url').value.trim(),
        journalist: document.getElementById('m-journalist').value.trim(),
        keyword: document.getElementById('m-keyword').value.trim(),
        index_str: document.getElementById('m-index').value.trim(),
        priority: document.getElementById('m-priority').checked ? 10 : 0,
        max_videos: max,
      },
    });
    toast(`Προστέθηκε: ${result.slug}.mxf`, 'ok');
    event.target.reset();
    document.getElementById('m-index').value = '1';
    showManualPreview();
    switchTab('queue');
  } catch (e) { toast(e.message, 'bad'); }
});

/* ------------------------------------------------------------------ *
 * Chrome
 * ------------------------------------------------------------------ */

const specsModal = document.getElementById('specs-modal');
document.getElementById('specs-btn').addEventListener('click', () => { specsModal.hidden = false; });
for (const id of ['specs-close', 'specs-close-2']) {
  document.getElementById(id).addEventListener('click', () => { specsModal.hidden = true; });
}
specsModal.addEventListener('click', (e) => { if (e.target === specsModal) specsModal.hidden = true; });
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') specsModal.hidden = true; });

document.getElementById('logout-btn').addEventListener('click', logout);

initInbox({ onCounts: showCounts, refresh: () => refresh(), groupName });
// Jingle and notification when every video of an email has finished (P5.4).
initNotify(document.querySelector('.topbar-status'), (id) => {
  const cut = id.indexOf(':');
  switchTab('email');
  openMail(id.slice(0, cut), id.slice(cut + 1));
});
loadFilters();
switchTab(currentTab);
live(() => { refresh(); loadStatus(); });

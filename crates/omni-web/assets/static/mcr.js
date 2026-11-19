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
  api, el, render, toast, fmtTime, fmtDuration, logout, safeHref, icon,
} from '/static/app.js?v=6';
import {
  stageLine, waitLine, stepTimeNode, limitNote, fileName, deliveredName, pager,
  retryJob as deskRetry, overrideJob as deskOverride, discardJob as deskDiscard, cancelJob as deskCancel,
  redownloadJob as deskRedownload, applyJobEvent, markDoneJob as deskMarkDone, queueOffer as deskQueueOffer,
  canRename, renameForm, renameOpen, timelineNode, autoDeliveredNode, lateReason, arrivedAgo,
} from '/static/desk.js?v=16';
import { deferAction, visible } from '/static/undo.js?v=1';
import { confirmDialog } from '/static/dialog.js?v=1';
import { STATUS, statusBadge } from '/static/status.js?v=1';
import { inGroupOrder, groupOf, groupLabel, primaryOf, whyText } from '/static/worklist.js?v=1';
import { initInbox, loadInbox, openMail } from '/static/inbox.js?v=23';
import { initNotify, watchStatus, serviceDown, serviceBack, CHECK_INFO } from '/static/notify.js?v=11';
import { syncApproval, approvalIds, focusApproval } from '/static/approval.js?v=11';

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
    if (button) {
      button.setAttribute('aria-selected', String(tab === name));
      button.tabIndex = tab === name ? 0 : -1;
    }
    if (section) section.hidden = tab !== name;
  }
  document.body.classList.toggle('desk-wide', name === 'email');
  // A held video keeps playing behind a hidden tab otherwise.
  if (name !== 'review') {
    for (const m of document.querySelectorAll('#approval-section video, #approval-section audio')) m.pause();
  }
  if (location.hash !== `#${name}`) history.replaceState(null, '', `#${name}`);
  if (name === 'journalists') loadJournalists();
  refresh();
}

for (const button of document.querySelectorAll('.tab[data-tab]')) {
  button.addEventListener('click', () => switchTab(button.dataset.tab));
  // Arrow keys, Home and End move to a tab and open it (WAI-ARIA tabs pattern).
  button.addEventListener('keydown', (ev) => {
    const at = TABS.indexOf(button.dataset.tab);
    let to = -1;
    if (ev.key === 'ArrowRight') to = (at + 1) % TABS.length;
    else if (ev.key === 'ArrowLeft') to = (at - 1 + TABS.length) % TABS.length;
    else if (ev.key === 'Home') to = 0;
    else if (ev.key === 'End') to = TABS.length - 1;
    if (to < 0) return;
    ev.preventDefault();
    switchTab(TABS[to]);
    document.getElementById(`tabbtn-${TABS[to]}`)?.focus();
  });
}

// The Email tab's «Για έγκριση» chip: open the review tab at that card.
window.addEventListener('desk:approval', (ev) => {
  switchTab('review');
  focusApproval(ev.detail.id);
});

// A link or back/forward that changes only the hash. switchTab's replaceState
// does not fire this, so there is no loop.
window.addEventListener('hashchange', () => {
  const name = tabFromHash();
  if (name !== currentTab) switchTab(name);
});

/* ------------------------------------------------------------------ *
 * Status bar
 * ------------------------------------------------------------------ */

/** Health state -> dot colour. */
const DOT = { ok: 'ok', degraded: 'warn', down: 'bad' };

// One status request at a time (P7.14): the 5 s tick and an SSE event can
// overlap, and a slow answer landing after a newer one made the review count
// go 15 -> 14 -> 15 and sound the alert twice.
let statusBusy = false;
let statusAgain = false;
async function loadStatus() {
  if (statusBusy) { statusAgain = true; return; }
  statusBusy = true;
  try {
    do {
      statusAgain = false;
      await loadStatusOnce();
    } while (statusAgain);
  } finally {
    statusBusy = false;
  }
}

// Consecutive status loads that failed because the service is not answering
// (no connection, or a 5xx). Two in a row (~10 s) raise the banner.
const OUTAGE_AFTER = 2;
let outageFails = 0;
let outageSince = null;
let outageShown = false;

function pad2(n) { return String(n).padStart(2, '0'); }

function showOutage() {
  const at = `${pad2(outageSince.getHours())}:${pad2(outageSince.getMinutes())}`;
  // Shown before it is filled: a live region that is display:none while its
  // text arrives may not be announced by a screen reader.
  const banner = document.getElementById('offline-banner');
  banner.hidden = false;
  render(banner, el('div', { class: 'card attention' },
    el('strong', {}, 'Η υπηρεσία λήψης δεν απαντά'),
    ` από ${at}`,
    el('div', { style: 'margin-top:8px' },
      'Οι λίστες δεν ενημερώνονται και οι λήψεις μπορεί να έχουν σταματήσει. Αν δεν επανέλθει σε λίγα λεπτά, ενημερώστε τον διαχειριστή.'),
  ));
}

function noteStatusFailure(e) {
  if (e.code !== 'NETWORK' && !(e.status >= 500)) return;
  outageFails += 1;
  if (outageFails === 1) outageSince = new Date();
  if (outageFails >= OUTAGE_AFTER && !outageShown) {
    outageShown = true;
    showOutage();
    serviceDown();
  }
}

function noteStatusSuccess() {
  outageFails = 0;
  outageSince = null;
  if (!outageShown) return;
  outageShown = false;
  document.getElementById('offline-banner').hidden = true;
  toast('Η υπηρεσία λήψης απαντά ξανά.', 'ok');
  serviceBack();
  refresh();
}

// A service that accepts the connection but never answers (frozen) would
// keep this request, and the single-flight lock, pending forever: the banner
// would never rise. Past the deadline it counts as not answering.
const STATUS_DEADLINE_MS = 15000;
function statusWithDeadline() {
  let timer;
  const late = new Promise((_, reject) => {
    timer = setTimeout(() => reject(Object.assign(new Error('Η υπηρεσία λήψης δεν απαντά.'), { code: 'NETWORK' })), STATUS_DEADLINE_MS);
  });
  return Promise.race([api('/api/system/status'), late]).finally(() => clearTimeout(timer));
}

async function loadStatusOnce() {
  let data;
  try {
    data = await statusWithDeadline();
  } catch (e) {
    setStatus('mail', 'Email: άγνωστο', 'bad', '');
    setStatus('llm', 'LLM: άγνωστο', 'bad', '');
    noteStatusFailure(e);
    return;
  }
  noteStatusSuccess();

  const checks = data.checks || {};

  // The detail goes in the tooltip, so the bar stays readable but the reason
  // is one hover away. These used to be the string literals "Active" and
  // "Ready", which said the mailbox was fine while it was refusing the
  // password (defect W-09).
  const mail = checks.mail || { state: 'ok' };
  setStatus('mail', `Email: ${label(mail)}`, DOT[mail.state] || '', mail.detail || '');

  const llm = checks.llm || { state: 'ok' };
  setStatus('llm', `LLM: ${label(llm)}`, DOT[llm.state] || '', llm.detail || '');

  const queue = data.queue;
  document.getElementById('queue-status').textContent = queue
    ? `Ουρά: ${queue.pending} ${STATUS.PENDING.phrase} · ${queue.running} ${STATUS.RUNNING.phrase}${queue.approval > 0 ? ` · ${queue.approval} ${STATUS.AWAITING_APPROVAL.phrase}` : ''}`
    : 'Ουρά: —';
  const tempFree = data.disk?.temp?.free_gb;
  document.getElementById('temp-status').textContent =
    tempFree === undefined || tempFree === null
      ? 'Temp: άγνωστο'
      : `Temp: ${tempFree.toLocaleString('el-GR', { maximumFractionDigits: 1, minimumFractionDigits: 1 })} GB ελεύθερα`;

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
  const problems = Object.entries(checks).filter(([name, c]) => c.state !== 'ok' && name !== 'llm');
  const banner = document.getElementById('health-banner');
  if (problems.length === 0) {
    banner.hidden = true;
  } else {
    banner.hidden = false;
    render(banner, el('div', { class: 'card attention' },
      el('strong', {}, problems.some(([name, c]) => name !== 'mail' && c.state === 'down') ? 'Η λήψη βίντεο έχει σταματήσει' : 'Προσοχή'),
      ...problems.map(([name, c]) => {
        const info = CHECK_INFO[name];
        return el('div', { style: 'margin-top:8px' },
          el('div', {}, info ? info.name : name),
          info?.action ? el('div', {}, info.action) : null,
          el('div', { class: 'note mono', style: 'font-size:12px' }, c.detail || c.state),
        );
      }),
    ));
  }
  renderSelfheal(data.selfheal);
  watchStatus(data);
}

// One line per site the overnight self-heal found a new way for in the last 24 h
// (P10.14). A line only: a broken site is the banner's, never a chime. The
// video it finds still waits for MCR's approval.
function renderSelfheal(list) {
  const box = document.getElementById('selfheal-lines');
  const found = new Set((Array.isArray(list) ? list : []).filter((r) => r.outcome === 'found').map((r) => r.domain));
  if (found.size === 0) {
    box.hidden = true;
    render(box);
    return;
  }
  box.hidden = false;
  render(box, el('div', { class: 'card' },
    ...[...found].map((domain) => el('div', {}, `${domain}: η σελίδα άλλαξε· βρέθηκε νέος τρόπος (θα ζητηθεί έγκριση)`)),
  ));
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
  // The work list is grouped within a page (P12.2): one page holds a normal day's work.
  review: { page: 1, perPage: 100 },
  approval: { page: 1, perPage: 100 },
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
  // Held videos wait for a person too, so they count on the same tab.
  set('review-count', (counts.review || 0) + (counts.approval || 0));
  // The Completed badge is today's deliveries; the total is in the tooltip.
  set('completed-count', counts.completed_today);
  document.getElementById('completed-count').title = `Σήμερα · σύνολο ${counts.completed || 0}`;
  showHiddenCount(counts.review_hidden || 0);
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
  if (e.code === 'UNAUTHENTICATED') return;
  // The lists reload every 5 s: an unreachable service is the outage
  // banner's to say (within ~10 s), not a toast per list per poll. Actions
  // MCR clicks report their own errors.
  if (e.code === 'NETWORK') return;
  toast(e.message, 'bad');
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

/** The waiting and running cards on screen, by job id: a live `job` event
 *  redraws just its card (P1.10). Rebuilt by every loadQueue. */
const liveCards = new Map();

async function loadQueue() {
  let data;
  try {
    data = await fetchPage('live');
  } catch (e) { failed(e); return; }
  showCounts(data.counts);
  // What waits for its «Αναίρεση» to run out is already gone from the list.
  const jobs = visible('live', data.jobs || []);
  renderOffers('article-offers', jobs);
  if (jobs.length === 0) {
    liveCards.clear();
    queueList.cards.clear();
    render('queue-cards', el('div', { class: 'card center' },
      'Δεν περιμένει τίποτα. Οι νέοι σύνδεσμοι από email εμφανίζονται εδώ αυτόματα.'));
  } else {
    syncKeyedList(queueList, document.getElementById('queue-cards'), jobs, {
      build: (j) => {
        if (!isActive(j)) {
          liveCards.delete(j.id);
          return deliveredCard(j);
        }
        const node = activeCard(j);
        liveCards.set(j.id, { job: j, node });
        return node;
      },
      sig: queueSig,
    });
    // A card that left the page (or finished) is no longer a live card.
    for (const id of [...liveCards.keys()]) if (!queueList.cards.has(id)) liveCards.delete(id);
  }
  pager('queue-pager', pages.live, data.total || 0, loadQueue);
}

const isActive = (job) => job.status === 'PENDING' || job.status === 'RUNNING';

/** A queue card's signature: the job, plus the one thing a card shows that
 *  changes with the clock (the countdown / minutes-in-step text and its warning
 *  colour), so a ticking card is redrawn and a still one is left alone. */
function queueSig(job) {
  if (!isActive(job)) return JSON.stringify(job);
  const t = job.status === 'RUNNING' ? stepTimeNode(job) : null;
  const wait = job.status === 'PENDING' ? waitLine(job) : '';
  return JSON.stringify([job, wait, t ? t.className : '', t ? t.textContent : '', groupGen, renameOpen.has(job.id)]);
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
          title: 'Ακύρωση του βίντεο: δεν θα σταλεί στο Dalet',
          onClick: () => cancelJob(job),
        }, 'Ακύρωση'),
      ),
    ),
    canRename(job) ? renameFormFor(job) : null,
    el('div', {},
      el('div', { class: 'job-meta' },
        el('span', {},
          running ? stageLine(job) : waitLine(job),
          running && job.speed && job.stage === 'DOWNLOAD' ? ` @ ${job.speed}` : '',
          running ? stepTimeNode(job) : null),
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
    autoDeliveredNode(job, refresh),
  );
}

// Nothing is deleted: the delivered videos stay in «Ολοκληρωμένα». They leave
// the list at once and the request goes when the «Αναίρεση» runs out (P12.4).
const isDelivered = (job) => job.status === 'COMPLETED' || job.status === 'COMPLETED_MANUAL';
document.getElementById('clear-finished').addEventListener('click', (event) => {
  const n = Number(event.currentTarget.dataset.count || 0);
  if (!n) return;
  pages.live.page = 1;
  deferAction({
    text: `${n === 1 ? 'Το 1 παραδοθέν βίντεο έφυγε' : `Τα ${n} παραδοθέντα βίντεο έφυγαν`} από τη λίστα· είναι στα «Ολοκληρωμένα».`,
    hides: (view, job) => view === 'live' && isDelivered(job),
    run: () => api('/api/jobs/clear-finished', { method: 'POST' }),
    after: refresh,
  });
});

/* ------------------------------------------------------------------ *
 * Needs attention
 * ------------------------------------------------------------------ */

/* The review tab's own filters and tidy-up (P7.21); its groups are worklist.js's (P12.2). */
const reviewJournalist = document.getElementById('review-journalist');
const reviewSearch = document.getElementById('review-search');
const reviewShowHidden = document.getElementById('review-show-hidden');
const restartReview = () => { pages.review.page = 1; loadReview(); };
reviewJournalist.addEventListener('change', restartReview);
reviewShowHidden.addEventListener('change', restartReview);
let reviewSearchTimer = null;
reviewSearch.addEventListener('input', () => {
  clearTimeout(reviewSearchTimer);
  reviewSearchTimer = setTimeout(restartReview, 300);
});

function showHiddenCount(n) {
  const label = document.getElementById('review-hidden-label');
  // Keep the box visible while it is ticked, so it can be unticked.
  label.hidden = !n && !reviewShowHidden.checked;
  document.getElementById('review-hidden-text').textContent = `Εμφάνιση κρυμμένων (${n})`;
}

const HIDE_OLD_HOURS = 72;
document.getElementById('hide-old-review').addEventListener('click', async () => {
  let hidden;
  const cutoff = Date.now() - HIDE_OLD_HOURS * 3600 * 1000;
  const isOld = (j) => !j.cleared_at && !!j.updated_at && Date.parse(j.updated_at) < cutoff;
  try {
    // Count first so the message can say how many.
    const data = await api('/api/jobs?view=review&page=1&per_page=100');
    const jobs = data.jobs || [];
    hidden = jobs.filter(isOld).length;
    if (data.total > jobs.length) hidden = null; // more than one page: the exact number is unknown
  } catch (e) { toast(e.message, 'bad'); return; }
  if (hidden === 0) { toast('Δεν υπάρχει τίποτα παλαιότερο από 3 ημέρες.', 'ok'); return; }
  const how = hidden === null ? 'Τα παλαιότερα βίντεο κρύφτηκαν' : hidden === 1 ? 'Το 1 βίντεο κρύφτηκε' : `Τα ${hidden} βίντεο κρύφτηκαν`;
  pages.review.page = 1;
  // Nothing is deleted; they show again with «Εμφάνιση κρυμμένων».
  deferAction({
    text: `${how} (πάνω από 3 ημέρες)· εμφανίζονται με «Εμφάνιση κρυμμένων».`,
    hides: (view, job) => view === 'review' && isOld(job),
    run: () => api('/api/jobs/review/hide-old', { method: 'POST', body: { hours: HIDE_OLD_HOURS } }),
    after: refresh,
  });
});

/* Keyed lists (P12.3). The lists refresh every 5 s; rebuilding every card
 * threw away what an operator was typing, the focus and the open panels. Each
 * list keeps `cards` (job id -> { node, sig }) and `days` (heading key -> node).
 * A card whose signature is unchanged is not touched; a changed one is built
 * again and takes over the typed values and the focus of the old one. The
 * signature is the whole job (everything a card reads is in it), so no field
 * can be forgotten. */
const reviewList = { cards: new Map(), days: new Map() };
const queueList = { cards: new Map(), days: new Map() };
const completedList = { cards: new Map(), days: new Map() };

const CONTROLS = 'input, textarea, select';

/** True when the operator changed the control after it was built. */
function controlEdited(c) {
  if (c.tagName === 'SELECT') return [...c.options].some((o) => o.selected !== o.defaultSelected);
  if (c.type === 'checkbox' || c.type === 'radio') return c.checked !== c.defaultChecked;
  return c.value !== c.defaultValue;
}

/** Copy what the operator typed (and the focus) from `oldNode`'s controls to
 *  the matching ones of `newNode`. Returns a function to run once `newNode` is
 *  on the page, which gives back the focus and the selection. */
function carryControls(oldNode, newNode) {
  const fresh = [...newNode.querySelectorAll(CONTROLS)];
  const active = document.activeElement;
  const restores = [];
  [...oldNode.querySelectorAll(CONTROLS)].forEach((old, i) => {
    const next = old.id ? fresh.find((c) => c.id === old.id) : fresh[i];
    if (!next || next.tagName !== old.tagName) return;
    if (controlEdited(old)) {
      if (old.type === 'checkbox' || old.type === 'radio') next.checked = old.checked;
      else next.value = old.value;
    }
    if (old !== active) return;
    let start = null;
    let end = null;
    let dir = 'none';
    try { start = old.selectionStart; end = old.selectionEnd; dir = old.selectionDirection || 'none'; } catch { /* no selection on this type */ }
    restores.push(() => {
      if (!next.isConnected) return;
      next.focus({ preventScroll: true });
      if (start === null || end === null) return;
      try { next.setSelectionRange(start, end, dir); } catch { /* not a text control */ }
    });
  });
  return restores;
}

/** Put `wanted` in `host` in that order, moving only what is out of place and
 *  removing the rest: a node that is not moved keeps its focus and scroll. */
function reconcileChildren(host, wanted) {
  const keep = new Set(wanted);
  let cur = host.firstChild;
  for (const node of wanted) {
    // Stale nodes (replaced, gone, placeholders) go first, in place, so the
    // kept nodes after them are not moved: a moved node loses focus and caret.
    while (cur && !keep.has(cur)) {
      const stale = cur.nextSibling;
      cur.remove();
      cur = stale;
    }
    if (node === cur) {
      cur = cur.nextSibling;
      continue;
    }
    host.insertBefore(node, cur);
  }
  while (cur) {
    const next = cur.nextSibling;
    cur.remove();
    cur = next;
  }
}

/** Show `jobs` in `host`, one keyed node each (`build(job)`), reusing the node
 *  of every job whose `sig(job)` is unchanged. With `heading(job)` (a label,
 *  or null), a small heading precedes the first job of each run of one label;
 *  `headingText(label, n)` words it, with `n` the jobs under it. */
function syncKeyedList(list, host, jobs, { build, sig, heading = null, headingText = (label) => label }) {
  if (!host) return;
  const wanted = [];
  const restores = [];
  const seen = new Set();
  const days = new Map();
  const dayCount = new Map();
  const runs = new Map();
  if (heading) {
    let prev;
    let key;
    for (const job of jobs) {
      const label = heading(job);
      if (label && label !== prev) {
        key = `${label}#${(runs.get(`${label}#`) || 0)}`;
        runs.set(`${label}#`, (runs.get(`${label}#`) || 0) + 1);
      }
      if (label) runs.set(key, (runs.get(key) || 0) + 1);
      prev = label;
    }
  }
  let day;
  for (const job of jobs) {
    if (heading) {
      const label = heading(job);
      if (label && label !== day) {
        // Counted, so a label that comes back later in the list gets its own node.
        const n = dayCount.get(label) || 0;
        dayCount.set(label, n + 1);
        const key = `${label}#${n}`;
        const node = list.days.get(key) || el('h2', { class: 'wl-group' });
        const text = headingText(label, runs.get(key) || 0);
        if (node.textContent !== text) node.textContent = text;
        days.set(key, node);
        wanted.push(node);
      }
      day = label;
    }
    const s = sig(job);
    const prev = list.cards.get(job.id);
    let node;
    if (prev && prev.sig === s && prev.node.parentNode === host) {
      node = prev.node;
    } else {
      node = build(job);
      if (prev && prev.node.parentNode === host) restores.push(...carryControls(prev.node, node));
      list.cards.set(job.id, { node, sig: s });
    }
    seen.add(job.id);
    wanted.push(node);
  }
  list.days = days;
  for (const id of [...list.cards.keys()]) if (!seen.has(id)) list.cards.delete(id);
  reconcileChildren(host, wanted);
  for (const restore of restores) restore();
  if (focusAfterSync) {
    const target = document.getElementById(focusAfterSync);
    if (target && host.contains(target)) {
      focusAfterSync = null;
      target.focus();
    }
  }
}

/* The held videos (P10.7): fetched with every review refresh, but their cards
 * are reconciled by job id (approval.js), never rebuilt. A failed fetch leaves
 * the cards as they are. */
async function loadApproval() {
  try {
    const data = await fetchPage('approval');
    syncApproval(document.getElementById('approval-section'), data.jobs || [], {
      mailLink, historyPanel, markDone: (job) => deskMarkDone(job, refresh), refresh,
    });
  } catch (e) { failed(e); }
}

async function loadReview() {
  await loadApproval();
  let data;
  try {
    const extra = {};
    const q = reviewSearch.value.trim();
    if (q) extra.q = q;
    if (reviewJournalist.value) extra.journalist = reviewJournalist.value;
    if (reviewShowHidden.checked) extra.hidden = '1';
    data = await fetchPage('review', extra);
  } catch (e) { failed(e); return; }
  showCounts(data.counts);
  // Grouped by what the operator has to do (P12.2); a job that will retry by
  // itself is not in this list.
  const jobs = inGroupOrder(visible('review', data.jobs || []));
  // Forget the history and the open panels of jobs that left the page; keep the rest.
  const here = new Set([...jobs.map((j) => j.id), ...approvalIds()]);
  for (const id of [...historyState.keys(), ...historyBox.keys(), ...historyOpen, ...historyTech, ...moreOpen, ...techOpen]) {
    if (here.has(id)) continue;
    historyState.delete(id); historyBox.delete(id); historyOpen.delete(id); historyTech.delete(id);
    moreOpen.delete(id); techOpen.delete(id);
  }
  if (jobs.length === 0) {
    const filtered = reviewSearch.value.trim() || reviewJournalist.value;
    reviewList.cards.clear();
    reviewList.days.clear();
    render('review-cards', el('div', { class: 'card center' },
      filtered ? 'Κανένα βίντεο δεν ταιριάζει με την αναζήτηση.' : 'Δεν υπάρχει κάτι που να περιμένει εσάς.'));
  } else {
    syncKeyedList(reviewList, document.getElementById('review-cards'), jobs, {
      build: reviewCard,
      sig: (job) => JSON.stringify([job, renameOpen.has(job.id)]),
      heading: (job) => groupLabel(groupOf(job)) || null,
      headingText: (label, n) => `${label} · ${n}`,
    });
    for (const id of historyOpen) restoreHistoryScroll(id);
  }
  pager('review-pager', pages.review, data.total || 0, loadReview);
}

/* A review card's history (P7.15). The cards are rebuilt every 5 s, so what is
 * open and what was fetched lives here, by job id. The events are fetched
 * again only when the job has changed since (`updated_at`). */
const historyOpen = new Set();
const historyTech = new Set();
const historyState = new Map(); // id -> { at, events, failed, loading }
const historyBox = new Map();   // id -> the panel currently on screen

/** A detached list cannot scroll, and the rebuilt card is put on the page
 *  only after paintHistory (render): loadReview calls this again once it is
 *  attached. Not requestAnimationFrame, which never runs in a hidden tab. */
function restoreHistoryScroll(id) {
  const st = historyState.get(id);
  const log = st?.node?.querySelector('.tech-log');
  if (log && st.scroll && log.isConnected) log.scrollTop = st.scroll;
}

function paintHistory(id) {
  const box = historyBox.get(id);
  const st = historyState.get(id);
  if (!box || !st) return;
  if (st.events) {
    // The built node is kept, so the 5 s rebuild of the cards re-attaches the
    // same list (and its scroll position) until new events arrive.
    // Every fetch stores a new array, so identity says "new events" even when
    // the newest 80 end with the same message as last time.
    if (!st.node || st.nodeEvents !== st.events) {
      st.node = timelineNode(st.events, historyTech.has(id), (open) => {
        if (open) historyTech.add(id); else historyTech.delete(id);
      });
      st.nodeEvents = st.events;
      st.scroll = 0;
      const log = st.node.querySelector('.tech-log');
      if (log) log.addEventListener('scroll', () => { st.scroll = log.scrollTop; });
    }
    box.replaceChildren(st.node);
    restoreHistoryScroll(id);
  } else if (st.failed) {
    box.replaceChildren(el('p', { class: 'note' }, 'Δεν φορτώθηκε το ιστορικό. ',
      el('button', { class: 'btn', type: 'button', onClick: () => loadHistory(id, st.at) }, 'Ξανά')));
  } else {
    box.replaceChildren(el('p', { class: 'note' }, 'Φόρτωση…'));
  }
}

async function loadHistory(id, at) {
  const prev = historyState.get(id);
  const st = { at, events: prev ? prev.events : null, node: prev ? prev.node : null, nodeSig: prev ? prev.nodeSig : '', scroll: prev ? prev.scroll : 0, failed: false, loading: true };
  historyState.set(id, st);
  paintHistory(id);
  try {
    const r = await api(`/api/jobs/${id}`);
    st.events = r.events || [];
  } catch {
    // A failed re-fetch must not blank a history that already loaded.
    if (!st.events) st.failed = true;
  }
  st.loading = false;
  paintHistory(id);
}

function historyPanel(job) {
  const box = el('div', { class: 'jc-history-panel' });
  box.hidden = !historyOpen.has(job.id);
  historyBox.set(job.id, box);
  const toggle = el('button', {
    class: 'btn', type: 'button', 'aria-expanded': String(historyOpen.has(job.id)),
    onClick: () => {
      const open = !historyOpen.has(job.id);
      if (open) historyOpen.add(job.id); else historyOpen.delete(job.id);
      toggle.setAttribute('aria-expanded', String(open));
      box.hidden = !open;
      const st = historyState.get(job.id);
      if (open && (!st || (st.at !== job.updated_at && !st.loading))) loadHistory(job.id, job.updated_at);
    },
  }, 'Ιστορικό');
  if (historyOpen.has(job.id)) {
    const st = historyState.get(job.id);
    if (!st || (st.at !== job.updated_at && !st.loading)) loadHistory(job.id, job.updated_at);
    else paintHistory(job.id);
  }
  return [toggle, box];
}

/* The «Περισσότερα» and «Τεχνικές λεπτομέρειες» panels a person opened, by
 * job id: a card rebuilt by the 5 s refresh opens them again (P12.3). */
const moreOpen = new Set();
const techOpen = new Set();

/** A `<details>` whose open state lives in `set` under the job's id. */
function keptDetails(set, id, cls, summary, ...body) {
  const node = el('details', { class: cls }, el('summary', {}, summary), ...body);
  if (set.has(id)) node.open = true;
  node.addEventListener('toggle', () => { if (node.open) set.add(id); else set.delete(id); });
  return node;
}

/**
 * A card of the work list (P12.2): why it waits, in one sentence; one
 * primary action; everything else under «Περισσότερα»; the code and the
 * tool's own (English) words under «Τεχνικές λεπτομέρειες».
 */
function reviewCard(job) {
  const href = safeHref(job.url);
  const locker = job.status === 'MANUAL_DOWNLOAD';
  const primary = primaryOf(job);
  const history = historyPanel(job);

  // Another link for the same video: the primary action of the cookies and
  // no-video groups, otherwise under «Περισσότερα». A file-locker link has none.
  const overrideForm = () => {
    const input = el('input', { type: 'url', class: 'mono', id: `override-${job.id}`, value: job.url });
    // Enter in the field does what the button beside it does.
    input.addEventListener('keydown', (event) => {
      if (event.key !== 'Enter' || event.isComposing) return;
      event.preventDefault();
      overrideJob(job, input.value);
    });
    return el('div', { class: 'wl-override' },
      el('label', { for: `override-${job.id}` },
        'Επικολλήστε άλλον σύνδεσμο για το ίδιο βίντεο (την ίδια την ανάρτηση, ή απευθείας σύνδεσμο .mp4 / .m3u8):'),
      el('div', { class: 'row' },
        el('div', { class: 'grow' }, input),
        el('button', {
          class: primary === 'override' ? 'btn btn-primary' : 'btn btn-warn',
          type: 'button',
          onClick: () => overrideJob(job, input.value),
        }, 'Χρήση αυτού του συνδέσμου')));
  };
  const retry = (cls) => el('button', { class: cls, type: 'button', onClick: () => retryJob(job) }, 'Δοκιμή ξανά');
  const markDone = (cls) => el('button', { class: cls, type: 'button', onClick: () => markDoneJob(job) }, 'Το έβαλα στο Dalet');

  let main;
  if (primary === 'override' && !locker) main = overrideForm();
  else if (primary === 'markdone' || locker) main = el('div', { class: 'row' }, markDone('btn btn-primary'));
  else main = el('div', { class: 'row' }, retry('btn btn-primary'));

  const more = keptDetails(moreOpen, job.id, 'wl-more', 'Περισσότερα',
    el('div', { class: 'row' },
      href ? el('a', { class: 'btn', href, target: '_blank', rel: 'noopener noreferrer' }, 'Άνοιγμα συνδέσμου') : null,
      !locker && primary !== 'retry' ? retry('btn') : null,
      primary !== 'markdone' && !locker ? markDone('btn') : null,
      canRename(job) ? el('button', { class: 'btn', type: 'button', onClick: () => renameJob(job) }, 'Μετονομασία…') : null,
      el('button', { class: 'btn btn-danger', type: 'button', onClick: () => discardJob(job) }, 'Αφαίρεση'),
      history[0]),
    history[1],
    !locker && primary !== 'override' ? overrideForm() : null);

  const tech = job.error_code || (job.error_message && job.error_message !== job.error_code)
    ? keptDetails(techOpen, job.id, 'tech', 'Τεχνικές λεπτομέρειες',
      job.error_code ? el('p', { class: 'mono' }, `Κωδικός: ${job.error_code}`) : null,
      job.error_message && job.error_message !== job.error_code
        ? el('p', { class: 'mono wl-cause' }, `Αιτία: ${job.error_message}`)
        : null)
    : null;

  return el('div', { class: 'card attention stack', 'data-job': String(job.id) },
    el('div', { class: 'job-head' },
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
      el('div', { class: 'row tight' },
        job.cleared_at ? el('span', { class: 'badge', title: 'Κρύφτηκε από τη λίστα· δεν σβήστηκε' }, 'κρυμμένο') : null,
        statusBadge(job),
      ),
    ),
    el('p', { class: 'wl-why' }, whyText(job)),
    main,
    canRename(job) ? renameFormFor(job) : null,
    more,
    tech,
  );
}

const retryJob = (job) => deskRetry(job, refresh);
const overrideJob = (job, url) => deskOverride(job, url, refresh);
const discardJob = (job) => deskDiscard(job, refresh);
const cancelJob = (job) => deskCancel(job, refresh);
/** «Μετονομασία…»: open (or close) the card's own field (P12.4); the
 *  refresh redraws the card with it, and the field takes the focus. */
let focusAfterSync = null;
const renameJob = (job) => {
  if (renameOpen.has(job.id)) {
    renameOpen.delete(job.id);
  } else {
    renameOpen.add(job.id);
    focusAfterSync = `rename-${job.id}`;
  }
  refresh();
};
const renameFormFor = (job) => (renameOpen.has(job.id) ? renameForm(job, refresh, refresh) : null);
const markDoneJob = (job) => deskMarkDone(job, refresh);

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
// Bumped whenever groupNames is replaced: cards that show a group's name are
// kept between refreshes, so their signatures carry it.
let groupGen = 0;
function groupName(code) {
  return groupNames.get(code) || code;
}

async function loadFilters() {
  try {
    const [g, j] = await Promise.all([api('/api/groups'), api('/api/journalists')]);
    groupNames = new Map((g.groups || []).map((x) => [x.code, x.name]));
    groupGen += 1;
    journalists = j.journalists || [];
  } catch { /* the lists show codes and the filter stays at "all" */ }
  const surnames = journalists.map((x) => x.surname).filter(Boolean).sort();
  render(filterSelect,
    el('option', { value: '' }, 'Όλοι οι δημοσιογράφοι'),
    surnames.map((name) => el('option', { value: name }, name)),
  );
  render(reviewJournalist,
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
  const jobs = visible('completed', data.jobs || []);
  renderOffers('completed-offers', jobs);
  if (jobs.length === 0) {
    const filtered = searchInput.value.trim() || filterSelect.value || groupSelect.value;
    completedList.cards.clear();
    render('completed-body', el('tr', {},
      el('td', { colspan: '6', class: 'empty' }, filtered ? 'Τίποτα δεν ταιριάζει με την αναζήτηση.' : 'Δεν έχει παραδοθεί τίποτα ακόμη.')));
  } else {
    syncKeyedList(completedList, document.getElementById('completed-body'), jobs, {
      build: completedRow,
      sig: (job) => JSON.stringify([job, groupGen]),
    });
  }
  pager('completed-pager', pages.completed, data.total || 0, loadCompleted);
}

function completedRow(job) {
  const href = safeHref(job.url);
  return el('tr', {},
    el('td', {},
      el('div', { class: 'strong' }, deliveredName(job), limitNote(job),
        // No file was made here: the name is the slug, not a file in the watchfolder (P7.16).
        job.status === 'COMPLETED_MANUAL'
          ? el('span', { class: 'badge', style: 'margin-left:8px', title: 'Το βίντεο μπήκε στο Dalet από το MCR, όχι από εδώ' }, 'χειροκίνητα')
          : null),
      el('div', { class: 'note mono', title: job.url, style: 'max-width:420px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap' }, job.url),
      mailLink(job),
      autoDeliveredNode(job, refresh),
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
  if (!(await confirmDialog({
    title: `Διαγραφή του δημοσιογράφου ${surname};`,
    body: 'Τα βίντεο που έχουν ήδη παραδοθεί κρατούν τα ονόματά τους.',
    confirm: 'Διαγραφή',
    danger: true,
  }))) return;
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

initInbox({ onCounts: showCounts, refresh: () => refresh(), groupName, onLoadError: failed });
// Jingle and notification when every video of an email has finished (P5.4).
initNotify(document.querySelector('.topbar-status'), (id) => {
  const cut = id.indexOf(':');
  switchTab('email');
  openMail(id.slice(0, cut), id.slice(cut + 1));
}, () => switchTab('review'));
loadFilters();
switchTab(currentTab);

/* ------------------------------------------------------------------ *
 * Videos that are late (P11.3b)
 * ------------------------------------------------------------------ */

/* A strip above the tabs, on every tab: one row per video that is waiting or
 * running longer than the late limit, with the reason, and «Τώρα» for a video
 * that only waits out a retry's back-off. Fetched with every refresh and
 * reconciled by job id (syncKeyedList), never rebuilt. */
const LATE_ROWS = 5;
const URGENT_PRIORITY = 10; // omni_core::models::URGENT_PRIORITY_BOOST
const lateStrip = document.getElementById('late-strip');
const lateList = { cards: new Map(), days: new Map() };
const lateRows = el('div', { class: 'late-rows' });
const lateMore = el('p', { class: 'late-more', hidden: true });
if (lateStrip) lateStrip.append(el('p', { class: 'late-title' }, 'Καθυστερούν'), lateRows, lateMore);

/** «Τώρα»: the next free worker takes the video at once. The server says no
 *  (409) when it started meanwhile; either way the strip is refreshed. */
async function runNow(job, button) {
  button.disabled = true;
  try {
    await api(`/api/jobs/${job.id}/now`, { method: 'POST' });
  } catch (e) {
    toast(e.message, 'bad');
    button.disabled = false; // the row may stay as it is: let the operator try again
  }
  reloadAll();
}

function lateRow(job) {
  return el('div', { class: 'late-row', 'data-id': String(job.id) },
    el('span', { class: 'late-name' }, fileName(job)),
    Number(job.priority) >= URGENT_PRIORITY ? el('span', { class: 'badge warn' }, 'Επείγον') : null,
    el('span', { class: 'late-reason' }, lateReason(job)),
    el('span', { class: 'late-age' }, arrivedAgo(job)),
    // A back-off, or a stream still live waiting for its next check (P11.7).
    job.wait_reason === 'retry' || job.wait_reason === 'live'
      ? el('button', {
          class: 'btn btn-warn',
          type: 'button',
          title: 'Να δοκιμάσει το σύστημα αμέσως, χωρίς να περιμένει την επόμενη προσπάθεια',
          onClick: (ev) => runNow(job, ev.currentTarget),
        }, 'Τώρα')
      : null,
  );
}

/** A row's signature: what it shows, minute counters included, so a still row is left alone. */
const lateSig = (job) => JSON.stringify([job.id, fileName(job), job.priority, job.wait_reason, lateReason(job), arrivedAgo(job)]);

let lateBusy = false;
async function loadLate() {
  if (!lateStrip || lateBusy) return;
  lateBusy = true;
  try {
    let data;
    try {
      data = await api('/api/jobs?view=late');
    } catch (e) { failed(e); return; }
    const jobs = visible('late', data.jobs || []);
    const shown = jobs.slice(0, LATE_ROWS);
    const extra = Math.max(0, (Number(data.total) || jobs.length) - shown.length);
    // A button the operator tabbed to keeps its focus when its row is rebuilt.
    const focused = document.activeElement && lateStrip.contains(document.activeElement)
      ? document.activeElement.closest('.late-row')?.dataset.id : null;
    syncKeyedList(lateList, lateRows, shown, { build: lateRow, sig: lateSig });
    if (focused && !lateStrip.contains(document.activeElement)) {
      lateRows.querySelector(`.late-row[data-id="${CSS.escape(focused)}"] button`)?.focus({ preventScroll: true });
    }
    const more = extra > 0 ? `και άλλα ${extra}` : '';
    if (lateMore.textContent !== more) lateMore.textContent = more;
    if (lateMore.hidden !== (extra === 0)) lateMore.hidden = extra === 0;
    if (lateStrip.hidden !== (shown.length === 0)) lateStrip.hidden = shown.length === 0;
  } finally {
    lateBusy = false;
  }
}

/* Live updates (P1.10). The server streams one JSON event per change; a job
 * event redraws that job's card, anything else (or anything unreadable)
 * reloads the open list, at most once a second. The 5 s poll stays underneath,
 * always. */
const reloadAll = () => { refresh(); loadStatus(); loadLate(); };
let reloadTimer = null;
function reloadSoon() {
  if (reloadTimer) return;
  reloadTimer = setTimeout(() => { reloadTimer = null; reloadAll(); }, 1000);
}

function onLiveMessage(message) {
  let ev = null;
  try { ev = JSON.parse(message.data); } catch { /* not JSON: reload */ }
  if (!ev || ev.type !== 'job' || typeof ev.id !== 'number') { reloadSoon(); return; }
  const entry = currentTab === 'queue' ? liveCards.get(ev.id) : null;
  const knownStage = entry ? entry.job.stage : seenStage.get(ev.id);
  if (typeof ev.stage === 'string' && ev.status === 'RUNNING') seenStage.set(ev.id, ev.stage);
  else seenStage.delete(ev.id);
  if (entry && entry.node.isConnected && applyJobEvent(entry.job, ev)) {
    const node = activeCard(entry.job);
    entry.node.replaceWith(node);
    entry.node = node;
    // Keep the reconciler's record of this card in step with what is on screen.
    queueList.cards.set(entry.job.id, { node, sig: queueSig(entry.job) });
    return;
  }
  // A progress tick for a card that is not on screen: the 5 s poll covers it.
  if (ev.status === 'RUNNING' && typeof ev.stage === 'string' && ev.stage === knownStage) return;
  reloadSoon();
}

// The stage each running job was last seen in, to tell a progress tick from a stage change.
const seenStage = new Map();

let liveSource = null;
try {
  liveSource = new EventSource('/api/events');
  liveSource.onmessage = onLiveMessage;
} catch { /* polling alone */ }
// Events are an accelerator; the poll stays the source of truth (new, requeued
// and reaped jobs and health changes send none), so it never depends on the stream.
setInterval(reloadAll, 5000);
reloadAll();

/* MCR panel behaviour (plan P2.3, P2.5, P7.1).
 *
 * Every value rendered here -- slug, url, error_message, journalist -- can
 * originate in an email sent to the ingest address. Nothing in this file
 * builds HTML from a string; `el()` creates nodes and text goes through
 * `textContent`, so a job whose URL is `"><img src=x onerror=...>` renders as
 * that exact text in the operator's browser and nothing else happens.
 *
 * The desk is written for operators who are not technical and for people on
 * their first shift: every list says what it holds, every button says what it
 * does, and anything that cannot be undone asks first, in plain words.
 */

import {
  api, el, render, live, toast, fmtTime, fmtDuration, logout, safeHref,
} from '/static/app.js?v=3';

let currentTab = 'queue';
let journalists = [];

/* ------------------------------------------------------------------ *
 * Words
 * ------------------------------------------------------------------ */

/** What a job is doing, for a person. */
const STAGE_TEXT = {
  QUEUED: 'Waiting its turn',
  EXTRACT: 'Finding the video on the page',
  DOWNLOAD: 'Downloading',
  PROBE: 'Checking the downloaded file',
  TRANSCODE: 'Converting to the broadcast format',
  REWRAP: 'Packaging as MXF',
  VERIFY: 'Checking the broadcast format',
  DELIVER: 'Delivering to the Dalet watchfolder',
  ARCHIVE: 'Archiving',
  DONE: 'Finishing',
};
const STAGE_ORDER = ['EXTRACT', 'DOWNLOAD', 'PROBE', 'TRANSCODE', 'REWRAP', 'VERIFY', 'DELIVER'];

function stageLine(job) {
  if (job.status === 'PENDING') return 'Waiting its turn';
  const text = STAGE_TEXT[job.stage] || 'Working';
  const step = STAGE_ORDER.indexOf(job.stage);
  return step >= 0 ? `Step ${step + 1} of ${STAGE_ORDER.length} · ${text}` : text;
}

function statusBadge(job) {
  switch (job.status) {
    case 'PENDING': return el('span', { class: 'badge' }, 'Waiting');
    case 'RUNNING': return el('span', { class: 'badge info' }, 'Working');
    case 'COMPLETED': return el('span', { class: 'badge ok' }, 'Delivered');
    case 'COMPLETED_MANUAL': return el('span', { class: 'badge ok' }, 'Delivered by hand');
    case 'MANUAL_DOWNLOAD': return el('span', { class: 'badge warn' }, 'Download by hand');
    case 'FAILED': return el('span', { class: 'badge bad' }, 'Failed');
    default: return el('span', { class: 'badge warn' }, 'Needs attention');
  }
}

function fileName(job) {
  return `${job.slug}.mxf`;
}

/** The delivered file's name, which differs from the slug when a file of
 *  the same name was already in the watchfolder (`_2`). */
function deliveredName(job) {
  if (!job.file_path) return fileName(job);
  const parts = String(job.file_path).split(/[\\/]/);
  return parts[parts.length - 1] || fileName(job);
}

/* ------------------------------------------------------------------ *
 * Tabs
 * ------------------------------------------------------------------ */

const TABS = ['queue', 'review', 'completed', 'journalists', 'manual'];

function switchTab(name) {
  currentTab = name;
  for (const tab of TABS) {
    const button = document.querySelector(`.tab[data-tab="${tab}"]`);
    const section = document.getElementById(`tab-${tab}`);
    if (button) button.setAttribute('aria-selected', String(tab === name));
    if (section) section.hidden = tab !== name;
  }
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
  tools: 'Tools',
  encoder: 'Encoder',
  watchfolder: 'Watchfolder',
  browser: 'Browser',
  disk: 'Disk',
  queue: 'Queue',
  selfcheck: 'Self-check',
};

async function loadStatus() {
  let data;
  try {
    data = await api('/api/system/status');
  } catch {
    setStatus('mail', 'Mail: unknown', 'bad', '');
    setStatus('llm', 'LLM: unknown', 'bad', '');
    return;
  }

  const checks = data.checks || {};

  // The detail goes in the tooltip, so the bar stays readable but the reason
  // is one hover away. These used to be the string literals "Active" and
  // "Ready", which said the mailbox was fine while it was refusing the
  // password (defect W-09).
  const mail = checks.mail || { state: 'ok' };
  setStatus('mail', `Mail: ${label(mail)}`, DOT[mail.state] || '', mail.detail || '');

  const llm = checks.llm || { state: 'ok' };
  setStatus('llm', `LLM: ${label(llm)}`, DOT[llm.state] || '', llm.detail || '');

  const free = data.disk?.watchfolder?.free_gb;
  const storage = document.getElementById('storage-status');
  storage.textContent =
    free === undefined || free === null
      ? 'Watchfolder: unknown'
      : `Watchfolder: ${free.toFixed(1)} GB free`;
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
      el('strong', {}, data.status === 'down' ? 'Ingest is blocked. ' : 'Attention. '),
      problems.join(' · '),
      el('span', { class: 'note', style: 'display:block;margin-top:6px' },
        'Tell the engineer on call; the Administration page has the detail.'),
    ));
  }
}

function label(check) {
  switch (check.state) {
    case 'ok': return 'OK';
    case 'degraded': return 'degraded';
    case 'down': return 'down';
    default: return 'unknown';
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

/** "Showing 26–50 of 132  ‹ Previous  Page 2 of 6  Next ›". Hidden when
 *  everything fits on one page. */
function pager(targetId, state, total, onChange) {
  const pageCount = Math.max(1, Math.ceil(total / state.perPage));
  if (state.page > pageCount) state.page = pageCount;
  if (total <= state.perPage) {
    render(targetId);
    return;
  }
  const first = (state.page - 1) * state.perPage + 1;
  const last = Math.min(total, state.page * state.perPage);
  const go = (p) => { state.page = p; onChange(); window.scrollTo({ top: 0, behavior: 'smooth' }); };
  render(targetId, el('div', { class: 'section-head card', style: 'padding:10px 14px' },
    el('span', { class: 'note' }, `Showing ${first}–${last} of ${total}`),
    el('div', { class: 'row tight' },
      el('button', { class: 'btn', type: 'button', disabled: state.page <= 1, onClick: () => go(1) }, '« First'),
      el('button', { class: 'btn', type: 'button', disabled: state.page <= 1, onClick: () => go(state.page - 1) }, '‹ Previous'),
      el('span', { class: 'note', style: 'padding:0 8px' }, `Page ${state.page} of ${pageCount}`),
      el('button', { class: 'btn', type: 'button', disabled: state.page >= pageCount, onClick: () => go(state.page + 1) }, 'Next ›'),
      el('button', { class: 'btn', type: 'button', disabled: state.page >= pageCount, onClick: () => go(pageCount) }, 'Last »'),
    ),
  ));
}

async function fetchPage(view, extra = {}) {
  const state = pages[view];
  const params = new URLSearchParams({ view, page: String(state.page), per_page: String(state.perPage), ...extra });
  return api(`/api/jobs?${params}`);
}

function showCounts(counts) {
  if (!counts) return;
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
    ? `Clear delivered from this list (${counts.finished})`
    : 'Clear delivered from this list';
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
      if (currentTab === 'review') await loadReview();
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

/* ------------------------------------------------------------------ *
 * Other videos found in an article
 * ------------------------------------------------------------------ */

/* Videos the sniffer found in a submitted article and left for MCR to
 * decide (raw page streams, and platform posts beyond the automatic limit).
 * Platform posts within the limit were already queued as 1B, 1C, … */
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
    el('h3', { class: 'job-name' }, 'More videos were found in these articles'),
    el('p', { class: 'note', style: 'margin:0' },
      'They were not added on their own. Open one to look, and add it if it belongs to the story.'),
    ...withOffers.flatMap((job) => offersOf(job).map((offer) => {
      const href = safeHref(offer.url);
      return el('div', { class: 'job-head', style: 'border-top:1px solid var(--line);padding-top:10px' },
        el('div', {},
          el('p', { class: 'job-meta' },
            `From job #${job.id} (${fileName(job)}) — would be ${offer.index_str}_${job.journalist}_${job.keyword}.mxf`),
          el('p', { class: 'job-url', title: offer.url }, offer.url),
        ),
        el('div', { class: 'row tight' },
          href ? el('a', { class: 'btn', href, target: '_blank', rel: 'noopener noreferrer' }, 'Open') : null,
          el('button', {
            class: 'btn btn-warn',
            type: 'button',
            onClick: () => queueOffer(job.id, offer),
          }, `Add as ${offer.index_str}`),
        ),
      );
    })),
  ));
}

async function queueOffer(id, offer) {
  try {
    const r = await api(`/api/jobs/${id}/offers/queue`, { method: 'POST', body: { url: offer.url } });
    toast(`Added as ${r.index_str} (job #${r.job_id}).`, 'ok');
    refresh();
  } catch (e) { toast(e.message, 'bad'); }
}

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
      'Nothing is waiting. New links from email appear here on their own.'));
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
        ),
      ),
      el('div', { class: 'row tight' },
        statusBadge(job),
        el('button', {
          class: 'btn btn-danger',
          type: 'button',
          title: 'Take this video off the queue',
          onClick: () => discardJob(job),
        }, 'Remove'),
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
        el('span', {}, `Journalist: ${job.journalist}`,
          job.group_code ? el('span', { class: 'badge info', style: 'margin-left:8px', title: groupName(job.group_code) }, job.group_code) : null),
        el('span', {}, running && job.eta && job.eta !== '--:--' ? `About ${job.eta} left` : ''),
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
            `Delivered ${fmtTime(job.completed_at || job.updated_at)} · ${fmtDuration(job.duration_secs)} · ${job.journalist}`),
        ),
      ),
      el('div', { class: 'row tight' },
        statusBadge(job),
        el('button', { class: 'btn', type: 'button', onClick: () => redownload(job) }, 'Download again'),
      ),
    ),
  );
}

document.getElementById('clear-finished').addEventListener('click', async (event) => {
  const n = Number(event.currentTarget.dataset.count || 0);
  if (!n) return;
  if (!confirm(
    `Take the ${n} delivered video(s) off the live queue?\n\n` +
    'Nothing is deleted. They stay under Completed, where you can download them again.',
  )) return;
  try {
    const r = await api('/api/jobs/clear-finished', { method: 'POST' });
    toast(`${r.cleared} delivered video(s) moved off the live queue. They are under Completed.`, 'ok');
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
    render('review-cards', el('div', { class: 'card center' }, 'Nothing needs attention.'));
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
            `Journalist: ${job.journalist} · since ${fmtTime(job.updated_at)}`),
        ),
      ),
      statusBadge(job),
    ),
    el('div', { class: 'reason' },
      el('strong', { style: 'display:block' }, job.hint
        || (locker
          ? 'A file-transfer link: download the file from the link by hand and drop it in Dalet.'
          : 'The video could not be captured automatically.')),
      job.error_code ? el('span', { class: 'note mono' }, `Code: ${job.error_code}`) : null,
    ),
    el('div', { class: 'row' },
      href ? el('a', { class: 'btn', href, target: '_blank', rel: 'noopener noreferrer' }, 'Open the link') : null,
      locker ? null : el('button', { class: 'btn btn-primary', type: 'button', onClick: () => retryJob(job) }, 'Try again'),
      el('button', { class: 'btn btn-danger', type: 'button', onClick: () => discardJob(job) }, 'Remove'),
    ),
    locker ? null : el('div', {},
      el('label', { for: `override-${job.id}` },
        'Or paste a different link to the same video (the post itself, or a direct .mp4 / .m3u8 link):'),
      el('div', { class: 'row' },
        el('div', { class: 'grow' }, input),
        el('button', {
          class: 'btn btn-warn',
          type: 'button',
          onClick: () => overrideJob(job, input.value),
        }, 'Use this link'),
      ),
    ),
  );
}

async function retryJob(job) {
  try {
    await api(`/api/jobs/${job.id}/retry`, { method: 'POST' });
    toast(`${fileName(job)} is back in the live queue.`, 'ok');
    refresh();
  } catch (e) { toast(e.message, 'bad'); }
}

async function overrideJob(job, url) {
  const trimmed = (url || '').trim();
  if (!trimmed) { toast('Paste a link first.', 'bad'); return; }
  try {
    await api(`/api/jobs/${job.id}/override`, { method: 'POST', body: { url: trimmed } });
    toast(`${fileName(job)} is back in the live queue with the new link.`, 'ok');
    refresh();
  } catch (e) { toast(e.message, 'bad'); }
}

async function discardJob(job) {
  if (!confirm(
    `Remove ${fileName(job)} (job #${job.id})?\n\n` +
    'It is taken off the lists for good. A file already delivered to Dalet is not touched.',
  )) return;
  try {
    await api(`/api/jobs/${job.id}/discard`, { method: 'POST' });
    toast(`Job #${job.id} removed.`, 'ok');
    refresh();
  } catch (e) { toast(e.message, 'bad'); }
}

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
    el('option', { value: '' }, 'All journalists'),
    surnames.map((name) => el('option', { value: name }, name)),
  );
  render(groupSelect,
    el('option', { value: '' }, 'All groups'),
    el('option', { value: '-' }, 'No group'),
    [...groupNames.keys()].sort().map((code) => el('option', { value: code }, `${code} — ${groupName(code)}`)),
  );
  render('m-journalists', surnames.map((name) => el('option', { value: name })));
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
      el('td', { colspan: '6', class: 'empty' }, filtered ? 'Nothing matches the search.' : 'Nothing delivered yet.')));
  } else {
    render('completed-body', jobs.map(completedRow));
  }
  pager('completed-pager', pages.completed, data.total || 0, loadCompleted);
}

function completedRow(job) {
  const href = safeHref(job.url);
  return el('tr', {},
    el('td', {},
      el('div', { class: 'strong' }, deliveredName(job)),
      el('div', { class: 'note mono', title: job.url, style: 'max-width:420px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap' }, job.url),
    ),
    el('td', {}, el('span', { class: 'badge info' }, job.journalist)),
    el('td', { title: job.group_code ? groupName(job.group_code) : '' }, job.group_code || '—'),
    // Timestamps are Option on the wire: a missing one renders blank, never
    // as "now" (which is what the old archive showed for every job).
    el('td', { class: 'num' }, fmtTime(job.completed_at || job.delivered_at)),
    el('td', {}, fmtDuration(job.duration_secs)),
    el('td', { class: 'right' },
      el('div', { class: 'row tight', style: 'justify-content:flex-end;flex-wrap:nowrap' },
        href ? el('a', { class: 'btn', href, target: '_blank', rel: 'noopener noreferrer', title: 'Open the original link' }, 'Open link') : null,
        el('button', { class: 'btn btn-primary', type: 'button', onClick: () => redownload(job) }, 'Download again'),
      ),
    ),
  );
}

async function redownload(job) {
  const name = deliveredName(job);
  if (!confirm(
    `Download ${name} again?\n\n` +
    'The video is fetched from its link, converted and delivered to the watchfolder once more. ' +
    'If the old file is still there, the new one is saved next to it with a number added (…_2.mxf); nothing is overwritten.',
  )) return;
  try {
    await api(`/api/jobs/${job.id}/redownload`, { method: 'POST' });
    toast(`${name} is back in the live queue.`, 'ok');
    refresh();
  } catch (e) { toast(e.message, 'bad'); }
}

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
      el('td', { colspan: '5', class: 'empty' }, 'No journalists configured.')));
    return;
  }

  render('journalists-body', journalists.map((j) => el('tr', {},
    el('td', { class: 'strong' }, j.surname),
    el('td', {}, j.full_name),
    el('td', { class: 'mono' }, (j.emails || []).join(', ')),
    el('td', { class: 'num' }, String(j.default_priority)),
    el('td', { class: 'right' },
      j.surname === 'MCR'
        // MCR is structural: the parser files every unresolved job under it
        // and delivery uses it as a folder name.
        ? el('span', { class: 'note' }, 'fallback')
        : el('button', {
            class: 'btn btn-icon btn-danger',
            type: 'button',
            title: `Delete ${j.surname}`,
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
    toast('Journalist saved.', 'ok');
    event.target.reset();
    document.getElementById('j-priority').value = '0';
    loadJournalists();
    loadFilters();
  } catch (e) { toast(e.message, 'bad'); }
});

async function deleteJournalist(surname) {
  if (!confirm(`Delete journalist ${surname}?\n\nVideos already delivered keep their names.`)) return;
  try {
    await api(`/api/journalists/${encodeURIComponent(surname)}`, { method: 'POST' });
    toast(`${surname} deleted.`, 'ok');
    loadJournalists();
    loadFilters();
  } catch (e) { toast(e.message, 'bad'); }
}

/* ------------------------------------------------------------------ *
 * Add a link
 * ------------------------------------------------------------------ */

const manualFields = ['m-index', 'm-journalist', 'm-keyword'].map((id) => document.getElementById(id));
function showManualPreview() {
  const [index, journalist, keyword] = manualFields.map((f) => f.value.trim().toUpperCase());
  document.getElementById('m-preview').textContent = index && journalist && keyword
    ? `The file will be called ${index}_${journalist}_${keyword}.mxf`
    : '';
}
for (const f of manualFields) f.addEventListener('input', showManualPreview);

document.getElementById('manual-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  try {
    const result = await api('/api/jobs', {
      method: 'POST',
      body: {
        url: document.getElementById('m-url').value.trim(),
        journalist: document.getElementById('m-journalist').value.trim(),
        keyword: document.getElementById('m-keyword').value.trim(),
        index_str: document.getElementById('m-index').value.trim(),
        priority: document.getElementById('m-priority').checked ? 10 : 0,
      },
    });
    toast(`Added: ${result.slug}.mxf`, 'ok');
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

loadFilters();
live(() => { refresh(); loadStatus(); });

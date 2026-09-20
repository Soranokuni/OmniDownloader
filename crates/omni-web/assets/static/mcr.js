/* MCR panel behaviour (plan P2.3, P2.5).
 *
 * Every value rendered here -- slug, url, error_message, journalist -- can
 * originate in an email sent to the ingest address. Nothing in this file
 * builds HTML from a string; `el()` creates nodes and text goes through
 * `textContent`, so a job whose URL is `"><img src=x onerror=...>` renders as
 * that exact text in the operator's browser and nothing else happens.
 */

import {
  api, el, render, live, toast, fmtTime, fmtDuration, statusClass, logout,
} from '/static/app.js?v=2';

let jobs = [];
let journalists = [];
let currentTab = 'queue';

/* ------------------------------------------------------------------ *
 * Tabs
 * ------------------------------------------------------------------ */

const TABS = ['queue', 'review', 'archive', 'journalists', 'manual'];

function switchTab(name) {
  currentTab = name;
  for (const tab of TABS) {
    const button = document.querySelector(`.tab[data-tab="${tab}"]`);
    const section = document.getElementById(`tab-${tab}`);
    if (button) button.setAttribute('aria-selected', String(tab === name));
    if (section) section.hidden = tab !== name;
  }
  if (name === 'journalists') loadJournalists();
  if (name === 'archive') renderArchive();
}

for (const button of document.querySelectorAll('.tab[data-tab]')) {
  button.addEventListener('click', () => switchTab(button.dataset.tab));
}

/* ------------------------------------------------------------------ *
 * Status bar
 * ------------------------------------------------------------------ */

async function loadStatus() {
  try {
    const data = await api('/api/system/status');
    setStatus('mail', `Mail: ${data.mail_status}`, data.mail_status === 'Active' ? 'ok' : 'warn');
    setStatus('llm', `LLM: ${data.llm_status}`, data.llm_status === 'Ready' ? 'ok' : 'warn');
    const free = Number(data.free_disk_gb) || 0;
    document.getElementById('storage-status').textContent =
      `Watchfolder: ${free.toFixed(1)} GB free`;
  } catch {
    setStatus('mail', 'Mail: unknown', 'bad');
    setStatus('llm', 'LLM: unknown', 'bad');
  }
}

function setStatus(prefix, text, dotClass) {
  document.getElementById(`${prefix}-status`).textContent = text;
  document.getElementById(`${prefix}-dot`).className = `dot ${dotClass}`;
}

/* ------------------------------------------------------------------ *
 * Jobs
 * ------------------------------------------------------------------ */

async function loadJobs() {
  try {
    const data = await api('/api/jobs');
    jobs = data.jobs || [];
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }
  renderQueue();
  renderReview();
  if (currentTab === 'archive') renderArchive();
}

const NEEDS_REVIEW = new Set(['REQUIRES_REVIEW', 'MANUAL_DOWNLOAD']);

function renderQueue() {
  const active = jobs.filter((j) => !NEEDS_REVIEW.has(j.status));
  if (active.length === 0) {
    render('queue-cards', el('div', { class: 'card center' },
      'Ingest queue is clear. No active jobs.'));
    return;
  }
  render('queue-cards', active.map(jobCard));
}

function jobCard(job) {
  const progress = Number(job.progress) || 0;
  const stageText =
    job.status === 'DOWNLOADING' ? `Downloading${job.speed ? ` @ ${job.speed}` : ''}`
    : job.status === 'TRANSCODING' ? 'Transcoding to XDCAM HD422'
    : job.status;

  return el('div', { class: 'card stack' },
    el('div', { class: 'job-head' },
      el('div', { class: 'row', style: 'gap:10px;align-items:flex-start' },
        el('span', { class: 'job-id' }, String(job.id)),
        el('div', {},
          el('h3', { class: 'job-name' }, `${job.slug}.mxf`),
          el('p', { class: 'job-url', title: job.url }, job.url),
        ),
      ),
      el('div', { class: 'row tight' },
        el('span', { class: `badge ${statusClass(job.status)}` }, job.status),
        el('button', {
          class: 'btn btn-icon btn-danger',
          type: 'button',
          title: 'Discard',
          onClick: () => discardJob(job.id),
        }, '✕'),
      ),
    ),
    el('div', {},
      el('div', { class: 'job-meta' },
        el('span', {}, stageText),
        el('span', {}, `${progress.toFixed(1)}%`),
      ),
      el('div', { class: 'progress' },
        el('span', { style: `width:${Math.max(0, Math.min(100, progress))}%` })),
      el('div', { class: 'job-meta' },
        el('span', {}, `Journalist: ${job.journalist}`),
        el('span', {}, job.eta ? `ETA ${job.eta}` : ''),
      ),
    ),
  );
}

function renderReview() {
  const review = jobs.filter((j) => NEEDS_REVIEW.has(j.status));
  const badge = document.getElementById('review-count');
  badge.hidden = review.length === 0;
  badge.textContent = String(review.length);

  if (review.length === 0) {
    render('review-cards', el('div', { class: 'card center' },
      'No jobs require operator review.'));
    return;
  }
  render('review-cards', review.map(reviewCard));
}

function reviewCard(job) {
  const input = el('input', {
    type: 'url',
    class: 'mono',
    id: `override-${job.id}`,
    value: job.url,
  });

  return el('div', { class: 'card attention stack' },
    el('div', { class: 'job-head', style: 'border-bottom:1px solid var(--line);padding-bottom:12px' },
      el('div', {},
        el('h3', { class: 'job-name' }, `${job.slug}.mxf`),
        el('p', { class: 'job-url', title: job.url }, job.url),
      ),
      el('span', { class: 'badge warn' },
        job.status === 'MANUAL_DOWNLOAD' ? 'File locker intercepted' : 'Requires review'),
    ),
    el('div', { class: 'reason' },
      job.error_message || 'The video stream could not be captured automatically.'),
    el('div', {},
      el('label', { for: `override-${job.id}` },
        'Direct stream (.m3u8 / .mp4) or a corrected page URL'),
      el('div', { class: 'row' },
        el('div', { class: 'grow' }, input),
        el('a', { class: 'btn', href: job.url, target: '_blank', rel: 'noopener noreferrer' },
          'Open link'),
        el('button', {
          class: 'btn btn-warn',
          type: 'button',
          onClick: () => overrideJob(job.id, input.value),
        }, 'Force ingest'),
        el('button', {
          class: 'btn btn-danger',
          type: 'button',
          onClick: () => discardJob(job.id),
        }, 'Discard'),
      ),
    ),
  );
}

async function overrideJob(id, url) {
  const trimmed = (url || '').trim();
  if (!trimmed) { toast('Enter a URL first.', 'bad'); return; }
  try {
    await api(`/api/jobs/${id}/override`, { method: 'POST', body: { url: trimmed } });
    toast(`Job #${id} re-queued.`, 'ok');
    loadJobs();
  } catch (e) { toast(e.message, 'bad'); }
}

async function discardJob(id) {
  if (!confirm(`Discard job #${id}?`)) return;
  try {
    await api(`/api/jobs/${id}/discard`, { method: 'POST' });
    toast(`Job #${id} discarded.`, 'ok');
    loadJobs();
  } catch (e) { toast(e.message, 'bad'); }
}

/* ------------------------------------------------------------------ *
 * Archive
 * ------------------------------------------------------------------ */

const filterSelect = document.getElementById('journalist-filter');
const searchInput = document.getElementById('archive-search');
filterSelect.addEventListener('change', renderArchive);
searchInput.addEventListener('input', renderArchive);

function renderArchive() {
  syncJournalistFilter();

  const wanted = filterSelect.value;
  const needle = searchInput.value.trim().toLowerCase();

  const rows = jobs.filter((job) => {
    if (wanted && job.journalist !== wanted) return false;
    if (!needle) return true;
    return [job.slug, job.url, job.journalist, job.keyword]
      .some((field) => String(field || '').toLowerCase().includes(needle));
  });

  if (rows.length === 0) {
    render('archive-body', el('tr', {},
      el('td', { colspan: '7', class: 'empty' }, 'No jobs match the filter.')));
    return;
  }

  render('archive-body', rows.map((job) => el('tr', {},
    el('td', { class: 'num' }, String(job.id)),
    el('td', { class: 'strong' }, `${job.slug}.mxf`),
    el('td', {}, el('span', { class: 'badge info' }, job.journalist)),
    el('td', {}, el('span', { class: `badge ${statusClass(job.status)}` }, job.status)),
    el('td', {}, fmtDuration(job.duration_secs)),
    // `media_format` is free text from ffprobe; it is a text node like
    // everything else here.
    el('td', { class: 'mono' }, job.media_format || '—'),
    // Timestamps are Option on the wire: a missing one renders blank, never
    // as "now" (which is what the old archive showed for every job).
    el('td', { class: 'num' }, fmtTime(job.updated_at)),
  )));
}

function syncJournalistFilter() {
  const names = [...new Set(jobs.map((j) => j.journalist).filter(Boolean))].sort();
  const current = filterSelect.value;
  render(filterSelect,
    el('option', { value: '' }, 'All journalists'),
    names.map((name) => el('option', { value: name, selected: name === current }, name)),
  );
  filterSelect.value = names.includes(current) ? current : '';
}

/* ------------------------------------------------------------------ *
 * Journalists
 * ------------------------------------------------------------------ */

async function loadJournalists() {
  try {
    const data = await api('/api/journalists');
    journalists = data.journalists || [];
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }

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
  } catch (e) { toast(e.message, 'bad'); }
});

async function deleteJournalist(surname) {
  if (!confirm(`Delete journalist ${surname}?`)) return;
  try {
    await api(`/api/journalists/${encodeURIComponent(surname)}`, { method: 'POST' });
    toast(`${surname} deleted.`, 'ok');
    loadJournalists();
  } catch (e) { toast(e.message, 'bad'); }
}

/* ------------------------------------------------------------------ *
 * Quick queue
 * ------------------------------------------------------------------ */

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
    toast(`Queued as ${result.slug}.mxf`, 'ok');
    event.target.reset();
    document.getElementById('m-journalist').value = 'MCR';
    document.getElementById('m-index').value = '1';
    switchTab('queue');
    loadJobs();
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

live(() => { loadJobs(); loadStatus(); });

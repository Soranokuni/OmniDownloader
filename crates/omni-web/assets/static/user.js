/* Journalist panel behaviour (plan P2.3, P2.5), in Greek. */

import {
  api, el, render, live, toast, fmtTime, statusClass, logout,
} from '/static/app.js?v=6';

/** A job's state, for the journalist who sent it. */
const STATUS_TEXT = {
  PENDING: 'Σε αναμονή',
  RUNNING: 'Σε επεξεργασία',
  COMPLETED: 'Παραδόθηκε',
  COMPLETED_MANUAL: 'Παραδόθηκε',
  REQUIRES_REVIEW: 'Το εξετάζει το MCR',
  MANUAL_DOWNLOAD: 'Το κατεβάζει το MCR',
  AWAITING_APPROVAL: 'Το βλέπει το MCR',
  FAILED: 'Απέτυχε',
  CANCELLED: 'Ακυρώθηκε',
};

/** Where a working video is, in words a journalist knows. */
const STAGE_TEXT = {
  QUEUED: 'Στην ουρά',
  EXTRACT: 'Εύρεση βίντεο',
  DOWNLOAD: 'Κατεβαίνει',
  PROBE: 'Έλεγχος',
  TRANSCODE: 'Μετατροπή',
  REWRAP: 'Συσκευασία',
  VERIFY: 'Έλεγχος',
  DELIVER: 'Παράδοση',
  ARCHIVE: 'Παράδοση',
  DONE: 'Έτοιμο',
};

/* ------------------------------------------------------------------ *
 * Identity
 * ------------------------------------------------------------------ */

async function loadMe() {
  try {
    const me = await api('/api/auth/me');
    const label = me.full_name || me.email;
    // `full_name` is set by an administrator and `email` comes from the same
    // form; both are rendered as text, never as markup.
    document.getElementById('user-display').textContent =
      me.journalist_surname ? `${label} · ${me.journalist_surname}` : label;
  } catch { /* the api helper already redirects on 401 */ }
}

/* ------------------------------------------------------------------ *
 * Submissions
 * ------------------------------------------------------------------ */

async function loadMyJobs() {
  let jobs = [];
  try {
    const data = await api('/api/jobs/mine');
    jobs = data.jobs || [];
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }

  if (jobs.length === 0) {
    render('my-jobs', el('div', { class: 'card center' },
      'Δεν έχετε στείλει κάτι ακόμη. Επικολλήστε έναν σύνδεσμο παραπάνω και πηγαίνει κατευθείαν στο MCR.'));
    return;
  }

  render('my-jobs', jobs.map(jobCard));
}

function jobCard(job) {
  const progress = Number(job.progress) || 0;
  const done = job.status === 'COMPLETED' || job.status === 'COMPLETED_MANUAL';
  const finished = done || job.status === 'CANCELLED' || job.status === 'FAILED';

  return el('div', { class: 'card stack' },
    el('div', { class: 'job-head' },
      el('div', { class: 'row', style: 'gap:10px;align-items:flex-start' },
        el('span', { class: 'job-id' }, String(job.id)),
        el('div', {},
          el('h3', { class: 'job-name' }, `${job.slug}.mxf`),
          el('p', { class: 'job-url', title: job.url }, job.url),
        ),
      ),
      el('span', { class: `badge ${statusClass(job.status)}` }, STATUS_TEXT[job.status] || job.status),
    ),
    !finished && el('div', { class: 'progress' },
      el('span', { style: `width:${Math.max(0, Math.min(100, progress))}%` })),
    (job.status === 'REQUIRES_REVIEW' || job.status === 'FAILED')
      && el('div', { class: 'reason' }, el('strong', { style: 'display:block' },
        'Δεν κατέβηκε αυτόματα· το MCR το έχει δει. Αν έχετε άλλον σύνδεσμο για το ίδιο βίντεο, στείλτε τον.')),
    done && job.delivered_name
      && el('div', { class: 'job-meta' }, el('span', {}, `Αρχείο: ${job.delivered_name}`)),
    el('div', { class: 'job-meta' },
      el('span', {}, `Στάδιο: ${STAGE_TEXT[job.stage] || '—'}`),
      el('span', {}, `Στάλθηκε: ${fmtTime(job.created_at)}`),
      el('span', {}, `Τελείωσε: ${fmtTime(job.completed_at)}`),
    ),
    el('div', { class: 'job-meta' },
      el('span', {}, job.notes ? `Σημείωση: ${job.notes}` : ''),
      job.status === 'PENDING'
        && el('button', { class: 'btn btn-danger', type: 'button', onclick: () => cancelJob(job.id) }, 'Ακύρωση'),
    ),
  );
}

async function cancelJob(id) {
  if (!window.confirm('Να ακυρωθεί αυτή η αποστολή;')) return;
  try {
    await api(`/api/user/jobs/${id}/cancel`, { method: 'POST', body: {} });
    toast('Η αποστολή ακυρώθηκε.', 'ok');
  } catch (e) { toast(e.message, 'bad'); }
  loadMyJobs();
}

const multi = document.getElementById('multi');
multi.addEventListener('change', () => {
  document.getElementById('single-box').hidden = multi.checked;
  document.getElementById('multi-box').hidden = !multi.checked;
  // A hidden required field would block the submit with no visible reason.
  document.getElementById('url').required = !multi.checked;
});

/** The submit call, kept apart from `api()` because a failure part-way through
 *  a list answers 500 with the links already queued, which `api()` would drop.
 *  Same CSRF header and credentials as `api()`. */
async function postSubmit(body) {
  let response;
  try {
    response = await fetch('/api/user/jobs', {
      method: 'POST',
      headers: { 'X-Omni-Request': '1', 'Content-Type': 'application/json' },
      credentials: 'same-origin',
      body: JSON.stringify(body),
    });
  } catch {
    throw new Error('Η υπηρεσία λήψης δεν απαντά. Ελέγξτε ότι τρέχει και ξαναδοκιμάστε.');
  }
  if (response.status === 401) { location.href = '/login'; throw new Error('Η σύνδεση έληξε. Συνδεθείτε ξανά.'); }
  let payload = null;
  try { payload = JSON.parse(await response.text()); } catch { payload = null; }
  if (!response.ok && !(payload && Array.isArray(payload.results))) {
    throw new Error((payload && payload.error && payload.error.message) || 'Το αίτημα απέτυχε.');
  }
  return { ok: response.ok, payload };
}

/** Show what was queued before the failure, the link that failed, and how many
 *  were not tried. */
function showPartialFailure(payload) {
  const rows = payload.results.map((r) => el('div', { class: 'job-meta' },
    el('span', {}, r.slug ? `${r.slug}.mxf` : '—'),
    el('span', {}, r.error ? r.error : (r.duplicate ? 'Υπάρχει ήδη στην ουρά' : 'Στάλθηκε στο MCR')),
  ));
  rows.push(el('div', { class: 'reason' }, `Δεν δοκιμάστηκαν: ${Number(payload.not_attempted) || 0}`));
  render('submit-result', rows);
  toast(payload.error.message, 'bad');
  loadMyJobs();
}

document.getElementById('submit-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const body = {
    keyword: document.getElementById('keyword').value.trim(),
    index_str: document.getElementById('index').value.trim(),
    notes: document.getElementById('notes').value.trim() || null,
    priority: document.getElementById('priority').checked,
  };
  if (multi.checked) {
    body.urls = document.getElementById('urls').value.split('\n').map((l) => l.trim()).filter(Boolean);
  } else {
    body.url = document.getElementById('url').value.trim();
  }
  try {
    const { ok, payload } = await postSubmit(body);
    if (!ok) { showPartialFailure(payload); return; }
    const results = payload;
    render('submit-result', results.map((r) => el('div', { class: 'job-meta' },
      el('span', {}, r.slug ? `${r.slug}.mxf` : '—'),
      el('span', {}, r.duplicate ? 'Υπάρχει ήδη στην ουρά' : 'Στάλθηκε στο MCR'),
    )));
    toast(results.length > 1 ? `Στάλθηκαν ${results.length} σύνδεσμοι στο MCR.` : (results[0].slug ? `Στάλθηκε στο MCR ως ${results[0].slug}.mxf` : 'Ο σύνδεσμος υπάρχει ήδη στην ουρά.'), 'ok');
    event.target.reset();
    document.getElementById('single-box').hidden = false;
    document.getElementById('multi-box').hidden = true;
    document.getElementById('url').required = true;
    document.getElementById('index').value = '1';
    loadMyJobs();
  } catch (e) { toast(e.message, 'bad'); }
});

document.getElementById('refresh-btn').addEventListener('click', loadMyJobs);

/* ------------------------------------------------------------------ *
 * Notify-on-completion preference
 * ------------------------------------------------------------------ */

const notify = document.getElementById('notify');

async function loadPrefs() {
  try {
    notify.checked = (await api('/api/user/prefs')).notify_on_completion === true;
  } catch { /* the api helper already redirects on 401 */ }
}

notify.addEventListener('change', async () => {
  const wanted = notify.checked;
  try {
    await api('/api/user/prefs', { method: 'PUT', body: { notify_on_completion: wanted } });
    toast('Η προτίμηση αποθηκεύτηκε.', 'ok');
  } catch (e) {
    notify.checked = !wanted;
    toast(e.message, 'bad');
  }
});

/* ------------------------------------------------------------------ *
 * Password
 * ------------------------------------------------------------------ */

const modal = document.getElementById('password-modal');
document.getElementById('password-btn').addEventListener('click', () => { modal.hidden = false; });
document.getElementById('pw-cancel').addEventListener('click', () => { modal.hidden = true; });
modal.addEventListener('click', (e) => { if (e.target === modal) modal.hidden = true; });

document.getElementById('password-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  try {
    await api('/api/auth/password', {
      method: 'POST',
      body: {
        current_password: document.getElementById('pw-current').value,
        new_password: document.getElementById('pw-new').value,
      },
    });
    // The server ended every session, including this one, so there is nothing
    // to stay on this page for.
    toast('Ο κωδικός άλλαξε. Συνδεθείτε ξανά…', 'ok');
    setTimeout(() => { location.href = '/login'; }, 1200);
  } catch (e) { toast(e.message, 'bad'); }
});

document.getElementById('logout-btn').addEventListener('click', logout);

loadMe();
loadPrefs();
/* The live feed fires on every server event, progress included; refresh at
 * most once every 2 s, and keep one trailing call so the last change shows. */
let lastRefresh = 0;
let trailing = null;
function refreshThrottled() {
  const wait = 2000 - (Date.now() - lastRefresh);
  if (wait <= 0) {
    lastRefresh = Date.now();
    loadMyJobs();
  } else if (trailing === null) {
    trailing = setTimeout(() => {
      trailing = null;
      lastRefresh = Date.now();
      loadMyJobs();
    }, wait);
  }
}
live(refreshThrottled, 8000);

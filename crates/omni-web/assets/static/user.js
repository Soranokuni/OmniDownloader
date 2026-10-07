/* Journalist panel behaviour (plan P2.3, P2.5), in Greek. */

import {
  api, el, render, live, toast, fmtTime, statusClass, logout,
} from '/static/app.js?v=5';

/** A job's state, for the journalist who sent it. */
const STATUS_TEXT = {
  PENDING: 'Σε αναμονή',
  RUNNING: 'Σε επεξεργασία',
  COMPLETED: 'Παραδόθηκε',
  COMPLETED_MANUAL: 'Παραδόθηκε',
  REQUIRES_REVIEW: 'Το εξετάζει το MCR',
  MANUAL_DOWNLOAD: 'Το κατεβάζει το MCR',
  FAILED: 'Απέτυχε',
  CANCELLED: 'Ακυρώθηκε',
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
    !done && el('div', { class: 'progress' },
      el('span', { style: `width:${Math.max(0, Math.min(100, progress))}%` })),
    (job.status === 'REQUIRES_REVIEW' || job.status === 'FAILED')
      && el('div', { class: 'reason' }, el('strong', { style: 'display:block' },
        'Δεν κατέβηκε αυτόματα· το MCR το έχει δει. Αν έχετε άλλον σύνδεσμο για το ίδιο βίντεο, στείλτε τον.')),
    el('div', { class: 'job-meta' },
      el('span', {}, job.notes ? `Σημείωση: ${job.notes}` : ''),
      el('span', {}, fmtTime(job.updated_at)),
    ),
  );
}

document.getElementById('submit-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  try {
    const result = await api('/api/jobs', {
      method: 'POST',
      body: {
        url: document.getElementById('url').value.trim(),
        keyword: document.getElementById('keyword').value.trim(),
        index_str: document.getElementById('index').value.trim(),
        notes: document.getElementById('notes').value.trim() || null,
        priority: document.getElementById('priority').checked ? 10 : 0,
      },
    });
    toast(`Στάλθηκε στο MCR ως ${result.slug}.mxf`, 'ok');
    event.target.reset();
    document.getElementById('index').value = '1';
    loadMyJobs();
  } catch (e) { toast(e.message, 'bad'); }
});

document.getElementById('refresh-btn').addEventListener('click', loadMyJobs);

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
live(loadMyJobs, 8000);

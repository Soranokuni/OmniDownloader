/* Journalist panel behaviour (plan P2.3, P2.5). Every word is in text.el.js (P12.7). */

import {
  api, el, render, live, fmtTime, logout,
} from '/static/app.js?v=6';
import { toast } from '/static/toast.js?v=7';
// A job's state, for the journalist who sent it: the `user` words of the
// one table the desk uses too (P12.1), in the same colours.
import { statusBadge } from '/static/status.js?v=7';
import { deferAction, hiddenNow } from '/static/undo.js?v=8';
import { t, tn, applyText } from '/static/text.js?v=6';

// The page's fixed words, from the dictionary (P12.10).
applyText();

/** Where a working video is, in words a journalist knows (`user.stage.<STAGE>`). */
const stageText = (stage) => (stage ? t(`user.stage.${stage}`) : '—');

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
    myJournalist = me.journalist_surname || '';
    showSlugPreview();
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
    render('my-jobs', el('div', { class: 'card center' }, t('user.empty')));
    return;
  }

  render('my-jobs', jobs.map(jobCard));
}

function jobCard(job) {
  const progress = Number(job.progress) || 0;
  const done = job.status === 'COMPLETED' || job.status === 'COMPLETED_MANUAL';
  // The bar only while the system works on it (P12.12): a job waiting for MCR
  // (review, approval, a file-locker link) is not moving, and a bar would say it is.
  const working = job.status === 'PENDING' || job.status === 'RUNNING';

  return el('div', { class: 'card stack' },
    el('div', { class: 'job-head' },
      el('div', { class: 'row head-row' },
        el('span', { class: 'job-id' }, String(job.id)),
        el('div', {},
          el('h3', { class: 'job-name' }, `${job.slug}.mxf`),
          el('p', { class: 'job-url', title: job.url }, job.url),
        ),
      ),
      statusBadge(job, 'user'),
    ),
    working && el('div', { class: 'progress' },
      el('span', { style: `width:${Math.max(0, Math.min(100, progress))}%` })),
    (job.status === 'REQUIRES_REVIEW' || job.status === 'FAILED')
      && el('div', { class: 'reason' }, el('strong', { class: 'block' }, t('user.not_auto'))),
    done && job.delivered_name
      && el('div', { class: 'job-meta' }, el('span', {}, t('user.file', { name: job.delivered_name }))),
    el('div', { class: 'job-meta' },
      el('span', {}, t('user.stage', { text: stageText(job.stage) })),
      el('span', {}, t('user.sent', { at: fmtTime(job.created_at) })),
      el('span', {}, t('user.finished', { at: fmtTime(job.completed_at) })),
    ),
    el('div', { class: 'job-meta' },
      el('span', {}, job.notes ? t('user.note', { text: job.notes }) : ''),
      job.status === 'PENDING' && hiddenNow(CANCELLING, job)
        ? el('span', { class: 'note' }, t('user.cancelling'))
        : job.status === 'PENDING'
          && el('button', { class: 'btn btn-danger', type: 'button', onclick: () => cancelJob(job) }, t('user.cancel')),
    ),
  );
}

/* A journalist's cancel (P12.4): no «Are you sure?»; the card says
 * «Ακυρώνεται…» and the cancel is sent once its 8 s of «Αναίρεση» are over. */
const CANCELLING = 'user-cancelling';
function cancelJob(job) {
  deferAction({
    text: t('user.cancel.pending'),
    hides: (view, j) => view === CANCELLING && j.id === job.id,
    run: () => api(`/api/user/jobs/${job.id}/cancel`, { method: 'POST', body: {} }),
    after: loadMyJobs,
    done: t('user.cancel.done'),
  });
}

const multi = document.getElementById('multi');
multi.addEventListener('change', () => {
  document.getElementById('single-box').hidden = multi.checked;
  document.getElementById('multi-box').hidden = !multi.checked;
  // A hidden required field would block the submit with no visible reason.
  document.getElementById('url').required = !multi.checked;
  showSlugPreview();
});

/* The file name the video will get (P12.11), as the server's compose makes
 * it (ELOT 743 and the file-name rule live there only): asked 250 ms after
 * the typing stops; an answer to older fields is dropped. With several
 * links, the first one's name (1A, then 1B, 1C…). */
let myJournalist = '';
let previewTimer = null;
let previewAsked = 0;
function showSlugPreview() {
  const keyword = document.getElementById('keyword').value.trim();
  const index = document.getElementById('index').value.trim() || '1';
  const preview = document.getElementById('slug-preview');
  clearTimeout(previewTimer);
  if (!keyword) { previewAsked += 1; preview.textContent = ''; return; }
  const many = multi.checked;
  previewTimer = setTimeout(async () => {
    const mine = ++previewAsked;
    try {
      const params = new URLSearchParams({ index: many ? `${index}A` : index, journalist: myJournalist || 'MCR', keyword });
      const r = await api(`/api/slug-preview?${params}`);
      if (mine !== previewAsked) return;
      preview.textContent = t(many ? 'user.preview.many' : 'manual.preview', { name: `${r.slug}.mxf` });
    } catch { /* the preview is a help; the submit says what is wrong */ }
  }, 250);
}
for (const id of ['keyword', 'index']) document.getElementById(id).addEventListener('input', showSlugPreview);

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
    throw new Error(t('error.network'));
  }
  if (response.status === 401) { location.href = '/login'; throw new Error(t('error.session')); }
  let payload = null;
  try { payload = JSON.parse(await response.text()); } catch { payload = null; }
  if (!response.ok && !(payload && Array.isArray(payload.results))) {
    throw new Error((payload && payload.error && payload.error.message) || t('error.failed'));
  }
  return { ok: response.ok, payload };
}

/** One submitted link's line: its file name and what became of it. */
const resultLine = (r, withError) => el('div', { class: 'job-meta' },
  el('span', {}, r.slug ? `${r.slug}.mxf` : '—'),
  el('span', {}, withError && r.error ? r.error : t(r.duplicate ? 'user.result.duplicate' : 'user.result.sent')),
);

/** Show what was queued before the failure, the link that failed, and how many
 *  were not tried. */
function showPartialFailure(payload) {
  const rows = payload.results.map((r) => resultLine(r, true));
  rows.push(el('div', { class: 'reason' }, t('user.not_tried', { n: Number(payload.not_attempted) || 0 })));
  render('submit-result', rows);
  toast(payload.error.message, 'bad');
  loadMyJobs();
}

/* At most MAX_LINKS links at once, as the server takes them (P12.12): the
 * count is shown under the list as it is typed, and a longer list is not
 * sent, so nothing is queued half-way. */
const MAX_LINKS = 20;
const urlsField = document.getElementById('urls');
const linksOf = () => urlsField.value.split('\n').map((l) => l.trim()).filter(Boolean);
function showLinkCount() {
  const n = linksOf().length;
  const line = document.getElementById('urls-count');
  line.textContent = n ? t(n > MAX_LINKS ? 'user.links.over' : 'user.links.count', { n, max: MAX_LINKS }) : '';
  line.classList.toggle('bad', n > MAX_LINKS);
  urlsField.setAttribute('aria-invalid', String(n > MAX_LINKS));
}
urlsField.addEventListener('input', showLinkCount);

document.getElementById('submit-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  if (multi.checked && linksOf().length > MAX_LINKS) {
    toast(t('user.links.over', { n: linksOf().length, max: MAX_LINKS }), 'bad');
    urlsField.focus();
    return;
  }
  const body = {
    keyword: document.getElementById('keyword').value.trim(),
    index_str: document.getElementById('index').value.trim(),
    notes: document.getElementById('notes').value.trim() || null,
    priority: document.getElementById('priority').checked,
  };
  if (multi.checked) {
    body.urls = linksOf();
  } else {
    body.url = document.getElementById('url').value.trim();
  }
  try {
    const { ok, payload } = await postSubmit(body);
    if (!ok) { showPartialFailure(payload); return; }
    const results = payload;
    render('submit-result', results.map((r) => resultLine(r, false)));
    toast(results.length > 1
      ? tn('user.sent_many', results.length)
      : (results[0].slug ? t('user.sent_as', { name: `${results[0].slug}.mxf` }) : t('user.already')), 'ok');
    event.target.reset();
    document.getElementById('single-box').hidden = false;
    document.getElementById('multi-box').hidden = true;
    document.getElementById('url').required = true;
    document.getElementById('index').value = '1';
    showSlugPreview();
    showLinkCount();
    loadMyJobs();
  } catch (e) { toast(e.message, 'bad'); }
});


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
    toast(t('user.prefs.saved'), 'ok');
  } catch (e) {
    notify.checked = !wanted;
    toast(e.message, 'bad');
  }
});

/* ------------------------------------------------------------------ *
 * Password
 * ------------------------------------------------------------------ */

/* The password window (P12.12): opens with the focus in its first field;
 * Escape, «Άκυρο» or a click outside closes it, empties it and gives the
 * focus back to «Αλλαγή κωδικού». */
const modal = document.getElementById('password-modal');
const passwordButton = document.getElementById('password-btn');

function openPassword() {
  modal.hidden = false;
  document.getElementById('pw-current').focus();
}

function closePassword() {
  if (modal.hidden) return;
  modal.hidden = true;
  document.getElementById('password-form').reset();
  passwordButton.focus();
}

passwordButton.addEventListener('click', openPassword);
document.getElementById('pw-cancel').addEventListener('click', closePassword);
modal.addEventListener('click', (e) => { if (e.target === modal) closePassword(); });
// On the document: a click on the window's text takes the focus out of its fields.
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape' || modal.hidden) return;
  e.preventDefault();
  closePassword();
});

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
    toast(t('user.password.done'), 'ok');
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

/* «Για έγκριση» (plan P10.7): the videos made and checked but held back until
 * MCR has watched them. Each is a card with the preview, the contact sheet and
 * the buttons that send it to Dalet or throw it away.
 *
 * Unlike the review cards, these are NOT rebuilt every 5 s: a playing video
 * would restart. Cards are kept by job id; a refresh adds the new ones,
 * removes the gone ones and only rewrites the small expiry text.
 *
 * Nothing here builds HTML from a string: see app.js.
 */

import { api, el, fmtDuration, safeHref, toast } from '/static/app.js?v=6';
import { fileName, clock } from '/static/desk.js?v=10';

const cards = new Map(); // job id -> { node, job, expiry }
let wantedFocus = null;

/** The ids of the cards on screen, so the review list keeps their history. */
export function approvalIds() {
  return new Set(cards.keys());
}

/** Scroll to a card (and light it) once it is on screen; now if it is already. */
export function focusApproval(id) {
  wantedFocus = id;
  applyFocus();
}

function applyFocus() {
  const card = wantedFocus === null ? null : cards.get(wantedFocus);
  // Keep the wish until the card is actually displayed (tab or section hidden).
  if (!card || !card.node.isConnected || card.node.offsetParent === null) return;
  wantedFocus = null;
  // Clear of the sticky page header, whatever height it has wrapped to.
  const bar = document.querySelector('.topbar');
  card.node.style.scrollMarginTop = `${(bar ? bar.offsetHeight : 60) + 12}px`;
  card.node.scrollIntoView({ behavior: 'smooth', block: 'start' });
  card.node.focus({ preventScroll: true });
  card.node.classList.add('hl');
  setTimeout(() => card.node.classList.remove('hl'), 2500);
}

/** How the video was found, for the person deciding whether to trust it. */
export function foundHow(method) {
  const m = String(method || '');
  if (m.startsWith('repair:recipe:')) return 'Βρέθηκε με γνωστό τρόπο (συνταγή)';
  if (m.startsWith('repair:llm:')) return 'Βρέθηκε με τη βοήθεια AI — ελέγξτε προσεκτικά';
  return 'Αυτόματη επισκευή';
}

/** «Λήγει σε 5 ώρες (14:05)», «Λήγει σε 40 λεπτά», or «Έληξε». */
export function expiryText(iso, now = Date.now()) {
  const t = iso ? Date.parse(iso) : NaN;
  if (Number.isNaN(t)) return '';
  const mins = Math.floor((t - now) / 60000);
  if (mins < 0) return 'Έληξε';
  const at = clock(iso);
  if (mins < 60) return `Λήγει σε ${Math.max(mins, 1)} λεπτά (${at})`;
  const hours = Math.round(mins / 60);
  return `Λήγει σε ${hours} ${hours === 1 ? 'ώρα' : 'ώρες'} (${at})`;
}

const previewUrl = (job, kind) => `/api/jobs/${job.id}/preview/${kind}`;

/** The contact sheet is 4 columns by 3 rows of equal tiles. */
const SHEET_COLS = 4;
const SHEET_ROWS = 3;

function seekFromSheet(ev, img, video, held) {
  const rect = img.getBoundingClientRect();
  if (!rect.width || !rect.height) return;
  const col = Math.min(SHEET_COLS - 1, Math.max(0, Math.floor(((ev.clientX - rect.left) / rect.width) * SHEET_COLS)));
  const row = Math.min(SHEET_ROWS - 1, Math.max(0, Math.floor(((ev.clientY - rect.top) / rect.height) * SHEET_ROWS)));
  const tiles = SHEET_COLS * SHEET_ROWS;
  const duration = Number.isFinite(video.duration) && video.duration > 0 ? video.duration : Number(held.duration) || 0;
  if (!duration) return;
  video.currentTime = ((row * SHEET_COLS + col + 0.5) * duration) / tiles;
}

function mediaNode(job, held) {
  const pv = held.preview || {};
  if (pv.video) {
    const video = el('video', { class: 'ap-video', controls: true, preload: 'metadata', src: previewUrl(job, 'video') });
    const sheet = pv.contact
      ? el('img', {
        class: 'ap-sheet', src: previewUrl(job, 'contact'), alt: 'Καρέ από όλο το βίντεο',
        title: 'Πατήστε σε ένα καρέ για να πάτε εκεί',
        onClick: (ev) => seekFromSheet(ev, ev.currentTarget, video, held),
      })
      : null;
    return el('div', { class: 'ap-media' }, video, sheet);
  }
  if (pv.audio_only) {
    return el('div', { class: 'ap-media' }, el('audio', { class: 'ap-audio', controls: true, preload: 'metadata', src: previewUrl(job, 'video') }));
  }
  return el('div', { class: 'ap-media' }, el('p', { class: 'note' }, 'Δεν υπάρχει προεπισκόπηση· ανοίξτε τον σύνδεσμο.'));
}

function buildCard(job, ctx) {
  const held = job.held || {};
  const href = safeHref(job.url);
  const buttons = [];
  const mk = (cls, text, handler) => {
    const b = el('button', { class: cls, type: 'button' }, text);
    b.addEventListener('click', handler);
    buttons.push(b);
    return b;
  };
  // One request at a time per card: the buttons are off until it answers. After
  // a success they stay off, with the card marked, until the sync removes the
  // card; a failure or a cancelled confirm turns them back on.
  const status = el('span', { class: 'note' });
  const guarded = (fn) => async () => {
    for (const b of buttons) b.disabled = true;
    let sent = false;
    try { sent = (await fn()) === true; } finally {
      if (!sent) for (const b of buttons) b.disabled = false;
    }
  };
  const done = (okText, mark) => {
    status.textContent = mark;
    toast(okText, 'ok');
    ctx.refresh();
    return true;
  };
  const failedAction = (e) => {
    if (e.status === 409 || e.code === 'NOT_HELD') {
      toast('Το βίντεο άλλαξε ή δεν περιμένει πια έγκριση', 'bad');
      ctx.refresh();
    } else {
      toast(e.message, 'bad');
    }
  };

  const approve = mk('btn btn-primary', 'Έγκριση → Dalet', guarded(async () => {
    if (!confirm(`Να σταλεί αυτό το βίντεο στο Dalet ως ${fileName(job)};`)) return false;
    try {
      await api(`/api/jobs/${job.id}/approve`, { method: 'POST', body: { sha256: held.sha256 } });
      return done(`Το ${fileName(job)} εγκρίθηκε και στέλνεται στο Dalet.`, 'Στάλθηκε…');
    } catch (e) { failedAction(e); return false; }
  }));
  const reject = async (reason, note) => {
    try {
      const body = { reason };
      if (note) body.note = note;
      await api(`/api/jobs/${job.id}/reject`, { method: 'POST', body });
      return done(`Το ${fileName(job)} απορρίφθηκε.`, 'Απορρίφθηκε');
    } catch (e) { failedAction(e); return false; }
  };
  const wrong = mk('btn btn-danger', 'Λάθος βίντεο', guarded(() => reject('wrong_video')));

  // «Απόρριψη…» opens a small inline form instead of a dialog.
  const reason = el('select', { 'aria-label': 'Λόγος απόρριψης' },
    el('option', { value: 'not_usable' }, 'Δεν είναι χρήσιμο'),
    el('option', { value: 'other' }, 'Άλλο'));
  const note = el('input', { type: 'text', maxlength: '300', placeholder: 'Σημείωση (προαιρετικά)', 'aria-label': 'Σημείωση απόρριψης' });
  const confirmReject = mk('btn btn-danger', 'Απόρριψη', guarded(() => reject(reason.value, note.value.trim())));
  const form = el('div', { class: 'row ap-reject' }, reason, note, confirmReject);
  form.hidden = true;
  const toggle = mk('btn', 'Απόρριψη…', () => { form.hidden = !form.hidden; });

  // desk.js's markDoneJob confirms and reports on its own and says nothing
  // about the outcome, so this one is re-enabled afterwards (it refreshes).
  const markDone = mk('btn', 'Το έβαλα στο Dalet', guarded(async () => { await ctx.markDone(job); return false; }));
  const history = ctx.historyPanel(job);
  const expiry = el('span', { class: 'note' }, expiryText(held.expires_at));

  const node = el('div', { class: 'card attention stack ap-card', 'data-job': String(job.id), tabindex: '-1' },
    el('div', { class: 'job-head' },
      el('div', { class: 'row', style: 'gap:10px;align-items:flex-start' },
        el('span', { class: 'job-id' }, String(job.id)),
        el('div', {},
          el('h3', { class: 'job-name' }, fileName(job)),
          el('p', { class: 'job-meta', style: 'margin:2px 0 0' },
            `Δημοσιογράφος: ${job.journalist} · από ${clock(job.updated_at || held.created_at)}`),
          ctx.mailLink(job),
        )),
      el('span', { class: 'badge warn' }, 'Για έγκριση')),
    mediaNode(job, held),
    pvArticle(job, held),
    el('div', { class: 'ap-facts note' },
      el('span', {}, `Διάρκεια ${fmtDuration(held.duration)}`),
      el('span', {}, foundHow(job.extraction_method)),
      href ? el('a', { href, target: '_blank', rel: 'noopener noreferrer', title: job.url }, 'Άνοιγμα σελίδας') : null,
      expiry),
    el('div', { class: 'row' }, approve, wrong, toggle, markDone, history[0], status),
    form,
    history[1]);
  return { node, job, expiry };
}

function pvArticle(job, held) {
  if (!(held.preview && held.preview.article)) return null;
  return el('div', { class: 'ap-article' },
    el('span', { class: 'note' }, 'Εικόνα άρθρου'),
    el('img', { src: previewUrl(job, 'article'), alt: 'Εικόνα άρθρου' }));
}

/**
 * Bring the cards in `root` in line with `jobs` (oldest first) without
 * touching the ones already there. `ctx`: mailLink(job), historyPanel(job),
 * markDone(job), refresh().
 */
export function syncApproval(root, jobs, ctx) {
  const list = root.querySelector('.ap-list');
  const wanted = new Set(jobs.map((j) => j.id));
  for (const [id, card] of cards) {
    if (wanted.has(id)) continue;
    card.node.remove();
    cards.delete(id);
  }
  for (const job of jobs) {
    if (!job.held) continue;
    let card = cards.get(job.id);
    // A different file under the same job (held again after a retry): the
    // old card's approve button would carry the wrong SHA-256.
    if (card && card.job.held && card.job.held.sha256 !== job.held.sha256) {
      card.node.remove();
      cards.delete(job.id);
      card = null;
    }
    if (!card) {
      card = buildCard(job, ctx);
      cards.set(job.id, card);
      list.append(card.node);
    }
    card.job = job;
    card.expiry.textContent = expiryText(job.held.expires_at);
  }
  const n = cards.size;
  root.hidden = n === 0;
  const title = root.querySelector('.ap-title');
  if (title) title.textContent = `Για έγκριση (${n})`;
  applyFocus();
}

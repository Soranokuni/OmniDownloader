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
import { fileName, clock } from '/static/desk.js?v=12';

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

/** m:ss for a number of seconds; empty when unknown. */
export function clockSecs(secs) {
  const n = Number(secs);
  if (!Number.isFinite(n) || n < 0) return '';
  const s = Math.round(n);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

/** «Βρέθηκε με …» from the repair record; the job's method when there is none. */
export function foundText(repair, method) {
  if (!repair) return foundHow(method);
  switch (repair.found) {
    case 'shipped': return 'Βρέθηκε με: γενική συνταγή';
    case 'recipe': {
      const n = Number(repair.recipe_approvals) || 0;
      return n > 0 ? `Βρέθηκε με: γνωστή συνταγή, εγκρίθηκε ${n} ${n === 1 ? 'φορά' : 'φορές'}` : 'Βρέθηκε με: νέα συνταγή';
    }
    case 'llm': return 'Βρέθηκε με: τη βοήθεια AI — ελέγξτε προσεκτικά';
    default: return foundHow(method);
  }
}

const CHECK_LABELS = {
  egress: 'Διεύθυνση',
  ad: 'Διαφήμιση',
  probe: 'Βίντεο',
  length: 'Διάρκεια',
  short: 'Μήκος',
};

/** The Greek label of a gate check; an unknown name is shown as it is. */
export function checkLabel(name) {
  return Object.prototype.hasOwnProperty.call(CHECK_LABELS, name) ? CHECK_LABELS[name] : String(name || '');
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
    if (e.code === 'NO_NEXT') {
      toast(e.message, 'bad');
      ctx.refresh();
    } else if (e.status === 409 || e.code === 'NOT_HELD') {
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
  // «Επόμενη επιλογή»: only while the repair verified another video after this
  // one. The held one counts as wrong; the next is downloaded and held again.
  const repair = job.repair || null;
  const nextLeft = repair && Number(repair.next_left) > 0 ? Number(repair.next_left) : 0;
  const next = nextLeft > 0
    ? mk('btn', `Επόμενη επιλογή (${nextLeft})`, guarded(async () => {
      if (!confirm('Το τρέχον βίντεο απορρίπτεται ως λάθος και κατεβαίνει η επόμενη επιλογή.')) return false;
      try {
        await api(`/api/jobs/${job.id}/next`, { method: 'POST', body: { sha256: held.sha256 } });
        return done('Η επόμενη επιλογή κατεβαίνει.', 'Επόμενη επιλογή…');
      } catch (e) { failedAction(e); return false; }
    }))
    : null;

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
  const media = mediaNode(job, held);
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
    media,
    pvArticle(job, held, repair, media.querySelector('video')),
    pvChecks(repair),
    el('div', { class: 'ap-facts note' },
      ...pvFacts(job, held, repair),
      href ? el('a', { href, target: '_blank', rel: 'noopener noreferrer', title: job.url }, 'Άνοιγμα σελίδας') : null,
      expiry),
    el('div', { class: 'row' }, approve, wrong, next, toggle, markDone, history[0], status),
    form,
    history[1]);
  return { node, job, expiry };
}

/** One picture with its caption; `onClick` makes it a button that seeks the video. */
function figure(src, alt, caption, onClick) {
  const img = el('img', { src, alt });
  if (onClick) {
    img.classList.add('ap-seek');
    img.title = 'Πατήστε για να πάτε σε αυτό το σημείο του βίντεο';
    img.addEventListener('click', onClick);
  }
  return el('figure', { class: 'ap-figure' }, img, el('figcaption', { class: 'note' }, caption));
}

/**
 * The article's own picture beside the video frame that looks most like it.
 * A match is a good sign; no match is neutral (many pictures are designed
 * graphics), never a failure.
 */
function pvArticle(job, held, repair, video) {
  const pv = held.preview || {};
  if (!pv.article) return null;
  const image = repair && repair.image;
  const parts = [figure(previewUrl(job, 'article'), 'Εικόνα άρθρου', 'Εικόνα άρθρου')];
  if (pv.frame) {
    const t = image && Number.isFinite(Number(image.frame_secs)) ? Number(image.frame_secs) : null;
    const seek = video && t !== null ? () => { video.currentTime = t; } : null;
    const label = t !== null ? `Πλησιέστερο καρέ ${clockSecs(t)}` : 'Πλησιέστερο καρέ';
    parts.push(figure(previewUrl(job, 'frame'), label, label, seek));
  }
  let verdict = null;
  if (image && image.match) {
    verdict = el('span', { class: 'ap-same' }, 'ίδια εικόνα ✓');
  } else if (image && image.distance !== null && image.distance !== undefined) {
    verdict = el('span', { class: 'note' }, 'η εικόνα του άρθρου δεν μοιάζει με κάποιο καρέ (συχνό σε γραφικά)');
  }
  return el('div', { class: 'ap-article' }, el('div', { class: 'row ap-pics' }, ...parts), verdict);
}

/** «Έλεγχοι»: the gate's checklist for the video that was held. */
function pvChecks(repair) {
  const checks = repair && Array.isArray(repair.checks) ? repair.checks : [];
  if (!checks.length) return null;
  return el('div', { class: 'ap-checks' },
    el('span', { class: 'note' }, 'Έλεγχοι'),
    el('ul', { class: 'ap-checklist' },
      ...checks.map((c) => el('li', { class: c.ok ? 'ap-ok' : 'ap-bad' },
        el('span', { class: 'ap-mark', 'aria-label': c.ok ? 'πέρασε' : 'απέτυχε' }, c.ok ? '✓' : '✗'),
        el('b', {}, checkLabel(c.name)),
        c.detail ? el('span', { class: 'note' }, ` ${c.detail}`) : null))));
}

/** The short facts line: length against the page's, the source host, a short-clip warning. */
function pvFacts(job, held, repair) {
  const expected = repair && Number(repair.expected_secs) > 0 ? clockSecs(repair.expected_secs) : '';
  const length = expected
    ? `Διάρκεια: ${clockSecs(held.duration)} (σελίδα: ${expected})`
    : `Διάρκεια ${fmtDuration(held.duration)}`;
  return [
    el('span', {}, length),
    repair && repair.source_host ? el('span', {}, `Πηγή: ${repair.source_host}`) : null,
    repair && repair.short_clip ? el('span', { class: 'ap-warn' }, 'Σύντομο κλιπ — μπορεί να είναι διαφήμιση') : null,
    el('span', {}, foundText(repair, job.extraction_method)),
  ];
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

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

import { api, el, fmtDuration, safeHref } from '/static/app.js?v=6';
import { toast } from '/static/toast.js?v=11';
import { fileName, clock, confirmWrongVideo, scrollBehavior } from '/static/desk.js?v=28';
import { confirmDialog, infoDialog } from '/static/dialog.js?v=11';
import { navCard } from '/static/keys.js?v=9';
import { statusBadge } from '/static/status.js?v=11';
import { t, tn } from '/static/text.js?v=10';

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
  card.node.scrollIntoView({ behavior: scrollBehavior(), block: 'start' });
  card.node.focus({ preventScroll: true });
  card.node.classList.add('hl');
  setTimeout(() => card.node.classList.remove('hl'), 2500);
}

/** How the video was found, for the person deciding whether to trust it. */
export function foundHow(method) {
  const m = String(method || '');
  if (m.startsWith('repair:recipe:')) return t('approval.found.recipe_method');
  if (m.startsWith('repair:llm:')) return t('approval.found.llm_method');
  return t('approval.found.repair');
}

/** «Λήγει σε 5 ώρες (14:05)», «Λήγει σε 40 λεπτά», or «Έληξε». */
export function expiryText(iso, now = Date.now()) {
  const until = iso ? Date.parse(iso) : NaN;
  if (Number.isNaN(until)) return '';
  const mins = Math.floor((until - now) / 60000);
  if (mins < 0) return t('approval.expired');
  const at = clock(iso);
  if (mins < 60) return t('approval.expires_mins', { n: Math.max(mins, 1), at });
  return tn('approval.expires_hours', Math.round(mins / 60), { at });
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
    case 'shipped': return t('approval.found.shipped');
    case 'recipe': {
      const n = Number(repair.recipe_approvals) || 0;
      return n > 0 ? tn('approval.found.known', n) : t('approval.found.new');
    }
    case 'llm': return t('approval.found.llm');
    default: return foundHow(method);
  }
}

/** The gate's checks that have a label (`approval.check.<name>`). */
const CHECK_LABELS = ['egress', 'ad', 'probe', 'length', 'short'];

/** The label of a gate check; an unknown name is shown as it is. */
export function checkLabel(name) {
  return CHECK_LABELS.includes(name) ? t(`approval.check.${name}`) : String(name || '');
}

const previewUrl = (job, kind) => `/api/jobs/${job.id}/preview/${kind}`;

/** The contact sheet is 4 columns by 3 rows of equal tiles. */
const SHEET_COLS = 4;
const SHEET_ROWS = 3;

const TILES = SHEET_COLS * SHEET_ROWS;

/** Put the video at the middle of tile `tile` (0..11) of the contact sheet. */
function seekToTile(video, held, tile) {
  const duration = Number.isFinite(video.duration) && video.duration > 0 ? video.duration : Number(held.duration) || 0;
  if (!duration) return;
  video.currentTime = ((tile + 0.5) * duration) / TILES;
}

/** The tile under a pointer click on the sheet's picture. */
function tileAt(ev, img) {
  const rect = img.getBoundingClientRect();
  if (!rect.width || !rect.height) return null;
  const col = Math.min(SHEET_COLS - 1, Math.max(0, Math.floor(((ev.clientX - rect.left) / rect.width) * SHEET_COLS)));
  const row = Math.min(SHEET_ROWS - 1, Math.max(0, Math.floor(((ev.clientY - rect.top) / rect.height) * SHEET_ROWS)));
  return row * SHEET_COLS + col;
}

/**
 * The contact sheet as a real button (P12.8): a click goes to the frame
 * under the pointer; from the keyboard, ← and → pick a frame and go there,
 * Enter goes to the one picked. Its name says which frame.
 */
function sheetButton(job, held, video) {
  const img = el('img', { class: 'ap-sheet', src: previewUrl(job, 'contact'), alt: '' });
  const button = el('button', { class: 'ap-sheet-btn', type: 'button', title: t('approval.sheet.title') }, img);
  let tile = 0;
  const name = () => button.setAttribute('aria-label', t('approval.sheet.frame', { n: tile + 1, of: TILES }));
  name();
  button.addEventListener('click', (ev) => {
    // A pointer click knows where it landed; Enter or Space (detail 0) goes to the frame picked.
    const at = ev.detail > 0 ? tileAt(ev, img) : null;
    if (at !== null) tile = at;
    name();
    seekToTile(video, held, tile);
  });
  button.addEventListener('keydown', (ev) => {
    if (ev.key !== 'ArrowRight' && ev.key !== 'ArrowLeft') return;
    ev.preventDefault();
    tile = (tile + (ev.key === 'ArrowRight' ? 1 : TILES - 1)) % TILES;
    name();
    seekToTile(video, held, tile);
  });
  return button;
}

function mediaNode(job, held) {
  const pv = held.preview || {};
  if (pv.video) {
    const video = el('video', { class: 'ap-video', controls: true, preload: 'metadata', src: previewUrl(job, 'video') });
    const sheet = pv.contact ? sheetButton(job, held, video) : null;
    return el('div', { class: 'ap-media' }, video, sheet);
  }
  if (pv.audio_only) {
    return el('div', { class: 'ap-media' }, el('audio', { class: 'ap-audio', controls: true, preload: 'metadata', src: previewUrl(job, 'video') }));
  }
  return el('div', { class: 'ap-media' }, el('p', { class: 'note' }, t('approval.no_preview')));
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
      toast(t('approval.changed'), 'bad');
      ctx.refresh();
    } else {
      toast(e.message, 'bad');
    }
  };

  const approve = mk('btn btn-primary', t('approval.approve'), guarded(async () => {
    if (!(await confirmDialog({
      title: t('approval.approve.title', { name: fileName(job) }),
      body: t('approval.approve.body'),
      confirm: t('approval.approve'),
    }))) return false;
    try {
      await api(`/api/jobs/${job.id}/approve`, { method: 'POST', body: { sha256: held.sha256 } });
      return done(t('approval.approve.done', { name: fileName(job) }), t('approval.approve.sent'));
    } catch (e) { failedAction(e); return false; }
  }));
  const reject = async (reason, note) => {
    try {
      const body = { reason };
      if (note) body.note = note;
      await api(`/api/jobs/${job.id}/reject`, { method: 'POST', body });
      return done(t('approval.reject.done', { name: fileName(job) }), t('approval.reject.mark'));
    } catch (e) { failedAction(e); return false; }
  };
  // «Λάθος βίντεο» asks in the same dialog as on a delivered card (desk.js).
  const wrong = mk('btn btn-danger', t('desk.wrong.button'), guarded(async () => {
    if (!(await confirmWrongVideo(fileName(job), [t('approval.wrong.body1'), t('approval.wrong.body2')]))) return false;
    return reject('wrong_video');
  }));
  // «Επόμενη επιλογή»: only while the repair verified another video after this
  // one. The held one counts as wrong; the next is downloaded and held again.
  const repair = job.repair || null;
  const nextLeft = repair && Number(repair.next_left) > 0 ? Number(repair.next_left) : 0;
  const next = nextLeft > 0
    ? mk('btn', t('approval.next', { n: nextLeft }), guarded(async () => {
      if (!(await confirmWrongVideo(fileName(job), [t('approval.next.body')]))) return false;
      try {
        await api(`/api/jobs/${job.id}/next`, { method: 'POST', body: { sha256: held.sha256 } });
        return done(t('approval.next.done'), t('approval.next.mark'));
      } catch (e) { failedAction(e); return false; }
    }))
    : null;

  // «Απόρριψη…» opens a small inline form instead of a dialog.
  const reason = el('select', { 'aria-label': t('approval.reject.reason') },
    el('option', { value: 'not_usable' }, t('approval.reject.not_usable')),
    el('option', { value: 'other' }, t('approval.reject.other')));
  const note = el('input', { type: 'text', maxlength: '300', placeholder: t('approval.reject.note'), 'aria-label': t('approval.reject.note_label') });
  const confirmReject = mk('btn btn-danger', t('approval.reject'), guarded(() => reject(reason.value, note.value.trim())));
  const form = el('div', { class: 'row ap-reject' }, reason, note, confirmReject);
  form.hidden = true;
  const toggle = mk('btn', t('approval.reject.open'), () => { form.hidden = !form.hidden; });

  // desk.js's markDoneJob confirms and reports on its own and says nothing
  // about the outcome, so this one is re-enabled afterwards (it refreshes).
  const markDone = mk('btn', t('desk.markdone.button'), guarded(async () => { await ctx.markDone(job); return false; }));
  const history = ctx.historyPanel(job);
  const media = mediaNode(job, held);
  const expiry = el('span', { class: 'note' }, expiryText(held.expires_at));

  const node = el('div', { class: 'card attention stack ap-card', 'data-job': String(job.id), tabindex: '-1' },
    el('div', { class: 'job-head' },
      el('div', { class: 'row head-row' },
        el('span', { class: 'job-id' }, String(job.id)),
        el('div', {},
          el('h3', { class: 'job-name' }, fileName(job)),
          el('p', { class: 'job-meta under-name' },
            t('card.journalist_since', { name: job.journalist, at: clock(job.updated_at || held.created_at) })),
          ctx.mailLink(job),
        )),
      statusBadge({ status: 'AWAITING_APPROVAL' })),
    media,
    pvArticle(job, held, repair, media.querySelector('video')),
    pvChecks(repair),
    el('div', { class: 'ap-facts note' },
      ...pvFacts(job, held, repair),
      href ? el('a', { href, target: '_blank', rel: 'noopener noreferrer', title: job.url }, t('approval.open_page')) : null,
      expiry),
    el('div', { class: 'row' }, approve, wrong, next, toggle, markDone, history[0], status),
    form,
    history[1]);
  // J/K reach the card; Enter opens its history; A asks to approve, as the button does (P12.8).
  navCard(node, {
    open: () => history[0].click(),
    approve: () => { if (!approve.disabled) approve.click(); },
  });
  return { node, job, expiry };
}

/**
 * One picture with its caption, as a real button (P12.8): `onClick` seeks the
 * video (the nearest frame); without one, it opens the picture large.
 */
function figure(src, alt, caption, onClick) {
  const img = el('img', { src, alt: '' });
  const open = onClick || (() => infoDialog({ title: caption, content: el('img', { class: 'ap-large', src, alt }) }));
  const button = el('button', {
    class: `ap-pic ${onClick ? 'ap-seek' : 'ap-zoom'}`,
    type: 'button',
    title: onClick ? t('approval.seek') : t('approval.zoom'),
    'aria-label': `${alt} · ${onClick ? t('approval.seek') : t('approval.zoom')}`,
    onClick: open,
  }, img);
  return el('figure', { class: 'ap-figure' }, button, el('figcaption', { class: 'note' }, caption));
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
  const parts = [figure(previewUrl(job, 'article'), t('approval.article_image'), t('approval.article_image'))];
  if (pv.frame) {
    const at = image && Number.isFinite(Number(image.frame_secs)) ? Number(image.frame_secs) : null;
    const seek = video && at !== null ? () => { video.currentTime = at; } : null;
    const label = at !== null ? t('approval.frame_at', { at: clockSecs(at) }) : t('approval.frame');
    parts.push(figure(previewUrl(job, 'frame'), label, label, seek));
  }
  let verdict = null;
  if (image && image.match) {
    verdict = el('span', { class: 'ap-same' }, t('approval.same_image'));
  } else if (image && image.distance !== null && image.distance !== undefined) {
    verdict = el('span', { class: 'note' }, t('approval.no_match'));
  }
  return el('div', { class: 'ap-article' }, el('div', { class: 'row ap-pics' }, ...parts), verdict);
}

/** «Έλεγχοι»: the gate's checklist for the video that was held. */
function pvChecks(repair) {
  const checks = repair && Array.isArray(repair.checks) ? repair.checks : [];
  if (!checks.length) return null;
  return el('div', { class: 'ap-checks' },
    el('span', { class: 'note' }, t('approval.checks')),
    el('ul', { class: 'ap-checklist' },
      ...checks.map((c) => el('li', { class: c.ok ? 'ap-ok' : 'ap-bad' },
        el('span', { class: 'ap-mark', 'aria-label': t(c.ok ? 'approval.check.passed' : 'approval.check.failed') }, c.ok ? '✓' : '✗'),
        el('b', {}, checkLabel(c.name)),
        c.detail ? el('span', { class: 'note' }, ` ${c.detail}`) : null))));
}

/** The short facts line: length against the page's, the source host, a short-clip warning. */
function pvFacts(job, held, repair) {
  const expected = repair && Number(repair.expected_secs) > 0 ? clockSecs(repair.expected_secs) : '';
  const length = expected
    ? t('approval.length_vs_page', { length: clockSecs(held.duration), page: expected })
    : t('approval.length', { length: fmtDuration(held.duration) });
  return [
    el('span', {}, length),
    repair && repair.source_host ? el('span', {}, t('approval.source', { host: repair.source_host })) : null,
    repair && repair.short_clip ? el('span', { class: 'ap-warn' }, t('approval.short_clip')) : null,
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
  if (title) title.textContent = t('approval.title', { n });
  applyFocus();
}

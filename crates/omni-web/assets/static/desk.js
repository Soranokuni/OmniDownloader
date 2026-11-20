/* What the MCR desk's lists share (plan P7.1, P7.10): the words for a job's
 * step, its file name, the pager, and the actions on a job. The words and
 * colours of a job's status are in status.js (P12.1). Moved out of
 * mcr.js when the Email tab (inbox.js) came to need the same ones; the
 * confirmations and messages are the same wherever a job is acted on.
 * Every word is in text.el.js (P12.7).
 *
 * Nothing here builds HTML from a string: see app.js.
 */

import { api, el, render } from '/static/app.js?v=6';
import { toast } from '/static/toast.js?v=4';
import { STATUS } from '/static/status.js?v=4';
import { deferAction, undoToast } from '/static/undo.js?v=5';
import { confirmDialog } from '/static/dialog.js?v=4';
import { t, tn } from '/static/text.js?v=3';

/** What a job is doing, for a person (`stage.<STAGE>` in text.el.js). */
const STAGES = ['QUEUED', 'EXTRACT', 'DOWNLOAD', 'PROBE', 'TRANSCODE', 'REWRAP', 'VERIFY', 'DELIVER', 'ARCHIVE', 'DONE'];
export const STAGE_TEXT = Object.fromEntries(STAGES.map((s) => [s, undefined]));
for (const s of STAGES) Object.defineProperty(STAGE_TEXT, s, { get: () => t(`stage.${s}`), enumerable: true });
export const STAGE_ORDER = ['EXTRACT', 'DOWNLOAD', 'PROBE', 'TRANSCODE', 'REWRAP', 'VERIFY', 'DELIVER'];

export function stageLine(job) {
  if (job.status === 'PENDING') return STATUS.PENDING.label;
  const text = STAGE_TEXT[job.stage] || STATUS.RUNNING.label;
  const step = STAGE_ORDER.indexOf(job.stage);
  return step >= 0 ? t('desk.step', { n: step + 1, of: STAGE_ORDER.length, text }) : text;
}

/** Apply a live `job` event to a job the desk already shows (P1.10). `true`
 *  when the card can be redrawn in place: the job is waiting or running and
 *  stays so. A finished or otherwise changed job returns `false`, and the
 *  desk reloads the list instead, since a delivered card needs fields the
 *  event does not carry. `null` stage or progress means "unchanged". */
export function applyJobEvent(job, ev) {
  if (!job || !ev || ev.type !== 'job') return false;
  if (job.status !== 'PENDING' && job.status !== 'RUNNING') return false;
  if (ev.status !== 'PENDING' && ev.status !== 'RUNNING') return false;
  job.status = ev.status;
  if (typeof ev.stage === 'string' && ev.stage !== job.stage) {
    job.stage = ev.stage;
    job.stage_started_at = new Date().toISOString();
  }
  if (typeof ev.progress === 'number') job.progress = ev.progress;
  if (typeof ev.speed === 'string') job.speed = ev.speed;
  if (typeof ev.eta === 'string') job.eta = ev.eta;
  return true;
}

/* Time in the step, place in the queue, next retry (plan P7.19). The server
 * sends RFC 3339 UTC; a missing or unreadable time shows nothing. */
const STEP_WARN_MIN = 20;

function msSince(iso, now) {
  const at = iso ? Date.parse(iso) : NaN;
  return Number.isNaN(at) ? null : now - at;
}

const minutes = (ms) => Math.max(0, Math.floor(ms / 60000));

/** What a waiting or running video's sub-line adds to the step text.
 *  PENDING: the retry countdown, the place in the queue, or plain
 *  «Σε αναμονή». RUNNING: «· σε αυτό το βήμα N′», or '' when unknown. */
export function waitLine(job, now = Date.now()) {
  if (job.status === 'PENDING') {
    // A finished video waits for the share, not for a back-off (P11.3b).
    if (job.delivery_wait || job.wait_reason === 'delivery') return t('desk.wait.delivery');
    const since = msSince(job.not_before, now); // negative while the back-off lasts
    if (since !== null && since < 0) {
      const secs = Math.ceil(-since / 1000);
      return secs < 90 ? t('desk.wait.retry_secs', { n: secs }) : t('desk.wait.retry_mins', { n: Math.round(secs / 60) });
    }
    const pos = Number(job.queue_position);
    if (pos > 0) return t('desk.wait.position', { n: pos });
    return STATUS.PENDING.label;
  }
  if (job.status === 'RUNNING') {
    const ms = msSince(job.stage_started_at, now);
    if (ms === null) return '';
    const mins = minutes(ms);
    return t('desk.wait.in_step', { n: mins < 1 ? '<1' : mins });
  }
  return '';
}

/** Why a late video is late, for the strip above the tabs (P11.3b). */
export function lateReason(job, now = Date.now()) {
  switch (job.wait_reason) {
    case 'delivery':
      return t('desk.wait.delivery');
    case 'live': {
      // A stream still live, checked again at its next try (P11.7); no time when none is known.
      const at = job.next_try_at ? clock(job.next_try_at) : '—';
      return at === '—' ? t('desk.late.live') : t('desk.late.live_at', { at });
    }
    case 'retry': {
      const text = t('desk.late.retry_at', { at: clock(job.next_try_at) });
      return job.hint ? `${text} · ${job.hint}` : text;
    }
    case 'running': {
      const ms = msSince(job.stage_started_at, now);
      return ms === null ? STATUS.RUNNING.label : t('desk.late.running', { label: STATUS.RUNNING.label, n: minutes(ms) });
    }
    default:
      return t('desk.late.queue');
  }
}

/** «πριν από 12′»: how long ago the video arrived (or was last restarted); '' when unknown. */
export function arrivedAgo(job, now = Date.now()) {
  const ms = msSince(job.retry_from || job.created_at, now);
  return ms === null ? '' : t('desk.ago', { n: minutes(ms) });
}

/** True when a RUNNING video has been in its step longer than it should. */
export function stepIsLong(job, now = Date.now()) {
  const ms = job.status === 'RUNNING' ? msSince(job.stage_started_at, now) : null;
  return ms !== null && ms > STEP_WARN_MIN * 60000;
}

/** The running video's time in step as a small node (amber when long), or null. */
export function stepTimeNode(job, now = Date.now()) {
  const text = waitLine(job, now);
  if (!text) return null;
  const long = stepIsLong(job, now);
  return el('span', long
    ? { class: 'step-time warn', title: t('desk.step_long') }
    : { class: 'step-time' }, text);
}

/** "Μόνο τα 2 πρώτα βίντεο", when the journalist asked for that. */
export function limitNote(job) {
  const n = Number(job.max_videos) || 0;
  if (!n) return null;
  return el('span', { class: 'badge gap-left', title: t('desk.limit.title') }, tn('desk.limit', n));
}

/** 'smooth', or 'auto' for a person who asked the system for less motion (P12.8). */
const lessMotion = window.matchMedia('(prefers-reduced-motion: reduce)');
export function scrollBehavior() {
  return lessMotion.matches ? 'auto' : 'smooth';
}

export function fileName(job) {
  return `${job.slug}.mxf`;
}

/** The delivered file's name, which differs from the slug when a file of
 *  the same name was already in the watchfolder (`_2`). */
export function deliveredName(job) {
  if (!job.file_path) return fileName(job);
  const parts = String(job.file_path).split(/[\\/]/);
  return parts[parts.length - 1] || fileName(job);
}

/** "Εμφανίζονται 26–50 από 132  ‹ Προηγούμενη  Σελίδα 2 από 6  Επόμενη ›".
 *  Hidden when everything fits on one page. `state` is `{page, perPage}`. */
export function pager(targetId, state, total, onChange) {
  const pageCount = Math.max(1, Math.ceil(total / state.perPage));
  if (state.page > pageCount) state.page = pageCount;
  if (total <= state.perPage) {
    render(targetId);
    return;
  }
  const first = (state.page - 1) * state.perPage + 1;
  const last = Math.min(total, state.page * state.perPage);
  const go = (p) => { state.page = p; onChange(); window.scrollTo({ top: 0, behavior: scrollBehavior() }); };
  render(targetId, el('div', { class: 'section-head card pager' },
    el('span', { class: 'note' }, t('pager.showing', { first, last, total })),
    el('div', { class: 'row tight' },
      el('button', { class: 'btn', type: 'button', disabled: state.page <= 1, onClick: () => go(1) }, t('pager.first')),
      el('button', { class: 'btn', type: 'button', disabled: state.page <= 1, onClick: () => go(state.page - 1) }, t('pager.prev')),
      el('span', { class: 'note pager-page' }, t('pager.page', { page: state.page, count: pageCount })),
      el('button', { class: 'btn', type: 'button', disabled: state.page >= pageCount, onClick: () => go(state.page + 1) }, t('pager.next')),
      el('button', { class: 'btn', type: 'button', disabled: state.page >= pageCount, onClick: () => go(pageCount) }, t('pager.last')),
    ),
  ));
}

/* ------------------------------------------------------------------ *
 * Actions. What can be taken back acts at once with «Αναίρεση»
 * (undo.js); what sends a video on air or closes a job for good asks in
 * the page's dialog (dialog.js). Each says what happened and calls `after`
 * so the list it came from refreshes. No confirm() or prompt() (P12.4).
 * ------------------------------------------------------------------ */

export async function retryJob(job, after) {
  try {
    await api(`/api/jobs/${job.id}/retry`, { method: 'POST' });
    toast(t('desk.retry.done', { name: fileName(job) }), 'ok');
    after();
  } catch (e) { toast(e.message, 'bad'); }
}

export async function overrideJob(job, url, after) {
  const trimmed = (url || '').trim();
  if (!trimmed) { toast(t('desk.override.empty'), 'bad'); return false; }
  try {
    await api(`/api/jobs/${job.id}/override`, { method: 'POST', body: { url: trimmed } });
    toast(t('desk.override.done', { name: fileName(job) }), 'ok');
    after();
    return true;
  } catch (e) { toast(e.message, 'bad'); return false; }
}

/** Whether the file name can still change (P7.12): the server refuses
 *  once the file is being made (rewrap onwards) or delivered. */
export function canRename(job) {
  if (['PENDING', 'REQUIRES_REVIEW', 'MANUAL_DOWNLOAD', 'FAILED'].includes(job.status)) return true;
  return job.status === 'RUNNING' && ['QUEUED', 'EXTRACT', 'DOWNLOAD', 'TRANSCODE'].includes(job.stage);
}

/* Rename in place (P12.4): the card shows a field instead of a prompt(),
 * with the file name it will get, as the server makes it, under it. The
 * lists keep a card's field open across their refreshes by this set. */
export const renameOpen = new Set();

/** The inline rename field of a card. `after` refreshes the list once saved;
 *  `rerender` redraws the card when the field closes unsaved. */
export function renameForm(job, after, rerender) {
  const id = `rename-${job.id}`;
  const input = el('input', {
    type: 'text', id, value: job.keyword || '', maxlength: '60', autocomplete: 'off', class: 'mono',
    'aria-describedby': `${id}-preview`,
  });
  const preview = el('p', { class: 'note mono rename-preview', id: `${id}-preview`, 'aria-live': 'polite' },
    t('desk.rename.as', { name: fileName(job) }));
  let timer = null;
  let asked = 0;
  // The page does not guess the Latin form of a Greek word (ELOT 743): the
  // server answers with the keyword it would store.
  const update = () => {
    clearTimeout(timer);
    timer = setTimeout(async () => {
      const mine = ++asked;
      try {
        const r = await api(`/api/keyword-preview?text=${encodeURIComponent(input.value)}`);
        if (mine !== asked) return;
        preview.textContent = r.keyword
          ? t('desk.rename.as', { name: `${job.index_str}_${job.journalist}_${r.keyword}.mxf` })
          : t('desk.rename.short');
      } catch { /* the preview is a help; the save says what is wrong */ }
    }, 200);
  };
  const close = () => { renameOpen.delete(job.id); rerender(); };
  const save = async () => {
    const typed = input.value.trim();
    if (!typed || typed === (job.keyword || '')) { close(); return; }
    try {
      const r = await api(`/api/jobs/${job.id}/rename`, { method: 'POST', body: { keyword: typed } });
      renameOpen.delete(job.id);
      toast(t('desk.rename.done', { name: `${r.slug}.mxf` }), 'ok');
      after();
    } catch (e) { toast(e.message, 'bad'); }
  };
  input.addEventListener('input', update);
  input.addEventListener('keydown', (ev) => {
    if (ev.isComposing) return;
    if (ev.key === 'Enter') { ev.preventDefault(); save(); }
    if (ev.key === 'Escape') { ev.preventDefault(); ev.stopPropagation(); close(); }
  });
  // Read once the list has put back what was typed before a refresh.
  update();
  return el('div', { class: 'rename-form' },
    el('label', { for: id }, t('desk.rename.label')),
    el('div', { class: 'row' },
      el('div', { class: 'grow' }, input),
      el('button', { class: 'btn btn-primary', type: 'button', onClick: save }, t('desk.rename.save')),
      el('button', { class: 'btn', type: 'button', onClick: close }, t('dialog.cancel'))),
    preview);
}

/** Remove a job from the lists: gone from the screen now, sent after the
 *  8 seconds of «Αναίρεση» (the server keeps no way back). */
export function discardJob(job, after) {
  deferAction({
    text: t('desk.discard.done', { name: fileName(job) }),
    hides: (_view, j) => j.id === job.id,
    run: () => api(`/api/jobs/${job.id}/discard`, { method: 'POST' }),
    after,
  });
}

/** MCR cancels a waiting or running video; the row and its history stay (P7.20).
 *  Sent at once, so a running tool stops now; «Αναίρεση» puts it back in the queue. */
export async function cancelJob(job, after) {
  const name = fileName(job);
  try {
    await api(`/api/jobs/${job.id}/cancel`, { method: 'POST' });
  } catch (e) { toast(e.message, 'bad'); return; }
  after();
  undoToast(t('desk.cancel.done', { name }), async () => {
    try {
      await api(`/api/jobs/${job.id}/retry`, { method: 'POST' });
      toast(t('desk.retry.done', { name }), 'ok');
    } catch (e) { toast(e.message, 'bad'); }
    after();
  });
}

/** Download a delivered video again: it goes on air again, so it is asked. */
export async function redownloadJob(job, after) {
  const name = deliveredName(job);
  if (!(await confirmDialog({
    title: t('desk.redownload.title', { name }),
    body: [t('desk.redownload.body1'), t('desk.redownload.body2')],
    confirm: t('desk.redownload.button'),
  }))) return;
  try {
    await api(`/api/jobs/${job.id}/redownload`, { method: 'POST' });
    toast(t('desk.retry.done', { name }), 'ok');
    after();
  } catch (e) { toast(e.message, 'bad'); }
}

/** MCR put the video into Dalet by hand: record it as done (P7.16). Closes the job for good. */
export async function markDoneJob(job, after) {
  const name = fileName(job);
  if (!(await confirmDialog({
    title: t('desk.markdone.title', { name }),
    body: t('desk.markdone.body'),
    confirm: t('desk.markdone.button'),
  }))) return;
  try {
    await api(`/api/jobs/${job.id}/mark-done`, { method: 'POST' });
    toast(t('desk.markdone.done', { name }), 'ok');
    after();
  } catch (e) { toast(e.message, 'bad'); }
}

/** «Λάθος βίντεο»: the same dialog wherever it appears (a held video in
 *  approval.js, an automatic delivery here); `what` says what follows. */
export function confirmWrongVideo(name, what) {
  return confirmDialog({
    title: t('desk.wrong.title', { name }),
    body: what,
    confirm: t('desk.wrong.button'),
    danger: true,
  });
}

/** MCR says a video that a trusted recipe delivered without approval was the wrong one (P10.16a):
 *  the recipe loses its trust on that site. The file is already in Dalet and is never touched. */
export async function wrongVideoJob(job, after) {
  const name = deliveredName(job);
  if (!(await confirmWrongVideo(name, [t('desk.wrong.auto.body1'), t('desk.wrong.auto.body2')]))) return;
  try {
    await api(`/api/jobs/${job.id}/wrong-video`, { method: 'POST' });
    toast(t('desk.wrong.auto.done'), 'ok');
    after();
  } catch (e) { toast(e.message, 'bad'); }
}

/** The note of a delivery that skipped MCR's approval, with "Λάθος βίντεο" while MCR has not pressed it; null for other jobs. */
export function autoDeliveredNode(job, after) {
  const repair = job && job.repair;
  if (!repair || repair.auto_delivered !== true) return null;
  const n = Number(repair.recipe_approvals) || 0;
  return el('div', { class: 'jc-found auto-delivered' },
    el('span', {}, t('desk.auto.note', { n })),
    repair.wrong_video
      ? el('span', { class: 'badge bad gap-left' }, t('desk.auto.revoked'))
      : el('button', {
        class: 'btn btn-danger gap-left', type: 'button',
        title: t('desk.auto.wrong_title'),
        onClick: () => wrongVideoJob(job, after),
      }, t('desk.wrong.button')));
}

/** Queue one of the videos the sniffer found in a job's article. */
export async function queueOffer(jobId, offer, after) {
  try {
    const r = await api(`/api/jobs/${jobId}/offers/queue`, { method: 'POST', body: { url: offer.url } });
    toast(t('desk.offer.done', { index: r.index_str, id: r.job_id }), 'ok');
    after();
  } catch (e) { toast(e.message, 'bad'); }
}

/* ------------------------------------------------------------------ *
 * A video's history, in words, with the raw lines underneath (P7.15).
 * Shared by the Email tab's job card and the review card.
 * ------------------------------------------------------------------ */

/** `14:05`, or a dash when there is no usable time. */
export function clock(at) {
  const d = at ? new Date(at) : null;
  if (!d || Number.isNaN(d.getTime())) return '—';
  return d.toLocaleTimeString(t('locale'), { hour: '2-digit', minute: '2-digit', hour12: false });
}

/** One event in words, or null when it is only for engineers. The server's
 *  messages are English; a Greek ERROR line is already for MCR. */
export function eventText(ev) {
  const m = String(ev.message || '');
  let x;
  if (/^Queued as /.test(m)) return t('event.queued');
  if ((x = /^Stage (\w+)$/.exec(m))) return STAGE_TEXT[x[1]] || x[1];
  if (/^Finished as COMPLETED_MANUAL/.test(m)) return t('event.done_manual');
  if (/^Finished as COMPLETED/.test(m)) return t('event.done');
  if (/^Finished as AWAITING_APPROVAL/.test(m)) return t('event.held');
  if (/^Finished as MANUAL_DOWNLOAD/.test(m)) return t('event.manual');
  if (/^Finished as /.test(m)) return t('event.review');
  if ((x = /retrying in (\d+) s/.exec(m))) return t('event.retrying', { n: x[1] });
  if (/^Download again requested/.test(m)) return t('event.redownload');
  if ((x = /^Renamed to (\S+)/.exec(m))) return t('event.renamed', { name: x[1] });
  if (/^Queued by .* from the email/.test(m)) return t('event.added_from_mail');
  if ((x = /^Offered video queued by MCR as (\S+)/.exec(m))) return t('event.offer_queued', { index: x[1] });
  if (/more video\(s\) in this article offered to MCR/.test(m)) return t('event.offers');
  if (/^The page holds \d+ videos/.test(m)) return t('event.many');
  if (/^Direct download failed; sniffing/.test(m)) return t('event.sniffing');
  if (/^Sniffed stream:/.test(m)) return t('event.sniffed');
  if (/^Sniffer found no stream/.test(m)) return t('event.sniff_none');
  if (ev.level === 'ERROR' && [...m].some((c) => c.charCodeAt(0) >= 0x370 && c.charCodeAt(0) <= 0x3ff)) return m;
  return null;
}

export function eventClass(ev) {
  if (/^Finished as COMPLETED/.test(ev.message || '')) return 'ok';
  if (ev.level === 'ERROR') return 'bad';
  if (ev.level === 'WARN') return 'warn';
  return /^Stage /.test(ev.message || '') ? 'run' : '';
}

/**
 * The timeline list plus the raw lines under «Τεχνικές λεπτομέρειες».
 * `techIsOpen` says whether the raw lines start open; `onTechToggle(open)` is
 * told when the person opens or closes them, so a re-render keeps the choice.
 */
export function timelineNode(events, techIsOpen, onTechToggle) {
  const told = events.map((ev) => [ev, eventText(ev)]).filter(([, text]) => text);
  const details = el('details', { class: 'tech' },
    el('summary', {}, t('desk.tech_count', { n: events.length })),
    el('ul', { class: 'tech-log mono' }, events.map((ev) => el('li', {}, `${clock(ev.at)}  ${ev.level}  ${ev.stage || ''}  ${ev.message}`))));
  if (techIsOpen) details.open = true;
  details.addEventListener('toggle', () => onTechToggle(details.open));
  return el('div', { class: 'jc-history' },
    told.length ? el('ul', { class: 'timeline' }, told.map(([ev, text]) => el('li', { class: eventClass(ev) }, el('time', {}, clock(ev.at)), text))) : null,
    details);
}

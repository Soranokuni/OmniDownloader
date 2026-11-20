/* Every job status in one place (plan P12.1): its words, its colour family
 * and the mark that goes with the colour. The desk, the Email tab's rail and
 * summary line, the approval cards and the journalist's /user page all read
 * this table, so a status says the same thing and has the same colour
 * wherever it appears.
 *
 * Colour never stands alone: every badge carries its word, and every small
 * coloured tag carries its mark, for an operator who cannot tell amber from
 * violet on a control-room monitor.
 *
 * The colours themselves are in status.css, one rule per tone; the words are
 * in text.el.js (`status.<STATUS>.label|short|phrase|user`, `tone.<tone>`).
 */

import { el } from '/static/app.js?v=6';
import { t } from '/static/text.js?v=2';

/** The colour families and their marks (a text glyph: no sprite needed, so a
 *  tag keeps it at any size). The badge classes are the tone names. */
export const TONES = {
  ok: { mark: '✓' },
  run: { mark: '↻' },
  wait: { mark: '◷' },
  approval: { mark: '◉' },
  warn: { mark: '!' },
  bad: { mark: '✕' },
  off: { mark: '⊘' },
};

/** The tone of every status the server sends (`JobStatus::as_str`). */
export const STATUS_TONE = {
  PENDING: 'wait',
  RUNNING: 'run',
  COMPLETED: 'ok',
  COMPLETED_MANUAL: 'ok',
  AWAITING_APPROVAL: 'approval',
  REQUIRES_REVIEW: 'warn',
  MANUAL_DOWNLOAD: 'warn',
  FAILED: 'bad',
  CANCELLED: 'off',
};

/**
 * A status's row, read in the current language each time:
 *   label   the badge, on the desk
 *   short   the narrow rail beside a mail's text
 *   phrase  inside a sentence («2 σε αναμονή · 1 για έγκριση»)
 *   user    what the journalist who sent it reads on /user
 *   tone    the colour family above
 */
function row(status, tone) {
  return {
    get label() { return t(`status.${status}.label`); },
    get short() { return t(`status.${status}.short`); },
    get phrase() { return t(`status.${status}.phrase`); },
    get user() { return t(`status.${status}.user`); },
    tone,
  };
}

export const STATUS = Object.fromEntries(Object.entries(STATUS_TONE).map(([s, tone]) => [s, row(s, tone)]));

/** A status the page does not know yet (a newer server): treat it as
 *  needing a look rather than hide it. */
const UNKNOWN = row('UNKNOWN', 'warn');

/** The row for a status string. */
export function statusInfo(status) {
  return STATUS[status] || UNKNOWN;
}

/** The colour family of a job's state. */
export function tone(job) {
  return statusInfo(job && job.status).tone;
}

/** The glyph of a tone, aria-hidden: the word beside it is what is read. */
export function markNode(name) {
  return el('span', { class: 'mark', 'aria-hidden': 'true' }, (TONES[name] || TONES.warn).mark);
}

/**
 * The status badge: mark and word in the tone's colour. `form` picks the
 * text: 'label' (the desk), 'short' (the rail) or 'user' (/user).
 */
export function statusBadge(job, form = 'label') {
  const info = statusInfo(job && job.status);
  return el('span', { class: `badge ${info.tone}` }, markNode(info.tone), info[form] || info.label);
}

/** A small coloured tag, such as a file's «1B» beside its link, with its
 *  mark, and the status word as its tooltip and accessible name. */
export function statusTag(job, text, title) {
  const info = statusInfo(job && job.status);
  const name = title || `${text}: ${info.label}`;
  return el('span', { class: `tag ${info.tone}`, title: name, 'aria-label': name }, markNode(info.tone), text);
}

/** The key under the Email tab's text: one entry per tone, plus the dashed
 *  «δεν κατέβηκε» of a link that became no file. */
export function legendNode() {
  const item = (name) => el('span', {}, el('i', { class: name }, name === 'skip' ? '' : TONES[name].mark), t(`tone.${name}`));
  return el('span', { class: 'legend', 'aria-hidden': 'true' },
    ...Object.keys(TONES).map(item),
    item('skip'));
}

/** Needs a person: review, a file-locker download, a failure. */
export function needsAttention(job) {
  return job.status === 'REQUIRES_REVIEW' || job.status === 'MANUAL_DOWNLOAD' || job.status === 'FAILED';
}

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
 * The colours themselves are in status.css, one rule per tone.
 */

import { el } from '/static/app.js?v=6';

/**
 * The colour families. `mark` is a text glyph (no sprite needed, so a tag
 * keeps it at any size); `legend` is the word under the Email tab's text.
 * The badge classes are the tone names (`badge run`, `badge off`, …).
 */
export const TONES = {
  ok: { mark: '✓', legend: 'παραδόθηκε' },
  run: { mark: '↻', legend: 'σε εξέλιξη' },
  wait: { mark: '◷', legend: 'σε αναμονή' },
  approval: { mark: '◉', legend: 'για έγκριση' },
  warn: { mark: '!', legend: 'χρειάζεται έλεγχο' },
  bad: { mark: '✕', legend: 'απέτυχε' },
  off: { mark: '⊘', legend: 'ακυρώθηκε' },
};

/**
 * One row per status the server sends (`JobStatus::as_str`):
 *   label   the badge, on the desk
 *   short   the narrow rail beside a mail's text
 *   phrase  inside a sentence («2 σε αναμονή · 1 για έγκριση»)
 *   user    what the journalist who sent it reads on /user
 *   tone    the colour family above
 */
export const STATUS = {
  PENDING: { label: 'Σε αναμονή', short: 'Αναμονή', phrase: 'σε αναμονή', user: 'Σε αναμονή', tone: 'wait' },
  RUNNING: { label: 'Σε εξέλιξη', short: 'Σε εξέλιξη', phrase: 'σε εξέλιξη', user: 'Σε επεξεργασία', tone: 'run' },
  COMPLETED: { label: 'Παραδόθηκε', short: 'Παραδόθηκε', phrase: 'παραδόθηκε', user: 'Παραδόθηκε', tone: 'ok' },
  COMPLETED_MANUAL: { label: 'Παραδόθηκε χειροκίνητα', short: 'Παραδόθηκε', phrase: 'παραδόθηκε χειροκίνητα', user: 'Παραδόθηκε', tone: 'ok' },
  AWAITING_APPROVAL: { label: 'Για έγκριση', short: 'Για έγκριση', phrase: 'για έγκριση', user: 'Το βλέπει το MCR', tone: 'approval' },
  REQUIRES_REVIEW: { label: 'Χρειάζεται έλεγχο', short: 'Έλεγχος', phrase: 'χρειάζεται έλεγχο', user: 'Το εξετάζει το MCR', tone: 'warn' },
  MANUAL_DOWNLOAD: { label: 'Χειροκίνητη λήψη', short: 'Χειροκίνητα', phrase: 'χειροκίνητη λήψη', user: 'Το κατεβάζει το MCR', tone: 'warn' },
  FAILED: { label: 'Απέτυχε', short: 'Απέτυχε', phrase: 'απέτυχε', user: 'Απέτυχε', tone: 'bad' },
  CANCELLED: { label: 'Ακυρώθηκε', short: 'Ακυρώθηκε', phrase: 'ακυρώθηκε', user: 'Ακυρώθηκε', tone: 'off' },
};

/** A status the page does not know yet (a newer server): treat it as
 *  needing a look rather than hide it. */
const UNKNOWN = { label: 'Χρειάζεται έλεγχο', short: 'Έλεγχος', phrase: 'χρειάζεται έλεγχο', user: 'Το εξετάζει το MCR', tone: 'warn' };

/** The row for a status string. */
export function statusInfo(status) {
  return STATUS[status] || UNKNOWN;
}

/** The colour family of a job's state. */
export function tone(job) {
  return statusInfo(job && job.status).tone;
}

/** The glyph of a tone, aria-hidden: the word beside it is what is read. */
export function markNode(t) {
  return el('span', { class: 'mark', 'aria-hidden': 'true' }, (TONES[t] || TONES.warn).mark);
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
  const item = (t, text) => el('span', {}, el('i', { class: t }, t === 'skip' ? '' : TONES[t].mark), text);
  return el('span', { class: 'legend', 'aria-hidden': 'true' },
    ...Object.entries(TONES).map(([t, v]) => item(t, v.legend)),
    item('skip', 'δεν κατέβηκε'));
}

/** Needs a person: review, a file-locker download, a failure. */
export function needsAttention(job) {
  return job.status === 'REQUIRES_REVIEW' || job.status === 'MANUAL_DOWNLOAD' || job.status === 'FAILED';
}

/* The review tab as a work list (plan P12.2): grouped by what the operator
 * has to do, not by date. «Για έγκριση» is the approval section above the
 * list (approval.js); the four groups here follow it in this order.
 *
 * A job is in the list only while it waits for a person. A video that will
 * be tried again by itself is still PENDING and is on «Σε εξέλιξη», never
 * here, and nothing in this list says it will retry by itself.
 *
 * Pure: no DOM, no requests. mcr.js builds the cards from what this says.
 */

import { t } from '/static/text.js?v=5';
import EL from '/static/text.el.js?v=5';

/** The groups after «Για έγκριση», in the order they are shown (their names
 *  are `work.group.<key>` in text.el.js). `primary` is the one action the
 *  card shows outside «Περισσότερα». */
export const GROUPS = [
  { key: 'cookies', primary: 'override' },
  { key: 'novideo', primary: 'override' },
  { key: 'manual', primary: 'markdone' },
  { key: 'other', primary: 'retry' },
];

/** A login wall: the site wants a signed-in visitor. */
const COOKIE_CODES = ['LOGIN_REQUIRED'];
/** The link has no video the system can find: another link is the fix. */
const NO_VIDEO_CODES = ['NO_STREAM_FOUND', 'NO_VIDEO_ON_PAGE', 'UNSUPPORTED_URL', 'PAGE_NOT_FOUND', 'PRIVATE_OR_REMOVED'];
/** The video is there but a person has to fetch it and put it in Dalet. */
const MANUAL_CODES = ['MANUAL_DOWNLOAD', 'DRM_PROTECTED', 'VIDEO_UNRESOLVED', 'REPAIR_EXHAUSTED', 'REJECTED_BY_MCR'];

/** The statuses that wait for a person. Everything else (waiting, running,
 *  held for approval, delivered, cancelled) is not in this list. */
const NEEDS_YOU = ['REQUIRES_REVIEW', 'MANUAL_DOWNLOAD', 'FAILED'];

export function needsYou(job) {
  return !!job && NEEDS_YOU.includes(job.status);
}

/** The group key of a job, or null when it is not in the list. The state
 *  comes first: a file-locker link (MANUAL_DOWNLOAD) is a manual download
 *  whatever code it carries. */
export function groupOf(job) {
  if (!needsYou(job)) return null;
  if (job.status === 'MANUAL_DOWNLOAD') return 'manual';
  const code = String(job.error_code || '');
  if (COOKIE_CODES.includes(code)) return 'cookies';
  if (NO_VIDEO_CODES.includes(code)) return 'novideo';
  if (MANUAL_CODES.includes(code)) return 'manual';
  return 'other';
}

export function groupLabel(key) {
  return GROUPS.some((x) => x.key === key) ? t(`work.group.${key}`) : '';
}

/** The card's one primary action: 'override' (another link), 'markdone'
 *  («Το έβαλα στο Dalet») or 'retry'. An interrupted delivery whose file may
 *  already be in Dalet never offers a retry first: a second copy could go on air. */
export function primaryOf(job) {
  if (job.error_code === 'DELIVERY_UNCONFIRMED') return 'markdone';
  const g = GROUPS.find((x) => x.key === groupOf(job));
  return g ? g.primary : 'retry';
}

/** The list's jobs in group order, the server's order (newest first) kept
 *  within a group. Jobs that do not wait for a person are dropped. */
export function inGroupOrder(jobs) {
  const rank = (j) => GROUPS.findIndex((g) => g.key === groupOf(j));
  return jobs
    .filter(needsYou)
    .map((job, i) => ({ job, i, r: rank(job) }))
    .sort((a, b) => a.r - b.r || a.i - b.i)
    .map((x) => x.job);
}

/**
 * Why the video waits for a person, in one sentence. It depends on the
 * state as well as the code: a file-locker link says what to do with the
 * file, and a job in this list never says it is being retried (a code's own
 * hint may, for a job that is still retrying; this one has stopped). The
 * server's hint is Greek, so it is matched against the Greek words.
 */
export function whyText(job) {
  if (job.status === 'MANUAL_DOWNLOAD') return job.hint || t('work.why.locker');
  const hint = String(job.hint || '').trim();
  if (hint && hint.includes(EL['server.retrying'])) return t('work.why.stopped');
  if (hint) return hint;
  return job.status === 'FAILED' ? t('work.why.failed') : t('work.why.review');
}

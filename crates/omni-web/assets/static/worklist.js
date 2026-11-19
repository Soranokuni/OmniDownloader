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

/** The groups after «Για έγκριση», in the order they are shown. `primary`
 *  is the one action the card shows outside «Περισσότερα». */
export const GROUPS = [
  { key: 'cookies', label: 'Χρειάζεται σύνδεση (cookies)', primary: 'override' },
  { key: 'novideo', label: 'Δεν βρέθηκε βίντεο', primary: 'override' },
  { key: 'manual', label: 'Χειροκίνητη λήψη', primary: 'markdone' },
  { key: 'other', label: 'Άλλο', primary: 'retry' },
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
  const g = GROUPS.find((x) => x.key === key);
  return g ? g.label : '';
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

const LOCKER_TEXT = 'Σύνδεσμος μεταφοράς αρχείων: κατεβάστε το αρχείο από τον σύνδεσμο, βάλτε το στο Dalet και πατήστε «Το έβαλα στο Dalet».';
const STOPPED_TEXT = 'Οι αυτόματες προσπάθειες τελείωσαν. Πατήστε «Δοκιμή ξανά» ή δώστε άλλον σύνδεσμο.';

/**
 * Why the video waits for a person, in one Greek sentence. It depends on the
 * state as well as the code: a file-locker link says what to do with the
 * file, and a job in this list never says it is being retried (a code's own
 * hint may, for a job that is still retrying; this one has stopped).
 */
export function whyText(job) {
  if (job.status === 'MANUAL_DOWNLOAD') return job.hint || LOCKER_TEXT;
  const hint = String(job.hint || '').trim();
  if (hint && /αυτόματη επανάληψη/.test(hint)) return STOPPED_TEXT;
  if (hint) return hint;
  return job.status === 'FAILED'
    ? 'Η λήψη σταμάτησε με σφάλμα και δεν θα ξαναδοκιμαστεί αυτόματα.'
    : 'Το βίντεο δεν παραδόθηκε αυτόματα.';
}

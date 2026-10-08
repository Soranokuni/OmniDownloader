/* What the MCR desk's lists share (plan P7.1, P7.10): the words for a job's
 * state, its file name, the pager, and the actions on a job. Moved out of
 * mcr.js when the Email tab (inbox.js) came to need the same ones; the
 * confirmations and messages are the same wherever a job is acted on.
 *
 * Nothing here builds HTML from a string: see app.js.
 */

import { api, el, render, toast } from '/static/app.js?v=6';

/** What a job is doing, for a person. */
export const STAGE_TEXT = {
  QUEUED: 'Σε αναμονή',
  EXTRACT: 'Αναζήτηση του βίντεο στη σελίδα',
  DOWNLOAD: 'Λήψη',
  PROBE: 'Έλεγχος του αρχείου που κατέβηκε',
  TRANSCODE: 'Μετατροπή στη μορφή εκπομπής',
  REWRAP: 'Πακετάρισμα σε MXF',
  VERIFY: 'Έλεγχος προδιαγραφών εκπομπής',
  DELIVER: 'Παράδοση στο watchfolder του Dalet',
  ARCHIVE: 'Αρχειοθέτηση',
  DONE: 'Ολοκλήρωση',
};
export const STAGE_ORDER = ['EXTRACT', 'DOWNLOAD', 'PROBE', 'TRANSCODE', 'REWRAP', 'VERIFY', 'DELIVER'];

export function stageLine(job) {
  if (job.status === 'PENDING') return 'Σε αναμονή';
  const text = STAGE_TEXT[job.stage] || 'Σε επεξεργασία';
  const step = STAGE_ORDER.indexOf(job.stage);
  return step >= 0 ? `Βήμα ${step + 1} από ${STAGE_ORDER.length} · ${text}` : text;
}

export function statusBadge(job) {
  switch (job.status) {
    case 'PENDING': return el('span', { class: 'badge' }, 'Σε αναμονή');
    case 'RUNNING': return el('span', { class: 'badge info' }, 'Σε επεξεργασία');
    case 'COMPLETED': return el('span', { class: 'badge ok' }, 'Παραδόθηκε');
    case 'COMPLETED_MANUAL': return el('span', { class: 'badge ok' }, 'Παραδόθηκε χειροκίνητα');
    case 'MANUAL_DOWNLOAD': return el('span', { class: 'badge warn' }, 'Χειροκίνητη λήψη');
    case 'FAILED': return el('span', { class: 'badge bad' }, 'Απέτυχε');
    case 'CANCELLED': return el('span', { class: 'badge' }, 'Ακυρώθηκε');
    default: return el('span', { class: 'badge warn' }, 'Χρειάζεται έλεγχο');
  }
}

/** The colour family of a job's state: ok, run, wait, warn, bad. */
export function tone(job) {
  switch (job.status) {
    case 'COMPLETED':
    case 'COMPLETED_MANUAL':
      return 'ok';
    case 'RUNNING': return 'run';
    case 'PENDING':
    case 'CANCELLED':
      return 'wait';
    case 'FAILED': return 'bad';
    default: return 'warn';
  }
}

/** Needs a person: review, a file-locker download, a failure. */
export function needsAttention(job) {
  return job.status === 'REQUIRES_REVIEW' || job.status === 'MANUAL_DOWNLOAD' || job.status === 'FAILED';
}

/** "Μόνο τα 2 πρώτα βίντεο", when the journalist asked for that. */
export function limitNote(job) {
  const n = Number(job.max_videos) || 0;
  if (!n) return null;
  return el('span', { class: 'badge', style: 'margin-left:8px', title: 'Όπως ζήτησε ο δημοσιογράφος· τα υπόλοιπα βίντεο του άρθρου προτείνονται για προσθήκη' },
    n === 1 ? 'Μόνο το πρώτο βίντεο' : `Μόνο τα ${n} πρώτα βίντεο`);
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
  const go = (p) => { state.page = p; onChange(); window.scrollTo({ top: 0, behavior: 'smooth' }); };
  render(targetId, el('div', { class: 'section-head card', style: 'padding:10px 14px' },
    el('span', { class: 'note' }, `Εμφανίζονται ${first}–${last} από ${total}`),
    el('div', { class: 'row tight' },
      el('button', { class: 'btn', type: 'button', disabled: state.page <= 1, onClick: () => go(1) }, '« Πρώτη'),
      el('button', { class: 'btn', type: 'button', disabled: state.page <= 1, onClick: () => go(state.page - 1) }, '‹ Προηγούμενη'),
      el('span', { class: 'note', style: 'padding:0 8px' }, `Σελίδα ${state.page} από ${pageCount}`),
      el('button', { class: 'btn', type: 'button', disabled: state.page >= pageCount, onClick: () => go(state.page + 1) }, 'Επόμενη ›'),
      el('button', { class: 'btn', type: 'button', disabled: state.page >= pageCount, onClick: () => go(pageCount) }, 'Τελευταία »'),
    ),
  ));
}

/* ------------------------------------------------------------------ *
 * Actions. Each confirms what cannot be undone, says what happened, and
 * calls `after` so the list it came from refreshes.
 * ------------------------------------------------------------------ */

export async function retryJob(job, after) {
  try {
    await api(`/api/jobs/${job.id}/retry`, { method: 'POST' });
    toast(`Το ${fileName(job)} μπήκε ξανά στην ουρά.`, 'ok');
    after();
  } catch (e) { toast(e.message, 'bad'); }
}

export async function overrideJob(job, url, after) {
  const trimmed = (url || '').trim();
  if (!trimmed) { toast('Επικολλήστε πρώτα έναν σύνδεσμο.', 'bad'); return false; }
  try {
    await api(`/api/jobs/${job.id}/override`, { method: 'POST', body: { url: trimmed } });
    toast(`Το ${fileName(job)} μπήκε ξανά στην ουρά με τον νέο σύνδεσμο.`, 'ok');
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

/** Give a video a better keyword before it is delivered: the last part of
 *  its file name, typed in Greek or Latin. */
export async function renameJob(job, after) {
  const now = job.keyword || '';
  const typed = prompt(
    `Νέα λέξη-κλειδί για το ${fileName(job)}\n\n` +
    `Γράψτε το θέμα του βίντεο, ελληνικά ή λατινικά (π.χ. ΣΕΙΣΜΟΣ ΣΗΤΕΙΑ). ` +
    `Το αρχείο θα παραδοθεί ως ${job.index_str}_${job.journalist}_ΛΕΞΗ.mxf.`,
    now,
  );
  if (typed === null || !typed.trim() || typed.trim() === now) return;
  try {
    const r = await api(`/api/jobs/${job.id}/rename`, { method: 'POST', body: { keyword: typed } });
    toast(`Θα παραδοθεί ως ${r.slug}.mxf.`, 'ok');
    after();
  } catch (e) { toast(e.message, 'bad'); }
}

export async function discardJob(job, after) {
  if (!confirm(
    `Να αφαιρεθεί το ${fileName(job)} (εργασία #${job.id});\n\n` +
    'Φεύγει οριστικά από τις λίστες. Ένα αρχείο που έχει ήδη παραδοθεί στο Dalet δεν επηρεάζεται.',
  )) return;
  try {
    await api(`/api/jobs/${job.id}/discard`, { method: 'POST' });
    toast(`Η εργασία #${job.id} αφαιρέθηκε.`, 'ok');
    after();
  } catch (e) { toast(e.message, 'bad'); }
}

export async function redownloadJob(job, after) {
  const name = deliveredName(job);
  if (!confirm(
    `Νέα λήψη του ${name};\n\n` +
    'Το βίντεο κατεβαίνει από τον σύνδεσμό του, μετατρέπεται και παραδίδεται ξανά στο watchfolder. ' +
    'Αν το παλιό αρχείο υπάρχει ακόμη, το νέο αποθηκεύεται δίπλα του με έναν αριθμό στο τέλος (…_2.mxf)· τίποτα δεν αντικαθίσταται.',
  )) return;
  try {
    await api(`/api/jobs/${job.id}/redownload`, { method: 'POST' });
    toast(`Το ${name} μπήκε ξανά στην ουρά.`, 'ok');
    after();
  } catch (e) { toast(e.message, 'bad'); }
}

/** MCR put the video into Dalet by hand: record it as done (P7.16). */
export async function markDoneJob(job, after) {
  const name = fileName(job);
  if (!confirm(
    `Σημείωση του ${name} ως παραδομένου χειροκίνητα;\n\n` +
    'Βεβαιωθείτε ότι το βίντεο είναι ήδη στο Dalet: από εδώ δεν θα σταλεί αρχείο.',
  )) return;
  try {
    await api(`/api/jobs/${job.id}/mark-done`, { method: 'POST' });
    toast(`Το ${name} σημειώθηκε ως παραδομένο.`, 'ok');
    after();
  } catch (e) { toast(e.message, 'bad'); }
}

/** Queue one of the videos the sniffer found in a job's article. */
export async function queueOffer(jobId, offer, after) {
  try {
    const r = await api(`/api/jobs/${jobId}/offers/queue`, { method: 'POST', body: { url: offer.url } });
    toast(`Προστέθηκε ως ${r.index_str} (εργασία #${r.job_id}).`, 'ok');
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
  return d.toLocaleTimeString('el-GR', { hour: '2-digit', minute: '2-digit', hour12: false });
}

/** One event in Greek, or null when it is only for engineers. */
export function eventText(ev) {
  const m = String(ev.message || '');
  let x;
  if (/^Queued as /.test(m)) return 'Μπήκε στην ουρά';
  if ((x = /^Stage (\w+)$/.exec(m))) return STAGE_TEXT[x[1]] || x[1];
  if (/^Finished as COMPLETED_MANUAL/.test(m)) return 'Παραδόθηκε χειροκίνητα';
  if (/^Finished as COMPLETED/.test(m)) return 'Παραδόθηκε στο Dalet';
  if (/^Finished as MANUAL_DOWNLOAD/.test(m)) return 'Σταμάτησε: χρειάζεται χειροκίνητη λήψη';
  if (/^Finished as /.test(m)) return 'Σταμάτησε: χρειάζεται έλεγχο';
  if ((x = /retrying in (\d+) s/.exec(m))) return `Πρόβλημα· νέα προσπάθεια σε ${x[1]} δευτ.`;
  if (/^Download again requested/.test(m)) return 'Νέα λήψη από το MCR';
  if ((x = /^Renamed to (\S+)/.exec(m))) return `Έγινε ${x[1]}: το άρθρο έχει κι άλλα βίντεο`;
  if (/^Queued by .* from the email/.test(m)) return 'Προστέθηκε από το MCR μέσα από το email';
  if ((x = /^Offered video queued by MCR as (\S+)/.exec(m))) return `Το MCR πρόσθεσε το προτεινόμενο ${x[1]}`;
  if (/more video\(s\) in this article offered to MCR/.test(m)) return 'Βρέθηκαν κι άλλα βίντεο στο άρθρο· προτείνονται';
  if (/^The page holds \d+ videos/.test(m)) return 'Η σελίδα έχει πολλά βίντεο· αυτή η εργασία παίρνει το πρώτο';
  if (/^Direct download failed; sniffing/.test(m)) return 'Ψάχνει το βίντεο μέσα στη σελίδα';
  if (/^Sniffed stream:/.test(m)) return 'Βρέθηκε το βίντεο στη σελίδα';
  if (/^Sniffer found no stream/.test(m)) return 'Δεν βρέθηκε βίντεο στη σελίδα';
  if (ev.level === 'ERROR' && /[Ͱ-Ͽ]/.test(m)) return m;
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
  const told = events.map((ev) => [ev, eventText(ev)]).filter(([, t]) => t);
  const details = el('details', { class: 'tech' },
    el('summary', {}, `Τεχνικές λεπτομέρειες (${events.length})`),
    el('ul', { class: 'tech-log mono' }, events.map((ev) => el('li', {}, `${clock(ev.at)}  ${ev.level}  ${ev.stage || ''}  ${ev.message}`))));
  if (techIsOpen) details.open = true;
  details.addEventListener('toggle', () => onTechToggle(details.open));
  return el('div', { class: 'jc-history' },
    told.length ? el('ul', { class: 'timeline' }, told.map(([ev, t]) => el('li', { class: eventClass(ev) }, el('time', {}, clock(ev.at)), t))) : null,
    details);
}

/* The MCR desk's Email tab (plan P7.10).
 *
 * One entry per mail (and per link added by hand), newest first. An entry
 * opens in place: the mail's text on the left, with every link tagged by
 * the file it became and coloured by how that file is doing, and the mail's
 * videos on the right. Pointing at a video lights up its link in the text;
 * clicking either pins the pair, shows the video's history and what can be
 * done about it.
 *
 * Everything shown here comes from email: subjects, names, the text itself.
 * Nothing is built from an HTML string; `el()` makes nodes and text goes in
 * as text (see app.js), so a mail whose body is markup shows as markup.
 *
 * The list refreshes every few seconds. Only what changed is redrawn, so a
 * mail being read does not jump, the pointer's highlight stays put, and a
 * half-typed link is never lost.
 */

import { api, el, render, toast, fmtTime, fmtDuration, safeHref, icon } from '/static/app.js?v=5';
import {
  STAGE_TEXT, STAGE_ORDER, tone, needsAttention, limitNote, fileName, deliveredName,
  pager, retryJob, overrideJob, discardJob, redownloadJob, queueOffer, canRename, renameJob,
} from '/static/desk.js?v=2';

/* ------------------------------------------------------------------ *
 * State
 * ------------------------------------------------------------------ */

const list = { filter: 'all', q: '', page: 1, perPage: 20 };
let entries = [];
let windowDays = 30;
/** The newest arrival already shown: anything newer slides in as news. */
let newestShown = null;

/** entry id -> { card, head, inner, entry, sig } */
const cards = new Map();
const dayNodes = new Map();

/** The open entry: { id, kind, key } */
let open = null;
/** Its full view, and lookups over it. */
let detail = null;
let index = null;
/** What is pinned: { job } | { url, chip } */
let selected = null;
/** Jobs whose «Άλλος σύνδεσμος» field is open. */
const overrideOpen = new Set();
/** Job id -> its timeline, fetched when it is selected. */
const timelines = new Map();
/** Job ids whose technical details are unfolded. */
const techOpen = new Set();

let hooks = { onCounts: () => {}, refresh: () => {}, groupName: (c) => c };

const reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)');
const smooth = () => (reducedMotion.matches ? 'auto' : 'smooth');

const entryId = (e) => `${e.kind}:${e.key}`;
const isActiveTab = () => {
  const section = document.getElementById('tab-email');
  return !!section && !section.hidden;
};

/* ------------------------------------------------------------------ *
 * Words
 * ------------------------------------------------------------------ */

/** Short stage names, for the one-line summary of a mail. */
const STAGE_SHORT = {
  EXTRACT: 'αναζήτηση στη σελίδα',
  DOWNLOAD: 'λήψη',
  PROBE: 'έλεγχος αρχείου',
  TRANSCODE: 'μετατροπή',
  REWRAP: 'πακετάρισμα MXF',
  VERIFY: 'έλεγχος προδιαγραφών',
  DELIVER: 'παράδοση',
};

function shortStatus(j) {
  switch (j.status) {
    case 'RUNNING': return `${STAGE_SHORT[j.stage] || 'σε επεξεργασία'} ${pct(j)}%`;
    case 'PENDING': return 'σε αναμονή';
    case 'COMPLETED': return 'παραδόθηκε';
    case 'COMPLETED_MANUAL': return 'παραδόθηκε χειροκίνητα';
    case 'MANUAL_DOWNLOAD': return 'χειροκίνητη λήψη';
    case 'FAILED': return 'απέτυχε';
    case 'CANCELLED': return 'ακυρώθηκε';
    default: return 'χρειάζεται έλεγχο';
  }
}

const pct = (j) => Math.max(0, Math.min(100, Math.round(Number(j.progress) || 0)));

/** The state in a word or two: the rail is narrow, and the file name
 *  beside it is what an operator reads first. */
function railBadge(j) {
  const [cls, text] = {
    PENDING: ['', 'Αναμονή'],
    RUNNING: ['info', 'Σε εξέλιξη'],
    COMPLETED: ['ok', 'Παραδόθηκε'],
    COMPLETED_MANUAL: ['ok', 'Παραδόθηκε'],
    MANUAL_DOWNLOAD: ['warn', 'Χειροκίνητα'],
    FAILED: ['bad', 'Απέτυχε'],
    CANCELLED: ['', 'Ακυρώθηκε'],
  }[j.status] || ['warn', 'Έλεγχος'];
  return el('span', { class: `badge ${cls}`.trim() }, text);
}

/** "3 βίντεο · 1 απέτυχε · 1B: μετατροπή 63% · 1 παραδόθηκε" */
function summary(e) {
  if (e.state === 'failed') return [el('b', {}, 'Δεν διαβάστηκε'), ' · ανοίξτε το για να διαβαστεί ξανά'];
  const js = e.jobs || [];
  if (!js.length) {
    return [e.outcome === 'PHOTOS_ONLY' ? 'Μόνο φωτογραφίες· τίποτα για λήψη' : 'Δεν βρέθηκε βίντεο'];
  }
  const c = { ok: 0, run: 0, wait: 0, warn: 0, bad: 0 };
  for (const j of js) c[tone(j)] += 1;
  const head = el('b', {}, js.length === 1 ? '1 βίντεο' : `${js.length} βίντεο`);
  if (c.ok === js.length) return [head, js.length === 1 ? ' · παραδόθηκε' : ' · όλα παραδόθηκαν'];
  const parts = [];
  if (c.bad) parts.push(c.bad === 1 ? '1 απέτυχε' : `${c.bad} απέτυχαν`);
  if (c.warn) parts.push(c.warn === 1 ? '1 θέλει έλεγχο' : `${c.warn} θέλουν έλεγχο`);
  if (c.run) {
    const r = js.find((j) => j.status === 'RUNNING');
    parts.push(c.run === 1 ? `${r.index_str}: ${shortStatus(r)}` : `${c.run} σε εξέλιξη`);
  }
  if (c.wait) parts.push(`${c.wait} σε αναμονή`);
  if (c.ok) parts.push(c.ok === 1 ? '1 παραδόθηκε' : `${c.ok} παραδόθηκαν`);
  return [head, ' · ', parts.join(' · ')];
}

function dayLabel(at) {
  const d = at ? new Date(at) : null;
  if (!d || Number.isNaN(d.getTime())) return 'Χωρίς ημερομηνία';
  const today = new Date();
  const yesterday = new Date(today.getFullYear(), today.getMonth(), today.getDate() - 1);
  const same = (a, b) => a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
  const long = d.toLocaleDateString('el-GR', { weekday: 'long', day: 'numeric', month: 'long' });
  if (same(d, today)) return 'Σήμερα';
  if (same(d, yesterday)) return `Χθες · ${long}`;
  return long;
}

function clock(at) {
  const d = at ? new Date(at) : null;
  if (!d || Number.isNaN(d.getTime())) return '—';
  return d.toLocaleTimeString('el-GR', { hour: '2-digit', minute: '2-digit', hour12: false });
}

/** `portal.gr/kosmos/…`: enough of a link to recognise it. */
function shortUrl(url) {
  const s = String(url || '').replace(/^https?:\/\/(www\.)?/i, '');
  return s.length > 64 ? `${s.slice(0, 61)}…` : s;
}

/* ------------------------------------------------------------------ *
 * What this browser has opened (per-browser, best effort)
 * ------------------------------------------------------------------ */

const SEEN_KEY = 'omni.mcr.inbox.seen';

function loadSeen() {
  try {
    const v = JSON.parse(localStorage.getItem(SEEN_KEY) || 'null');
    if (v && typeof v.since === 'string' && Array.isArray(v.ids)) return { since: Date.parse(v.since) || Date.now(), ids: new Set(v.ids) };
  } catch { /* storage blocked: nothing is "new" */ }
  // First visit: what is already there is not news.
  const fresh = { since: Date.now(), ids: new Set() };
  saveSeen(fresh);
  return fresh;
}

function saveSeen(s) {
  try {
    localStorage.setItem(SEEN_KEY, JSON.stringify({ since: new Date(s.since).toISOString(), ids: [...s.ids].slice(-500) }));
  } catch { /* ignore */ }
}

const seen = loadSeen();

function isUnread(e) {
  const at = Date.parse(e.at || '');
  return !Number.isNaN(at) && at > seen.since && !seen.ids.has(entryId(e));
}

function markSeen(id) {
  if (seen.ids.has(id)) return;
  seen.ids.add(id);
  saveSeen(seen);
}

function updateUnreadBadge() {
  const badge = document.getElementById('email-count');
  if (!badge) return;
  const n = entries.filter(isUnread).length;
  badge.hidden = n === 0;
  badge.textContent = String(n);
  badge.title = 'Νέα που δεν έχετε ανοίξει ακόμη';
}

/* ------------------------------------------------------------------ *
 * Loading
 * ------------------------------------------------------------------ */

export function initInbox(h) {
  hooks = { ...hooks, ...h };
  const search = document.getElementById('inbox-search');
  let timer = null;
  search.addEventListener('input', () => {
    clearTimeout(timer);
    timer = setTimeout(() => {
      list.q = search.value.trim();
      list.page = 1;
      loadInbox();
    }, 300);
  });
  document.addEventListener('keydown', (ev) => {
    if (ev.key !== 'Escape' || !open || !isActiveTab()) return;
    if (ev.target.closest && ev.target.closest('input, textarea, select')) return;
    if (document.querySelector('.modal:not([hidden])')) return;
    const c = cards.get(open.id);
    close();
    if (c) c.head.focus();
  });
}

let running = null;
let again = false;
/** The entry the next load must show (`openMail`). */
let focus = null;

/** Refresh the list (and the open mail). One request at a time; a call
 *  made meanwhile runs once more after it, and waits for that. */
export function loadInbox() {
  if (running) {
    again = true;
    return running;
  }
  running = (async () => {
    try {
      do {
        again = false;
        await loadOnce();
      } while (again);
    } finally {
      running = null;
    }
  })();
  return running;
}

async function loadOnce() {
  let data;
  try {
    const params = new URLSearchParams({ filter: list.filter, page: String(list.page), per_page: String(list.perPage) });
    if (list.q) params.set('q', list.q);
    if (focus) params.set('focus', focus);
    data = await api(`/api/mails?${params}`);
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }
  windowDays = data.window_days || windowDays;
  list.page = data.page || list.page;
  hooks.onCounts(data.job_counts);
  entries = data.entries || [];
  renderFilters(data.counts || {});
  renderList();
  pager('inbox-pager', list, data.total || 0, loadInbox);
  updateUnreadBadge();
  for (const e of entries) {
    const at = Date.parse(e.at || '');
    if (!Number.isNaN(at) && (newestShown === null || at > newestShown)) newestShown = at;
  }
  if (newestShown === null) newestShown = 0;
  if (open && detail) await refreshOpen();
}

/* ------------------------------------------------------------------ *
 * Filter chips
 * ------------------------------------------------------------------ */

const FILTERS = [
  ['all', 'Όλα', null],
  ['active', 'Σε εξέλιξη', 'var(--info)'],
  ['attention', 'Θέλουν προσοχή', 'var(--warn)'],
  ['done', 'Ολοκληρώθηκαν', 'var(--ok)'],
  ['empty', 'Χωρίς βίντεο', '#3a4762'],
];
let filtersSig = '';

function renderFilters(counts) {
  const sig = JSON.stringify([list.filter, counts]);
  if (sig === filtersSig) return;
  filtersSig = sig;
  render('inbox-filters', FILTERS.map(([key, label, color]) => el('button', {
    class: 'fchip',
    type: 'button',
    'aria-pressed': String(list.filter === key),
    onClick: () => {
      if (list.filter === key) return;
      list.filter = key;
      list.page = 1;
      loadInbox();
    },
  }, color ? el('span', { class: 'pip', style: `background:${color}` }) : null, label,
  el('span', { class: 'n' }, String(counts[key] ?? 0)))));
}

/* ------------------------------------------------------------------ *
 * The list
 * ------------------------------------------------------------------ */

function renderList() {
  const host = document.getElementById('inbox-list');
  if (!entries.length && !open) {
    cards.clear();
    render(host, el('div', { class: 'card center' }, list.q || list.filter !== 'all'
      ? 'Τίποτα δεν ταιριάζει με την αναζήτηση ή το φίλτρο.'
      : `Δεν έχει έρθει κανένα email τις τελευταίες ${windowDays} ημέρες. Τα νέα εμφανίζονται εδώ αυτόματα.`));
    return;
  }
  const wanted = [];
  const ids = new Set(entries.map(entryId));
  // The open mail stays where the operator is reading it, even if it no
  // longer matches the filter (it just finished, say).
  if (open && !ids.has(open.id) && cards.has(open.id)) {
    wanted.push(dayNode('Ανοιχτό'), cards.get(open.id).card);
  }
  let day = null;
  for (const e of entries) {
    const id = entryId(e);
    const label = dayLabel(e.at);
    if (label !== day) {
      wanted.push(dayNode(label));
      day = label;
    }
    let c = cards.get(id);
    if (!c) {
      c = createCard(id);
      cards.set(id, c);
      // News, not an older mail a filter or a page brought into view.
      const at = Date.parse(e.at || '');
      if (newestShown !== null && !Number.isNaN(at) && at > newestShown) {
        c.card.classList.add('arrived');
        // Once: a card moved later must not arrive again.
        setTimeout(() => c.card.classList.remove('arrived'), 2600);
      }
    }
    c.entry = e;
    paintHead(c);
    wanted.push(c.card);
  }
  for (const id of [...cards.keys()]) {
    if (!ids.has(id) && !(open && open.id === id)) cards.delete(id);
  }
  reconcile(host, wanted);
}

function dayNode(label) {
  let n = dayNodes.get(label);
  if (!n) {
    n = el('div', { class: 'ib-day' }, label);
    dayNodes.set(label, n);
  }
  return n;
}

/** Put `wanted` in `host` in that order, moving only what is out of place:
 *  a node that is not moved keeps its focus, scroll and hover. */
function reconcile(host, wanted) {
  let cur = host.firstChild;
  for (const node of wanted) {
    if (node === cur) {
      cur = cur.nextSibling;
      continue;
    }
    host.insertBefore(node, cur);
  }
  while (cur) {
    const next = cur.nextSibling;
    cur.remove();
    cur = next;
  }
}

function createCard(id) {
  const card = el('article', { class: 'mail', 'data-id': id });
  const head = el('button', { class: 'mail-head', type: 'button', 'aria-expanded': 'false' });
  head.addEventListener('click', () => toggle(id));
  head.addEventListener('keydown', moveFocus);
  const inner = el('div', { class: 'mail-x-inner' });
  card.append(head, el('div', { class: 'mail-x' }, inner));
  return { card, head, inner, entry: null, sig: '' };
}

function moveFocus(ev) {
  if (ev.key !== 'ArrowDown' && ev.key !== 'ArrowUp') return;
  ev.preventDefault();
  const heads = [...document.querySelectorAll('#inbox-list .mail-head')];
  const i = heads.indexOf(ev.currentTarget);
  const next = heads[i + (ev.key === 'ArrowDown' ? 1 : -1)];
  if (next) next.focus();
}

function initials(e) {
  if (e.kind === 'manual') return '+';
  const name = (e.from_name || e.from_address || '?').replace(/[<>"]/g, ' ');
  const words = name.split(/[\s.@_-]+/).filter(Boolean);
  return words.slice(0, 2).map((w) => w[0]).join('').toUpperCase() || '?';
}

function paintHead(c) {
  const e = c.entry;
  if (!e) return;
  const id = entryId(e);
  const isOpen = !!open && open.id === id;
  const unread = isUnread(e);
  c.card.dataset.state = e.state;
  c.card.classList.toggle('unread', unread);
  c.card.classList.toggle('open', isOpen);
  c.head.setAttribute('aria-expanded', String(isOpen));
  const sig = JSON.stringify([e.subject, e.from_name, e.from_address, e.journalist, e.how, e.group_code, e.urgent,
    e.state, e.outcome, e.preview, e.attachments, e.added_by_name, e.jobs, unread, e.at]);
  if (sig === c.sig) return;
  c.sig = sig;

  const who = e.kind === 'manual'
    ? `Χειροκίνητη προσθήκη · ${e.added_by_name || 'MCR'}`
    : (e.from_name || e.from_address || 'Άγνωστος αποστολέας');
  let journalist = null;
  if (e.kind === 'manual') {
    journalist = e.journalist && e.journalist !== 'MCR' ? el('span', { class: 'badge info' }, e.journalist) : null;
  } else if (e.state !== 'failed') {
    journalist = !e.journalist || e.journalist === 'MCR'
      ? el('span', { class: 'badge warn', title: 'Δεν βρέθηκε δημοσιογράφος: τα αρχεία παίρνουν το όνομα MCR' }, 'Χωρίς δημοσιογράφο')
      : el('span', { class: 'badge info', title: `Δημοσιογράφος, ${e.how || ''}` }, e.journalist);
  }
  c.head.replaceChildren(
    el('span', { class: `avatar ${e.kind === 'manual' ? 'manual' : ''}`.trim(), 'aria-hidden': 'true' }, initials(e)),
    el('span', { class: 'mail-main' },
      el('span', { class: 'mail-from' },
        el('span', { class: 'who', title: e.from_address || '' }, who),
        journalist,
        e.group_code ? el('span', { class: 'badge', title: hooks.groupName(e.group_code) }, e.group_code) : null,
        e.urgent ? el('span', { class: 'badge bad' }, 'Επείγον') : null,
        e.attachments ? el('span', { class: 'clip', title: 'Συνημμένα' }, icon('paperclip'), String(e.attachments)) : null,
        unread ? el('span', { class: 'new-pill' }, 'Νέο') : null,
      ),
      el('span', { class: 'mail-subject' }, e.subject || '(χωρίς θέμα)'),
      el('span', { class: 'mail-preview' }, e.preview || ''),
    ),
    el('span', { class: 'mail-status' }, segbar(e), el('span', { class: 'mail-sum' }, summary(e))),
    el('span', { class: 'mail-time', title: fmtTime(e.at) }, clock(e.at)),
    icon('chevron-down', 'chev'),
  );
}

/** One segment per video: the mail's progress at a glance. */
function segbar(e) {
  const bar = el('span', { class: 'segbar', 'aria-hidden': 'true' });
  const js = e.jobs || [];
  if (!js.length) {
    bar.append(el('span', { class: e.state === 'failed' ? 'seg bad' : 'seg none' }));
    return bar;
  }
  for (const j of js) {
    const t = tone(j);
    const seg = el('span', { class: `seg ${t}`, title: `${j.index_str}: ${shortStatus(j)}` });
    if (t === 'run') {
      seg.style.setProperty('--p', `${pct(j)}%`);
      seg.append(el('i'));
    }
    bar.append(seg);
  }
  return bar;
}

/* ------------------------------------------------------------------ *
 * Opening and closing
 * ------------------------------------------------------------------ */

function toggle(id) {
  if (open && open.id === id) {
    close();
    return;
  }
  const c = cards.get(id);
  if (!c || !c.entry) return;
  if (open) close();
  open = { id, kind: c.entry.kind, key: c.entry.key };
  detail = null;
  index = null;
  selected = null;
  markSeen(id);
  c.sig = '';
  paintHead(c);
  updateUnreadBadge();
  render(c.inner, el('div', { class: 'md-loading' }, 'Φόρτωση…'));
  loadDetail(c, open);
}

async function loadDetail(c, want) {
  try {
    const d = await api(`/api/mails/view?${new URLSearchParams({ kind: want.kind, key: want.key })}`);
    if (!open || open.id !== want.id) return;
    setDetail(d);
    render(c.inner, detailNode(d));
    setTimeout(() => {
      if (open && open.id === want.id) c.card.scrollIntoView({ block: 'nearest', behavior: smooth() });
    }, 330);
  } catch (e) {
    if (open && open.id === want.id) render(c.inner, el('div', { class: 'md-loading note bad' }, e.message));
  }
}

function close() {
  const c = open && cards.get(open.id);
  open = null;
  detail = null;
  index = null;
  selected = null;
  overrideOpen.clear();
  if (!c) return;
  c.card.classList.remove('open');
  c.head.setAttribute('aria-expanded', 'false');
  setTimeout(() => {
    if (!c.card.classList.contains('open')) c.inner.replaceChildren();
  }, 320);
  if (!entries.some((e) => entryId(e) === c.card.dataset.id)) renderList();
}

const openRoot = () => (open ? cards.get(open.id)?.inner : null);

/** After an action: refresh the desk (list, counts and the open mail). */
const afterAction = () => hooks.refresh();

/* ------------------------------------------------------------------ *
 * The open mail
 * ------------------------------------------------------------------ */

function setDetail(d) {
  detail = d;
  const push = (m, k, v) => { if (!m.has(k)) m.set(k, []); m.get(k).push(v); };
  const linkById = new Map(d.links.map((l) => [l.id, l]));
  const jobById = new Map(d.jobs.map((j) => [j.id, j]));
  const chipsOfJob = new Map();
  const jobsOfChip = new Map();
  for (const l of d.links) {
    jobsOfChip.set(l.id, l.jobs);
    for (const id of l.jobs) push(chipsOfJob, id, l.id);
  }
  for (const a of d.attachments) {
    jobsOfChip.set(a.id, a.jobs);
    for (const id of a.jobs) push(chipsOfJob, id, a.id);
  }
  index = { linkById, jobById, chipsOfJob, jobsOfChip };
}

function detailNode(d) {
  const root = el('div', { class: 'md' });
  root.append(metaNode(d));
  if (d.notes && d.notes.length) {
    root.append(el('div', { class: 'md-notes' }, d.notes.map((n) => el('div', { class: `note-line ${n.level === 'warn' ? 'warn' : ''}`.trim() },
      icon(n.level === 'warn' ? 'alert-triangle' : 'info'), el('span', {}, n.text)))));
  }
  if (d.outcome === 'FAILED') {
    root.append(failedNode());
    return root;
  }
  const rail = el('div', { class: 'rail-sticky' });
  paintRail(rail);
  root.append(el('div', { class: 'reader' },
    el('div', { class: 'letter-pane' },
      el('div', { class: 'pane-title' }, el('span', {}, d.kind === 'manual' ? 'Ο σύνδεσμος' : 'Το κείμενο του email'), legend()),
      letterNode(d)),
    el('div', { class: 'rail' }, rail)));
  return root;
}

function metaNode(d) {
  const entry = open && cards.get(open.id)?.entry;
  const rows = [];
  const add = (label, ...value) => rows.push(el('dt', {}, label), el('dd', {}, ...value));
  if (d.kind === 'manual') {
    add('Από', el('b', {}, entry?.added_by_name || 'MCR'), ' · μέσα από τη φόρμα «Προσθήκη συνδέσμου»');
    add('Προστέθηκε', fmtTime(d.received_at));
  } else {
    add('Από', el('b', {}, d.from_name || d.from_address || '—'), d.from_name && d.from_address ? ` <${d.from_address}>` : '');
    if (d.to && d.to.length) add('Προς', d.to.join(', '), d.cc && d.cc.length ? `  ·  Κοιν.: ${d.cc.join(', ')}` : '');
    add('Ελήφθη', fmtTime(d.received_at || d.processed_at));
  }
  if (d.journalist && d.outcome !== 'FAILED') {
    add('Δημοσιογράφος', el('b', {}, d.journalist === 'MCR' ? 'MCR (κανένας)' : d.journalist), d.how ? el('span', { class: 'how' }, `  ·  ${d.how}`) : '');
  }
  if (d.group_code) add('Ομάδα', `${d.group_code} — ${hooks.groupName(d.group_code)}`);
  if (d.attachments && d.attachments.length) {
    add('Συνημμένα', d.attachments.map((a) => chipNode(a.id, `${a.name} · ${fmtSize(a.size)}`)));
  }
  return el('dl', { class: 'md-meta' }, rows);
}

function fmtSize(bytes) {
  const n = Number(bytes) || 0;
  if (n >= 1024 * 1024 * 1024) return `${(n / 1024 / 1024 / 1024).toFixed(1).replace('.', ',')} GB`;
  if (n >= 1024 * 1024) return `${Math.round(n / 1024 / 1024)} MB`;
  return `${Math.max(1, Math.round(n / 1024))} KB`;
}

function failedNode() {
  return el('div', { class: 'md-failed' },
    el('p', {}, 'Το σύστημα δεν μπόρεσε να διαβάσει αυτό το email από το γραμματοκιβώτιο, ούτε μετά από επανειλημμένες προσπάθειες, και δεν μπήκε κανένα βίντεο στην ουρά. Συνήθως φταίει μια προσωρινή διακοπή.'),
    el('div', { class: 'row' },
      el('button', { class: 'btn btn-primary', type: 'button', onClick: (ev) => reprocess(ev.currentTarget) }, icon('refresh'), 'Να διαβαστεί ξανά'),
      el('span', { class: 'note' }, 'Διαβάζεται στον επόμενο έλεγχο του γραμματοκιβωτίου. Ό,τι έχει ήδη μπει στην ουρά δεν μπαίνει δεύτερη φορά.')));
}

async function reprocess(button) {
  if (!open) return;
  button.disabled = true;
  try {
    await api('/api/mails/reprocess', { method: 'POST', body: { key: open.key } });
    toast('Το email θα διαβαστεί ξανά σε λίγα δευτερόλεπτα.', 'ok');
  } catch (e) {
    toast(e.message, 'bad');
    button.disabled = false;
  }
}

function legend() {
  const item = (cls, text) => el('span', {}, el('i', { class: cls }), text);
  return el('span', { class: 'legend', 'aria-hidden': 'true' },
    item('ok', 'παραδόθηκε'), item('run', 'σε εξέλιξη'), item('wait', 'αναμονή'),
    item('warn', 'έλεγχος'), item('bad', 'απέτυχε'), item('skip', 'δεν κατέβηκε'));
}

/* The text, block by block: what the parser read; a forward's headers,
 * dimmed; quoted history and the signature, folded. */
function letterNode(d) {
  const box = el('div', { class: 'letter' });
  if (!d.text) {
    box.append(el('p', { class: 'note' },
      `Το κείμενο αυτού του email δεν κρατήθηκε: το κείμενο κρατιέται ${windowDays} ημέρες, και όσα email ήρθαν πριν από την ενημέρωση του συστήματος δεν το έχουν. Τα βίντεο του είναι δεξιά.`));
    return box;
  }
  for (const block of d.text) {
    const body = el('div', { class: 'blk' });
    block.lines.forEach((line, i) => {
      if (i) body.append('\n');
      for (const seg of line) body.append(seg.l ? chipNode(seg.l, seg.t) : seg.t);
    });
    if (block.role === 'read') {
      box.append(body);
    } else if (block.role === 'forwarded') {
      body.classList.add('fwd');
      body.title = 'Κεφαλίδες του προωθημένου μηνύματος';
      box.append(body);
    } else {
      box.append(folded(block.role === 'signature' ? 'Υπογραφή' : 'Παλιότερο μήνυμα', body));
    }
  }
  return box;
}

function folded(label, body) {
  body.hidden = true;
  const text = el('span', {}, `${label} · δεν το διαβάζει το σύστημα · εμφάνιση`);
  const button = el('button', { type: 'button', class: 'fold', 'aria-expanded': 'false' }, icon('eye'), text);
  const wrap = el('div', { class: 'skipped', 'data-label': label }, button, body);
  button.addEventListener('click', () => setFold(wrap, body.hidden));
  return wrap;
}

function setFold(wrap, show) {
  const button = wrap.querySelector('.fold');
  const body = wrap.querySelector('.blk');
  body.hidden = !show;
  button.setAttribute('aria-expanded', String(show));
  button.lastChild.textContent = `${wrap.dataset.label} · δεν το διαβάζει το σύστημα · ${show ? 'απόκρυψη' : 'εμφάνιση'}`;
}

/* A link (or an attachment) in the text, tagged with the files it became. */
function chipNode(id, text) {
  const chip = el('span', { class: 'lnk', role: 'button', tabindex: '0', 'data-chip': id });
  chip.dataset.text = text;
  paintChip(chip);
  chip.addEventListener('mouseenter', () => hover({ chip: id }, true));
  chip.addEventListener('mouseleave', () => hover({ chip: id }, false));
  chip.addEventListener('focus', () => hover({ chip: id }, true));
  chip.addEventListener('blur', () => hover({ chip: id }, false));
  chip.addEventListener('click', () => pickChip(id));
  chip.addEventListener('keydown', (ev) => {
    if (ev.key === 'Enter' || ev.key === ' ') {
      ev.preventDefault();
      pickChip(id);
    }
  });
  return chip;
}

function paintChip(chip) {
  if (!index) return;
  const id = chip.dataset.chip;
  const jobs = (index.jobsOfChip.get(id) || []).map((x) => index.jobById.get(x)).filter(Boolean);
  const link = index.linkById.get(id);
  chip.classList.toggle('skip', jobs.length === 0);
  chip.classList.toggle('hl', pinnedChips().includes(id));
  chip.title = jobs.length
    ? jobs.map((j) => `${j.index_str}: ${shortStatus(j)}`).join(' · ')
    : (link && link.skip ? link.skip.reason : 'Δεν κατέβηκε');
  const tags = jobs.length
    ? jobs.map((j) => el('span', { class: `tag ${tone(j)}` }, j.index_str))
    : [el('span', { class: 'tag' }, '—')];
  chip.replaceChildren(...tags, chip.dataset.text);
}

/* ------------------------------------------------------------------ *
 * The videos beside the text
 * ------------------------------------------------------------------ */

function paintRail(rail) {
  const d = detail;
  const nodes = [el('div', { class: 'pane-title' }, el('span', {},
    `${d.kind === 'manual' ? 'Βίντεο από αυτόν τον σύνδεσμο' : 'Βίντεο από αυτό το email'} · ${d.jobs.length}`))];
  if (!d.jobs.length) nodes.push(el('p', { class: 'empty-mail' }, 'Δεν μπήκε κανένα βίντεο στην ουρά.'));
  for (const j of d.jobs) {
    nodes.push(jobCard(j));
    for (const o of d.offers.filter((x) => x.job_id === j.id)) nodes.push(offerCard(j, o));
  }
  const skipped = [];
  const urls = new Set();
  for (const l of d.links) {
    if (l.jobs.length || !l.skip || l.role === 'signature' || urls.has(l.url)) continue;
    urls.add(l.url);
    skipped.push(l);
  }
  if (skipped.length) {
    nodes.push(el('div', { class: 'rail-sep' }, `Σύνδεσμοι που δεν κατέβηκαν · ${skipped.length}`));
    for (const l of skipped) nodes.push(skipCard(l));
  }
  rail.replaceChildren(...nodes);
  rail.dataset.shape = railShape();
}

/** What, if it changes, means the rail is rebuilt rather than patched. */
function railShape() {
  return JSON.stringify([
    detail.jobs.map((j) => [j.id, j.place]),
    detail.offers.map((o) => o.url),
    detail.links.filter((l) => !l.jobs.length && l.skip).map((l) => l.url),
    detail.next_index,
  ]);
}

function jobCard(j) {
  const card = el('div', { class: 'jcard', tabindex: '0', role: 'button', 'data-job': String(j.id) });
  const id = j.id;
  card.addEventListener('mouseenter', () => hover({ job: id }, true));
  card.addEventListener('mouseleave', () => hover({ job: id }, false));
  card.addEventListener('click', (ev) => {
    if (!ev.target.closest('button, input, a, summary, details')) select({ job: id }, 'rail');
  });
  card.addEventListener('keydown', (ev) => {
    if (ev.target === card && (ev.key === 'Enter' || ev.key === ' ')) {
      ev.preventDefault();
      select({ job: id }, 'rail');
    }
  });
  paintJob(card, j, true);
  return card;
}

const jobSig = (j) => JSON.stringify([j.status, j.stage, pct(j), j.speed, j.eta, j.error_code, j.hint, j.file_path,
  j.completed_at, j.slug, j.index_str, j.attempts, j.url, j.place, j.max_videos]);

function paintJob(card, j, force) {
  const sel = !!selected && selected.job === j.id;
  const sig = `${jobSig(j)}|${sel}|${overrideOpen.has(j.id)}|${sel ? (timelines.get(j.id) || []).length : ''}`;
  if (!force && card.dataset.sig === sig) return;
  // Never rebuild a card under a field being typed in.
  if (!force && card.contains(document.activeElement) && document.activeElement.tagName === 'INPUT') return;
  card.dataset.sig = sig;
  card.classList.toggle('child', !!j.place.parent);
  card.classList.toggle('sel', sel);
  card.setAttribute('aria-pressed', String(sel));

  const t = tone(j);
  const done = j.status === 'COMPLETED' || j.status === 'COMPLETED_MANUAL';
  const parts = [
    el('div', { class: 'jc-top' },
      el('span', { class: `tag ${t}` }, j.index_str),
      el('span', { class: 'jc-name', title: done ? deliveredName(j) : fileName(j) }, done ? deliveredName(j) : fileName(j)),
      railBadge(j)),
    subLine(j),
  ];
  if (j.status === 'RUNNING') {
    parts.push(el('div', { class: 'progress jc-progress' }, el('span', { style: `width:${Math.max(2, pct(j))}%` })));
  }
  const origin = originLine(j);
  if (origin) parts.push(origin);
  const limit = limitNote(j);
  if (limit) parts.push(el('div', { class: 'jc-found' }, limit));
  if (needsAttention(j) || sel) parts.push(moreNode(j, sel));
  card.replaceChildren(...parts);
}

function subLine(j) {
  let left = [];
  let right = '';
  switch (j.status) {
    case 'RUNNING': {
      const step = STAGE_ORDER.indexOf(j.stage);
      left = [step >= 0 ? `Βήμα ${step + 1} από ${STAGE_ORDER.length} · ` : '', el('span', { class: 'st' }, STAGE_TEXT[j.stage] || 'Σε επεξεργασία')];
      if (j.stage === 'DOWNLOAD' && j.speed && !/^0(\.0+)? /.test(j.speed)) left.push(` · ${j.speed}`);
      right = j.stage === 'DOWNLOAD' && j.eta && j.eta !== '--:--' ? `${pct(j)}% · ${j.eta}` : `${pct(j)}%`;
      break;
    }
    case 'PENDING':
      left = [j.error_code && j.attempts
        ? `Νέα προσπάθεια σε λίγο (έγιναν ${j.attempts} από ${j.max_attempts})`
        : 'Σε αναμονή'];
      break;
    case 'COMPLETED':
    case 'COMPLETED_MANUAL':
      left = [`Παραδόθηκε ${fmtTime(j.completed_at || j.delivered_at)}`];
      right = fmtDuration(j.duration_secs);
      break;
    case 'MANUAL_DOWNLOAD':
      left = [el('span', { class: 'st' }, 'Χρειάζεται χειροκίνητη λήψη')];
      break;
    case 'CANCELLED':
      left = ['Ακυρώθηκε'];
      break;
    default:
      left = [el('span', { class: 'st' }, 'Δεν κατέβηκε αυτόματα')];
  }
  return el('div', { class: 'jc-sub' }, el('span', {}, left), el('span', {}, right));
}

/** Where the video came from, when that is not just «its link». */
function originLine(j) {
  if (j.place.parent) {
    return el('div', { class: 'jc-found', title: j.url }, `↳ βρέθηκε μέσα στο ίδιο άρθρο · ${shortUrl(j.url)}`);
  }
  if (j.place.shared) {
    return el('div', { class: 'jc-found' }, `Ο σύνδεσμος ήταν ήδη στην ουρά: είναι η εργασία #${j.id}, από άλλο email ή χειροκίνητα.`);
  }
  if (String(j.url).startsWith('attachment://')) return el('div', { class: 'jc-found' }, 'Από το συνημμένο του email');
  const link = j.place.link ? index.linkById.get(j.place.link) : null;
  if (!j.place.link) return el('div', { class: 'jc-found' }, 'Ο σύνδεσμός του δεν φαίνεται στο κείμενο.');
  if (link && link.url !== j.url) {
    return el('div', { class: 'jc-found', title: j.url }, `Σύνδεσμος που έδωσε το MCR: ${shortUrl(j.url)}`);
  }
  return null;
}

function moreNode(j, sel) {
  const box = el('div', { class: 'jc-more' });
  const href = safeHref(j.url);
  const openLink = href ? el('a', { class: 'btn', href, target: '_blank', rel: 'noopener noreferrer', title: j.url }, icon('external-link'), 'Άνοιγμα') : null;
  if (needsAttention(j)) {
    const locker = j.status === 'MANUAL_DOWNLOAD';
    box.append(el('div', { class: `jc-hint ${j.status === 'FAILED' ? 'bad' : ''}`.trim() },
      el('strong', {}, j.hint || (locker
        ? 'Σύνδεσμος μεταφοράς αρχείων: κατεβάστε το αρχείο από τον σύνδεσμο και ρίξτε το στο Dalet.'
        : 'Το βίντεο δεν παραδόθηκε αυτόματα.')),
      j.error_code ? el('span', { class: 'note mono' }, `Κωδικός: ${j.error_code}`) : null));
    // What fixes it is always there; opening and removing it, once the
    // card is selected, so a mail with many failures stays readable.
    box.append(el('div', { class: 'jc-actions' },
      locker ? null : el('button', { class: 'btn btn-primary', type: 'button', onClick: () => retryJob(j, afterAction) }, 'Δοκιμή ξανά'),
      locker ? null : el('button', {
        class: 'btn btn-warn',
        type: 'button',
        'aria-expanded': String(overrideOpen.has(j.id)),
        onClick: () => {
          if (overrideOpen.has(j.id)) overrideOpen.delete(j.id); else overrideOpen.add(j.id);
          repaintJob(j.id, true);
          if (overrideOpen.has(j.id)) openRoot()?.querySelector(`.jcard[data-job="${j.id}"] input`)?.focus();
        },
      }, 'Άλλος σύνδεσμος…'),
      sel || locker ? openLink : null,
      sel && canRename(j) ? el('button', { class: 'btn', type: 'button', onClick: () => renameJob(j, afterAction) }, 'Μετονομασία…') : null,
      sel || locker ? el('button', { class: 'btn btn-danger', type: 'button', onClick: () => discardJob(j, afterAction) }, 'Αφαίρεση') : null));
    if (overrideOpen.has(j.id)) {
      const input = el('input', { type: 'url', class: 'mono', placeholder: 'https://… ο σύνδεσμος του ίδιου του βίντεο', 'aria-label': 'Άλλος σύνδεσμος για το ίδιο βίντεο' });
      const submit = async () => {
        if (await overrideJob(j, input.value, afterAction)) overrideOpen.delete(j.id);
      };
      input.addEventListener('keydown', (ev) => { if (ev.key === 'Enter') submit(); });
      box.append(el('div', { class: 'jc-override' }, el('div', { class: 'grow' }, input),
        el('button', { class: 'btn btn-warn', type: 'button', onClick: submit }, 'Χρήση')));
    }
  } else if (sel) {
    const done = j.status === 'COMPLETED' || j.status === 'COMPLETED_MANUAL';
    box.append(el('div', { class: 'jc-actions' },
      openLink,
      canRename(j) ? el('button', { class: 'btn', type: 'button', onClick: () => renameJob(j, afterAction) }, 'Μετονομασία…') : null,
      done
        ? el('button', { class: 'btn', type: 'button', onClick: () => redownloadJob(j, afterAction) }, 'Νέα λήψη')
        : el('button', { class: 'btn btn-danger', type: 'button', onClick: () => discardJob(j, afterAction) }, 'Αφαίρεση')));
  }
  if (sel) box.append(historyNode(j.id));
  return box;
}

function offerCard(j, o) {
  const href = safeHref(o.url);
  return el('div', { class: 'skipcard offer' },
    el('div', { class: 'why' }, `Προτείνεται: κι άλλο βίντεο στο ίδιο άρθρο · θα γίνει ${o.index_str}`),
    el('div', { class: 'u', title: o.url }, shortUrl(o.url)),
    el('div', { class: 'row tight' },
      href ? el('a', { class: 'btn', href, target: '_blank', rel: 'noopener noreferrer' }, icon('external-link'), 'Άνοιγμα') : null,
      el('button', { class: 'btn btn-warn', type: 'button', onClick: () => queueOffer(j.id, o, afterAction) }, icon('plus'), `Λήψη ως ${o.index_str}`)));
}

function skipCard(l) {
  const href = safeHref(l.url);
  const canQueue = l.skip.can_queue && detail.kind === 'mail';
  const button = canQueue
    ? el('button', { class: 'btn', type: 'button', onClick: (ev) => queueLink(l, ev.currentTarget) }, icon('plus'), `Λήψη και αυτού ως ${detail.next_index}`)
    : null;
  const card = el('div', { class: 'skipcard', 'data-url': l.url },
    el('div', { class: 'u', title: l.url }, shortUrl(l.url)),
    el('div', { class: 'why' }, l.skip.reason),
    (href || button) ? el('div', { class: 'row tight' },
      href ? el('a', { class: 'btn', href, target: '_blank', rel: 'noopener noreferrer' }, icon('external-link'), 'Άνοιγμα') : null,
      button) : null);
  card.addEventListener('mouseenter', () => hover({ url: l.url }, true));
  card.addEventListener('mouseleave', () => hover({ url: l.url }, false));
  return card;
}

async function queueLink(l, button) {
  if (!open) return;
  button.disabled = true;
  try {
    const r = await api('/api/mails/queue-link', { method: 'POST', body: { key: open.key, url: l.url } });
    toast(r.duplicate
      ? `Ο σύνδεσμος ήταν ήδη στην ουρά (εργασία #${r.job_id}).`
      : `Προστέθηκε ως ${r.slug}.mxf (εργασία #${r.job_id}).`, 'ok');
    selected = { job: r.job_id };
    afterAction();
  } catch (e) {
    toast(e.message, 'bad');
    button.disabled = false;
  }
}

/* ------------------------------------------------------------------ *
 * A video's history, in words, with the raw lines underneath
 * ------------------------------------------------------------------ */

function eventText(ev) {
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

function eventClass(ev) {
  if (/^Finished as COMPLETED/.test(ev.message || '')) return 'ok';
  if (ev.level === 'ERROR') return 'bad';
  if (ev.level === 'WARN') return 'warn';
  return /^Stage /.test(ev.message || '') ? 'run' : '';
}

function historyNode(jobId) {
  const events = timelines.get(jobId);
  if (!events) return el('p', { class: 'note jc-history' }, 'Φόρτωση ιστορικού…');
  const told = events.map((ev) => [ev, eventText(ev)]).filter(([, t]) => t);
  const details = el('details', { class: 'tech' },
    el('summary', {}, `Τεχνικές λεπτομέρειες (${events.length})`),
    el('ul', { class: 'tech-log mono' }, events.map((ev) => el('li', {}, `${clock(ev.at)}  ${ev.level}  ${ev.stage || ''}  ${ev.message}`))));
  if (techOpen.has(jobId)) details.open = true;
  details.addEventListener('toggle', () => { if (details.open) techOpen.add(jobId); else techOpen.delete(jobId); });
  return el('div', { class: 'jc-history' },
    told.length ? el('ul', { class: 'timeline' }, told.map(([ev, t]) => el('li', { class: eventClass(ev) }, el('time', {}, clock(ev.at)), t))) : null,
    details);
}

async function loadTimeline(jobId) {
  try {
    const r = await api(`/api/jobs/${jobId}`);
    timelines.set(jobId, r.events || []);
    if (selected && selected.job === jobId) repaintJob(jobId, true);
  } catch { /* the card still shows its state */ }
}

/* ------------------------------------------------------------------ *
 * Highlight and selection
 * ------------------------------------------------------------------ */

function chipsFor(target) {
  if (!target || !index) return [];
  if (target.job != null) return index.chipsOfJob.get(target.job) || [];
  if (target.url) return detail.links.filter((l) => l.url === target.url).map((l) => l.id);
  if (target.chip) return [target.chip];
  return [];
}

function jobsFor(target) {
  if (!target || !index) return [];
  if (target.job != null) return [target.job];
  if (target.chip) return index.jobsOfChip.get(target.chip) || [];
  return [];
}

const pinnedChips = () => chipsFor(selected);

function hover(target, on) {
  const root = openRoot();
  if (!root || !index) return;
  const pinned = pinnedChips();
  for (const id of chipsFor(target)) {
    for (const n of root.querySelectorAll(`[data-chip="${id}"]`)) n.classList.toggle('hl', on || pinned.includes(id));
  }
  for (const id of jobsFor(target)) {
    for (const n of root.querySelectorAll(`.jcard[data-job="${id}"]`)) n.classList.toggle('hl', on);
  }
  const url = target.url || (target.chip && index.linkById.get(target.chip)?.url);
  if (url) {
    for (const n of root.querySelectorAll('.skipcard[data-url]')) {
      if (n.dataset.url === url) n.classList.toggle('hl', on || (selected && selected.url === url));
    }
  }
}

function select(target, from) {
  const same = selected && ((target.job != null && selected.job === target.job) || (target.url && selected.url === target.url));
  selected = same && from === 'rail' ? null : target;
  if (selected && selected.job != null && !timelines.has(selected.job)) loadTimeline(selected.job);
  repaintOpen(true);
  if (!selected) return;
  const root = openRoot();
  const chipId = chipsFor(selected)[0];
  const chip = chipId ? root.querySelector(`[data-chip="${chipId}"]`) : null;
  const fold = chip && chip.closest('.skipped');
  if (fold && fold.querySelector('.blk').hidden) setFold(fold, true);
  if (from === 'rail') {
    if (chip) {
      chip.scrollIntoView({ block: 'center', behavior: smooth() });
      pulse(chip);
    }
  } else {
    const card = selected.job != null
      ? root.querySelector(`.jcard[data-job="${selected.job}"]`)
      : [...root.querySelectorAll('.skipcard[data-url]')].find((n) => n.dataset.url === selected.url);
    if (card) {
      card.scrollIntoView({ block: 'nearest', behavior: smooth() });
      pulse(card);
    }
    if (chip) pulse(chip);
  }
}

function pickChip(id) {
  const jobs = index ? index.jobsOfChip.get(id) || [] : [];
  if (jobs.length) {
    select({ job: jobs[0] }, 'text');
    return;
  }
  const link = index && index.linkById.get(id);
  if (link) select({ url: link.url, chip: id }, 'text');
}

function pulse(node) {
  node.classList.remove('pulse');
  void node.offsetWidth;
  node.classList.add('pulse');
  node.addEventListener('animationend', () => node.classList.remove('pulse'), { once: true });
}

function repaintJob(jobId, force) {
  const root = openRoot();
  const j = index && index.jobById.get(jobId);
  const card = root && root.querySelector(`.jcard[data-job="${jobId}"]`);
  if (j && card) paintJob(card, j, force);
}

function repaintOpen(force) {
  const root = openRoot();
  if (!root || !detail) return;
  for (const chip of root.querySelectorAll('[data-chip]')) paintChip(chip);
  for (const j of detail.jobs) repaintJob(j.id, force);
  for (const n of root.querySelectorAll('.skipcard[data-url]')) {
    n.classList.toggle('hl', !!selected && selected.url === n.dataset.url);
  }
}

/* The open mail, refreshed with the list: chips and cards are patched in
 * place; the rail is rebuilt only when a video was added or removed. */
async function refreshOpen() {
  const want = open;
  let part;
  try {
    part = await api(`/api/mails/view?${new URLSearchParams({ kind: want.kind, key: want.key, parts: 'jobs' })}`);
  } catch {
    return;
  }
  if (!open || open.id !== want.id || !detail) return;
  const before = new Map(detail.jobs.map((j) => [j.id, jobSig(j)]));
  Object.assign(detail, part);
  setDetail(detail);
  const root = openRoot();
  if (!root) return;
  const rail = root.querySelector('.rail-sticky');
  if (rail && rail.dataset.shape !== railShape()) {
    const typing = rail.contains(document.activeElement) && document.activeElement.tagName === 'INPUT';
    if (!typing) paintRail(rail);
  }
  if (selected && selected.job != null && before.get(selected.job) !== jobSig(index.jobById.get(selected.job) || {})) {
    loadTimeline(selected.job);
  }
  repaintOpen(false);
}

/* ------------------------------------------------------------------ *
 * From elsewhere on the desk (plan P7.11)
 * ------------------------------------------------------------------ */

/** Open the entry `kind:key` from elsewhere on the desk: on the page that
 *  holds it, whatever filter or search was on. */
export async function openMail(kind, key) {
  const id = `${kind}:${key}`;
  if (!cards.has(id)) {
    list.filter = 'all';
    list.q = '';
    const search = document.getElementById('inbox-search');
    if (search) search.value = '';
    focus = id;
    try {
      await loadInbox();
    } finally {
      focus = null;
    }
  }
  const c = cards.get(id);
  if (!c) {
    toast(`Το email δεν είναι πια στη λίστα: η λίστα κρατά τις τελευταίες ${windowDays} ημέρες.`, 'bad');
    return;
  }
  if (!open || open.id !== id) toggle(id);
  c.card.scrollIntoView({ block: 'start', behavior: smooth() });
}

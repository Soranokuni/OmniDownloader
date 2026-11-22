/* Admin panel, «Επισκόπηση» (P13.5): health, what needs the administrator,
 * the autonomy report and the repair-shadow card. Loaded when the section
 * is first opened.
 */

import { api, el, render, toast, fmtTime } from '/static/app.js?v=6';

// The autonomy report (P11.0): the soak test's figure and why people were needed.
const TOUCH_LABEL = {
  retry: 'Retry', override: 'Retry with another link', rename: 'Rename', redownload: 'Download again',
  manual_done: 'Put into Dalet by hand', approve: 'Approved a held video', reject: 'Rejected a held video',
  next: 'Next choice', wrong_video: 'Wrong video', cancel: 'Cancel', now: 'Run now', discard: 'Discard',
  queued: 'Queued by MCR from an offer or a skipped link',
};

const rate1 = (r) => (r === null || r === undefined ? '—' : `${(r * 100).toFixed(1)} %`);

function autonomyTable(title, head, rows) {
  return el('div', { class: 'stack' },
    el('h3', { class: 'card-title', style: 'font-size:0.95rem' }, title),
    el('div', { class: 'card flush' }, el('table', {},
      el('thead', {}, el('tr', {}, head.map((h, i) => el('th', i > 0 ? { class: 'right' } : {}, h)))),
      el('tbody', {}, rows.length > 0
        ? rows
        : el('tr', {}, el('td', { colspan: String(head.length), class: 'empty' }, 'None.'))))));
}

function splitLine(label, t) {
  const aired = t.delivered + t.delivered_manual;
  return `${label}: ${t.delivered_untouched} of ${aired} untouched (${t.ended} finished)`;
}

async function loadAutonomy() {
  const days = document.getElementById('autonomy-days').value;
  let data;
  try {
    data = await api(`/api/admin/autonomy-report?days=${encodeURIComponent(days)}`);
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }
  const t = data.totals;
  const waiting = el('p', {}, el('strong', {}, `Waiting for a person now: ${data.waiting_now}`));
  if (t.ended === 0) {
    render('autonomy-figures', el('p', { class: 'note' }, 'No finished jobs in this period yet.'), waiting);
    render('autonomy-tables');
    return;
  }
  const aired = t.delivered + t.delivered_manual;
  render('autonomy-figures',
    el('p', { style: 'font-size:1.6rem;margin:0' }, el('strong', {},
      `Delivered untouched: ${rate1(data.untouched_rate)} (${t.delivered_untouched} of ${aired} videos that reached air)`)),
    el('p', { class: 'note' },
      `Strict: ${rate1(data.strict_rate)} of all ${t.ended} finished jobs, counting ${t.cancelled} cancelled and ${t.discarded} discarded. ` +
      `${t.delivered_manual} put into Dalet by hand.`),
    el('p', { class: 'note' }, `${splitLine('From mail', data.from_mail)}. ${splitLine('Added on the desk or panel', data.manual)}.`),
    waiting);
  render('autonomy-tables',
    autonomyTable('Why a person was needed', ['First error code', 'Jobs'],
      (data.by_first_error || []).map((c) => el('tr', {},
        el('td', { class: 'mono' }, c.key), el('td', { class: 'right' }, String(c.count))))),
    autonomyTable('What the operator did', ['Action', 'Jobs'],
      (data.by_touch_reason || []).map((c) => el('tr', {},
        el('td', { title: c.key }, TOUCH_LABEL[c.key] || c.key), el('td', { class: 'right' }, String(c.count))))),
    autonomyTable('Sites', ['Site', 'Untouched', 'Total'],
      (data.by_site || []).map((s) => el('tr', {},
        el('td', { class: 'mono' }, s.site),
        el('td', { class: 'right' }, String(s.delivered_untouched)),
        el('td', { class: 'right' }, String(s.total))))));
}

document.getElementById('autonomy-refresh').addEventListener('click', loadAutonomy);
document.getElementById('autonomy-days').addEventListener('change', loadAutonomy);

/* ------------------------------------------------------------------ *
 * Health and what needs the administrator
 * ------------------------------------------------------------------ */

const STATE_CLASS = { ok: 'ok', degraded: 'warn', down: 'bad' };
const STATE_WORD = { ok: 'green', degraded: 'amber', down: 'red' };

// Where each health line is fixed, for the «needs the administrator» list.
const FIX_IN = {
  mail: 'settings', llm: 'settings', watchfolder: 'settings', archive: 'settings', config: 'settings',
  accounts: 'people',
  sites: 'sites', cookies: 'sites', selfcheck: 'sites', browser: 'sites',
  tools: 'maintenance', pinned_tools: 'maintenance', encoder: 'maintenance', deno: 'maintenance',
  disk: 'maintenance', queue: 'maintenance', workers: 'maintenance', lockdown: 'maintenance',
};
const SECTION_NAME = {
  settings: 'Ρυθμίσεις', people: 'Άνθρωποι', sites: 'Ιστότοποι', maintenance: 'Συντήρηση', overview: 'Επισκόπηση',
};

async function loadHealth() {
  let status;
  let settings;
  try {
    [status, settings] = await Promise.all([api('/api/system/status'), api('/api/admin/settings')]);
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }
  const checks = Object.entries(status.checks || {});
  render('health-list', checks.length === 0
    ? el('p', { class: 'note ok' }, 'Every check is green.')
    : checks.map(([name, c]) => el('div', { class: 'spec-row' },
      el('span', { class: 'mono' }, name),
      el('span', {}, el('span', { class: `badge ${STATE_CLASS[c.state] || ''}`, style: 'margin-right:8px' }, STATE_WORD[c.state] || c.state),
        c.detail || ''))));

  const needs = [];
  if (settings && settings.setup_completed === false) {
    needs.push(['wizard', 'Setup is not finished: the wizard ends with a test download that reaches the watchfolder (or an explicit skip).']);
  }
  if (settings && settings.watchfolder_pending) {
    needs.push(['settings', `A new watchfolder waits for the videos being made now: ${settings.watchfolder_pending.path}.`]);
  }
  for (const [name, c] of checks) {
    if (c.state !== 'ok') needs.push([FIX_IN[name] || 'overview', `${name}: ${c.detail || STATE_WORD[c.state]}`]);
  }
  render('needs-list', needs.length === 0
    ? el('p', { class: 'note ok' }, 'Nothing needs the administrator now.')
    : needs.map(([section, text]) => el('div', { class: 'row tight', style: 'justify-content:space-between' },
      el('span', {}, text),
      section === 'wizard'
        // Back into the wizard (P13.5b): the page shows it again while setup is unfinished.
        ? el('button', { class: 'btn btn-primary', type: 'button', onclick: () => { sessionStorage.removeItem('omni.setup.leave'); location.reload(); } }, 'Continue the setup wizard')
        : el('button', { class: 'btn', type: 'button', onclick: () => { location.hash = section; } }, SECTION_NAME[section] || section))));
}

/* ------------------------------------------------------------------ *
 * Repair in shadow (P11.9): what the repair ladder would have done.
 * Lane auto serves the report; until its shape is final the card shows
 * its figures as they come, and says so when the report is not there.
 * ------------------------------------------------------------------ */

const SHADOW_URL = '/api/admin/repair/shadow';

function shadowRows(value, prefix = '') {
  if (value === null || typeof value !== 'object') return [[prefix, String(value)]];
  return Object.entries(value).flatMap(([k, v]) => (v !== null && typeof v === 'object' && !Array.isArray(v)
    ? shadowRows(v, `${prefix}${k}.`)
    : [[`${prefix}${k}`, Array.isArray(v) ? `${v.length} item(s)` : String(v)]]));
}

async function loadShadow() {
  try {
    const report = await api(SHADOW_URL);
    render('shadow-body', shadowRows(report).slice(0, 40).map(([k, v]) => el('div', { class: 'spec-row' },
      el('span', { class: 'mono' }, k), el('span', {}, v))));
  } catch (e) {
    render('shadow-body', el('p', { class: 'note' }, e.status === 404
      ? 'This version has no shadow report yet. The Repair report on «Ιστότοποι» counts shadow runs per site.'
      : e.message));
  }
}

/** The section opened: every card once; Refresh buttons reload. */
export function init() {
  loadHealth();
  loadAutonomy();
  loadShadow();
}

export function shown() {
  loadHealth();
}

document.getElementById('health-refresh').addEventListener('click', loadHealth);

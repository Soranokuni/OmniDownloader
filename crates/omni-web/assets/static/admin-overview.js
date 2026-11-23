/* Admin panel, «Επισκόπηση» (P13.5): health, what needs the administrator,
 * the autonomy report and the repair-shadow card. Loaded when the section
 * is first opened. Words by key from admin-text.*.js (P13.6).
 */

import { api, el, render, toast, fmtTime } from '/static/app.js?v=6';
import { t } from '/static/admin-text.js?v=2';

// The autonomy report (P11.0): the soak test's figure and why people were needed.
// Each touch reason has its words under `admin.touch.<reason>`.
const TOUCH_REASONS = [
  'retry', 'override', 'rename', 'redownload', 'manual_done', 'approve', 'reject',
  'next', 'wrong_video', 'cancel', 'now', 'discard', 'queued',
];
const touchLabel = (key) => (TOUCH_REASONS.includes(key) ? t(`admin.touch.${key}`) : key);

const rate1 = (r) => (r === null || r === undefined ? '—' : `${(r * 100).toFixed(1)} %`);

function autonomyTable(title, head, rows) {
  return el('div', { class: 'stack' },
    el('h3', { class: 'card-title', style: 'font-size:0.95rem' }, title),
    el('div', { class: 'card flush' }, el('table', {},
      el('thead', {}, el('tr', {}, head.map((h, i) => el('th', i > 0 ? { class: 'right' } : {}, h)))),
      el('tbody', {}, rows.length > 0
        ? rows
        : el('tr', {}, el('td', { colspan: String(head.length), class: 'empty' }, t('admin.none')))))));
}

function splitLine(label, x) {
  const aired = x.delivered + x.delivered_manual;
  return t('admin.autonomy.split', { label, untouched: x.delivered_untouched, aired, ended: x.ended });
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
  const n = data.totals;
  const waiting = el('p', {}, el('strong', {}, t('admin.autonomy.waiting', { n: data.waiting_now })));
  if (n.ended === 0) {
    render('autonomy-figures', el('p', { class: 'note' }, t('admin.autonomy.empty')), waiting);
    render('autonomy-tables');
    return;
  }
  const aired = n.delivered + n.delivered_manual;
  render('autonomy-figures',
    el('p', { style: 'font-size:1.6rem;margin:0' }, el('strong', {},
      t('admin.autonomy.untouched', { rate: rate1(data.untouched_rate), untouched: n.delivered_untouched, aired }))),
    el('p', { class: 'note' }, t('admin.autonomy.strict', {
      rate: rate1(data.strict_rate), ended: n.ended, cancelled: n.cancelled, discarded: n.discarded, manual: n.delivered_manual,
    })),
    el('p', { class: 'note' }, `${splitLine(t('admin.autonomy.from_mail'), data.from_mail)} ${splitLine(t('admin.autonomy.manual'), data.manual)}`),
    waiting);
  render('autonomy-tables',
    autonomyTable(t('admin.autonomy.why'), [t('admin.autonomy.first_error'), t('admin.autonomy.jobs')],
      (data.by_first_error || []).map((c) => el('tr', {},
        el('td', { class: 'mono' }, c.key), el('td', { class: 'right' }, String(c.count))))),
    autonomyTable(t('admin.autonomy.did'), [t('admin.autonomy.action'), t('admin.autonomy.jobs')],
      (data.by_touch_reason || []).map((c) => el('tr', {},
        el('td', { title: c.key }, touchLabel(c.key)), el('td', { class: 'right' }, String(c.count))))),
    autonomyTable(t('admin.autonomy.sites'), [t('admin.autonomy.site'), t('admin.autonomy.site_untouched'), t('admin.autonomy.total')],
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
const stateWord = (state) => (STATE_CLASS[state] ? t(`admin.state.${state}`) : state);

// Where each health line is fixed, for the «needs the administrator» list.
const FIX_IN = {
  mail: 'settings', llm: 'settings', watchfolder: 'settings', archive: 'settings', config: 'settings',
  accounts: 'people',
  sites: 'sites', cookies: 'sites', selfcheck: 'sites', browser: 'sites',
  tools: 'maintenance', pinned_tools: 'maintenance', encoder: 'maintenance', deno: 'maintenance',
  disk: 'maintenance', queue: 'maintenance', workers: 'maintenance', lockdown: 'maintenance',
};
const SECTIONS = ['settings', 'people', 'sites', 'maintenance', 'overview'];
const sectionName = (section) => (SECTIONS.includes(section) ? t(`admin.nav.${section}`) : section);

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
    ? el('p', { class: 'note ok' }, t('admin.health.green'))
    : checks.map(([name, c]) => el('div', { class: 'spec-row' },
      el('span', { class: 'mono' }, name),
      el('span', {}, el('span', { class: `badge ${STATE_CLASS[c.state] || ''}`, style: 'margin-right:8px' }, stateWord(c.state)),
        c.detail || ''))));

  const needs = [];
  if (settings && settings.setup_completed === false) {
    needs.push(['wizard', t('admin.needs.setup')]);
  }
  if (settings && settings.watchfolder_pending) {
    needs.push(['settings', t('admin.needs.wf_pending', { path: settings.watchfolder_pending.path })]);
  }
  for (const [name, c] of checks) {
    if (c.state !== 'ok') needs.push([FIX_IN[name] || 'overview', `${name}: ${c.detail || stateWord(c.state)}`]);
  }
  render('needs-list', needs.length === 0
    ? el('p', { class: 'note ok' }, t('admin.needs.nothing'))
    : needs.map(([section, text]) => el('div', { class: 'row tight', style: 'justify-content:space-between' },
      el('span', {}, text),
      section === 'wizard'
        // Back into the wizard (P13.5b): the page shows it again while setup is unfinished.
        ? el('button', { class: 'btn btn-primary', type: 'button', onclick: () => { sessionStorage.removeItem('omni.setup.leave'); location.reload(); } }, t('admin.needs.continue'))
        : el('button', { class: 'btn', type: 'button', onclick: () => { location.hash = section; } }, sectionName(section)))));
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
    : [[`${prefix}${k}`, Array.isArray(v) ? t('admin.shadow.items', { n: v.length }) : String(v)]]));
}

async function loadShadow() {
  try {
    const report = await api(SHADOW_URL);
    render('shadow-body', shadowRows(report).slice(0, 40).map(([k, v]) => el('div', { class: 'spec-row' },
      el('span', { class: 'mono' }, k), el('span', {}, v))));
  } catch (e) {
    render('shadow-body', el('p', { class: 'note' }, e.status === 404 ? t('admin.shadow.none') : e.message));
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

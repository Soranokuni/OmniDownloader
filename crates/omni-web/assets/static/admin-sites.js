/* Admin panel, «Ιστότοποι» (P13.5): sites, recipes, the repair report, the
 * test URL, cookies, self-heal and the self-check. Loaded when the section is
 * first opened. Words by key from admin-text.*.js (P13.6b); what the server
 * reports in its own words (details, verdicts, outcomes) is shown as it comes.
 */

import { api, el, render, toast, fmtTime } from '/static/app.js?v=6';
import { copyText, saveJson } from '/static/admin-common.js?v=3';
import { confirmDialog } from '/static/dialog.js?v=10';
import { t } from '/static/admin-text.js?v=3';

/** A word for a server code: `prefix.<code>` when `codes` has it, else the code itself. */
const word = (prefix, codes, code) => (codes.includes(code) ? t(`${prefix}.${code}`) : code);

/* ------------------------------------------------------------------ *
 * Self-check (plan P6.7)
 * ------------------------------------------------------------------ */

const CHECK_CLASS = { ok: 'ok', degraded: 'warn', down: 'bad' };

function checkLine(title, check) {
  if (!check) return el('div', { class: 'spec-row' }, el('span', {}, title), el('span', { class: 'note' }, t('admin.selfcheck.not_yet')));
  return el('div', { class: 'spec-row' },
    el('span', {}, title),
    el('span', {},
      el('span', { class: `badge ${CHECK_CLASS[check.state] ?? ''}`, style: 'margin-right:8px' },
        check.state === 'ok' ? t('admin.selfcheck.ok') : t('admin.selfcheck.problem')),
      check.detail || ''),
  );
}

// Each self-heal outcome's words are `admin.selfheal.<outcome>`.
const HEAL_RESULTS = ['found', 'nothing', 'skipped', 'outage'];

// The overnight self-heal runs (P10.14): a host, the time, what came of it.
function renderSelfheal(rows) {
  if (rows.length === 0) {
    render('selfheal-body', el('tr', {}, el('td', { colspan: '5', class: 'empty' }, t('admin.selfheal.none'))));
    return;
  }
  render('selfheal-body', rows.map((r) => el('tr', {},
    el('td', { class: 'mono' }, r.domain),
    el('td', {}, fmtTime(r.at)),
    el('td', {}, el('span', { class: `badge ${r.outcome === 'found' ? 'ok' : r.outcome === 'nothing' ? 'warn' : ''}` },
      word('admin.selfheal', HEAL_RESULTS, r.outcome))),
    el('td', { class: 'mono' }, r.origin || ''),
    el('td', { class: 'right' }, r.outcome === 'found' ? String(r.pages ?? 0) : ''),
  )));
}

async function loadSelfcheck() {
  let data;
  try {
    data = await api('/api/admin/selfcheck');
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }
  render('selfcheck-summary',
    checkLine(t('admin.selfcheck.browser'), data.browser),
    checkLine(t('admin.selfcheck.links'), data.summary),
  );
  renderSelfheal(data.selfheal || []);
  const links = data.links || [];
  if (links.length === 0) {
    render('selfcheck-body', el('tr', {}, el('td', { colspan: '5', class: 'empty' }, t('admin.selfcheck.no_links'))));
    return;
  }
  render('selfcheck-body', links.map((link) => el('tr', {},
    el('td', {},
      el('div', { class: 'strong' }, link.label),
      el('a', {
        class: 'note mono',
        href: link.url,
        target: '_blank',
        rel: 'noopener noreferrer',
        title: link.url,
        style: 'display:block;max-width:340px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap',
      }, link.url),
    ),
    el('td', { style: 'white-space:nowrap' },
      link.last_ok === null || link.last_ok === undefined
        ? el('span', { class: 'note' }, t('admin.selfcheck.not_yet'))
        : link.last_ok
          ? el('span', { class: 'badge ok' }, t('admin.selfcheck.works'))
          : el('span', { class: 'badge bad', title: link.last_detail || '' },
              t('admin.selfcheck.failing', { since: fmtTime(link.failing_since) })),
    ),
    el('td', { class: 'num' }, fmtTime(link.last_checked_at)),
    el('td', { class: 'note' }, link.last_detail || ''),
    el('td', { class: 'right' },
      el('button', {
        class: 'btn btn-icon btn-danger',
        type: 'button',
        title: t('admin.selfcheck.remove_title', { name: link.label }),
        onClick: () => removeSelfcheckLink(link),
      }, '✕'),
    ),
  )));
}

async function removeSelfcheckLink(link) {
  if (!await confirmDialog(t('admin.selfcheck.remove_ask', { name: link.label }), { okLabel: t('admin.remove'), danger: true })) return;
  try {
    await api(`/api/admin/selfcheck/links/${link.id}/delete`, { method: 'POST' });
    loadSelfcheck();
  } catch (e) { toast(e.message, 'bad'); }
}

document.getElementById('selfcheck-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  try {
    await api('/api/admin/selfcheck/links', {
      method: 'POST',
      body: {
        label: document.getElementById('selfcheck-label').value,
        url: document.getElementById('selfcheck-url').value,
      },
    });
    toast(t('admin.selfcheck.added'), 'ok');
    event.target.reset();
    loadSelfcheck();
  } catch (e) { toast(e.message, 'bad'); }
});

document.getElementById('selfcheck-refresh').addEventListener('click', loadSelfcheck);
document.getElementById('selfcheck-run').addEventListener('click', async () => {
  try {
    await api('/api/admin/maintenance/selfcheck/run', { method: 'POST' });
    toast(t('admin.selfcheck.started'), 'ok');
  } catch (e) { toast(e.message, 'bad'); }
});

/* ------------------------------------------------------------------ *
 * Sites, recipes and the repair report (plan P10.15a)
 * ------------------------------------------------------------------ */

const SITE_CLASS = { healthy: 'ok', degraded: 'warn', broken: 'bad' };
// Each state's word is `admin.sites.state.<state>`, each repair mode's `admin.op.mode.<mode>`.
const SITE_STATES = Object.keys(SITE_CLASS);
const REPAIR_MODES = ['off', 'shadow', 'hold'];
const repairMode = (mode) => word('admin.op.mode', REPAIR_MODES, mode);

const pct = (matched, known) => (known > 0 ? `${Math.round((matched / known) * 100)}% (${matched}/${known})` : '—');

function fmtMs(ms) {
  if (ms === null || ms === undefined) return '—';
  const s = Math.round(ms / 1000);
  return s >= 60 ? `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, '0')}s` : `${s}s`;
}

// Broken and degraded sites come first from the server; the table shows the first 25.
const SITE_PAGE = 25;
const sitesView = { all: [], shown: SITE_PAGE, canRun: false };

async function loadSites() {
  let data;
  try {
    data = await api('/api/admin/sites');
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }
  const mode = data.repair_mode || 'off';
  render('sites-note', document.createTextNode(
    `${t('admin.repair_mode', { mode: repairMode(mode) })} ${mode === 'off' ? t('admin.sites.mode_off') : t('admin.sites.mode_on')}`));
  sitesView.all = data.sites || [];
  sitesView.canRun = !!data.can_run;
  renderSites();
}

function renderSites() {
  const sites = sitesView.all.slice(0, sitesView.shown);
  document.getElementById('sites-more').hidden = sitesView.all.length <= sites.length;
  if (sitesView.all.length === 0) {
    render('sites-body', el('tr', {}, el('td', { colspan: '8', class: 'empty' }, t('admin.sites.none'))));
    return;
  }
  render('sites-body', sites.map((s) => el('tr', {},
    el('td', { class: 'mono' }, s.domain),
    el('td', {},
      el('span', { class: `badge ${SITE_CLASS[s.state] || ''}`, title: s.state_since ? t('admin.since', { at: fmtTime(s.state_since) }) : '' },
        word('admin.sites.state', SITE_STATES, s.state))),
    el('td', {}, s.recent_rate === null || s.recent_rate === undefined ? '—'
      : t('admin.sites.rate', { rate: Math.round(s.recent_rate * 100), n: s.recent_len })),
    el('td', { class: 'num' }, fmtTime(s.last_ok_at)),
    el('td', { class: 'mono' }, s.last_code || '—'),
    el('td', { class: 'num' }, s.llm_breaker_until ? fmtTime(s.llm_breaker_until) : '—'),
    el('td', {}, s.canaries === 0 ? '—' : `${s.canaries_ok}/${s.canaries}`),
    el('td', { class: 'right' },
      el('button', {
        class: 'btn',
        type: 'button',
        disabled: !sitesView.canRun || s.canaries === 0,
        title: s.canaries === 0 ? t('admin.sites.no_canaries') : t('admin.sites.check_now'),
        onClick: () => runSiteNow(s),
      }, t('admin.sites.run_now'))),
  )));
}

async function runSiteNow(site) {
  try {
    await api(`/api/admin/sites/${encodeURIComponent(site.domain)}/check`, { method: 'POST' });
    toast(t('admin.sites.checking', { domain: site.domain }), 'ok');
    setTimeout(loadSites, 20000);
  } catch (e) { toast(e.message, 'bad'); }
}

const RECIPE_PAGE = 25;
const recipesView = { all: [], shown: RECIPE_PAGE, retired: false };
// Each recipe state's word is `admin.recipes.state.<state>`.
const RECIPE_STATES = ['candidate', 'active', 'suspended', 'retired'];
const recipeState = (state) => word('admin.recipes.state', RECIPE_STATES, state);

function recipeScope(r) {
  if (r.scope_kind === 'generic') return t('admin.recipes.builtin', { key: r.scope_key });
  return `${r.scope_kind}: ${r.scope_key}`;
}

function recipeSiteRows(r) {
  const rows = r.sites.slice();
  // A site recipe can be suspended on its own site before it has any score there.
  if (r.scope_kind === 'site' && !rows.some((s) => s.domain === r.scope_key)) {
    rows.unshift({ domain: r.scope_key, state: 'no_score', placeholder: true, approvals: 0, rejections: 0, uses: 0 });
  }
  return rows;
}

function recipeCard(r) {
  const live = r.state !== 'retired';
  const sites = recipeSiteRows(r);
  const table = sites.length === 0
    ? el('p', { class: 'note' }, t('admin.recipes.unused'))
    : el('div', { class: 'card flush' }, el('table', {},
        el('thead', {}, el('tr', {},
          el('th', {}, t('admin.col.site')), el('th', {}, t('admin.col.state')), el('th', { class: 'right' }, t('admin.recipes.rank')),
          el('th', { class: 'right' }, t('admin.recipes.approvals')), el('th', { class: 'right' }, t('admin.recipes.rejections')),
          el('th', { class: 'right' }, t('admin.recipes.uses')), el('th', {}, t('admin.recipes.last_used')), el('th', { class: 'right' }, ''))),
        el('tbody', {}, sites.map((s) => el('tr', {},
          el('td', { class: 'mono' }, s.domain),
          el('td', {}, el('span', { class: `badge ${s.state === 'active' ? 'ok' : s.state === 'suspended' ? 'bad' : ''}` },
            s.placeholder ? t('admin.recipes.no_score')
              : (s.suspended_reason ? t('admin.recipes.suspended_why', { why: s.suspended_reason }) : recipeState(s.state)))),
          el('td', { class: 'right' }, s.placeholder ? '—' : s.rank.toFixed(2)),
          el('td', { class: 'right' }, String(s.approvals)),
          el('td', { class: 'right' }, String(s.rejections)),
          el('td', { class: 'right' }, String(s.uses)),
          el('td', { class: 'num' }, fmtTime(s.last_used_at)),
          el('td', { class: 'right' }, live && s.state !== 'suspended'
            ? el('button', { class: 'btn', type: 'button', onClick: () => suspendRecipe(r, s.domain) }, t('admin.recipes.suspend'))
            : null),
        )))));
  return el('details', { class: 'card' },
    el('summary', { class: 'row tight', style: 'cursor:pointer' },
      el('span', { class: 'mono strong' }, recipeScope(r)),
      el('span', { class: `badge ${r.state === 'active' ? 'ok' : r.state === 'retired' ? '' : r.state === 'suspended' ? 'bad' : 'info'}` },
        r.state === 'retired' && r.retired_reason ? t('admin.recipes.retired_why', { why: r.retired_reason }) : recipeState(r.state)),
      el('span', { class: 'badge' }, r.origin),
      el('span', { class: 'note' }, t('admin.recipes.rank_n', { rank: r.rank.toFixed(2) }))),
    el('div', { class: 'stack', style: 'margin-top:10px' },
      el('p', { class: 'note' },
        `#${r.id} · ${r.hash} · ${t('admin.recipes.steps', { steps: r.ops.join(' → ') || '—' })}` +
        (r.family ? ` · ${t('admin.recipes.player', { family: r.family })}` : '') +
        ` · ${t('admin.recipes.added', { at: fmtTime(r.created_at) })}` +
        (r.retired_at ? ` · ${t('admin.recipes.retired_at', { at: fmtTime(r.retired_at) })}` : '')),
      table,
      el('div', { class: 'row' }, (live || !['admin', 'rejected'].includes(r.retired_reason)) && r.origin !== 'shipped'
        ? el('button', { class: 'btn btn-danger', type: 'button', onClick: () => retireRecipe(r) }, t('admin.recipes.retire_for_good'))
        : null)));
}

function renderRecipes() {
  const list = recipesView.all.filter((r) => recipesView.retired || r.state !== 'retired');
  const page = list.slice(0, recipesView.shown);
  render('recipes-list', page.length === 0
    ? el('p', { class: 'note' }, t('admin.recipes.none'))
    : page.map(recipeCard));
  document.getElementById('recipes-more').hidden = list.length <= page.length;
}

async function loadRecipes() {
  let data;
  try {
    data = await api('/api/admin/recipes');
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }
  recipesView.all = data.recipes || [];
  renderRecipes();
}

async function suspendRecipe(recipe, domain) {
  if (!await confirmDialog(t('admin.recipes.suspend_ask', { domain }), { okLabel: t('admin.recipes.suspend') })) return;
  try {
    await api(`/api/admin/recipes/${recipe.id}/suspend`, { method: 'POST', body: { domain } });
    toast(t('admin.recipes.suspended'), 'ok');
    loadRecipes();
  } catch (e) { toast(e.message, 'bad'); }
}

async function retireRecipe(recipe) {
  if (!await confirmDialog(t('admin.recipes.retire_ask'), { okLabel: t('admin.recipes.retire'), danger: true })) return;
  try {
    await api(`/api/admin/recipes/${recipe.id}/retire`, { method: 'POST' });
    toast(t('admin.recipes.retired'), 'ok');
    loadRecipes();
  } catch (e) { toast(e.message, 'bad'); }
}

const recipesResult = document.getElementById('recipes-result');

function showRecipes(message, ok) {
  recipesResult.textContent = message;
  recipesResult.className = ok ? 'note ok' : 'note bad';
}

// Each import outcome's words are `admin.recipes.import.<outcome>`.
const IMPORT_RESULTS = ['inserted', 'merged', 'revived', 'refused', 'over_cap'];

async function importRecipes(text) {
  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch {
    showRecipes(t('admin.bad_json'), false);
    return;
  }
  // A bare array is accepted as well as {"recipes": [...]}.
  const body = Array.isArray(parsed) ? { recipes: parsed } : parsed;
  try {
    const out = await api('/api/admin/recipes/import', { method: 'POST', body });
    const line = (r) => `${r.index}. ${r.scope}: ${word('admin.recipes.import', IMPORT_RESULTS, r.outcome)}${r.detail ? ` (${r.detail})` : ''}` +
      (r.evicted || []).map((e) => `\n    ${t('admin.recipes.evicted', { id: e.id, scope: e.scope, why: e.why })}`).join('');
    showRecipes(`${t('admin.recipes.imported')}\n${out.results.map(line).join('\n')}`, true);
    loadRecipes();
  } catch (e) { showRecipes(e.message, false); }
}

async function loadReport() {
  let data;
  try {
    data = await api('/api/admin/repair-report?days=30');
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }
  render('report-note', document.createTextNode(
    `${t('admin.repair_mode', { mode: repairMode(data.repair_mode) })} ${t('admin.report.about', { max: data.image_match_max })}`));
  const sites = data.sites || [];
  if (sites.length === 0) {
    render('report-body', el('tr', {}, el('td', { colspan: '15', class: 'empty' }, t('admin.report.none'))));
    return;
  }
  const sum = (key) => sites.reduce((n, s) => n + s[key], 0);
  const row = (name, s, strong) => el('tr', {},
    el('td', { class: strong ? 'mono strong' : 'mono' }, name),
    el('td', { class: 'right', title: t('admin.report.runs_title', { shadow: s.shadow_attempts, hold: s.hold_attempts }) }, String(s.attempts)),
    el('td', { class: 'right' }, String(s.next_choices)),
    el('td', { class: 'right' }, String(s.found)),
    el('td', { class: 'right' }, String(s.held)),
    el('td', { class: 'right', title: t('admin.report.auto_title') }, String(s.auto_delivered)),
    el('td', { class: 'right' }, String(s.approved)),
    el('td', { class: 'right' }, String(s.rejected_wrong_video)),
    el('td', { class: 'right' }, String(s.rejected_other)),
    el('td', { class: 'right' }, String(s.expired)),
    el('td', { class: 'right' }, pct(s.length_stated, s.found)),
    el('td', { class: 'right' }, pct(s.image_matched, s.image_known)),
    el('td', { class: 'right', title: t('admin.report.tokens_title', { in: s.tokens_in, out: s.tokens_out }) }, String(s.llm_calls)),
    el('td', { class: 'right' }, fmtMs(s.median_duration_ms)),
    el('td', { class: 'right' }, s.heal_runs > 0 ? `${s.heal_found}/${s.heal_runs}` : '—'),
  );
  const total = {
    attempts: sum('attempts'), shadow_attempts: sum('shadow_attempts'), hold_attempts: sum('hold_attempts'),
    found: sum('found'), held: sum('held'), auto_delivered: sum('auto_delivered'), approved: sum('approved'),
    rejected_wrong_video: sum('rejected_wrong_video'), rejected_other: sum('rejected_other'), expired: sum('expired'),
    length_stated: sum('length_stated'), next_choices: sum('next_choices'),
    image_known: sum('image_known'), image_matched: sum('image_matched'),
    llm_calls: sum('llm_calls'), tokens_in: sum('tokens_in'), tokens_out: sum('tokens_out'),
    median_duration_ms: null, heal_runs: sum('heal_runs'), heal_found: sum('heal_found'),
  };
  render('report-body', sites.map((s) => row(s.domain, s)), row(t('admin.report.all_sites'), total, true));
}

document.getElementById('sites-refresh').addEventListener('click', loadSites);
document.getElementById('sites-more').addEventListener('click', () => {
  sitesView.shown += SITE_PAGE;
  renderSites();
});
document.getElementById('recipes-refresh').addEventListener('click', loadRecipes);
document.getElementById('report-refresh').addEventListener('click', loadReport);
document.getElementById('recipes-retired').addEventListener('change', (event) => {
  recipesView.retired = event.target.checked;
  recipesView.shown = RECIPE_PAGE;
  renderRecipes();
});
document.getElementById('recipes-more').addEventListener('click', () => {
  recipesView.shown += RECIPE_PAGE;
  renderRecipes();
});
document.getElementById('recipes-export').addEventListener('click', async () => {
  try {
    saveJson('recipes.json', await api('/api/admin/recipes/export'));
    showRecipes(t('admin.recipes.exported'), true);
  } catch (e) { showRecipes(e.message, false); }
});
document.getElementById('recipes-import-btn').addEventListener('click', () => {
  importRecipes(document.getElementById('recipes-import-text').value);
});
const recipeFile = document.getElementById('recipes-import-file');
document.getElementById('recipes-import-pick').addEventListener('click', () => recipeFile.click());
recipeFile.addEventListener('change', async () => {
  const file = recipeFile.files && recipeFile.files[0];
  recipeFile.value = '';
  if (file) document.getElementById('recipes-import-text').value = await file.text();
});

/* ------------------------------------------------------------------ *
 * Test URL (plan P10.15b): a dry run, nothing is downloaded or queued
 * ------------------------------------------------------------------ */

const fmtSecs = (s) => (s === null || s === undefined ? '—' : `${Math.floor(s / 60)}:${String(Math.round(s % 60)).padStart(2, '0')}`);
const fmtMsShort = (ms) => (ms < 1000 ? `${ms} ms` : `${(ms / 1000).toFixed(1)} s`);

function tuBlock(title, ...children) {
  return el('div', { class: 'card flush stack', style: 'padding:12px' },
    el('h3', { class: 'card-title', style: 'font-size:0.95rem;margin:0' }, title),
    ...children);
}

function tuTable(head, rows) {
  return el('div', { class: 'card flush' }, el('table', {},
    el('thead', {}, el('tr', {}, head.map((h) => el('th', {}, h)))),
    el('tbody', {}, rows)));
}

const specRow = (label, value, cls) => el('div', { class: 'spec-row' }, el('span', {}, label), el('span', cls ? { class: cls } : {}, value));

function renderTestNormal(n) {
  const out = [];
  out.push(tuBlock(t('admin.tu.kind_title'), specRow(t('admin.tu.kind'), n.kind || '—', 'mono')));
  const a = n.site_api;
  if (a) {
    out.push(tuBlock(t('admin.tu.api_title'),
      el('p', {}, el('span', { class: `badge ${a.video ? 'ok' : (a.error ? 'bad' : '')}` },
        a.video ? t('admin.tu.api_video') : (a.error ? t('admin.tu.failed') : t('admin.tu.api_nothing'))), ` ${fmtMsShort(a.ms)}`),
      a.video ? specRow(t('admin.tu.video'), a.video, 'mono') : null,
      a.error ? el('p', { class: 'note mono' }, a.error) : null));
  }
  const y = n.ytdlp;
  if (y) {
    out.push(tuBlock(t('admin.tu.ytdlp_title'),
      el('p', {}, el('span', { class: `badge ${y.ok ? 'ok' : 'bad'}` }, y.ok ? t('admin.tu.ytdlp_ok') : t('admin.tu.ytdlp_fails')), ` ${fmtMsShort(y.ms)}`),
      y.ok
        ? specRow(t('admin.tu.extractor'), y.extractor, 'mono')
        : el('p', { class: 'note mono' }, y.error || ''),
      y.ok ? specRow(t('admin.tu.title'), y.title || '—') : null,
      y.ok ? specRow(t('admin.tu.length'), fmtSecs(y.duration_secs)) : null,
      y.ok ? specRow(t('admin.tu.formats'), String(y.formats)) : null));
  }
  const s = n.sniff;
  if (s) {
    out.push(tuBlock(t('admin.tu.sniff_title'),
      el('p', {}, el('span', { class: `badge ${s.ok ? 'ok' : 'bad'}` }, s.ok ? t('admin.tu.sniff_ok') : t('admin.tu.sniff_none')), ` ${fmtMsShort(s.ms)}`),
      s.ok ? specRow(t('admin.tu.best_of', { n: s.streams }), s.stream, 'mono') : null,
      s.error ? el('p', { class: 'note mono' }, s.error) : null));
  }
  return out;
}

function renderTestCandidates(list) {
  if (list.length === 0) return el('p', { class: 'note' }, t('admin.tu.no_candidates'));
  return tuTable([t('admin.recipes.rank'), t('admin.tu.address'), t('admin.tu.origin'), t('admin.tu.length'), t('admin.tu.height'), t('admin.tu.score'), t('admin.tu.checks')],
    list.map((c) => el('tr', {},
      el('td', {}, c.verified ? `#${c.rank}` : el('span', { class: 'badge bad' }, t('admin.tu.rejected'))),
      el('td', { class: 'mono' }, c.address),
      el('td', { class: 'mono' }, c.origin),
      el('td', { class: 'num' }, fmtSecs(c.duration_secs)),
      el('td', { class: 'num' }, c.height ? `${c.height}p` : '—'),
      el('td', { class: 'num' }, c.score === null || c.score === undefined ? '—' : String(c.score)),
      el('td', {}, c.checks.map((k) => el('div', { class: k.ok ? 'ok' : 'bad', title: k.detail },
        `${k.ok ? '✓' : '✗'} ${k.name}: ${k.detail}`))))));
}

function renderTestLadder(l) {
  const out = [];
  if (!l.page_looked) {
    out.push(tuBlock(t('admin.tu.page_title'), el('p', { class: 'note' }, t('admin.tu.page_none'))));
    return out;
  }
  out.push(tuBlock(t('admin.tu.page_title'),
    el('div', { class: 'spec-row' }, el('span', {}, t('admin.tu.class')), el('span', { class: 'badge' }, l.class || '—')),
    specRow(t('admin.tu.signals'), l.signals.length ? l.signals.join(', ') : '—', 'mono'),
    specRow(t('admin.tu.family'), l.family || '—', 'mono')));
  out.push(tuBlock(t('admin.tu.recipes_title'),
    l.recipes.length === 0
      ? el('p', { class: 'note' }, t('admin.tu.no_recipe'))
      : tuTable([t('admin.tu.recipe'), t('admin.tu.tier'), t('admin.tu.origin'), t('admin.tu.outcome'), t('admin.tu.address'), t('admin.tu.gate'), t('admin.tu.time')],
        l.recipes.map((r) => el('tr', {},
          el('td', { class: 'mono', title: r.hash }, r.name),
          el('td', {}, r.tier === 'generic' ? t('admin.tu.builtin') : r.tier),
          el('td', {}, r.origin),
          el('td', {}, r.outcome),
          el('td', { class: 'mono' }, r.address || '—'),
          el('td', {}, r.gate || '—'),
          el('td', { class: 'num' }, fmtMsShort(r.ms)))))));
  if (l.live) {
    out.push(tuBlock(t('admin.tu.live_title'),
      el('p', {}, `${l.live.run}, ${fmtMsShort(l.live.ms)}`),
      l.live.actions.length ? el('p', { class: 'note mono' }, l.live.actions.join(', ')) : null));
  }
  if (l.llm) {
    out.push(tuBlock(t('admin.tu.llm_title'),
      el('p', {}, l.llm.asked
        ? t('admin.tu.llm_calls', { calls: l.llm.calls, rounds: l.llm.rounds, in: l.llm.tokens_in, out: l.llm.tokens_out })
        : t('admin.tu.llm_none')),
      el('p', { class: 'note' }, l.llm.outcome)));
  }
  out.push(tuBlock(t('admin.tu.candidates_title'), renderTestCandidates(l.candidates)));
  return out;
}

function renderTestReport(r) {
  render('test-url-result', [
    el('p', {}, el('strong', {}, r.verdict)),
    el('p', { class: 'note' }, `${t('admin.repair_mode', { mode: repairMode(r.repair_mode) })} ${t('admin.tu.total', { time: fmtMsShort(r.total_ms) })}`),
    ...renderTestNormal(r.normal),
    ...(r.ladder ? renderTestLadder(r.ladder) : []),
    tuBlock(t('admin.tu.timings'), tuTable([t('admin.tu.step'), t('admin.tu.time'), t('admin.tu.note')], r.steps.map((s) => el('tr', {},
      el('td', {}, s.name), el('td', { class: 'num' }, fmtMsShort(s.ms)), el('td', {}, s.note))))),
  ]);
}

async function loadTestUrl() {
  let info;
  try {
    info = await api('/api/admin/test-url');
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }
  const box = document.getElementById('test-url-llm');
  box.disabled = !info.llm_available || info.llm_left <= 0;
  if (box.disabled) box.checked = false;
  document.getElementById('test-url-run').disabled = !info.can_run;
  document.getElementById('test-url-llm-label').textContent = !info.llm_available
    ? t('admin.tu.llm_unset')
    : t('admin.tu.llm_left', { left: info.llm_left, cap: info.llm_cap });
}

document.getElementById('test-url-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const run = document.getElementById('test-url-run');
  const url = document.getElementById('test-url-input').value.trim();
  const useLlm = document.getElementById('test-url-llm').checked;
  run.disabled = true;
  render('test-url-result', []);
  const started = Date.now();
  const progress = document.getElementById('test-url-progress');
  const tick = setInterval(() => {
    progress.textContent = t('admin.tu.running', { n: Math.round((Date.now() - started) / 1000) });
  }, 1000);
  progress.textContent = t('admin.tu.running', { n: 0 });
  try {
    renderTestReport(await api('/api/admin/test-url', { method: 'POST', body: { url, use_llm: useLlm } }));
    progress.textContent = '';
  } catch (e) {
    progress.textContent = e.message;
  } finally {
    clearInterval(tick);
    run.disabled = false;
    loadTestUrl();
  }
});


/* ------------------------------------------------------------------ *
 * Cookies (plan P3.5): one cookie jar per login-walled site
 * ------------------------------------------------------------------ */

// The file's text is held only until it is uploaded; the page never shows it
// and the server never sends cookies back.
let cookieFileText = '';
const cookiesResult = document.getElementById('cookies-result');

function showCookies(message, ok) {
  cookiesResult.textContent = message;
  cookiesResult.className = ok ? 'note ok' : 'note bad';
}

function cookieTestBadge(jar) {
  if (jar.last_test_ok === null || jar.last_test_ok === undefined) return el('span', { class: 'badge' }, t('admin.cookies.untested'));
  return el('span', { class: jar.last_test_ok ? 'badge ok' : 'badge bad', title: jar.last_test_detail || '' },
    jar.last_test_ok ? t('admin.cookies.ok') : t('admin.tu.failed'));
}

async function testCookies(jar, input, button) {
  const url = input.value.trim() || (jar.sample ? `https://${jar.sample}` : '');
  if (!url) { toast(t('admin.cookies.need_link'), 'bad'); return; }
  button.disabled = true;
  button.textContent = t('admin.testing');
  try {
    const out = await api(`/api/admin/cookies/${encodeURIComponent(jar.domain)}/test`, { method: 'POST', body: { url } });
    toast(out.ok ? t('admin.cookies.log_in') : t('admin.cookies.test_failed', { detail: out.detail }), out.ok ? 'ok' : 'bad');
  } catch (e) {
    toast(e.message, 'bad');
  }
  loadCookies();
}

async function deleteCookies(jar) {
  if (!await confirmDialog(t('admin.cookies.delete_ask', { domain: jar.domain }), { okLabel: t('admin.delete'), danger: true })) return;
  try {
    await api(`/api/admin/cookies/${encodeURIComponent(jar.domain)}/delete`, { method: 'POST' });
    toast(t('admin.deleted'), 'ok');
  } catch (e) { toast(e.message, 'bad'); }
  loadCookies();
}

// Sites whose login is captured by `omni-ingest cookies sign-in <site>` (P3.17).
const SIGNIN_SITES = ['youtube.com', 'x.com', 'facebook.com', 'instagram.com'];


function signinStatus(jar) {
  if (!jar) return el('span', { class: 'badge' }, t('admin.cookies.none_badge'));
  const bits = [t('admin.cookies.count', { n: jar.cookie_count }), `${jar.updated_by} · ${fmtTime(jar.updated_at)}`,
    jar.expires_min ? t('admin.cookies.expires', { at: fmtTime(jar.expires_min) }) : t('admin.cookies.session')];
  return el('span', {}, cookieTestBadge(jar), ' ', el('span', { class: 'note' }, bits.join(' · ')));
}

function renderSignin(data) {
  const jars = data.jars || [];
  const prefix = data.signin_command_prefix;
  const prefixCmd = data.signin_command_prefix_cmd;
  render('cookies-signin',
    el('p', { class: 'note' }, t('admin.cookies.signin_how')),
    // P13.9: no terminal needed: Setup puts the same command in the Start menu.
    el('p', { class: 'note ok' }, t('admin.cookies.signin_start')),
    ...SIGNIN_SITES.map((domain) => {
      const jar = jars.find((j) => j.domain === domain);
      const command = prefix ? `${prefix} ${domain}` : `& ".\\omni-ingest.exe" cookies sign-in ${domain}`;
      const commandCmd = prefixCmd ? `${prefixCmd} ${domain}` : `omni-ingest.exe cookies sign-in ${domain}`;
      const row = (label, text) => {
        const copy = el('button', { class: 'btn', type: 'button' }, t('wizard.copy'));
        copy.addEventListener('click', () => copyText(text, copy));
        return el('div', { class: 'row tight' }, el('span', { class: 'note' }, label), el('code', { class: 'mono grow' }, text), copy);
      };
      return el('div', { class: 'stack' },
        el('div', { class: 'row tight' }, el('span', { class: 'mono strong' }, domain), signinStatus(jar)),
        row('PowerShell', command),
        row('cmd', commandCmd));
    }));
}

async function loadCookies() {
  let data;
  try {
    data = await api('/api/admin/cookies');
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }
  renderSignin(data);
  const jars = data.jars || [];
  if (jars.length === 0) {
    render('cookies-body', el('tr', {}, el('td', { colspan: '7', class: 'empty' }, t('admin.cookies.none'))));
    return;
  }
  render('cookies-body', jars.map((jar) => {
    const input = el('input', { type: 'text', class: 'mono', placeholder: jar.sample || `https://${jar.domain}/…`, autocomplete: 'off' });
    const testBtn = el('button', { class: 'btn', type: 'button' }, t('admin.test'));
    testBtn.addEventListener('click', () => testCookies(jar, input, testBtn));
    const delBtn = el('button', { class: 'btn btn-danger', type: 'button' }, t('admin.delete'));
    delBtn.addEventListener('click', () => deleteCookies(jar));
    return el('tr', {},
      el('td', { class: 'mono strong' }, jar.domain),
      el('td', { class: 'right' }, String(jar.cookie_count)),
      el('td', {}, jar.expires_min ? fmtTime(jar.expires_min) : t('admin.cookies.session')),
      el('td', {}, `${jar.updated_by} · ${fmtTime(jar.updated_at)}`),
      el('td', {}, cookieTestBadge(jar), ' ', el('span', { class: 'note' }, jar.last_test_at ? fmtTime(jar.last_test_at) : ''),
        jar.last_test_ok === false && jar.last_test_detail ? el('div', { class: 'note' }, jar.last_test_detail) : null),
      el('td', {}, input, jar.sample ? el('div', { class: 'note mono' }, t('admin.cookies.last', { sample: jar.sample })) : null),
      el('td', {}, el('div', { class: 'row tight' }, testBtn, delBtn)),
    );
  }));
}

const cookiesFile = document.getElementById('cookies-file');
document.getElementById('cookies-pick').addEventListener('click', () => cookiesFile.click());
cookiesFile.addEventListener('change', async () => {
  const file = cookiesFile.files && cookiesFile.files[0];
  cookiesFile.value = '';
  if (!file) return;
  cookieFileText = await file.text();
  document.getElementById('cookies-file-note').textContent = t('admin.cookies.chosen', { name: file.name, kib: Math.ceil(file.size / 1024) });
});
document.getElementById('cookies-refresh').addEventListener('click', loadCookies);
document.getElementById('cookies-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const domain = document.getElementById('cookies-domain').value.trim().toLowerCase();
  if (!cookieFileText) { showCookies(t('admin.cookies.choose_first'), false); return; }
  const button = document.getElementById('cookies-upload');
  button.disabled = true;
  try {
    const out = await api('/api/admin/cookies', { method: 'POST', body: { domain, netscape: cookieFileText } });
    showCookies(
      t(out.replaced ? 'admin.cookies.replaced' : 'admin.cookies.stored', { n: out.cookie_count, domain: out.domain }) +
      (out.skipped_other_site ? `\n${t('admin.cookies.other_site', { n: out.skipped_other_site })}` : '') +
      (out.skipped_malformed ? `\n${t('admin.cookies.malformed', { n: out.skipped_malformed })}` : ''),
      true);
    cookieFileText = '';
    document.getElementById('cookies-file-note').textContent = '';
  } catch (e) {
    showCookies(e.message, false);
  } finally {
    button.disabled = false;
    loadCookies();
  }
});


/** The section opened: load every card once; Refresh buttons reload. */
export function init() {
  loadSelfcheck();
  loadSites();
  loadRecipes();
  loadReport();
  loadCookies();
  loadTestUrl();
}

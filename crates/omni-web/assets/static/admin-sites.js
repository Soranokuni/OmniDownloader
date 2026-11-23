/* Admin panel, «Ιστότοποι» (P13.5): sites, recipes, the repair report, the
 * test URL, cookies, self-heal and the self-check. Loaded when the section is
 * first opened.
 */

import { api, el, render, toast, fmtTime } from '/static/app.js?v=6';
import { copyText, saveJson } from '/static/admin-common.js?v=1';
import { confirmDialog } from '/static/dialog.js?v=10';

/* ------------------------------------------------------------------ *
 * Self-check (plan P6.7)
 * ------------------------------------------------------------------ */

const CHECK_CLASS = { ok: 'ok', degraded: 'warn', down: 'bad' };

function checkLine(title, check) {
  if (!check) return el('div', { class: 'spec-row' }, el('span', {}, title), el('span', { class: 'note' }, 'not checked yet'));
  return el('div', { class: 'spec-row' },
    el('span', {}, title),
    el('span', {},
      el('span', { class: `badge ${CHECK_CLASS[check.state] ?? ''}`, style: 'margin-right:8px' }, check.state === 'ok' ? 'OK' : 'Problem'),
      check.detail || ''),
  );
}

const HEAL_RESULT = {
  found: 'Found a way',
  nothing: 'Nothing found',
  skipped: 'Not tried',
  outage: 'Our own outage',
};

// The overnight self-heal runs (P10.14): a host, the time, what came of it.
function renderSelfheal(rows) {
  if (rows.length === 0) {
    render('selfheal-body', el('tr', {}, el('td', { colspan: '5', class: 'empty' }, 'No site needed it in the last 30 days.')));
    return;
  }
  render('selfheal-body', rows.map((r) => el('tr', {},
    el('td', { class: 'mono' }, r.domain),
    el('td', {}, fmtTime(r.at)),
    el('td', {}, el('span', { class: `badge ${r.outcome === 'found' ? 'ok' : r.outcome === 'nothing' ? 'warn' : ''}` }, HEAL_RESULT[r.outcome] || r.outcome)),
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
    checkLine('Browser (for news articles)', data.browser),
    checkLine('Links', data.summary),
  );
  renderSelfheal(data.selfheal || []);
  const links = data.links || [];
  if (links.length === 0) {
    render('selfcheck-body', el('tr', {}, el('td', { colspan: '5', class: 'empty' }, 'No links. Add one below.')));
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
        ? el('span', { class: 'note' }, 'not checked yet')
        : link.last_ok
          ? el('span', { class: 'badge ok' }, 'Works')
          : el('span', { class: 'badge bad', title: link.last_detail || '' },
              `Not working since ${fmtTime(link.failing_since)}`),
    ),
    el('td', { class: 'num' }, fmtTime(link.last_checked_at)),
    el('td', { class: 'note' }, link.last_detail || ''),
    el('td', { class: 'right' },
      el('button', {
        class: 'btn btn-icon btn-danger',
        type: 'button',
        title: `Remove ${link.label}`,
        onClick: () => removeSelfcheckLink(link),
      }, '✕'),
    ),
  )));
}

async function removeSelfcheckLink(link) {
  if (!await confirmDialog(`Remove "${link.label}" from the self-check?`, { okLabel: 'Remove', danger: true })) return;
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
    toast('Link added. It is checked on the next run.', 'ok');
    event.target.reset();
    loadSelfcheck();
  } catch (e) { toast(e.message, 'bad'); }
});

document.getElementById('selfcheck-refresh').addEventListener('click', loadSelfcheck);
document.getElementById('selfcheck-run').addEventListener('click', async () => {
  try {
    await api('/api/admin/maintenance/selfcheck/run', { method: 'POST' });
    toast('The check starts within a minute and takes a few minutes. Press Refresh to see the results.', 'ok');
  } catch (e) { toast(e.message, 'bad'); }
});

/* ------------------------------------------------------------------ *
 * Sites, recipes and the repair report (plan P10.15a)
 * ------------------------------------------------------------------ */

const SITE_CLASS = { healthy: 'ok', degraded: 'warn', broken: 'bad' };

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
    `Repair mode: ${mode}. ` + (mode === 'off'
      ? 'With repair off, «Run now» only checks the site.'
      : 'A broken site is also probed for a new way to find its video.')));
  sitesView.all = data.sites || [];
  sitesView.canRun = !!data.can_run;
  renderSites();
}

function renderSites() {
  const sites = sitesView.all.slice(0, sitesView.shown);
  document.getElementById('sites-more').hidden = sitesView.all.length <= sites.length;
  if (sitesView.all.length === 0) {
    render('sites-body', el('tr', {}, el('td', { colspan: '8', class: 'empty' }, 'No site has been seen yet.')));
    return;
  }
  render('sites-body', sites.map((s) => el('tr', {},
    el('td', { class: 'mono' }, s.domain),
    el('td', {},
      el('span', { class: `badge ${SITE_CLASS[s.state] || ''}`, title: s.state_since ? `since ${fmtTime(s.state_since)}` : '' }, s.state)),
    el('td', {}, s.recent_rate === null || s.recent_rate === undefined ? '—' : `${Math.round(s.recent_rate * 100)}% of ${s.recent_len}`),
    el('td', { class: 'num' }, fmtTime(s.last_ok_at)),
    el('td', { class: 'mono' }, s.last_code || '—'),
    el('td', { class: 'num' }, s.llm_breaker_until ? fmtTime(s.llm_breaker_until) : '—'),
    el('td', {}, s.canaries === 0 ? '—' : `${s.canaries_ok}/${s.canaries}`),
    el('td', { class: 'right' },
      el('button', {
        class: 'btn',
        type: 'button',
        disabled: !sitesView.canRun || s.canaries === 0,
        title: s.canaries === 0 ? 'This site has no canary pages yet.' : 'Check this site now',
        onClick: () => runSiteNow(s),
      }, 'Run now')),
  )));
}

async function runSiteNow(site) {
  try {
    await api(`/api/admin/sites/${encodeURIComponent(site.domain)}/check`, { method: 'POST' });
    toast(`Checking ${site.domain}. This takes a few minutes; press Refresh to see the result.`, 'ok');
    setTimeout(loadSites, 20000);
  } catch (e) { toast(e.message, 'bad'); }
}

const RECIPE_PAGE = 25;
const recipesView = { all: [], shown: RECIPE_PAGE, retired: false };

function recipeScope(r) {
  if (r.scope_kind === 'generic') return `built-in: ${r.scope_key}`;
  return `${r.scope_kind}: ${r.scope_key}`;
}

function recipeSiteRows(r) {
  const rows = r.sites.slice();
  // A site recipe can be suspended on its own site before it has any score there.
  if (r.scope_kind === 'site' && !rows.some((s) => s.domain === r.scope_key)) {
    rows.unshift({ domain: r.scope_key, state: 'no score yet', placeholder: true, approvals: 0, rejections: 0, uses: 0 });
  }
  return rows;
}

function recipeCard(r) {
  const live = r.state !== 'retired';
  const sites = recipeSiteRows(r);
  const table = sites.length === 0
    ? el('p', { class: 'note' }, 'No site has used it yet.')
    : el('div', { class: 'card flush' }, el('table', {},
        el('thead', {}, el('tr', {},
          el('th', {}, 'Site'), el('th', {}, 'State'), el('th', { class: 'right' }, 'Rank'),
          el('th', { class: 'right' }, 'Approvals'), el('th', { class: 'right' }, 'Rejections'),
          el('th', { class: 'right' }, 'Uses'), el('th', {}, 'Last used'), el('th', { class: 'right' }, ''))),
        el('tbody', {}, sites.map((s) => el('tr', {},
          el('td', { class: 'mono' }, s.domain),
          el('td', {}, el('span', { class: `badge ${s.state === 'active' ? 'ok' : s.state === 'suspended' ? 'bad' : ''}` },
            s.suspended_reason ? `suspended (${s.suspended_reason})` : s.state)),
          el('td', { class: 'right' }, s.placeholder ? '—' : s.rank.toFixed(2)),
          el('td', { class: 'right' }, String(s.approvals)),
          el('td', { class: 'right' }, String(s.rejections)),
          el('td', { class: 'right' }, String(s.uses)),
          el('td', { class: 'num' }, fmtTime(s.last_used_at)),
          el('td', { class: 'right' }, live && s.state !== 'suspended'
            ? el('button', { class: 'btn', type: 'button', onClick: () => suspendRecipe(r, s.domain) }, 'Suspend')
            : null),
        )))));
  return el('details', { class: 'card' },
    el('summary', { class: 'row tight', style: 'cursor:pointer' },
      el('span', { class: 'mono strong' }, recipeScope(r)),
      el('span', { class: `badge ${r.state === 'active' ? 'ok' : r.state === 'retired' ? '' : r.state === 'suspended' ? 'bad' : 'info'}` },
        r.state === 'retired' && r.retired_reason ? `retired (${r.retired_reason})` : r.state),
      el('span', { class: 'badge' }, r.origin),
      el('span', { class: 'note' }, `rank ${r.rank.toFixed(2)}`)),
    el('div', { class: 'stack', style: 'margin-top:10px' },
      el('p', { class: 'note' },
        `#${r.id} · ${r.hash} · steps: ${r.ops.join(' → ') || '—'}` +
        (r.family ? ` · player: ${r.family}` : '') +
        ` · added ${fmtTime(r.created_at)}` + (r.retired_at ? ` · retired ${fmtTime(r.retired_at)}` : '')),
      table,
      el('div', { class: 'row' }, (live || !['admin', 'rejected'].includes(r.retired_reason)) && r.origin !== 'shipped'
        ? el('button', { class: 'btn btn-danger', type: 'button', onClick: () => retireRecipe(r) }, 'Retire for good')
        : null)));
}

function renderRecipes() {
  const list = recipesView.all.filter((r) => recipesView.retired || r.state !== 'retired');
  const page = list.slice(0, recipesView.shown);
  render('recipes-list', page.length === 0
    ? el('p', { class: 'note' }, 'No recipes.')
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
  if (!await confirmDialog(`Suspend this recipe on ${domain}? It is tried there again after two canary checks in a row pass.`, { okLabel: 'Suspend' })) return;
  try {
    await api(`/api/admin/recipes/${recipe.id}/suspend`, { method: 'POST', body: { domain } });
    toast('Suspended.', 'ok');
    loadRecipes();
  } catch (e) { toast(e.message, 'bad'); }
}

async function retireRecipe(recipe) {
  if (!await confirmDialog('Retire this recipe for good? It can never be added again, by the model or by an import.', { okLabel: 'Retire', danger: true })) return;
  try {
    await api(`/api/admin/recipes/${recipe.id}/retire`, { method: 'POST' });
    toast('Retired.', 'ok');
    loadRecipes();
  } catch (e) { toast(e.message, 'bad'); }
}

const recipesResult = document.getElementById('recipes-result');

function showRecipes(message, ok) {
  recipesResult.textContent = message;
  recipesResult.className = ok ? 'note ok' : 'note bad';
}

const IMPORT_RESULT = {
  inserted: 'added',
  merged: 'already there (merged)',
  revived: 'brought back',
  refused: 'refused (it was retired or rejected before)',
  over_cap: 'not stored (the store is full)',
};

async function importRecipes(text) {
  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch {
    showRecipes('That is not valid JSON.', false);
    return;
  }
  // A bare array is accepted as well as {"recipes": [...]}.
  const body = Array.isArray(parsed) ? { recipes: parsed } : parsed;
  try {
    const out = await api('/api/admin/recipes/import', { method: 'POST', body });
    const line = (r) => `${r.index}. ${r.scope}: ${IMPORT_RESULT[r.outcome] || r.outcome}${r.detail ? ` (${r.detail})` : ''}` +
      (r.evicted || []).map((e) => `\n    evicted #${e.id} (${e.scope}, ${e.why})`).join('');
    showRecipes(
      'Imported:\n' + out.results.map(line).join('\n'),
      true,
    );
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
    `Repair mode: ${data.repair_mode}. Page stated a length: of the found videos, how many had a length on their page (a video that did not match it was already dropped). Runs are the ladder's own; next choices are MCR's «next choice» downloads, counted apart. ` +
    `Picture matched: of those with an article picture, how many looked like it (distance up to ${data.image_match_max}). ` +
    'A rate is blank when nothing could be measured.'));
  const sites = data.sites || [];
  if (sites.length === 0) {
    render('report-body', el('tr', {}, el('td', { colspan: '15', class: 'empty' }, 'No repair run in the last 30 days.')));
    return;
  }
  const sum = (key) => sites.reduce((n, s) => n + s[key], 0);
  const row = (name, s, strong) => el('tr', {},
    el('td', { class: strong ? 'mono strong' : 'mono' }, name),
    el('td', { class: 'right', title: `${s.shadow_attempts} shadow, ${s.hold_attempts} hold` }, String(s.attempts)),
    el('td', { class: 'right' }, String(s.next_choices)),
    el('td', { class: 'right' }, String(s.found)),
    el('td', { class: 'right' }, String(s.held)),
    el('td', { class: 'right', title: 'Delivered without approval by a trusted recipe' }, String(s.auto_delivered)),
    el('td', { class: 'right' }, String(s.approved)),
    el('td', { class: 'right' }, String(s.rejected_wrong_video)),
    el('td', { class: 'right' }, String(s.rejected_other)),
    el('td', { class: 'right' }, String(s.expired)),
    el('td', { class: 'right' }, pct(s.length_stated, s.found)),
    el('td', { class: 'right' }, pct(s.image_matched, s.image_known)),
    el('td', { class: 'right', title: `${s.tokens_in} tokens in, ${s.tokens_out} out` }, String(s.llm_calls)),
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
  render('report-body', sites.map((s) => row(s.domain, s)), row('All sites', total, true));
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
    showRecipes('Exported the live admin and model recipes. Built-in recipes are not included.', true);
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

function renderTestNormal(n) {
  const out = [];
  out.push(tuBlock('Normal path: kind of link',
    el('div', { class: 'spec-row' }, el('span', {}, 'Kind'), el('span', { class: 'mono' }, n.kind || '—'))));
  const a = n.site_api;
  if (a) {
    out.push(tuBlock('Normal path: site API',
      el('p', {}, el('span', { class: `badge ${a.video ? 'ok' : (a.error ? 'bad' : '')}` },
        a.video ? 'names a video' : (a.error ? 'failed' : 'has nothing for this page')), ` ${fmtMsShort(a.ms)}`),
      a.video ? el('div', { class: 'spec-row' }, el('span', {}, 'Video'), el('span', { class: 'mono' }, a.video)) : null,
      a.error ? el('p', { class: 'note mono' }, a.error) : null));
  }
  const y = n.ytdlp;
  if (y) {
    out.push(tuBlock('Normal path: yt-dlp',
      el('p', {}, el('span', { class: `badge ${y.ok ? 'ok' : 'bad'}` }, y.ok ? 'reads the link' : 'fails'), ` ${fmtMsShort(y.ms)}`),
      y.ok
        ? el('div', { class: 'spec-row' }, el('span', {}, 'Extractor'), el('span', { class: 'mono' }, y.extractor))
        : el('p', { class: 'note mono' }, y.error || ''),
      y.ok ? el('div', { class: 'spec-row' }, el('span', {}, 'Title'), el('span', {}, y.title || '—')) : null,
      y.ok ? el('div', { class: 'spec-row' }, el('span', {}, 'Length'), el('span', {}, fmtSecs(y.duration_secs))) : null,
      y.ok ? el('div', { class: 'spec-row' }, el('span', {}, 'Formats'), el('span', {}, String(y.formats))) : null));
  }
  const s = n.sniff;
  if (s) {
    out.push(tuBlock('Normal path: page sniffer',
      el('p', {}, el('span', { class: `badge ${s.ok ? 'ok' : 'bad'}` }, s.ok ? 'finds a stream' : 'finds no stream'), ` ${fmtMsShort(s.ms)}`),
      s.ok ? el('div', { class: 'spec-row' }, el('span', {}, `Best of ${s.streams}`), el('span', { class: 'mono' }, s.stream)) : null,
      s.error ? el('p', { class: 'note mono' }, s.error) : null));
  }
  return out;
}

function renderTestCandidates(list) {
  if (list.length === 0) return el('p', { class: 'note' }, 'No candidate address reached the gate.');
  return tuTable(['Rank', 'Address', 'Origin', 'Length', 'Height', 'Score', 'Checks'], list.map((c) => el('tr', {},
    el('td', {}, c.verified ? `#${c.rank}` : el('span', { class: 'badge bad' }, 'rejected')),
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
    out.push(tuBlock('Repair ladder: page', el('p', { class: 'note' }, 'No complete look at the page could be made.')));
    return out;
  }
  out.push(tuBlock('Repair ladder: the page',
    el('div', { class: 'spec-row' }, el('span', {}, 'Class'), el('span', { class: 'badge' }, l.class || '—')),
    el('div', { class: 'spec-row' }, el('span', {}, 'Signals'), el('span', { class: 'mono' }, l.signals.length ? l.signals.join(', ') : '—')),
    el('div', { class: 'spec-row' }, el('span', {}, 'Player family'), el('span', { class: 'mono' }, l.family || '—'))));
  out.push(tuBlock('Repair ladder: recipes',
    l.recipes.length === 0
      ? el('p', { class: 'note' }, 'No recipe applies to this site.')
      : tuTable(["Recipe", "Tier", "Origin", 'Outcome', 'Address', 'Gate', 'Time'], l.recipes.map((r) => el('tr', {},
        el('td', { class: 'mono', title: r.hash }, r.name),
        el('td', {}, r.tier === 'generic' ? 'built-in' : r.tier),
        el('td', {}, r.origin),
        el('td', {}, r.outcome),
        el('td', { class: 'mono' }, r.address || '—'),
        el('td', {}, r.gate || '—'),
        el('td', { class: 'num' }, fmtMsShort(r.ms)))))));
  if (l.live) {
    out.push(tuBlock('Repair ladder: live page load',
      el('p', {}, `${l.live.run}, ${fmtMsShort(l.live.ms)}`),
      l.live.actions.length ? el('p', { class: 'note mono' }, l.live.actions.join(', ')) : null));
  }
  if (l.llm) {
    out.push(tuBlock('Repair ladder: the model',
      el('p', {}, l.llm.asked
        ? `${l.llm.calls} call(s), ${l.llm.rounds} round(s), ${l.llm.tokens_in} tokens in, ${l.llm.tokens_out} out`
        : 'The model was not called.'),
      el('p', { class: 'note' }, l.llm.outcome)));
  }
  out.push(tuBlock('Candidates and the gate', renderTestCandidates(l.candidates)));
  return out;
}

function renderTestReport(r) {
  render('test-url-result', [
    el('p', {}, el('strong', {}, r.verdict)),
    el('p', { class: 'note' }, `Repair mode: ${r.repair_mode}. Total ${fmtMsShort(r.total_ms)}. Nothing was downloaded, queued or delivered.`),
    ...renderTestNormal(r.normal),
    ...(r.ladder ? renderTestLadder(r.ladder) : []),
    tuBlock('Timings', tuTable(['Step', 'Time', 'Note'], r.steps.map((s) => el('tr', {},
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
    ? 'Use the model (not set up: Settings, repair model)'
    : `Use the model (${info.llm_left} of ${info.llm_cap} calls left today; a run counts toward the limit)`;
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
    progress.textContent = `Running… ${Math.round((Date.now() - started) / 1000)} s of at most 180 s`;
  }, 1000);
  progress.textContent = 'Running… 0 s of at most 180 s';
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
  if (jar.last_test_ok === null || jar.last_test_ok === undefined) return el('span', { class: 'badge' }, 'not tested');
  return el('span', { class: jar.last_test_ok ? 'badge ok' : 'badge bad', title: jar.last_test_detail || '' },
    jar.last_test_ok ? 'ok' : 'failed');
}

async function testCookies(jar, input, button) {
  const url = input.value.trim() || (jar.sample ? `https://${jar.sample}` : '');
  if (!url) { toast('Enter a public link of the site to test with.', 'bad'); return; }
  button.disabled = true;
  button.textContent = 'Testing…';
  try {
    const out = await api(`/api/admin/cookies/${encodeURIComponent(jar.domain)}/test`, { method: 'POST', body: { url } });
    toast(out.ok ? 'The cookies log in.' : `The test failed: ${out.detail}`, out.ok ? 'ok' : 'bad');
  } catch (e) {
    toast(e.message, 'bad');
  }
  loadCookies();
}

async function deleteCookies(jar) {
  if (!await confirmDialog(`Delete the cookies for ${jar.domain}? Jobs on that site will no longer be logged in.`, { okLabel: 'Delete', danger: true })) return;
  try {
    await api(`/api/admin/cookies/${encodeURIComponent(jar.domain)}/delete`, { method: 'POST' });
    toast('Deleted.', 'ok');
  } catch (e) { toast(e.message, 'bad'); }
  loadCookies();
}

// Sites whose login is captured by `omni-ingest cookies sign-in <site>` (P3.17).
const SIGNIN_SITES = ['youtube.com', 'x.com', 'facebook.com', 'instagram.com'];


function signinStatus(jar) {
  if (!jar) return el('span', { class: 'badge' }, 'no cookies');
  const bits = [`${jar.cookie_count} cookies`, `${jar.updated_by} · ${fmtTime(jar.updated_at)}`,
    jar.expires_min ? `expires ${fmtTime(jar.expires_min)}` : 'session only'];
  return el('span', {}, cookieTestBadge(jar), ' ', el('span', { class: 'note' }, bits.join(' · ')));
}

function renderSignin(data) {
  const jars = data.jars || [];
  const prefix = data.signin_command_prefix;
  const prefixCmd = data.signin_command_prefix_cmd;
  render('cookies-signin',
    el('p', { class: 'note' }, 'Εκτελέστε την εντολή στο γραφείο (desktop) του server, από τον χειριστή, σε παράθυρο εντολών που άνοιξε «Εκτέλεση ως διαχειριστής»· χρησιμοποιεί ξεχωριστό προφίλ browser, όχι το δικό σας.'),
    el('p', { class: 'note' }, 'Run it on the server desktop, by the operator, in a prompt opened "as Administrator". It uses a separate browser profile, not yours.'),
    // P13.9: no terminal needed: Setup puts the same command in the Start menu.
    el('p', { class: 'note ok' }, 'Χωρίς γραμμή εντολών: στον server, Έναρξη → «OmniIngest – Σύνδεση σε ιστότοπο». Ζητά άδεια διαχειριστή, δείχνει λίστα ιστοτόπων και ανοίγει το παράθυρο σύνδεσης. / Without a prompt: on the server, Start → «OmniIngest – Σύνδεση σε ιστότοπο» asks for administrator rights, lists the sites and opens the sign-in window.'),
    ...SIGNIN_SITES.map((domain) => {
      const jar = jars.find((j) => j.domain === domain);
      const command = prefix ? `${prefix} ${domain}` : `& ".\\omni-ingest.exe" cookies sign-in ${domain}`;
      const commandCmd = prefixCmd ? `${prefixCmd} ${domain}` : `omni-ingest.exe cookies sign-in ${domain}`;
      const row = (label, text) => {
        const copy = el('button', { class: 'btn', type: 'button' }, 'Copy');
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
    render('cookies-body', el('tr', {}, el('td', { colspan: '7', class: 'empty' }, 'No cookies uploaded.')));
    return;
  }
  render('cookies-body', jars.map((jar) => {
    const input = el('input', { type: 'text', class: 'mono', placeholder: jar.sample || `https://${jar.domain}/…`, autocomplete: 'off' });
    const testBtn = el('button', { class: 'btn', type: 'button' }, 'Test');
    testBtn.addEventListener('click', () => testCookies(jar, input, testBtn));
    const delBtn = el('button', { class: 'btn btn-danger', type: 'button' }, 'Delete');
    delBtn.addEventListener('click', () => deleteCookies(jar));
    return el('tr', {},
      el('td', { class: 'mono strong' }, jar.domain),
      el('td', { class: 'right' }, String(jar.cookie_count)),
      el('td', {}, jar.expires_min ? fmtTime(jar.expires_min) : 'session only'),
      el('td', {}, `${jar.updated_by} · ${fmtTime(jar.updated_at)}`),
      el('td', {}, cookieTestBadge(jar), ' ', el('span', { class: 'note' }, jar.last_test_at ? fmtTime(jar.last_test_at) : ''),
        jar.last_test_ok === false && jar.last_test_detail ? el('div', { class: 'note' }, jar.last_test_detail) : null),
      el('td', {}, input, jar.sample ? el('div', { class: 'note mono' }, `last: ${jar.sample}`) : null),
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
  document.getElementById('cookies-file-note').textContent = `${file.name} chosen (${Math.ceil(file.size / 1024)} KiB). It is not shown.`;
});
document.getElementById('cookies-refresh').addEventListener('click', loadCookies);
document.getElementById('cookies-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const domain = document.getElementById('cookies-domain').value.trim().toLowerCase();
  if (!cookieFileText) { showCookies('Choose the cookies.txt file first.', false); return; }
  const button = document.getElementById('cookies-upload');
  button.disabled = true;
  try {
    const out = await api('/api/admin/cookies', { method: 'POST', body: { domain, netscape: cookieFileText } });
    showCookies(
      `${out.replaced ? 'Replaced' : 'Stored'} ${out.cookie_count} cookie(s) for ${out.domain}.` +
      (out.skipped_other_site ? `\n${out.skipped_other_site} line(s) of other sites were dropped.` : '') +
      (out.skipped_malformed ? `\n${out.skipped_malformed} line(s) were not cookies and were dropped.` : ''),
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

/* Administration panel behaviour (plan P2.3, P2.5).
 *
 * Audit messages contain operator- and email-derived text (job slugs, URLs,
 * journalist names). They used to be joined into an HTML string and assigned
 * to `innerHTML`; here they are text nodes.
 */

import { api, el, render, toast, fmtTime, logout, live } from '/static/app.js?v=6';

/* ------------------------------------------------------------------ *
 * Dependencies
 * ------------------------------------------------------------------ */

async function loadDependencies() {
  let data;
  try {
    data = await api('/api/admin/dependencies');
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    document.getElementById('ytdl-installed').textContent = 'unavailable';
    document.getElementById('ytdl-remote').textContent = 'unavailable';
    return;
  }

  document.getElementById('ytdl-installed').textContent = data.ytdl_installed || 'unknown';
  document.getElementById('ytdl-remote').textContent = data.ytdl_remote || 'unknown';

  const note = document.getElementById('ytdl-note');
  if (data.ytdl_update_available) {
    note.textContent = 'A newer yt-dlp is available.';
    note.className = 'note ok';
  } else {
    note.textContent = 'yt-dlp is up to date.';
    note.className = 'note';
  }

  // `dependencies` is a list of {name, installed, path, version} -- iterating
  // it as an object gave every row an array index for a name.
  const tools = data.dependencies || [];
  if (tools.length === 0) {
    render('dependencies', el('p', { class: 'note' }, 'No toolchain information.'));
    return;
  }
  render('dependencies', tools.map((tool) => el('div', { class: 'spec-row' },
    el('span', {}, tool.name),
    el('span', { class: tool.installed ? 'badge ok' : 'badge bad' },
      tool.installed ? (tool.version || 'present') : 'missing'),
  )));
}

document.getElementById('ytdl-update').addEventListener('click', async (event) => {
  const button = event.currentTarget;
  button.disabled = true;
  button.textContent = 'Updating…';
  try {
    await api('/api/admin/update-ytdl', { method: 'POST' });
    toast('yt-dlp updated.', 'ok');
    loadDependencies();
  } catch (e) {
    toast(e.message, 'bad');
  } finally {
    button.disabled = false;
    button.textContent = 'Update yt-dlp now';
  }
});

document.getElementById('ytdl-rollback').addEventListener('click', async () => {
  if (!confirm('Put the previous yt-dlp build back?')) return;
  try {
    await api('/api/admin/rollback-ytdl', { method: 'POST' });
    toast('Rolled back to the previous yt-dlp build.', 'ok');
    loadDependencies();
  } catch (e) { toast(e.message, 'bad'); }
});

/* ------------------------------------------------------------------ *
 * Users
 * ------------------------------------------------------------------ */

const ROLE_LABEL = {
  admin: 'Administrator',
  open_mcr: 'MCR operator',
  user: 'Journalist',
};

async function loadUsers() {
  let users = [];
  try {
    const data = await api('/api/admin/users');
    users = data.users || [];
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }

  if (users.length === 0) {
    render('users-body', el('tr', {},
      el('td', { colspan: '6', class: 'empty' }, 'No accounts yet.')));
    return;
  }

  render('users-body', users.map((user) => el('tr', {},
    el('td', { class: 'strong' }, user.email),
    el('td', {}, user.full_name),
    el('td', {}, el('span', { class: user.role === 'admin' ? 'badge bad' : 'badge' },
      ROLE_LABEL[user.role] || user.role)),
    el('td', {}, user.journalist_surname || '—'),
    el('td', {}, el('span', { class: user.is_active ? 'badge ok' : 'badge bad' }, user.is_active ? 'active' : 'deactivated')),
    el('td', { class: 'right' },
      el('div', { class: 'row tight', style: 'justify-content:flex-end' },
        el('button', {
          class: 'btn',
          type: 'button',
          onClick: () => resetPassword(user.id, user.email),
        }, 'Reset password'),
        el('button', {
          class: user.is_active ? 'btn btn-danger' : 'btn',
          type: 'button',
          onClick: () => setActive(user, !user.is_active),
        }, user.is_active ? 'Deactivate' : 'Reactivate'),
      ),
    ),
  )));
}

async function setActive(user, active) {
  if (!active && !confirm(`Deactivate ${user.email}? They are signed out at once and cannot sign in until reactivated.`)) return;
  try {
    await api(`/api/admin/users/${user.id}/active`, { method: 'POST', body: { active } });
    toast(`${user.email} ${active ? 'reactivated' : 'deactivated'}.`, 'ok');
    loadUsers();
  } catch (e) { toast(e.message, 'bad'); }
}

async function resetPassword(id, email) {
  const next = prompt(`New password for ${email} (at least 12 characters):`);
  if (next === null) return;
  try {
    await api(`/api/admin/users/${id}/password`, { method: 'POST', body: { password: next } });
    // The server also ends that account's sessions, so the operator is not
    // left believing a reset took effect while the old session is still live.
    toast(`Password reset for ${email}; their sessions were ended.`, 'ok');
  } catch (e) { toast(e.message, 'bad'); }
}

const userModal = document.getElementById('user-modal');
document.getElementById('add-user-btn').addEventListener('click', () => { userModal.hidden = false; });
document.getElementById('user-modal-cancel').addEventListener('click', () => { userModal.hidden = true; });
userModal.addEventListener('click', (e) => { if (e.target === userModal) userModal.hidden = true; });

document.getElementById('create-user-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const surname = document.getElementById('new-journalist').value.trim();
  try {
    await api('/api/admin/users', {
      method: 'POST',
      body: {
        email: document.getElementById('new-email').value.trim(),
        full_name: document.getElementById('new-name').value.trim(),
        password: document.getElementById('new-password').value,
        role: document.getElementById('new-role').value,
        journalist_surname: surname || null,
      },
    });
    toast('Account created.', 'ok');
    event.target.reset();
    userModal.hidden = true;
    loadUsers();
  } catch (e) { toast(e.message, 'bad'); }
});

/* ------------------------------------------------------------------ *
 * Credentials
 * ------------------------------------------------------------------ */

const SECRET_LABEL = {
  'graph.client_secret': 'Microsoft Graph client secret',
  'llm.api_key': 'LLM provider API key',
  'teams.webhook_url': 'Teams webhook URL',
  'web.tls_password': 'TLS certificate passphrase',
};

async function loadSecrets() {
  let status = {};
  try {
    const data = await api('/api/secrets');
    status = data.secrets || {};
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }

  // The API reports only whether each secret is set. There is no route that
  // returns a value, so nothing here can display one.
  render('secrets-list', Object.entries(status).map(([key, isSet]) => {
    const input = el('input', {
      type: 'password',
      autocomplete: 'off',
      placeholder: isSet ? '•••••••• (set)' : 'not set',
    });
    return el('div', { class: 'stack' },
      el('div', { class: 'row', style: 'justify-content:space-between' },
        el('label', { style: 'margin:0' }, SECRET_LABEL[key] || key),
        el('span', { class: isSet ? 'badge ok' : 'badge' }, isSet ? 'set' : 'not set'),
      ),
      el('div', { class: 'row' },
        el('div', { class: 'grow' }, input),
        el('button', {
          class: 'btn',
          type: 'button',
          onClick: () => saveSecret(key, input),
        }, 'Save'),
        isSet && el('button', {
          class: 'btn btn-danger',
          type: 'button',
          onClick: () => clearSecret(key),
        }, 'Clear'),
      ),
    );
  }));
}

async function saveSecret(key, input) {
  const value = input.value;
  if (!value) { toast('Enter a value first.', 'bad'); return; }
  try {
    await api('/api/secrets', { method: 'POST', body: { key, value } });
    input.value = '';
    toast(`${SECRET_LABEL[key] || key} updated.`, 'ok');
    loadSecrets();
  } catch (e) { toast(e.message, 'bad'); }
}

async function clearSecret(key) {
  if (!confirm(`Clear ${SECRET_LABEL[key] || key}? It cannot be recovered.`)) return;
  try {
    await api('/api/secrets', { method: 'POST', body: { key, value: '' } });
    toast('Cleared.', 'ok');
    loadSecrets();
  } catch (e) { toast(e.message, 'bad'); }
}

/* ------------------------------------------------------------------ *
 * Maintenance
 * ------------------------------------------------------------------ */

document.getElementById('purge-btn').addEventListener('click', async () => {
  const days = Number(document.getElementById('purge-days').value);
  if (!confirm(`Permanently remove completed jobs older than ${days} days?`)) return;
  try {
    const result = await api('/api/admin/purge', {
      method: 'POST',
      body: { older_than_days: days },
    });
    toast(`Purged ${result.purged_count} job(s).`, 'ok');
    loadLogs();
  } catch (e) { toast(e.message, 'bad'); }
});

document.getElementById('vacuum-btn').addEventListener('click', async () => {
  try {
    await api('/api/admin/vacuum', { method: 'POST' });
    toast('Database compacted.', 'ok');
  } catch (e) { toast(e.message, 'bad'); }
});

/* ------------------------------------------------------------------ *
 * Scheduled maintenance
 * ------------------------------------------------------------------ */

const OUTCOME_CLASS = { ok: 'ok', skipped: '', failed: 'bad' };

async function loadMaintenance() {
  let tasks = [];
  try {
    const data = await api('/api/admin/maintenance');
    tasks = data.tasks || [];
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }

  if (tasks.length === 0) {
    render('maintenance-body', el('tr', {},
      el('td', { colspan: '5', class: 'empty' }, 'No maintenance tasks registered.')));
    return;
  }

  render('maintenance-body', tasks.map((task) => el('tr', {},
    el('td', {},
      el('div', { class: 'strong' }, task.name),
      el('div', { class: 'note' }, task.description || ''),
    ),
    el('td', { class: 'num' }, fmtTime(task.next_run)),
    el('td', { class: 'num' }, fmtTime(task.last_run)),
    el('td', {},
      task.last_outcome
        ? el('span', {
            class: `badge ${OUTCOME_CLASS[task.last_outcome] ?? ''}`,
            // The failure reason belongs on hover: the column stays readable
            // and the detail is one gesture away.
            title: task.last_error || '',
          }, task.last_outcome)
        : el('span', { class: 'note' }, 'never run'),
    ),
    el('td', { class: 'right' },
      el('button', {
        class: 'btn',
        type: 'button',
        onClick: () => runTask(task.name),
      }, 'Run now'),
    ),
  )));
}

async function runTask(name) {
  try {
    const result = await api(`/api/admin/maintenance/${encodeURIComponent(name)}/run`, {
      method: 'POST',
    });
    toast(result.message || 'Scheduled.', 'ok');
    // The scheduler picks it up on its own tick, so re-read shortly after
    // rather than pretending it has already run.
    setTimeout(loadMaintenance, 2000);
  } catch (e) { toast(e.message, 'bad'); }
}

document.getElementById('maintenance-refresh').addEventListener('click', loadMaintenance);

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
  if (!confirm(`Remove "${link.label}" from the self-check?`)) return;
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
  if (!confirm(`Suspend this recipe on ${domain}? It is tried there again after two canary checks in a row pass.`)) return;
  try {
    await api(`/api/admin/recipes/${recipe.id}/suspend`, { method: 'POST', body: { domain } });
    toast('Suspended.', 'ok');
    loadRecipes();
  } catch (e) { toast(e.message, 'bad'); }
}

async function retireRecipe(recipe) {
  if (!confirm('Retire this recipe for good? It can never be added again, by the model or by an import.')) return;
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
    render('report-body', el('tr', {}, el('td', { colspan: '14', class: 'empty' }, 'No repair run in the last 30 days.')));
    return;
  }
  const sum = (key) => sites.reduce((n, s) => n + s[key], 0);
  const row = (name, s, strong) => el('tr', {},
    el('td', { class: strong ? 'mono strong' : 'mono' }, name),
    el('td', { class: 'right', title: `${s.shadow_attempts} shadow, ${s.hold_attempts} hold` }, String(s.attempts)),
    el('td', { class: 'right' }, String(s.next_choices)),
    el('td', { class: 'right' }, String(s.found)),
    el('td', { class: 'right' }, String(s.held)),
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
    found: sum('found'), held: sum('held'), approved: sum('approved'),
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
  if (!confirm(`Delete the cookies for ${jar.domain}? Jobs on that site will no longer be logged in.`)) return;
  try {
    await api(`/api/admin/cookies/${encodeURIComponent(jar.domain)}/delete`, { method: 'POST' });
    toast('Deleted.', 'ok');
  } catch (e) { toast(e.message, 'bad'); }
  loadCookies();
}

// Sites whose login is captured by `omni-ingest cookies sign-in <site>` (P3.17).
const SIGNIN_SITES = ['youtube.com', 'x.com', 'facebook.com', 'instagram.com'];

function copyText(text, button) {
  const done = () => { button.textContent = 'Copied'; setTimeout(() => { button.textContent = 'Copy'; }, 1500); };
  if (navigator.clipboard && window.isSecureContext) {
    navigator.clipboard.writeText(text).then(done, () => toast('Could not copy; select the command and copy it.', 'bad'));
    return;
  }
  // The panel is usually plain HTTP on the LAN, where the clipboard API is not offered.
  const box = el('textarea', { readonly: '', style: 'position:fixed;left:-9999px' }, text);
  document.body.appendChild(box);
  box.select();
  try { document.execCommand('copy'); done(); } catch (e) { toast('Could not copy; select the command and copy it.', 'bad'); }
  box.remove();
}

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


/* ------------------------------------------------------------------ *
 * Audit log
 * ------------------------------------------------------------------ */

const LEVEL_CLASS = { ERROR: 'bad', WARN: 'warn', INFO: '', DEBUG: '' };

async function loadLogs() {
  let logs = [];
  try {
    const data = await api('/api/system/logs');
    logs = data.logs || [];
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }

  if (logs.length === 0) {
    render('logs-body', el('tr', {},
      el('td', { colspan: '4', class: 'empty' }, 'Nothing logged yet.')));
    return;
  }

  render('logs-body', logs.map((entry) => el('tr', {},
    el('td', { class: 'num' }, fmtTime(entry.timestamp)),
    el('td', {}, el('span', { class: `badge ${LEVEL_CLASS[entry.level] ?? ''}` }, entry.level)),
    el('td', {}, entry.category),
    // Audit messages quote job slugs and URLs that arrived by email.
    el('td', { class: 'mono' }, entry.message),
  )));
}

document.getElementById('logs-refresh').addEventListener('click', loadLogs);

/* ------------------------------------------------------------------ *
 * Connection tests
 * ------------------------------------------------------------------ */

const testResult = document.getElementById('test-result');

function showTest(message, ok) {
  testResult.textContent = message;
  testResult.className = ok ? 'note ok' : 'note bad';
}

/* ------------------------------------------------------------------ *
 * Recent mail and reprocess (plan P4.25)
 * ------------------------------------------------------------------ */

async function loadMail() {
  let mails;
  try {
    mails = (await api('/api/admin/mail')).mails || [];
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }
  render('mail-body', mails.length === 0
    ? el('tr', {}, el('td', { colspan: '6', class: 'empty' }, 'No mail handled yet.'))
    : mails.map((m) => el('tr', {},
      el('td', { class: 'num' }, fmtTime(m.processed_at)),
      el('td', {}, m.subject || '(no subject)'),
      el('td', { class: 'mono' }, m.from_address || ''),
      el('td', {}, el('span', { class: m.outcome === 'JOBS' ? 'badge ok' : (m.outcome === 'FAILED' ? 'badge bad' : 'badge warn') }, m.outcome)),
      el('td', { class: 'num' }, String(m.jobs)),
      el('td', { class: 'right' }, m.reprocess_pending
        ? el('span', { class: 'badge info' }, 'next poll')
        : el('button', { class: 'btn', type: 'button', onClick: () => reprocessMail(m) }, 'Reprocess')),
    )));
}

async function reprocessMail(m) {
  try {
    await api('/api/admin/mail/reprocess', { method: 'POST', body: { internet_message_id: m.internet_message_id } });
    toast(`'${m.subject || 'mail'}' will be read again on the next poll.`, 'ok');
    loadMail();
  } catch (e) { toast(e.message, 'bad'); }
}

document.getElementById('mail-refresh').addEventListener('click', loadMail);

/* ------------------------------------------------------------------ *
 * LLM assist (plan P4.22)
 * ------------------------------------------------------------------ */

// Presets only fill the base URL and how the key is sent; every field stays
// editable, and "Custom" is any other OpenAI-compatible server.
const LLM_PRESETS = [
  ['lmstudio', 'LM Studio (this machine)', 'http://127.0.0.1:1234/v1', 'none'],
  ['ollama', 'Ollama (this machine)', 'http://127.0.0.1:11434/v1', 'none'],
  ['geniex', 'Qualcomm GenieX (Snapdragon NPU)', 'http://127.0.0.1:18181/v1', 'none'],
  ['llamacpp', 'llama.cpp / vLLM server', 'http://127.0.0.1:8000/v1', 'none'],
  ['openai', 'OpenAI', 'https://api.openai.com/v1', 'bearer'],
  ['azure', 'Azure OpenAI', 'https://YOUR-RESOURCE.openai.azure.com/openai/v1', 'api_key_header'],
  ['gemini', 'Google Gemini', 'https://generativelanguage.googleapis.com/v1beta/openai', 'bearer'],
  ['anthropic', 'Anthropic Claude', 'https://api.anthropic.com/v1', 'bearer'],
  ['openrouter', 'OpenRouter', 'https://openrouter.ai/api/v1', 'bearer'],
  ['mistral', 'Mistral', 'https://api.mistral.ai/v1', 'bearer'],
  ['groq', 'Groq', 'https://api.groq.com/openai/v1', 'bearer'],
  ['custom', 'Custom (any OpenAI-compatible)', '', ''],
];

const llm = (id) => document.getElementById(id);
const llmResult = llm('llm-result');

function showLlm(message, ok) {
  llmResult.textContent = message;
  llmResult.className = ok ? 'note ok' : 'note bad';
}

render(llm('llm-provider'), LLM_PRESETS.map(([id, label]) => el('option', { value: id }, label)));

/* Rough client-side check, to show the warning while typing; the server
 * decides with the same rule when it builds the prompt. */
function looksLocal(url) {
  try {
    const host = new URL(url).hostname.replace(/^\[|\]$/g, '').toLowerCase();
    return host === 'localhost' || host.endsWith('.localhost') || host.endsWith('.local')
      || host.endsWith('.lan') || host.endsWith('.internal') || !host.includes('.')
      || /^127\./.test(host) || /^10\./.test(host) || /^192\.168\./.test(host)
      || /^172\.(1[6-9]|2\d|3[01])\./.test(host) || /^169\.254\./.test(host) || host === '::1';
  } catch { return true; }
}

function syncOnline() {
  llm('llm-online').hidden = looksLocal(llm('llm-base').value.trim());
}

llm('llm-base').addEventListener('input', syncOnline);
llm('llm-provider').addEventListener('change', () => {
  const preset = LLM_PRESETS.find(([id]) => id === llm('llm-provider').value);
  if (preset && preset[2]) {
    llm('llm-base').value = preset[2];
    llm('llm-auth').value = preset[3];
  }
  syncOnline();
});

function llmPayload() {
  return {
    mode: llm('llm-mode').value,
    provider: llm('llm-provider').value,
    base_url: llm('llm-base').value.trim(),
    model: llm('llm-model').value.trim(),
    auth: llm('llm-auth').value,
    api_key: llm('llm-key').value,
    clear_key: llm('llm-clear-key').checked,
    timeout_secs: Number(llm('llm-timeout').value) || 30,
    max_tokens: Number(llm('llm-tokens').value) || 400,
    disable_thinking: llm('llm-no-thinking').checked,
    keyword_polish: llm('llm-polish').checked,
  };
}

async function loadLlm() {
  let s;
  try {
    s = await api('/api/admin/llm');
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }
  llm('llm-mode').value = s.mode === 'off' ? 'off' : 'assist';
  llm('llm-provider').value = LLM_PRESETS.some(([id]) => id === s.provider) ? s.provider : 'custom';
  llm('llm-base').value = s.base_url || '';
  llm('llm-model').value = s.model || '';
  llm('llm-auth').value = s.auth || 'none';
  llm('llm-key').value = '';
  llm('llm-key').placeholder = s.key_set ? '•••••••• (set; blank keeps it)' : 'not set';
  llm('llm-clear-key').checked = false;
  llm('llm-timeout').value = s.timeout_secs;
  llm('llm-tokens').value = s.max_tokens;
  llm('llm-no-thinking').checked = !!s.disable_thinking;
  llm('llm-polish').checked = !!s.keyword_polish;
  llm('llm-in-use').textContent = `In use: ${s.mode === 'off' ? 'off' : s.in_use}`;
  syncOnline();
}

async function llmCall(button, path, onOk) {
  button.disabled = true;
  const label = button.textContent;
  button.textContent = 'Working…';
  showLlm('Waiting for the server…', true);
  try {
    onOk(await api(path, { method: 'POST', body: llmPayload() }));
  } catch (e) {
    showLlm(e.message, false);
  } finally {
    button.disabled = false;
    button.textContent = label;
    // A key typed for a test is never left in the field.
    llm('llm-key').value = '';
  }
}

llm('llm-load-models').addEventListener('click', (event) => llmCall(event.currentTarget, '/api/admin/llm/models', (r) => {
  const models = r.models || [];
  render('llm-models', models.map((m) => el('option', { value: m })));
  showLlm(models.length ? `${models.length} model(s): ${models.slice(0, 12).join(', ')}${models.length > 12 ? ', …' : ''}` : 'The server lists no models; type the model id.', models.length > 0);
}));

llm('llm-test').addEventListener('click', (event) => llmCall(event.currentTarget, '/api/admin/llm/test', (r) => {
  const slow = r.seconds > Number(llm('llm-timeout').value) * 0.6;
  showLlm(`Answered in ${r.seconds} s: ${JSON.stringify(r.answer)}` +
    (slow ? '\nThat is close to the timeout; a real mail prompt is longer. Raise the timeout or use a faster machine.' : ''), true);
}));

llm('llm-save').addEventListener('click', (event) => llmCall(event.currentTarget, '/api/admin/llm', (r) => {
  showLlm(`Saved. The next mail uses ${r.in_use}.`, true);
  loadLlm();
  loadSecrets();
}));

// The mailbox test starts from the saved settings; edit a field to try
// another value before saving it on /setup.
api('/api/setup/state')
  .then((state) => {
    if (!state.graph) return;
    document.getElementById('test-mail-tenant').value = state.graph.tenant_id || '';
    document.getElementById('test-mail-client').value = state.graph.client_id || '';
    document.getElementById('test-mail-user').value = state.graph.mailbox || '';
  })
  .catch(() => { /* the fields stay blank; the server falls back to the saved values */ });

document.getElementById('test-mail-btn').addEventListener('click', async () => {
  showTest('Testing…', true);
  try {
    const res = await api('/api/system/test-email', {
      method: 'POST',
      body: {
        tenant_id: document.getElementById('test-mail-tenant').value.trim(),
        client_id: document.getElementById('test-mail-client').value.trim(),
        mailbox: document.getElementById('test-mail-user').value.trim(),
        client_secret: document.getElementById('test-mail-secret').value,
      },
    });
    showTest(res.detail || 'Mailbox readable.', true);
  } catch (e) { showTest(e.message, false); }
  // The secret is never left in the field after a test.
  document.getElementById('test-mail-secret').value = '';
});

/* ------------------------------------------------------------------ *
 * Newsroom taxonomy (plan P4.20)
 * ------------------------------------------------------------------ */

const taxonomyResult = document.getElementById('taxonomy-result');
let knownGroups = [];

function showTaxonomy(message, ok) {
  taxonomyResult.textContent = message;
  taxonomyResult.className = ok ? 'note ok' : 'note bad';
}

const splitList = (text) => text.split(',').map((s) => s.trim()).filter(Boolean);

async function loadTaxonomy() {
  let groups;
  let people;
  try {
    groups = (await api('/api/groups')).groups || [];
    people = (await api('/api/journalists')).journalists || [];
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }
  knownGroups = groups;

  render('groups-body', groups.length === 0
    ? el('tr', {}, el('td', { colspan: '6', class: 'empty' }, 'No groups yet. Add one below or import taxonomy.json.'))
    : groups.map((g) => el('tr', {},
      el('td', { class: 'mono strong' }, g.code),
      el('td', {}, g.name),
      el('td', {}, el('span', { class: 'badge info' }, g.kind)),
      el('td', {}, (g.keywords || []).join(', ')),
      el('td', {}, g.description || ''),
      el('td', { class: 'right' },
        el('div', { class: 'row tight', style: 'justify-content:flex-end' },
          el('button', { class: 'btn', type: 'button', onClick: () => editGroup(g) }, 'Edit'),
          el('button', { class: 'btn btn-danger', type: 'button', onClick: () => deleteGroup(g.code) }, 'Delete'),
        )),
    )));

  const staff = people.filter((p) => p.surname !== 'MCR');
  const known = new Set(groups.map((g) => g.code));
  render('people-body', staff.length === 0
    ? el('tr', {}, el('td', { colspan: '7', class: 'empty' }, 'Nobody yet. Add people below or import taxonomy.json.'))
    : staff.map((p) => el('tr', {},
      el('td', { class: 'mono strong' }, p.surname),
      el('td', {}, p.full_name),
      el('td', { class: 'mono' }, (p.emails || []).join(', ') || '—'),
      el('td', {}, (p.aliases || []).join(', ') || '—'),
      el('td', {}, (p.groups || []).length
        ? (p.groups || []).map((code, i) => el('span', {
          class: known.has(code) ? (i === 0 ? 'badge info' : 'badge') : 'badge bad',
          style: 'margin-right:4px',
          title: i === 0 ? 'default group' : '',
        }, code))
        : '—'),
      el('td', { class: 'num' }, String(p.default_priority || 0)),
      el('td', { class: 'right' },
        el('div', { class: 'row tight', style: 'justify-content:flex-end' },
          el('button', { class: 'btn', type: 'button', onClick: () => editPerson(p) }, 'Edit'),
          el('button', { class: 'btn btn-danger', type: 'button', onClick: () => deletePerson(p.surname) }, 'Delete'),
        )),
    )));
  loadBackups();
}

const pf = (id) => document.getElementById(id);

function editPerson(p) {
  pf('pf-surname').value = p.surname;
  pf('pf-name').value = p.full_name;
  pf('pf-emails').value = (p.emails || []).join(', ');
  pf('pf-aliases').value = (p.aliases || []).join(', ');
  pf('pf-groups').value = (p.groups || []).join(', ');
  pf('pf-priority').value = p.default_priority || 0;
  pf('pf-name').focus();
}

pf('pf-clear').addEventListener('click', () => pf('person-form').reset());

pf('person-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const surname = pf('pf-surname').value.trim().toUpperCase();
  try {
    await api('/api/journalists', {
      method: 'POST',
      body: {
        surname,
        full_name: pf('pf-name').value.trim(),
        emails: splitList(pf('pf-emails').value).map((e) => e.toLowerCase()),
        aliases: splitList(pf('pf-aliases').value),
        priority: Number(pf('pf-priority').value) || 0,
      },
    });
    await api(`/api/journalists/${encodeURIComponent(surname)}/groups`, {
      method: 'POST',
      body: { groups: splitList(pf('pf-groups').value).map((c) => c.toUpperCase()) },
    });
    toast(`${surname} saved.`, 'ok');
    pf('person-form').reset();
    loadTaxonomy();
  } catch (e) { toast(e.message, 'bad'); }
});

async function deletePerson(surname) {
  if (!confirm(`Delete ${surname}? Their jobs keep their name; a backup is taken first.`)) return;
  try {
    await api(`/api/journalists/${encodeURIComponent(surname)}`, { method: 'POST' });
    toast(`${surname} deleted.`, 'ok');
    loadTaxonomy();
  } catch (e) { toast(e.message, 'bad'); }
}

async function loadBackups() {
  let backups;
  try {
    backups = (await api('/api/admin/taxonomy/backups')).backups || [];
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    return;
  }
  render('backups-body', backups.length === 0
    ? el('tr', {}, el('td', { colspan: '4', class: 'empty' }, 'No backups yet.'))
    : backups.map((b) => el('tr', {},
      el('td', { class: 'mono' }, b.name),
      el('td', { class: 'num' }, String(b.groups)),
      el('td', { class: 'num' }, String(b.people)),
      el('td', { class: 'right' },
        el('div', { class: 'row tight', style: 'justify-content:flex-end' },
          el('button', { class: 'btn', type: 'button', onClick: () => downloadBackup(b.name) }, 'Download'),
          el('button', { class: 'btn btn-danger', type: 'button', onClick: () => restoreBackup(b.name) }, 'Restore'),
        )),
    )));
}

function saveJson(filename, data) {
  const blob = new Blob([JSON.stringify(data, null, 2) + '\n'], { type: 'application/json' });
  // Our own JSON as a local blob: a plain anchor, since el() only allows
  // http(s) links.
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = filename;
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(a.href), 1000);
}

async function downloadBackup(name) {
  try {
    saveJson(name, await api(`/api/admin/taxonomy/backups/${encodeURIComponent(name)}`));
  } catch (e) { toast(e.message, 'bad'); }
}

async function restoreBackup(name) {
  if (!confirm(`Restore ${name}? Groups and people are set back exactly as in that backup; what is there now is backed up first.`)) return;
  try {
    const { report } = await api(`/api/admin/taxonomy/backups/${encodeURIComponent(name)}/restore`, { method: 'POST' });
    showTaxonomy(`Restored ${name}: ${report.groups_saved} groups and ${report.people_saved} people; ${report.groups_removed} groups and ${report.people_removed} people removed.`, true);
    loadTaxonomy();
  } catch (e) { showTaxonomy(e.message, false); }
}

document.getElementById('taxonomy-backup-now').addEventListener('click', async () => {
  try {
    const { backup } = await api('/api/admin/taxonomy/backups', { method: 'POST' });
    toast(`Backed up: ${backup.name}`, 'ok');
    loadBackups();
  } catch (e) { toast(e.message, 'bad'); }
});

function editGroup(g) {
  document.getElementById('g-code').value = g.code;
  document.getElementById('g-name').value = g.name;
  document.getElementById('g-kind').value = g.kind;
  document.getElementById('g-keywords').value = (g.keywords || []).join(', ');
  document.getElementById('g-description').value = g.description || '';
  document.getElementById('g-name').focus();
}

document.getElementById('group-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  try {
    await api('/api/groups', {
      method: 'POST',
      body: {
        code: document.getElementById('g-code').value.trim(),
        name: document.getElementById('g-name').value.trim(),
        kind: document.getElementById('g-kind').value,
        keywords: splitList(document.getElementById('g-keywords').value),
        description: document.getElementById('g-description').value.trim(),
      },
    });
    toast('Group saved.', 'ok');
    event.target.reset();
    loadTaxonomy();
  } catch (e) { toast(e.message, 'bad'); }
});

async function deleteGroup(code) {
  if (!confirm(`Delete group ${code}? People lose this membership; jobs keep their label.`)) return;
  try {
    await api(`/api/groups/${encodeURIComponent(code)}`, { method: 'POST' });
    toast('Group deleted.', 'ok');
    loadTaxonomy();
  } catch (e) { toast(e.message, 'bad'); }
}


document.getElementById('taxonomy-export').addEventListener('click', async () => {
  try {
    saveJson('taxonomy.json', await api('/api/admin/taxonomy'));
    showTaxonomy('Exported. The file lists staff and their addresses: keep it out of shared folders and git.', true);
  } catch (e) { showTaxonomy(e.message, false); }
});

const fileInput = document.getElementById('taxonomy-file');
document.getElementById('taxonomy-import-btn').addEventListener('click', () => fileInput.click());
fileInput.addEventListener('change', async () => {
  const file = fileInput.files && fileInput.files[0];
  fileInput.value = '';
  if (!file) return;
  let taxonomy;
  try {
    taxonomy = JSON.parse(await file.text());
  } catch {
    showTaxonomy(`${file.name} is not valid JSON.`, false);
    return;
  }
  const replace = document.getElementById('taxonomy-replace').checked;
  if (replace && !confirm('Replace: groups and people not in the file will be removed. Continue?')) return;
  try {
    const { report } = await api('/api/admin/taxonomy', { method: 'POST', body: { taxonomy, replace } });
    showTaxonomy(
      `Imported ${file.name}: ${report.groups_saved} groups and ${report.people_saved} people saved` +
      (replace ? `, ${report.groups_removed} groups and ${report.people_removed} people removed.` : '.'),
      true,
    );
    loadTaxonomy();
  } catch (e) { showTaxonomy(e.message, false); }
});

/* ------------------------------------------------------------------ *
 * Benchmarks (plan P6.4): the stored 65-link corpus, run on demand
 * ------------------------------------------------------------------ */

// The run whose report is open; the newest one until the admin picks another.
let benchSelected = null;
let benchBusy = false;
const benchResult = document.getElementById('bench-result');

function showBench(message, ok) {
  benchResult.textContent = message;
  benchResult.className = ok ? 'note ok' : 'note bad';
}

const benchSecs = (ms) => (ms === null || ms === undefined ? '' : (ms / 1000).toFixed(1));
const benchPct = (v) => (v === null || v === undefined ? '' : `${v.toFixed(1)} %`);
const benchYes = (v) => (v === null || v === undefined ? '' : v ? 'yes' : 'NO');

function benchVerdict(pass) {
  if (pass === true) return el('span', { class: 'badge ok' }, 'PASS');
  if (pass === false) return el('span', { class: 'badge bad' }, 'FAIL');
  return el('span', { class: 'badge' }, 'n/a');
}

function benchList(indices) {
  return indices && indices.length ? indices.map((i) => `#${i}`).join(', ') : 'none';
}

function benchTable(head, rows) {
  return el('div', { class: 'card flush' },
    el('table', {},
      el('thead', {}, el('tr', {}, head.map((h) => el('th', {}, h)))),
      el('tbody', {}, rows.map((cells) => el('tr', {}, cells.map((c) => el('td', {}, c)))))));
}

function renderBenchReport(report) {
  const { run, summary, deltas, targets } = report;
  const parts = [
    el('h3', { class: 'card-title', style: 'font-size:0.95rem' },
      `Run ${run.id} (${run.mode === 'full' ? 'full' : 'extraction only'}) `, benchVerdict(summary.pass)),
    el('p', { class: 'note' },
      `${run.state}. Started ${fmtTime(run.started_at)}, finished ${fmtTime(run.finished_at)}. ` +
      `${summary.targets_run} targets run, ${summary.counted} counted (the rest are expected to fail).`),
    benchTable(['Threshold', 'Limit', 'Measured', 'Result'], summary.checks.map((c) => [
      c.name,
      `${c.op} ${c.limit}${c.unit ? ` ${c.unit}` : ''}`,
      c.value === null || c.value === undefined ? '' : `${c.value.toFixed(1)}${c.unit ? ` ${c.unit}` : ''}`,
      benchVerdict(c.pass),
    ])),
  ];
  const signed = (v, unit) => (v === null || v === undefined ? 'n/a' : `${v > 0 ? '+' : ''}${v.toFixed(1)}${unit}`);
  if (deltas) {
    parts.push(el('p', { class: 'note' },
      `Against run ${deltas.previous_run_id} (the last finished ${run.mode} run): ` +
      `extraction ${signed(deltas.extract_pct, ' points')}, delivery ${signed(deltas.delivery_pct, ' points')}, ` +
      `mean extraction ${signed(deltas.mean_extract_ms === null ? null : deltas.mean_extract_ms / 1000, ' s')}, ` +
      `compliance failures ${deltas.compliance_fails === null ? 'n/a' : (deltas.compliance_fails > 0 ? '+' : '') + deltas.compliance_fails}. ` +
      `Newly failing: ${benchList(deltas.regressions)}. Newly working: ${benchList(deltas.fixes)}.`));
  } else {
    parts.push(el('p', { class: 'note' }, `No earlier finished ${run.mode} run to compare with.`));
  }
  const t = summary.timings;
  parts.push(
    el('h4', { class: 'note strong' }, 'Mean seconds per stage'),
    benchTable(['Extract', 'Download', 'Transcode + rewrap', 'Per target'], [[
      benchSecs(t.extract_mean_ms), benchSecs(t.download_mean_ms), benchSecs(t.transcode_mean_ms), benchSecs(t.total_mean_ms),
    ]]),
    el('h4', { class: 'note strong' }, 'By domain'),
    benchTable(['Domain', 'Targets', 'Extracted', 'Delivery-equivalent', 'Mean extract (s)'], summary.by_domain.map((d) => [
      d.domain, String(d.targets), String(d.extract_ok), String(d.delivery_ok), benchSecs(d.mean_extract_ms),
    ])),
    summary.skipped && summary.skipped.length
      ? el('p', { class: 'note' }, `Skipped, the source is over ten minutes (not counted in the delivery rate): ${benchList(summary.skipped)}.`)
      : null,
    el('h4', { class: 'note strong' }, 'By target'),
    benchTable(
      ['#', 'Domain', 'Category', 'Expected', 'Extract', 's', 'Method', 'Streams', 'Download', 's', 'Transcode s', 'Verify', 'Total s', 'Error', 'Stream host'],
      targets.map((l) => [
        String(l.idx), l.domain, l.category, l.expected === 'expected_fail' ? 'expected fail' : 'pass',
        benchYes(l.extract_ok), benchSecs(l.extract_ms), l.method || '', l.candidates === null || l.candidates === undefined ? '' : String(l.candidates),
        benchYes(l.download_ok), benchSecs(l.download_ms), benchSecs(l.transcode_ms), benchYes(l.verify_ok), benchSecs(l.total_ms),
        l.skipped ? 'SOURCE_TOO_LONG (skipped)' : (l.error_code || ''), l.stream_host || '',
      ])),
  );
  render('bench-report', parts);
}

async function loadBenchReport(id) {
  try {
    renderBenchReport(await api(`/api/admin/benchmarks/runs/${id}`));
    return true;
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') showBench(e.message, false);
    return false;
  }
}

async function loadBenchmarks() {
  if (benchBusy) return;
  benchBusy = true;
  try {
    let data;
    try {
      data = await api('/api/admin/benchmarks');
    } catch (e) {
      if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
      return;
    }
    document.getElementById('bench-start').disabled = !data.can_start;
    document.getElementById('bench-parallel').max = String(data.max_parallel);
    const runs = data.runs || [];
    if (runs.length === 0) {
      render('bench-runs', el('tr', {}, el('td', { colspan: '9', class: 'empty' }, 'No run yet.')));
      render('bench-report', []);
      render('bench-progress', []);
      return;
    }
    render('bench-runs', runs.map((r) => {
      const open = el('button', { class: 'btn', type: 'button' }, 'Report');
      open.addEventListener('click', () => { benchSelected = r.id; loadBenchmarks(); });
      return el('tr', {},
        el('td', { class: 'strong' }, String(r.id)),
        el('td', {}, r.mode === 'full' ? 'full' : 'extraction'),
        el('td', {}, el('span', { class: r.state === 'done' ? 'badge ok' : r.state === 'running' ? 'badge' : 'badge bad' }, r.state === 'running' && r.paused ? 'paused for on-air jobs' : r.state)),
        el('td', {}, fmtTime(r.started_at)),
        el('td', { class: 'right' }, r.targets_run === null || r.targets_run === undefined ? '' : String(r.targets_run)),
        el('td', { class: 'right' }, benchPct(r.extract_pct)),
        el('td', { class: 'right' }, benchPct(r.delivery_pct)),
        el('td', {}, r.state === 'done' ? benchVerdict(r.pass) : ''),
        el('td', {}, open));
    }));
    // Progress of the run that is going: results are stored as each target finishes.
    const going = runs.find((r) => r.state === 'running');
    if (going) {
      const of = going.params && going.params.targets ? going.params.targets.length : 0;
      try {
        const current = await api(`/api/admin/benchmarks/runs/${going.id}`);
        render('bench-progress', el('span', {}, `Run ${going.id} is going: ${current.summary.targets_run} of ${of} targets done.${going.paused ? ' Paused for on-air jobs; it goes on when none is running.' : ''}`));
      } catch { /* the next event tries again */ }
    } else {
      render('bench-progress', []);
    }
    if (benchSelected === null || !runs.some((r) => r.id === benchSelected)) benchSelected = runs[0].id;
    await loadBenchReport(benchSelected);
  } finally {
    benchBusy = false;
  }
}

document.getElementById('bench-refresh').addEventListener('click', loadBenchmarks);
document.getElementById('bench-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const button = document.getElementById('bench-start');
  button.disabled = true;
  try {
    const out = await api('/api/admin/benchmarks/run', {
      method: 'POST',
      body: {
        mode: document.getElementById('bench-mode').value,
        targets: document.getElementById('bench-targets').value.trim(),
        parallel: Number(document.getElementById('bench-parallel').value) || 2,
        force: document.getElementById('bench-force').checked,
      },
    });
    benchSelected = out.run_id;
    showBench(`Run ${out.run_id} started: ${out.targets} targets, ${out.parallel} at once.`, true);
  } catch (e) {
    // 409 carries the reason (a job is running, or a run is going).
    showBench(e.message, false);
  } finally {
    button.disabled = false;
  }
  loadBenchmarks();
});

document.getElementById('logout-btn').addEventListener('click', logout);

loadDependencies();
loadSecrets();
loadMaintenance();
loadSelfcheck();
loadSites();
loadRecipes();
loadReport();
loadCookies();
// The card refetches on every event of the server (a target finished), and every 20 s regardless.
live(loadBenchmarks, 20000);
loadTestUrl();
loadUsers();
loadTaxonomy();
loadMail();
loadLlm();
loadLogs();
setInterval(loadLogs, 15000);

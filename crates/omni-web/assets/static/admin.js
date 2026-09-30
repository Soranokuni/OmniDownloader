/* Administration panel behaviour (plan P2.3, P2.5).
 *
 * Audit messages contain operator- and email-derived text (job slugs, URLs,
 * journalist names). They used to be joined into an HTML string and assigned
 * to `innerHTML`; here they are text nodes.
 */

import { api, el, render, toast, fmtTime, logout } from '/static/app.js?v=3';

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
      el('td', { colspan: '5', class: 'empty' }, 'No accounts yet.')));
    return;
  }

  render('users-body', users.map((user) => el('tr', {},
    el('td', { class: 'strong' }, user.email),
    el('td', {}, user.full_name),
    el('td', {}, el('span', { class: user.role === 'admin' ? 'badge bad' : 'badge' },
      ROLE_LABEL[user.role] || user.role)),
    el('td', {}, user.journalist_surname || '—'),
    el('td', { class: 'right' },
      el('button', {
        class: 'btn',
        type: 'button',
        onClick: () => resetPassword(user.id, user.email),
      }, 'Reset password'),
    ),
  )));
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

document.getElementById('test-llm-btn').addEventListener('click', async () => {
  showTest('Testing…', true);
  try {
    await api('/api/system/test-llm', {
      method: 'POST',
      body: {
        endpoint: document.getElementById('test-llm-endpoint').value.trim(),
        model: document.getElementById('test-llm-model').value.trim(),
      },
    });
    showTest('LLM endpoint answered.', true);
  } catch (e) { showTest(e.message, false); }
});

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
  render('members-body', staff.length === 0
    ? el('tr', {}, el('td', { colspan: '5', class: 'empty' }, 'No journalists yet.'))
    : staff.map((p) => {
      const input = el('input', {
        type: 'text',
        value: (p.groups || []).join(', '),
        placeholder: groups.map((g) => g.code).slice(0, 3).join(', '),
        'aria-label': `Groups of ${p.surname}`,
      });
      return el('tr', {},
        el('td', { class: 'mono strong' }, p.surname),
        el('td', {}, p.full_name),
        el('td', { class: 'mono' }, (p.emails || []).join(', ')),
        el('td', {}, input),
        el('td', { class: 'right' },
          el('button', { class: 'btn', type: 'button', onClick: () => saveMembership(p.surname, input) }, 'Save')),
      );
    }));
}

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

async function saveMembership(surname, input) {
  try {
    await api(`/api/journalists/${encodeURIComponent(surname)}/groups`, {
      method: 'POST',
      body: { groups: splitList(input.value).map((c) => c.toUpperCase()) },
    });
    toast(`Groups of ${surname} saved.`, 'ok');
    loadTaxonomy();
  } catch (e) { toast(e.message, 'bad'); }
}

document.getElementById('taxonomy-export').addEventListener('click', async () => {
  try {
    const data = await api('/api/admin/taxonomy');
    const blob = new Blob([JSON.stringify(data, null, 2) + '\n'], { type: 'application/json' });
    // Our own JSON as a local blob: a plain anchor, since el() only allows
    // http(s) links.
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = 'taxonomy.json';
    document.body.append(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(a.href), 1000);
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

document.getElementById('logout-btn').addEventListener('click', logout);

loadDependencies();
loadSecrets();
loadMaintenance();
loadUsers();
loadTaxonomy();
loadLogs();
setInterval(loadLogs, 15000);

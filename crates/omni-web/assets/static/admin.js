/* Administration panel behaviour (plan P2.3, P2.5).
 *
 * Audit messages contain operator- and email-derived text (job slugs, URLs,
 * journalist names). They used to be joined into an HTML string and assigned
 * to `innerHTML`; here they are text nodes.
 */

import { api, el, render, toast, fmtTime, logout } from '/static/app.js?v=2';

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
  'mail.password': 'Mailbox password',
  'graph.client_secret': 'Microsoft Graph client secret',
  'teams.webhook_url': 'Teams webhook URL',
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

document.getElementById('test-mail-btn').addEventListener('click', async () => {
  showTest('Testing…', true);
  try {
    await api('/api/system/test-email', {
      method: 'POST',
      body: {
        server: document.getElementById('test-mail-server').value.trim(),
        port: Number(document.getElementById('test-mail-port').value) || 993,
        email: document.getElementById('test-mail-user').value.trim(),
        pass: document.getElementById('test-mail-pass').value,
      },
    });
    showTest('Mailbox sign-in succeeded.', true);
  } catch (e) { showTest(e.message, false); }
  // The password is never left in the field after a test.
  document.getElementById('test-mail-pass').value = '';
});

document.getElementById('logout-btn').addEventListener('click', logout);

loadDependencies();
loadSecrets();
loadUsers();
loadLogs();
setInterval(loadLogs, 15000);

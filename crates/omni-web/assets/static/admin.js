/* Administration panel behaviour (plan P2.3, P2.5).
 *
 * Audit messages contain operator- and email-derived text (job slugs, URLs,
 * journalist names). They used to be joined into an HTML string and assigned
 * to `innerHTML`; here they are text nodes.
 */

import { api, el, render, toast, fmtTime, logout } from '/static/app.js?v=5';

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

document.getElementById('logout-btn').addEventListener('click', logout);

loadDependencies();
loadSecrets();
loadMaintenance();
loadSelfcheck();
loadUsers();
loadTaxonomy();
loadMail();
loadLlm();
loadLogs();
setInterval(loadLogs, 15000);

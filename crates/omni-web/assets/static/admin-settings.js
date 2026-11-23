/* Admin panel, «Ρυθμίσεις» (P13.5): setup and restart, watchfolder, mailbox,
 * network and TLS, credentials, the language model and the operation form.
 * Loaded when the section is first opened.
 */

import { api, el, render, toast } from '/static/app.js?v=6';
import { runWizard } from '/static/wizard.js?v=10';
import { confirmDialog } from '/static/dialog.js?v=10';

const setupState = await api('/api/admin/settings').catch(() => null);

// The Setup card: whether the wizard was finished, and a way to run it again.
// The account step appears only for the first-run session, which never gets here.
{
  const done = setupState && setupState.setup_completed;
  document.getElementById('setup-state').textContent = done
    ? `Setup was completed in ${setupState.setup_mode} mode.`
    : 'Setup not finished: the wizard was left before its last screen.';
  document.getElementById('setup-rerun').addEventListener('click', () => runWizard({ defaultAdmin: false }));

  // A new watchfolder that waits for the videos being made now (P13.2).
  const pending = setupState && setupState.watchfolder_pending;
  const pendingLine = document.getElementById('watchfolder-pending');
  if (pending) {
    pendingLine.hidden = false;
    pendingLine.textContent = `New watchfolder pending: ${pending.path}. It is used as soon as the videos being made now have finished; until then files go to ${setupState.watchfolder_in_use || 'the current one'}.`;
  }

  // «Επανεκκίνηση υπηρεσίας» (P13.2): asked in the page, then the panel waits
  // for the service to answer again and reloads.
  if (setupState && setupState.restart_available) {
    document.getElementById('restart-box').hidden = false;
    const btn = document.getElementById('restart-btn');
    const warn = document.getElementById('restart-warn');
    const result = document.getElementById('restart-result');
    btn.addEventListener('click', async () => {
      if (warn.hidden) {
        warn.hidden = false;
        btn.textContent = 'Επανεκκίνηση τώρα';
        btn.classList.add('btn-danger');
        return;
      }
      btn.disabled = true;
      try {
        await api('/api/admin/restart-service', { method: 'POST', body: {} });
      } catch (e) {
        result.className = 'note bad';
        result.textContent = e.message;
        btn.disabled = false;
        return;
      }
      result.className = 'note';
      result.textContent = 'Restarting… the page reloads when the service answers again.';
      const started = Date.now();
      // Down first, then up again; give up after three minutes.
      let wentDown = false;
      const poll = async () => {
        // Any answer, even a red health, means the service is back; only no answer is "down".
        const up = await api('/api/health').then(() => true, (e) => e.code !== 'NETWORK');
        if (!up) wentDown = true;
        if (up && wentDown) { location.reload(); return; }
        if (Date.now() - started > 180000) {
          result.className = 'note bad';
          result.textContent = 'The service has not answered for three minutes. Check it in Services, and the Event Log.';
          return;
        }
        setTimeout(poll, 2000);
      };
      setTimeout(poll, 2000);
    });
  }
}

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
  if (!await confirmDialog(`Clear ${SECRET_LABEL[key] || key}? It cannot be recovered.`, { okLabel: 'Clear', danger: true })) return;
  try {
    await api('/api/secrets', { method: 'POST', body: { key, value: '' } });
    toast('Cleared.', 'ok');
    loadSecrets();
  } catch (e) { toast(e.message, 'bad'); }
}

/* ------------------------------------------------------------------ *
 * Connection tests
 * ------------------------------------------------------------------ */

const testResult = document.getElementById('test-result');

function showTest(message, ok) {
  testResult.textContent = message;
  testResult.className = ok ? 'note ok' : 'note bad';
}

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

// The mailbox card starts from the saved settings; edit a field to test
// another value, and Save to use it from the next mail check (P11.11c).
function loadMailbox() {
  api('/api/admin/settings')
    .then((state) => {
      if (!state.graph) return;
      document.getElementById('test-mail-tenant').value = state.graph.tenant_id || '';
      document.getElementById('test-mail-client').value = state.graph.client_id || '';
      document.getElementById('test-mail-user').value = state.graph.mailbox || '';
      document.getElementById('test-mail-expires').value = state.graph.client_secret_expires || '';
    })
    .catch(() => { /* the fields stay blank; the server falls back to the saved values */ });
}
loadMailbox();

document.getElementById('save-mail-btn').addEventListener('click', async (event) => {
  const btn = event.currentTarget;
  btn.disabled = true;
  try {
    const res = await api('/api/admin/settings/mail', {
      method: 'POST',
      body: {
        tenant_id: document.getElementById('test-mail-tenant').value.trim(),
        client_id: document.getElementById('test-mail-client').value.trim(),
        mailbox: document.getElementById('test-mail-user').value.trim(),
        client_secret: document.getElementById('test-mail-secret').value,
        client_secret_expires: document.getElementById('test-mail-expires').value,
      },
    });
    showTest(res.restart_required
      ? 'Saved. Restart the service to use these settings.'
      : 'Saved. The next mail check uses these settings.', true);
    loadMailbox();
  } catch (e) { showTest(e.message, false); }
  document.getElementById('test-mail-secret').value = '';
  btn.disabled = false;
});

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
 * Watchfolder (P13.5): test, then save; used between jobs (P13.2)
 * ------------------------------------------------------------------ */

const wf = (id) => document.getElementById(id);
let wfTested = null;

function showWf(message, ok) {
  wf('wf-result').textContent = message;
  wf('wf-result').className = ok ? 'note ok' : 'note bad';
}

wf('wf-path').addEventListener('input', () => { wfTested = null; wf('wf-save').disabled = true; });
wf('wf-test').addEventListener('click', async () => {
  const path = wf('wf-path').value;
  showWf('Testing (up to 15 seconds)…', true);
  try {
    const r = await api('/api/admin/settings/watchfolder/test', { method: 'POST', body: { path } });
    showWf(r.message, r.ok === true);
    wfTested = r.ok === true ? path : null;
  } catch (e) { showWf(e.message, false); wfTested = null; }
  wf('wf-save').disabled = wfTested === null;
});
wf('wf-save').addEventListener('click', async () => {
  try {
    const r = await api('/api/admin/settings/watchfolder', { method: 'POST', body: { path: wfTested } });
    showWf(r.message || (r.pending ? 'Saved; it is used once the videos being made now have finished.' : 'Saved and in use.'), true);
    wf('wf-save').disabled = true;
  } catch (e) { showWf(e.message, false); }
});

/* ------------------------------------------------------------------ *
 * Network, TLS and archive (P13.5)
 * ------------------------------------------------------------------ */

const lines = (text) => text.split('\n').map((s) => s.trim()).filter(Boolean);

function fillNetwork(s) {
  wf('net-port').value = s.web_port;
  wf('net-open').value = (s.mcr_open_networks || []).join('\n');
  wf('net-cert').value = (s.tls && s.tls.cert_path) || '';
  wf('net-trust').checked = !!s.trust_proxy_header;
  wf('net-proxies').value = (s.trusted_proxies || []).join('\n');
  wf('net-hosts').value = (s.allowed_hosts || []).join('\n');
  wf('net-archive').checked = !!(s.archive && s.archive.enabled);
  wf('net-archive-days').value = s.archive ? s.archive.retention_days : 14;
}

wf('net-save').addEventListener('click', async () => {
  const out = wf('net-result');
  try {
    const r = await api('/api/admin/settings/advanced', { method: 'POST', body: {
      web_port: Number(wf('net-port').value),
      mcr_open_networks: lines(wf('net-open').value),
      tls_cert_path: wf('net-cert').value.trim(),
      tls_password: wf('net-certpw').value,
      trust_proxy_header: wf('net-trust').checked,
      trusted_proxies: lines(wf('net-proxies').value),
      allowed_hosts: lines(wf('net-hosts').value),
      archive_enabled: wf('net-archive').checked,
      archive_retention_days: Number(wf('net-archive-days').value),
    } });
    wf('net-certpw').value = '';
    out.className = 'note ok';
    out.textContent = (r.restart_required ? 'Saved. The web port, TLS and host names apply after «Επανεκκίνηση υπηρεσίας» (Setup card).' : 'Saved and in use.')
      + (r.warnings && r.warnings.length ? `\n${r.warnings.join('\n')}` : '');
  } catch (e) { out.className = 'note bad'; out.textContent = e.message; }
});

/* ------------------------------------------------------------------ *
 * Operation (P13.5): repair mode, concurrency, mail, pacing, retries
 * ------------------------------------------------------------------ */

const OPERATION_FIELDS = [
  ['op-repair', 'repair_mode', 'text'],
  ['op-downloads', 'max_concurrent_downloads', 'number'],
  ['op-transcodes', 'max_concurrent_transcodes', 'number'],
  ['op-poll', 'mail_poll_interval_secs', 'number'],
  ['op-write', 'graph_write_access', 'check'],
  ['op-processed', 'graph_processed_folder', 'text'],
  ['op-failed', 'graph_failed_folder', 'text'],
  ['op-host', 'pacing_per_host_concurrency', 'number'],
  ['op-gap', 'pacing_min_gap_secs', 'number'],
  ['op-age', 'retry_max_age_mins', 'number'],
  ['op-late', 'retry_late_after_mins', 'number'],
  ['op-late-urgent', 'retry_late_after_urgent_mins', 'number'],
  ['op-second', 'retry_second_chance_window_mins', 'number'],
];

function fillOperation(op) {
  for (const [id, key, kind] of OPERATION_FIELDS) {
    if (kind === 'check') wf(id).checked = !!op[key];
    else wf(id).value = op[key] ?? '';
  }
}

wf('op-save').addEventListener('click', async () => {
  const out = wf('op-result');
  const body = {};
  for (const [id, key, kind] of OPERATION_FIELDS) {
    body[key] = kind === 'check' ? wf(id).checked : (kind === 'number' ? Number(wf(id).value) : wf(id).value.trim());
  }
  try {
    const r = await api('/api/admin/settings/operation', { method: 'POST', body });
    fillOperation(r.operation);
    out.className = 'note ok';
    out.textContent = r.restart_required
      ? 'Saved. Repair mode, concurrency, the mail interval, pacing and the retry age apply after «Επανεκκίνηση υπηρεσίας» (Setup card); the late strip and the mailbox folders apply now.'
      : 'Saved and in use.';
  } catch (e) { out.className = 'note bad'; out.textContent = e.message; }
});

/** The section opened: every card from the saved settings. */
export function init() {
  loadSecrets();
  loadLlm();
  if (setupState) {
    wf('wf-path').value = setupState.watchfolder_path || '';
    wf('wf-in-use').textContent = setupState.watchfolder_in_use ? `In use now: ${setupState.watchfolder_in_use}` : '';
    fillNetwork(setupState);
    if (setupState.operation) fillOperation(setupState.operation);
  }
}

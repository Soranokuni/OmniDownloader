/* First-setup wizard (plan P7.12).
 *
 * One wizard, three modes: beginner, intermediate and it. It renders into the
 * page's <main> in place of the panel, and only ever talks to the settings
 * endpoints through api(), which sets the CSRF header. Every value shown is a
 * text node built by el(); nothing here assembles markup from a string.
 * Every word is looked up by key in admin-text.*.js (P13.6): Greek by default,
 * English with the page's language switch, which redraws the open step.
 */

import { api, el, render, toast } from '/static/app.js?v=6';
import { t, onLanguageChange } from '/static/admin-text.js?v=1';

// Each mode's name and summary are `wizard.mode.<id>` and `wizard.mode.<id>.about`.
const MODES = ['beginner', 'intermediate', 'it'];

const lines = (text) => text.split('\n').map((s) => s.trim()).filter(Boolean);

/** A labelled control with its one-line explanation and its default. */
function field(id, label, control, help, fallback) {
  return el('div', { class: 'field' },
    el('label', { for: id }, label),
    control,
    el('p', { class: 'note', style: 'margin:4px 0 0' },
      help, fallback ? ` ${t('wizard.default', { value: fallback })}` : ''));
}

const textInput = (id, value, placeholder, type = 'text', extra = {}) =>
  el('input', { type, id, value: value ?? '', placeholder: placeholder || null, autocomplete: 'off', ...extra });

/** Put `text` on the clipboard; plain HTTP on the LAN has no clipboard API. */
function copyText(text, button) {
  const done = () => { button.textContent = t('wizard.copied'); setTimeout(() => { button.textContent = t('wizard.copy'); }, 1500); };
  if (navigator.clipboard && window.isSecureContext) {
    navigator.clipboard.writeText(text).then(done, () => toast(t('wizard.copy_failed'), 'bad'));
    return;
  }
  const box = el('textarea', { readonly: '', style: 'position:fixed;left:-9999px' }, text);
  document.body.appendChild(box);
  box.select();
  try { document.execCommand('copy'); done(); } catch (e) { toast(t('wizard.copy_failed'), 'bad'); }
  box.remove();
}

/** A value to type into another program, with its Copy button. `value` may be a function (read at copy time). */
function copyable(value) {
  const now = () => (typeof value === 'function' ? value() : value);
  const code = el('code', { class: 'mono' }, now());
  const button = el('button', { class: 'btn', type: 'button', style: 'margin-left:6px;padding:2px 8px',
    onclick: (e) => { code.textContent = now(); copyText(now(), e.currentTarget); } }, t('wizard.copy'));
  const node = el('span', {}, code, button);
  node.update = () => { code.textContent = now(); };
  return node;
}

/** Read a chosen CSV file as text: UTF-8, else Greek Windows (what Excel's plain "CSV" writes). */
function readCsv(file) {
  const read = (encoding) => new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => resolve(String(r.result || ''));
    r.onerror = () => reject(new Error(t('wizard.file_unreadable')));
    r.readAsText(file, encoding);
  });
  return read('utf-8').then((text) => (text.includes('�') ? read('windows-1253') : text));
}

/**
 * Run the wizard.
 *   defaultAdmin  the session is the first-run account: the account step comes first.
 *   showPasswordCard  fallback for keeping the default account and only changing its password.
 */
export async function runWizard({ defaultAdmin = false, showPasswordCard = null } = {}) {
  const host = document.querySelector('main');
  const S = {
    mode: 'beginner',
    needAccount: defaultAdmin,
    cfg: null,
    step: 'welcome',
    // Per-screen state that must survive Back/Next.
    watch: { tested: false, saved: false, message: '', ok: null },
    mailDone: false,
    // The values the last passing «Δοκιμή σύνδεσης» was for (P13.4).
    mailPassed: null,
    people: { previewText: null, preview: null, imported: false, done: false, learn: false },
    test: { jobId: null, status: null, poll: 0 },
    restart: false,
  };

  const loadSettings = async () => { S.cfg = await api('/api/admin/settings'); };
  if (!S.needAccount) {
    await loadSettings();
    // An unknown stored mode (a hand-edited config) falls back to beginner.
    if (MODES.includes(S.cfg.setup_mode)) S.mode = S.cfg.setup_mode;
  }

  const order = () => {
    const ids = ['welcome'];
    if (S.needAccount) ids.push('account');
    ids.push('watchfolder', 'mail', 'people', 'tools');
    if (S.mode !== 'beginner') ids.push('network');
    if (S.mode === 'it') ids.push('it');
    // P13.4: setup ends with a video delivered into the watchfolder (or an explicit skip).
    ids.push('check', 'testdl');
    return ids;
  };

  /* ---------------------------- screens ---------------------------- */
  // Each screen returns { title, intro, body, valid(), next() } and calls `refresh()` when its
  // validity may have changed. `next()` may throw: the message is shown and the wizard stays.

  let refresh = () => {};

  const screens = {
    welcome() {
      const choices = MODES.map((id) => el('label', {
        class: 'card', style: 'display:flex;gap:12px;align-items:flex-start;cursor:pointer;margin:0 0 10px',
      },
        el('input', {
          type: 'radio', name: 'wizard-mode', value: id, checked: S.mode === id,
          style: 'margin-top:3px',
          onchange: () => { S.mode = id; refresh(); },
        }),
        el('span', {}, el('strong', {}, t(`wizard.mode.${id}`)), el('br'), el('span', { class: 'note' }, t(`wizard.mode.${id}.about`)))));
      return {
        title: t('wizard.welcome.title'),
        intro: t('wizard.welcome.intro'),
        body: [...choices],
        valid: () => true,
        next: async () => {},
      };
    },

    account() {
      const name = textInput('wz-name', '', t('wizard.account.name_example'));
      const login = textInput('wz-login', '', t('wizard.account.login_example'));
      const pw = textInput('wz-pw', '', '', 'password', { autocomplete: 'new-password' });
      const pw2 = textInput('wz-pw2', '', '', 'password', { autocomplete: 'new-password' });
      const tickLen = el('p', { class: 'note' });
      const tickMatch = el('p', { class: 'note' });
      const tickLogin = el('p', { class: 'note' });
      const rules = () => ({
        name: name.value.trim().length > 0,
        login: login.value.trim().length >= 3 && !/\s/.test(login.value.trim())
          && login.value.trim().toLowerCase() !== 'admin',
        len: pw.value.length >= 12,
        match: pw.value.length > 0 && pw.value === pw2.value,
      });
      const paint = () => {
        const r = rules();
        const tick = (node, ok, text) => { node.className = `note ${ok ? 'ok' : ''}`; node.textContent = `${ok ? '\u2713' : '\u2022'} ${text}`; };
        tick(tickLogin, r.login, t('wizard.account.rule_login'));
        tick(tickLen, r.len, t('wizard.account.rule_len'));
        tick(tickMatch, r.match, t('wizard.account.rule_match'));
        refresh();
      };
      for (const i of [name, login, pw, pw2]) i.addEventListener('input', paint);
      queueMicrotask(paint);
      return {
        title: t('wizard.account.title'),
        intro: t('wizard.account.intro'),
        body: [
          field('wz-name', t('wizard.account.name'), name, t('wizard.account.name_help')),
          field('wz-login', t('wizard.account.login'), login, t('wizard.account.login_help')),
          field('wz-pw', t('wizard.account.pw'), pw, t('wizard.account.pw_help')),
          field('wz-pw2', t('wizard.account.pw2'), pw2, t('wizard.account.pw2_help')),
          tickLogin, tickLen, tickMatch,
          showPasswordCard ? el('p', { class: 'note' },
            t('wizard.account.keep_admin'), ' ',
            el('button', { class: 'btn', type: 'button', onclick: () => showPasswordCard() }, t('wizard.account.change_pw')),
            ' ', t('wizard.account.keep_admin_later')) : null,
        ],
        valid: () => { const r = rules(); return r.name && r.login && r.len && r.match; },
        next: async () => {
          const made = await api('/api/setup/wizard/account', {
            method: 'POST',
            body: { full_name: name.value.trim(), login: login.value.trim(), password: pw.value },
          });
          if (made && made.signed_in === false) {
            // The account exists and the first-run one is off, but no session could be opened.
            toast(made.message || t('wizard.account.sign_in'), 'ok');
            setTimeout(() => { location.href = '/login'; }, 1500);
            await new Promise(() => {});
          }
          // The server has signed this browser in as the new administrator.
          pw.value = ''; pw2.value = '';
          S.needAccount = false;
          await loadSettings();
        },
      };
    },

    watchfolder() {
      const path = textInput('wz-wf', S.cfg.watchfolder_resolved || S.cfg.watchfolder_path,
        '\\\\192.168.4.42\\FileIngest');
      const result = el('p', { class: 'note', style: 'white-space:pre-line' });
      // Shown in every mode when the test says access denied: the usual cause is a service
      // that still runs as the default account, which a NAS or IP share does not accept.
      const advice = el('p', { class: 'note warn', hidden: true }, t('wizard.wf.denied'));
      const paint = () => {
        result.className = `note ${S.watch.ok === true ? 'ok' : (S.watch.ok === false ? 'bad' : '')}`;
        result.textContent = S.watch.message;
        advice.hidden = !/access denied/i.test(S.watch.message || '');
      };
      const skipWarn = el('p', { class: 'note warn', hidden: true },
        t('wizard.wf.skip_warn', { path: S.cfg.watchfolder_resolved || S.cfg.watchfolder_path }));
      const skip = el('button', { class: 'btn', type: 'button', onclick: () => {
        if (skipWarn.hidden) { skipWarn.hidden = false; skip.textContent = t('wizard.skip_anyway'); return; }
        S.skipNow();
      } }, t('wizard.skip'));
      paint();
      path.addEventListener('input', () => {
        // Any edit voids the last test: Save needs a pass for exactly this text.
        S.watch.tested = false; S.watch.ok = null; S.watch.message = ''; paint(); refresh();
      });
      const test = el('button', { class: 'btn', type: 'button', onclick: async (e) => {
        const btn = e.currentTarget;
        btn.disabled = true; result.className = 'note'; result.textContent = t('wizard.wf.testing');
        try {
          const r = await api('/api/admin/settings/watchfolder/test', { method: 'POST', body: { path: path.value } });
          S.watch = { tested: r.ok === true, ok: r.ok === true, message: r.message, saved: false, text: path.value };
        } catch (err) {
          S.watch = { tested: false, ok: false, message: err.message, saved: false };
        }
        btn.disabled = false; paint(); refresh();
      } }, t('wizard.test'));
      return {
        title: t('wizard.wf.title'),
        intro: t('wizard.wf.intro'),
        body: [
          field('wz-wf', t('wizard.wf.path'), path, t('wizard.wf.path_help'), t('wizard.wf.path_default')),
          el('div', { class: 'row' }, test, skip),
          result, advice, skipWarn,
          el('p', { class: 'note' }, t('wizard.wf.about')),
        ],
        valid: () => S.watch.tested && S.watch.text === path.value,
        next: async () => {
          const r = await api('/api/admin/settings/watchfolder', { method: 'POST', body: { path: path.value } });
          if (r.restart_required) S.restart = true;
          // Jobs were running: the new folder is used once they finish (P13.2).
          S.watchPending = r.pending ? r.message : '';
          S.cfg.watchfolder_path = path.value.trim();
        },
      };
    },

    mail() {
      const tenant = textInput('wz-tenant', S.cfg.graph.tenant_id, t('wizard.mail.tenant_ph'));
      const client = textInput('wz-client', S.cfg.graph.client_id, t('wizard.mail.client_ph'));
      const mailbox = textInput('wz-mailbox', S.cfg.graph.mailbox, 'ingest@yourstation.example');
      const secret = textInput('wz-secret', '', S.cfg.graph.secret_set ? t('wizard.mail.secret_kept') : '', 'password',
        { autocomplete: 'new-password' });
      const expires = textInput('wz-secret-expires', S.cfg.graph.client_secret_expires || '', 'YYYY-MM-DD', 'date');
      const result = el('p', { class: 'note', style: 'white-space:pre-line' });
      const filled = () => [tenant, client, mailbox].map((i) => i.value.trim());
      const complete = () => filled().every(Boolean) && (secret.value.trim() || S.cfg.graph.secret_set);
      // What a test is for: the stored secret only goes with the stored registration.
      const values = () => [...filled(), secret.value].join('\n');
      const sameRegistration = () => tenant.value.trim() === (S.cfg.graph.tenant_id || '')
        && client.value.trim() === (S.cfg.graph.client_id || '');
      const testable = () => filled().every(Boolean) && (secret.value.trim() || (S.cfg.graph.secret_set && sameRegistration()));
      const passed = () => S.mailPassed !== null && S.mailPassed === values();
      const test = el('button', { class: 'btn', type: 'button', onclick: async (e) => {
        const btn = e.currentTarget;
        if (!testable()) {
          result.className = 'note bad';
          result.textContent = t('wizard.mail.fill_in');
          return;
        }
        btn.disabled = true; result.className = 'note'; result.textContent = t('wizard.testing');
        const tried = values();
        try {
          const r = await api('/api/system/test-email', { method: 'POST', body: {
            tenant_id: tenant.value.trim(), client_id: client.value.trim(),
            mailbox: mailbox.value.trim(), client_secret: secret.value,
          } });
          S.mailPassed = tried;
          result.className = 'note ok'; result.textContent = r.detail || t('wizard.mail.readable');
        } catch (err) { S.mailPassed = null; result.className = 'note bad'; result.textContent = err.message; }
        btn.disabled = false; refresh();
      } }, t('wizard.mail.test'));
      for (const i of [tenant, client, mailbox, secret]) i.addEventListener('input', () => refresh());
      const warn = el('p', { class: 'note warn', hidden: true }, t('wizard.mail.skip_warn'));
      // Two clicks: the first shows what skipping costs, the second skips.
      const skip = el('button', { class: 'btn', type: 'button', onclick: () => {
        if (warn.hidden) { warn.hidden = false; skip.textContent = t('wizard.skip_anyway'); return; }
        S.skipNow();
      } }, t('wizard.skip'));
      const policy = () => `New-ApplicationAccessPolicy -AppId ${client.value.trim() || '<client id>'} -PolicyScopeGroupId ${mailbox.value.trim() || '<mailbox>'} -AccessRight RestrictAccess -Description "OmniIngest"`;
      const policyLine = copyable(policy);
      for (const i of [client, mailbox]) i.addEventListener('input', () => policyLine.update());
      const steps = el('ol', { class: 'stack', style: 'margin:0;padding-left:20px' },
        el('li', {}, t('wizard.mail.step1'), ' ', copyable('OmniIngest'), ' ', t('wizard.mail.step1b')),
        el('li', {}, t('wizard.mail.step2'), ' ', copyable('Mail.Read'), ' ', t('wizard.mail.step2b')),
        el('li', {}, t('wizard.mail.step3')),
        el('li', {}, t('wizard.mail.step4')),
        el('li', {}, t('wizard.mail.step5'), ' ', policyLine),
        el('li', {}, t('wizard.mail.step6')));
      return {
        title: t('wizard.mail.title'),
        intro: t('wizard.mail.intro'),
        body: [
          steps,
          field('wz-tenant', t('wizard.mail.tenant'), tenant, t('wizard.mail.tenant_help')),
          field('wz-client', t('wizard.mail.client'), client, t('wizard.mail.client_help')),
          field('wz-mailbox', t('wizard.mail.mailbox'), mailbox, t('wizard.mail.mailbox_help')),
          field('wz-secret', t('wizard.mail.secret'), secret, t('wizard.mail.secret_help')),
          field('wz-secret-expires', t('wizard.mail.expires'), expires, t('wizard.mail.expires_help')),
          el('div', { class: 'row' }, test,
            skip),
          result, warn,
        ],
        // P13.4: only a passing test for these values; blank fields need «Skip for now».
        valid: () => complete() && passed(),
        next: async () => {
          const r = await api('/api/admin/settings/mail', { method: 'POST', body: {
            tenant_id: tenant.value.trim(), client_id: client.value.trim(),
            mailbox: mailbox.value.trim(), client_secret: secret.value, client_secret_expires: expires.value,
          } });
          secret.value = '';
          if (r.restart_required) S.restart = true;
          S.cfg.graph = { ...S.cfg.graph, tenant_id: tenant.value.trim(), client_id: client.value.trim(),
            mailbox: mailbox.value.trim(), secret_set: r.secret_set, client_secret_expires: expires.value || null };
        },
      };
    },

    people() {
      const P = S.people;
      const text = el('textarea', { id: 'wz-people', rows: 7, class: 'mono',
        placeholder: t('wizard.people.placeholder') });
      if (P.previewText !== null) text.value = P.previewText;
      const file = el('input', { type: 'file', id: 'wz-people-file', accept: '.csv,.txt,text/csv,text/plain' });
      const out = el('div', { class: 'stack' });
      const paint = () => {
        const p = P.preview;
        if (!p) { render(out); return; }
        const row = (r) => el('div', { class: 'spec-row' }, el('span', {}, r.surname), el('span', {}, r.full_name),
          el('span', { class: 'note' }, r.emails.join(', ')));
        render(out,
          P.imported ? el('p', { class: 'note ok' }, `✓ ${t('wizard.people.added_n', { n: p.add.length })}`) : null,
          p.add.length ? el('strong', {}, P.imported ? t('wizard.people.added') : t('wizard.people.to_add', { n: p.add.length })) : null,
          ...p.add.map(row),
          p.existing.length ? el('p', { class: 'note' }, t('wizard.people.existing', { names: p.existing.map((r) => r.surname).join(', ') })) : null,
          ...p.problems.map((q) => el('p', { class: 'note bad' }, t('wizard.people.line', { line: q.line, message: q.message }))),
          !P.imported && p.add.length === 0 ? el('p', { class: 'note' }, t('wizard.people.nobody')) : null);
      };
      const send = async (dryRun) => {
        const body = { text: text.value, dry_run: dryRun };
        const r = await api('/api/admin/journalists/import', { method: 'POST', body });
        P.preview = r; P.previewText = text.value; P.imported = !dryRun && r.add.length > 0;
        if (P.imported) P.done = true;
        paint(); refreshButtons(); refresh();
      };
      const preview = el('button', { class: 'btn', type: 'button', onclick: async () => {
        try { await send(true); } catch (err) { toast(err.message, 'bad'); }
      } }, t('wizard.people.preview'));
      const doImport = el('button', { class: 'btn btn-primary', type: 'button', onclick: async () => {
        try { await send(false); } catch (err) { toast(err.message, 'bad'); }
      } }, t('wizard.people.import'));
      const canImport = () => !P.imported && P.preview && P.preview.add.length > 0 && P.previewText === text.value;
      text.addEventListener('input', () => { P.imported = false; refreshButtons(); refresh(); });
      file.addEventListener('change', async () => {
        if (!file.files || !file.files[0]) return;
        try { text.value = await readCsv(file.files[0]); P.imported = false; await send(true); } catch (err) { toast(err.message, 'bad'); }
        refreshButtons();
      });
      const refreshButtons = () => { doImport.disabled = !canImport(); };
      const learnNote = el('p', { class: 'note warn', hidden: !P.learn }, t('wizard.people.learn_note'));
      const learn = el('button', { class: 'btn', type: 'button', onclick: () => {
        P.learn = true; learnNote.hidden = false; refresh();
      } }, t('wizard.people.learn'));
      queueMicrotask(() => { paint(); refreshButtons(); });
      return {
        title: t('wizard.people.title'),
        intro: t('wizard.people.intro'),
        body: [
          field('wz-people', t('wizard.people.paste'), text, t('wizard.people.paste_help')),
          field('wz-people-file', t('wizard.people.file'), file, t('wizard.people.file_help')),
          el('div', { class: 'row' }, preview, doImport, learn),
          out, learnNote,
          el('p', { class: 'note' }, t('wizard.people.about')),
        ],
        valid: () => P.done || P.learn,
        next: async () => {},
      };
    },

    tools() {
      const box = el('div', { class: 'stack' }, el('p', { class: 'note' }, t('wizard.tools.checking')));
      const warn = el('p', { class: 'note warn', hidden: true }, t('wizard.tools.skip_warn'));
      const skip = el('button', { class: 'btn', type: 'button', onclick: () => {
        if (warn.hidden) { warn.hidden = false; skip.textContent = t('wizard.skip_anyway'); return; }
        S.skipNow();
      } }, t('wizard.skip'));
      let ready = false;
      let timer = 0;
      const load = async () => {
        clearTimeout(timer);
        if (!box.isConnected && S.step !== 'tools') return;
        let data;
        try { data = await api('/api/admin/tools'); } catch (err) { render(box, el('p', { class: 'note bad' }, err.message)); return; }
        const tools = data.tools || [];
        const busy = Boolean(data.running_tool);
        ready = tools.length > 0 && tools.every((tool) => tool.installed && !tool.action);
        render(box, tools.map((tool) => {
          const label = tool.action === 'install' ? t('wizard.tools.install') : t('wizard.tools.repair');
          const state = !tool.installed ? t('wizard.tools.missing')
            : (tool.action === 'repair' ? t('wizard.tools.not_pinned') : (tool.version || t('wizard.tools.installed')));
          const lines = [el('div', { class: 'spec-row' },
            el('span', {}, tool.name),
            el('span', { class: tool.installed && !tool.action ? 'badge ok' : 'badge bad' }, state),
            tool.pinned_version ? el('span', { class: 'note' }, t('wizard.tools.pinned', { version: tool.pinned_version })) : null,
            tool.action ? el('button', { class: 'btn', type: 'button', disabled: busy, onclick: async () => {
              try { await api(`/api/admin/tools/${encodeURIComponent(tool.name)}/install`, { method: 'POST' }); } catch (err) { toast(err.message, 'bad'); }
              load();
            } }, label) : null)];
          if (tool.running) lines.push(el('p', { class: 'note' }, `${tool.running.stage}…`));
          else if (tool.last_result) lines.push(el('p', { class: tool.last_result.ok ? 'note ok' : 'note bad' }, tool.last_result.message));
          return el('div', { class: 'stack' }, lines);
        }), ready ? el('p', { class: 'note ok' }, `✓ ${t('wizard.tools.ready')}`) : null);
        refresh();
        // Polled only while an install runs, and only while this step is open.
        if (busy && S.step === 'tools') timer = setTimeout(load, 2000);
      };
      load();
      return {
        title: t('wizard.tools.title'),
        intro: t('wizard.tools.intro'),
        body: [box, el('div', { class: 'row' }, skip), warn],
        valid: () => ready,
        next: async () => { clearTimeout(timer); },
      };
    },

    network() {
      const port = textInput('wz-port', S.cfg.web_port, '8080', 'number', { min: 1, max: 65535 });
      const nets = el('textarea', { id: 'wz-nets', rows: 4, class: 'mono', placeholder: '10.20.0.0/16' });
      nets.value = (S.cfg.mcr_open_networks || []).join('\n');
      const arch = el('input', { type: 'checkbox', id: 'wz-arch', checked: S.cfg.archive.enabled, style: 'width:auto' });
      const days = textInput('wz-days', S.cfg.archive.retention_days, '14', 'number', { min: 1, max: 3650 });
      const parseable = () => /^\d+$/.test(String(port.value).trim()) && port.value >= 1 && port.value <= 65535
        && /^\d+$/.test(String(days.value).trim()) && days.value >= 1 && days.value <= 3650;
      for (const i of [port, nets, days]) i.addEventListener('input', () => refresh());
      return {
        title: t('wizard.net.title'),
        intro: t('wizard.net.intro'),
        body: [
          field('wz-port', t('wizard.net.port'), port, t('wizard.net.port_help'), '8080'),
          field('wz-nets', t('wizard.net.open'), nets, t('wizard.net.open_help'), t('wizard.net.open_default')),
          el('p', { class: 'note', style: 'margin:0' }, t('admin.help.cidr')),
          el('div', { class: 'field' },
            el('label', { class: 'checkbox-row' }, arch, el('span', {}, t('wizard.net.archive'))),
            el('p', { class: 'note', style: 'margin:4px 0 0' }, t('wizard.net.archive_help'))),
          field('wz-days', t('wizard.net.days'), days, t('wizard.net.days_help'), '14'),
          el('div', { class: 'card', style: 'margin:0' },
            el('strong', {}, t('wizard.net.llm')),
            el('p', { class: 'note' }, t('wizard.net.llm_help')),
            el('button', { class: 'btn', type: 'button', onclick: () => {
              // Same tab, so the session's "left the wizard" mark applies on the panel.
              sessionStorage.setItem('omni.setup.leave', '1');
              // Same page: changing only the fragment would not reload, and the wizard
              // has replaced the panel, so reload onto the LLM card.
              location.hash = 'llm';
              location.reload();
            } }, t('wizard.net.llm_open'))),
          el('div', { class: 'card', style: 'margin:0' },
            el('strong', {}, t('wizard.net.account')),
            el('p', { class: 'note' }, t('wizard.net.account_help'))),
        ],
        valid: parseable,
        next: async () => {
          const r = await api('/api/admin/settings/advanced', { method: 'POST', body: {
            web_port: Number(port.value),
            mcr_open_networks: lines(nets.value),
            archive_enabled: arch.checked,
            archive_retention_days: Number(days.value),
          } });
          if (r.restart_required) S.restart = true;
          await loadSettings();
        },
      };
    },

    it() {
      const cert = textInput('wz-cert', S.cfg.tls.cert_path || '', 'C:\\certs\\ingest.pfx');
      const pass = textInput('wz-certpw', '', '', 'password', { autocomplete: 'new-password' });
      const trust = el('input', { type: 'checkbox', id: 'wz-trust', checked: S.cfg.trust_proxy_header, style: 'width:auto' });
      const proxies = el('textarea', { id: 'wz-proxies', rows: 2, class: 'mono', placeholder: '10.0.0.5/32' });
      proxies.value = (S.cfg.trusted_proxies || []).join('\n');
      // A trusted proxy wider than /24 (IPv4) or /64 (IPv6) lets every machine in it speak for any client.
      const wide = el('p', { class: 'note warn', hidden: true }, t('wizard.it.wide'));
      const checkWide = () => {
        wide.hidden = !lines(proxies.value).some((entry) => {
          const [addr, len] = entry.split('/');
          const bits = Number(len);
          return Number.isInteger(bits) && bits < (addr.includes(':') ? 64 : 24);
        });
      };
      proxies.addEventListener('input', checkWide);
      checkWide();
      const hosts = el('textarea', { id: 'wz-hosts', rows: 2, class: 'mono', placeholder: 'ingest.station.example' });
      hosts.value = (S.cfg.allowed_hosts || []).join('\n');
      const met = el('input', { type: 'checkbox', id: 'wz-met', checked: S.cfg.metrics.enabled, style: 'width:auto' });
      const metNets = el('textarea', { id: 'wz-metnets', rows: 2, class: 'mono', placeholder: '10.0.0.0/24' });
      metNets.value = (S.cfg.metrics.allow_networks || []).join('\n');
      const download = el('button', { class: 'btn', type: 'button', onclick: async () => {
        try {
          const doc = await api('/api/admin/settings/answer-file');
          const url = URL.createObjectURL(new Blob([JSON.stringify(doc, null, 2)], { type: 'application/json' }));
          const a = el('a', { download: 'omni-answers.json' });
          a.href = url; document.body.append(a); a.click(); a.remove();
          setTimeout(() => URL.revokeObjectURL(url), 2000);
        } catch (err) { toast(err.message, 'bad'); }
      } }, t('wizard.it.download'));
      return {
        title: t('wizard.it.title'),
        intro: t('wizard.it.intro'),
        body: [
          field('wz-cert', t('wizard.it.cert'), cert, t('wizard.it.cert_help'), t('wizard.it.off')),
          field('wz-certpw', t('wizard.it.certpw'), pass, t('wizard.it.certpw_help')),
          el('div', { class: 'field' },
            el('label', { class: 'checkbox-row' }, trust, el('span', {}, t('wizard.it.trust'))),
            el('p', { class: 'note', style: 'margin:4px 0 0' }, t('wizard.it.trust_help'))),
          el('p', { class: 'note', style: 'margin:0' }, t('admin.help.proxy')),
          field('wz-proxies', t('wizard.it.proxies'), proxies, t('wizard.it.proxies_help'), t('wizard.it.none')),
          wide,
          field('wz-hosts', t('wizard.it.hosts'), hosts, t('wizard.it.hosts_help'), t('wizard.it.hosts_default')),
          el('div', { class: 'field' },
            el('label', { class: 'checkbox-row' }, met, el('span', {}, t('wizard.it.metrics'))),
            el('p', { class: 'note', style: 'margin:4px 0 0' }, t('wizard.it.metrics_help'))),
          field('wz-metnets', t('wizard.it.metnets'), metNets, t('wizard.it.metnets_help'), t('wizard.net.open_default')),
          el('p', { class: 'note', style: 'margin:0' }, t('admin.help.cidr')),
          el('div', { class: 'row' }, download),
          el('p', { class: 'note' }, t('wizard.it.answer_help')),
        ],
        valid: () => true,
        next: async () => {
          const r = await api('/api/admin/settings/advanced', { method: 'POST', body: {
            tls_cert_path: cert.value.trim(),
            tls_password: pass.value,
            trust_proxy_header: trust.checked,
            trusted_proxies: lines(proxies.value),
            allowed_hosts: lines(hosts.value),
            metrics_enabled: met.checked,
            metrics_allow_networks: lines(metNets.value),
          } });
          pass.value = '';
          for (const w of (r.warnings || [])) toast(w, 'bad');
          if (r.restart_required) S.restart = true;
          await loadSettings();
        },
      };
    },

    check() {
      const list = el('div', { class: 'stack' }, el('p', { class: 'note' }, t('wizard.check.checking')));
      const note = el('div', { class: 'stack' });
      (async () => {
        try {
          const status = await api('/api/system/status');
          const entries = Object.entries(status.checks || {});
          const cls = { ok: 'ok', degraded: 'warn', down: 'bad' };
          const word = (state) => (cls[state] ? t(`admin.state.${state}`) : state);
          render(list, entries.length === 0
            ? el('p', { class: 'note ok' }, t('wizard.check.passed'))
            : entries.map(([name, c]) => el('div', { class: 'spec-row' },
              el('span', { class: `badge ${cls[c.state] || ''}` }, word(c.state)),
              el('span', {}, name),
              el('span', { class: 'note' }, c.detail || ''))));
        } catch (err) { render(list, el('p', { class: 'note bad' }, err.message)); }
      })();
      if (S.restart) {
        note.append(el('p', { class: 'note warn' }, t('wizard.check.restart')));
      }
      if (S.watchPending) {
        note.append(el('p', { class: 'note warn' }, S.watchPending));
      }
      // Repair runs in shadow on a new installation: it records what it would have
      // done and changes nothing (the Repair card has the mode).
      // Each mode's sentence is `wizard.check.repair.<mode>`.
      const mode = S.cfg.repair_mode;
      const repair = ['off', 'shadow', 'hold'].includes(mode) ? t(`wizard.check.repair.${mode}`) : mode;
      note.append(el('p', { class: 'note' }, t('wizard.check.repair', { mode: repair })));
      return {
        title: t('wizard.check.title'),
        intro: t('wizard.check.intro'),
        body: [list, note],
        valid: () => true,
        next: async () => {},
      };
    },

    testdl() {
      const T = S.test;
      const url = textInput('wz-test-url', S.cfg.setup_test_url, '');
      const STAGES = ['QUEUED', 'EXTRACT', 'DOWNLOAD', 'PROBE', 'TRANSCODE', 'REWRAP', 'VERIFY', 'DELIVER', 'DONE'];
      const stages = el('div', { class: 'row', style: 'flex-wrap:wrap;gap:6px' });
      const status = el('p', { class: 'note', style: 'white-space:pre-line' });
      const events = el('div', { class: 'stack' });
      const paintStages = (current, failed) => {
        const at = STAGES.indexOf(current);
        render(stages, STAGES.map((s, i) => el('span', {
          class: `badge ${i < at || current === 'DONE' ? 'ok' : (i === at ? (failed ? 'bad' : 'warn') : '')}`,
        }, s)));
      };
      const ENDED = ['FAILED', 'REQUIRES_REVIEW', 'MANUAL_DOWNLOAD', 'CANCELLED', 'COMPLETED_MANUAL', 'AWAITING_APPROVAL'];
      const poll = async (gen) => {
        if (gen !== T.poll || S.step !== 'testdl' || !T.jobId) return;
        let r;
        try { r = await api(`/api/admin/setup/test-download/${T.jobId}`); } catch (err) {
          // P13.4b: a lost answer (a restart, the network) is not the end of the test.
          if (gen !== T.poll) return;
          // P13.4c: only a lost connection or a server error is retried; a job that is
          // gone (404) or a refusal (403) ends the wait, and Start can begin a new test.
          if (err.code === 'NETWORK' || err.status >= 500) {
            status.className = 'note warn'; status.textContent = t('wizard.testdl.reconnect', { message: err.message });
            setTimeout(() => poll(gen), 3000);
          } else {
            status.className = 'note bad'; status.textContent = err.message;
            T.jobId = null; T.status = null; start.disabled = false; refresh();
          }
          return;
        }
        if (gen !== T.poll) return;
        T.status = r;
        const failed = ENDED.includes(r.status) || (r.status === 'COMPLETED' && !r.delivered);
        paintStages(r.stage, failed);
        render(events, (r.events || []).slice(0, 6).map((e) => el('p', { class: `note ${e.level === 'ERROR' ? 'bad' : ''}`, style: 'margin:0' },
          `${e.stage ? `${e.stage}: ` : ''}${e.message}`)));
        if (r.delivered) {
          status.className = 'note ok';
          status.textContent = `✓ ${t('wizard.testdl.delivered', { file: r.file_name, folder: r.watchfolder })}`;
        } else if (failed) {
          status.className = 'note bad';
          status.textContent = t('wizard.testdl.failed', {
            status: `${r.status}${r.error_code ? `, ${r.error_code}` : ''}`, message: r.error_message || '',
          });
          start.disabled = false;
        } else {
          status.className = 'note';
          status.textContent = r.status === 'PENDING' ? t('wizard.testdl.waiting')
            : `${r.stage}${r.progress > 0 && r.progress < 100 ? ` · ${Math.round(r.progress)}%` : ''}…`;
          setTimeout(() => poll(gen), 1500);
        }
        refresh();
      };
      const start = el('button', { class: 'btn btn-primary', type: 'button', onclick: async () => {
        start.disabled = true;
        status.className = 'note'; status.textContent = t('wizard.testdl.starting');
        try {
          const r = await api('/api/admin/setup/test-download', { method: 'POST', body: { url: url.value.trim() } });
          T.jobId = r.job_id; T.status = null; S.cfg.setup_test_url = url.value.trim();
          T.poll += 1; poll(T.poll);
        } catch (err) { status.className = 'note bad'; status.textContent = err.message; start.disabled = false; }
      } }, t('wizard.testdl.start'));
      const warn = el('p', { class: 'note warn', hidden: true }, t('wizard.testdl.skip_warn'));
      const skip = el('button', { class: 'btn', type: 'button', onclick: () => {
        if (warn.hidden) { warn.hidden = false; skip.textContent = t('wizard.testdl.skip_finish'); return; }
        S.finish({ skip_test: true }).catch((err) => toast(err.message, 'bad'));
      } }, t('wizard.skip'));
      paintStages(null, false);
      if (T.jobId) { start.disabled = true; T.poll += 1; poll(T.poll); }
      return {
        title: t('wizard.testdl.title'),
        intro: t('wizard.testdl.intro'),
        body: [
          field('wz-test-url', t('wizard.testdl.url'), url, t('wizard.testdl.url_help'), t('wizard.testdl.url_default')),
          el('p', { class: 'note warn' }, t('wizard.testdl.dalet')),
          el('div', { class: 'row' }, start, skip),
          stages, status, events, warn,
        ],
        valid: () => Boolean(T.status && T.status.delivered),
        finishLabel: t('wizard.finish'),
        next: async () => { await S.finish({ test_job_id: T.jobId }); },
      };
    },
  };

  /** Complete setup (P13.4: with the delivered test download, or skipped on purpose). */
  S.finish = async (proof) => {
    await api('/api/admin/settings/complete', { method: 'POST', body: { mode: S.mode, ...proof } });
    S.test.poll += 1;
    location.reload();
  };

  /* ---------------------------- frame ---------------------------- */

  S.skipNow = () => {};
  const frame = el('section', { class: 'card stack', style: 'max-width:760px;margin:0 auto' });
  render(host, el('div', { class: 'stack' }, frame));
  host.style.display = 'block';

  const show = () => {
    const ids = order();
    const idx = Math.max(0, ids.indexOf(S.step));
    const screen = screens[ids[idx]]();
    const last = idx === ids.length - 1;
    const percent = Math.round(((idx + 1) / ids.length) * 100);
    const msg = el('p', { class: 'note bad', role: 'alert' });
    const back = el('button', { class: 'btn', type: 'button', onclick: () => { S.step = ids[idx - 1]; show(); } }, t('wizard.back'));
    if (idx === 0) back.disabled = true;
    const next = el('button', { class: 'btn btn-primary', type: 'button', onclick: async () => {
      next.disabled = true; msg.textContent = '';
      try {
        await screen.next();
        if (!last) { S.step = ids[idx + 1]; show(); }
      } catch (err) { msg.textContent = err.message; next.disabled = !screen.valid(); }
    } }, screen.finishLabel || t('wizard.next'));
    refresh = () => { next.disabled = !screen.valid(); };
    S.skipNow = async () => {
      // "Skip for now" on the mail step: nothing is saved, on to the next screen.
      S.step = ids[idx + 1]; show();
    };
    refresh();
    render(frame,
      el('div', { class: 'row', style: 'justify-content:space-between' },
        el('h2', { class: 'card-title', style: 'border:none;margin:0;padding:0' }, screen.title),
        el('span', { class: 'note' }, t('wizard.step', { n: idx + 1, of: ids.length, mode: t(`wizard.mode.${S.mode}`) }))),
      el('div', { class: 'progress', role: 'progressbar', 'aria-valuenow': percent, 'aria-valuemin': 0, 'aria-valuemax': 100 },
        el('span', { style: `width:${percent}%` })),
      el('p', { class: 'note' }, screen.intro),
      ...screen.body,
      msg,
      el('div', { class: 'row', style: 'justify-content:space-between' }, back, next),
      // A real administrator may always leave: the panel opens for this session without
      // completing setup, and the Setup card then says it is not finished.
      S.needAccount ? null : el('button', { class: 'btn', type: 'button', style: 'align-self:flex-start', onclick: () => {
        sessionStorage.setItem('omni.setup.leave', '1');
        location.reload();
      } }, t('wizard.leave')));
  };

  // The page's language switch redraws the open step; values typed on it and
  // not yet saved are filled again from the saved settings.
  onLanguageChange(() => show());
  show();
}

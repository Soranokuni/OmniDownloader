/* First-setup wizard (plan P7.12).
 *
 * One wizard, three modes: beginner, intermediate and it. It renders into the
 * page's <main> in place of the panel, and only ever talks to the settings
 * endpoints through api(), which sets the CSRF header. Every value shown is a
 * text node built by el(); nothing here assembles markup from a string.
 */

import { api, el, render, toast } from '/static/app.js?v=6';

const MODES = [
  ['beginner', 'Beginner', 'The essentials with safe defaults: administrator, watchfolder and mail.'],
  ['intermediate', 'Intermediate', 'Plus service account, network access, archive and the language model.'],
  ['it', 'IT / sysadmin', 'Plus TLS, reverse proxy, metrics and an answer file for unattended installs.'],
];

const lines = (text) => text.split('\n').map((s) => s.trim()).filter(Boolean);

/** A labelled control with its one-line explanation and its default. */
function field(id, label, control, help, fallback) {
  return el('div', { class: 'field' },
    el('label', { for: id }, label),
    control,
    el('p', { class: 'note', style: 'margin:4px 0 0' },
      help, fallback ? ` Default: ${fallback}.` : ''));
}

const textInput = (id, value, placeholder, type = 'text', extra = {}) =>
  el('input', { type, id, value: value ?? '', placeholder: placeholder || null, autocomplete: 'off', ...extra });

/** Put `text` on the clipboard; plain HTTP on the LAN has no clipboard API. */
function copyText(text, button) {
  const done = () => { button.textContent = 'Copied'; setTimeout(() => { button.textContent = 'Copy'; }, 1500); };
  if (navigator.clipboard && window.isSecureContext) {
    navigator.clipboard.writeText(text).then(done, () => toast('Could not copy; select the text and copy it.', 'bad'));
    return;
  }
  const box = el('textarea', { readonly: '', style: 'position:fixed;left:-9999px' }, text);
  document.body.appendChild(box);
  box.select();
  try { document.execCommand('copy'); done(); } catch (e) { toast('Could not copy; select the text and copy it.', 'bad'); }
  box.remove();
}

/** A value to type into another program, with its Copy button. `value` may be a function (read at copy time). */
function copyable(value) {
  const now = () => (typeof value === 'function' ? value() : value);
  const code = el('code', { class: 'mono' }, now());
  const button = el('button', { class: 'btn', type: 'button', style: 'margin-left:6px;padding:2px 8px',
    onclick: (e) => { code.textContent = now(); copyText(now(), e.currentTarget); } }, 'Copy');
  const node = el('span', {}, code, button);
  node.update = () => { code.textContent = now(); };
  return node;
}

/** Read a chosen CSV file as text: UTF-8, else Greek Windows (what Excel's plain "CSV" writes). */
function readCsv(file) {
  const read = (encoding) => new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => resolve(String(r.result || ''));
    r.onerror = () => reject(new Error('The file could not be read.'));
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
    if (MODES.some((m) => m[0] === S.cfg.setup_mode)) S.mode = S.cfg.setup_mode;
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
      const choices = MODES.map(([id, name, text]) => el('label', {
        class: 'card', style: 'display:flex;gap:12px;align-items:flex-start;cursor:pointer;margin:0 0 10px',
      },
        el('input', {
          type: 'radio', name: 'wizard-mode', value: id, checked: S.mode === id,
          style: 'margin-top:3px',
          onchange: () => { S.mode = id; refresh(); },
        }),
        el('span', {}, el('strong', {}, name), el('br'), el('span', { class: 'note' }, text))));
      return {
        title: 'Welcome',
        intro: 'This sets up a new installation from the browser. Choose how much you want to see; you can run the wizard again later.',
        body: [...choices],
        valid: () => true,
        next: async () => {},
      };
    },

    account() {
      const name = textInput('wz-name', '', 'e.g. Maria Papadaki');
      const login = textInput('wz-login', '', 'e.g. station.admin');
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
        tick(tickLogin, r.login, 'Login is at least 3 characters, has no spaces, and is not "admin"');
        tick(tickLen, r.len, 'Password has at least 12 characters');
        tick(tickMatch, r.match, 'The two passwords match');
        refresh();
      };
      for (const i of [name, login, pw, pw2]) i.addEventListener('input', paint);
      queueMicrotask(paint);
      return {
        title: 'Administrator account',
        intro: 'The built-in admin / Admin account is replaced by your own. It is switched off as soon as the new account exists.',
        body: [
          field('wz-name', 'Full name', name, 'Shown in the audit log.'),
          field('wz-login', 'Login', login, 'What you type on the sign-in page. It cannot be "admin".'),
          field('wz-pw', 'Password', pw, 'At least 12 characters; a few ordinary words are fine.'),
          field('wz-pw2', 'Repeat the password', pw2, 'Type it again to be sure.'),
          tickLogin, tickLen, tickMatch,
          showPasswordCard ? el('p', { class: 'note' },
            'Prefer to keep the admin account and only change its password? ',
            el('button', { class: 'btn', type: 'button', onclick: () => showPasswordCard() }, 'Change the password instead'),
            ' (the rest of the wizard can be run later from the Setup card).') : null,
        ],
        valid: () => { const r = rules(); return r.name && r.login && r.len && r.match; },
        next: async () => {
          const made = await api('/api/setup/wizard/account', {
            method: 'POST',
            body: { full_name: name.value.trim(), login: login.value.trim(), password: pw.value },
          });
          if (made && made.signed_in === false) {
            // The account exists and the first-run one is off, but no session could be opened.
            toast(made.message || 'Your account was created; sign in with it.', 'ok');
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
      const advice = el('p', { class: 'note warn', hidden: true },
        'Access denied usually means the service runs as an account the share does not accept (a NAS, or a share addressed by IP, does not know the computer account). Change the account the service runs as to a Windows account that has Modify on the share: in Windows open Settings → Apps → Installed apps → OmniIngest → Modify (or run OmniIngest-Setup.exe /change-account from the installation folder) and choose "A Windows account…". Then test again. You can also skip this step for now and set the watchfolder later.');
      const paint = () => {
        result.className = `note ${S.watch.ok === true ? 'ok' : (S.watch.ok === false ? 'bad' : '')}`;
        result.textContent = S.watch.message;
        advice.hidden = !/access denied/i.test(S.watch.message || '');
      };
      const skipWarn = el('p', { class: 'note warn', hidden: true },
        `Finished videos then go to ${S.cfg.watchfolder_resolved || S.cfg.watchfolder_path}, a folder on this computer that playout never reads: nothing reaches Dalet until you set the share. Set it later from the Setup card.`);
      const skip = el('button', { class: 'btn', type: 'button', onclick: () => {
        if (skipWarn.hidden) { skipWarn.hidden = false; skip.textContent = 'Skip anyway'; return; }
        S.skipNow();
      } }, 'Skip for now');
      paint();
      path.addEventListener('input', () => {
        // Any edit voids the last test: Save needs a pass for exactly this text.
        S.watch.tested = false; S.watch.ok = null; S.watch.message = ''; paint(); refresh();
      });
      const test = el('button', { class: 'btn', type: 'button', onclick: async (e) => {
        const btn = e.currentTarget;
        btn.disabled = true; result.className = 'note'; result.textContent = 'Testing (up to 15 seconds)\u2026';
        try {
          const r = await api('/api/admin/settings/watchfolder/test', { method: 'POST', body: { path: path.value } });
          S.watch = { tested: r.ok === true, ok: r.ok === true, message: r.message, saved: false, text: path.value };
        } catch (err) {
          S.watch = { tested: false, ok: false, message: err.message, saved: false };
        }
        btn.disabled = false; paint(); refresh();
      } }, 'Test');
      return {
        title: 'Watchfolder',
        intro: 'Where finished MXF files are delivered for the playout system.',
        body: [
          field('wz-wf', 'Watchfolder path', path,
            'Use the share\u2019s \\\\server\\share path, never a mapped drive letter such as Z:. The service account needs Modify permission on the share.',
            'the "watchfolder" folder next to the program'),
          el('div', { class: 'row' }, test, skip),
          result, advice, skipWarn,
          el('p', { class: 'note' }, 'Test creates the folder if it is missing, writes a small probe file, renames it and deletes it. Next saves the path; Next is available only after a passing test.'),
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
      const tenant = textInput('wz-tenant', S.cfg.graph.tenant_id, 'Directory (tenant) ID');
      const client = textInput('wz-client', S.cfg.graph.client_id, 'Application (client) ID');
      const mailbox = textInput('wz-mailbox', S.cfg.graph.mailbox, 'ingest@yourstation.example');
      const secret = textInput('wz-secret', '', S.cfg.graph.secret_set ? '(stored; leave blank to keep it)' : '', 'password',
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
          result.textContent = 'Fill in tenant, client and mailbox, and the client secret (a new tenant or client needs its own secret).';
          return;
        }
        btn.disabled = true; result.className = 'note'; result.textContent = 'Testing\u2026';
        const tried = values();
        try {
          const r = await api('/api/system/test-email', { method: 'POST', body: {
            tenant_id: tenant.value.trim(), client_id: client.value.trim(),
            mailbox: mailbox.value.trim(), client_secret: secret.value,
          } });
          S.mailPassed = tried;
          result.className = 'note ok'; result.textContent = r.detail || 'The mailbox can be read.';
        } catch (err) { S.mailPassed = null; result.className = 'note bad'; result.textContent = err.message; }
        btn.disabled = false; refresh();
      } }, '\u00ab\u0394\u03bf\u03ba\u03b9\u03bc\u03ae \u03c3\u03cd\u03bd\u03b4\u03b5\u03c3\u03b7\u03c2\u00bb');
      for (const i of [tenant, client, mailbox, secret]) i.addEventListener('input', () => refresh());
      const warn = el('p', { class: 'note warn', hidden: true },
        'Without mail, no email is ingested: links must be added by hand on the MCR desk. You can add the mailbox later from this wizard or the Mailbox card.');
      // Two clicks: the first shows what skipping costs, the second skips.
      const skip = el('button', { class: 'btn', type: 'button', onclick: () => {
        if (warn.hidden) { warn.hidden = false; skip.textContent = 'Skip anyway'; return; }
        S.skipNow();
      } }, 'Skip for now');
      const policy = () => `New-ApplicationAccessPolicy -AppId ${client.value.trim() || '<client id>'} -PolicyScopeGroupId ${mailbox.value.trim() || '<mailbox>'} -AccessRight RestrictAccess -Description "OmniIngest"`;
      const policyLine = copyable(policy);
      for (const i of [client, mailbox]) i.addEventListener('input', () => policyLine.update());
      const steps = el('ol', { class: 'stack', style: 'margin:0;padding-left:20px' },
        el('li', {}, 'Sign in to entra.microsoft.com as an administrator. Open Identity \u2192 Applications \u2192 App registrations \u2192 New registration. Name: ',
          copyable('OmniIngest'), '. Supported account types: "Accounts in this organizational directory only". No redirect URI. Register.'),
        el('li', {}, 'In the new registration: API permissions \u2192 Add a permission \u2192 Microsoft Graph \u2192 Application permissions (not Delegated) \u2192 ',
          copyable('Mail.Read'), ' \u2192 Add permissions. Then press "Grant admin consent" and check that the status turns green.'),
        el('li', {}, 'Certificates & secrets \u2192 New client secret (24 months). Copy its Value (not the Secret ID) into Client secret below, and its Expires date into the date field.'),
        el('li', {}, 'Overview: copy Application (client) ID and Directory (tenant) ID into the fields below. Mailbox is the address the newsroom sends links to.'),
        el('li', {}, 'Recommended: allow the app to read only that mailbox. In Exchange Online PowerShell run ',
          policyLine, '.'),
        el('li', {}, 'Press \u00ab\u0394\u03bf\u03ba\u03b9\u03bc\u03ae \u03c3\u03cd\u03bd\u03b4\u03b5\u03c3\u03b7\u03c2\u00bb. Next is available once it passes for exactly the values below.'));
      return {
        title: 'Mail (Microsoft 365)',
        intro: 'The ingest mailbox is read through Microsoft Graph with an application registration and the application permission Mail.Read.',
        body: [
          steps,
          field('wz-tenant', 'Tenant ID', tenant, 'From the app registration\u2019s overview page.'),
          field('wz-client', 'Client ID', client, 'The application (client) ID of the same registration.'),
          field('wz-mailbox', 'Mailbox', mailbox, 'The address the newsroom sends links to.'),
          field('wz-secret', 'Client secret', secret, 'Stored encrypted on this machine; never shown again and never written to config.json.'),
          field('wz-secret-expires', 'Client secret expires on', expires,
            'The "Expires" date Entra shows for this secret under Certificates & secrets. Mail.Read cannot read it, so it is asked here: the mail status turns amber 30 days before and red 7 days before.'),
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
        placeholder: 'Surname\tFull name\tEmail\nPapadaki\tAnna Papadaki\tanna@newsroom.example' });
      if (P.previewText !== null) text.value = P.previewText;
      const file = el('input', { type: 'file', id: 'wz-people-file', accept: '.csv,.txt,text/csv,text/plain' });
      const out = el('div', { class: 'stack' });
      const paint = () => {
        const p = P.preview;
        if (!p) { render(out); return; }
        const row = (r) => el('div', { class: 'spec-row' }, el('span', {}, r.surname), el('span', {}, r.full_name),
          el('span', { class: 'note' }, r.emails.join(', ')));
        render(out,
          P.imported ? el('p', { class: 'note ok' }, `✓ ${p.add.length} added to the list.`) : null,
          p.add.length ? el('strong', {}, P.imported ? 'Added' : `To add (${p.add.length})`) : null,
          ...p.add.map(row),
          p.existing.length ? el('p', { class: 'note' }, `Already on the list, left as they are: ${p.existing.map((r) => r.surname).join(', ')}.`) : null,
          ...p.problems.map((q) => el('p', { class: 'note bad' }, `Line ${q.line}: ${q.message}`)),
          !P.imported && p.add.length === 0 ? el('p', { class: 'note' }, 'Nobody new to add.') : null);
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
      } }, 'Preview');
      const doImport = el('button', { class: 'btn btn-primary', type: 'button', onclick: async () => {
        try { await send(false); } catch (err) { toast(err.message, 'bad'); }
      } }, 'Import');
      const canImport = () => !P.imported && P.preview && P.preview.add.length > 0 && P.previewText === text.value;
      text.addEventListener('input', () => { P.imported = false; refreshButtons(); refresh(); });
      file.addEventListener('change', async () => {
        if (!file.files || !file.files[0]) return;
        try { text.value = await readCsv(file.files[0]); P.imported = false; await send(true); } catch (err) { toast(err.message, 'bad'); }
        refreshButtons();
      });
      const refreshButtons = () => { doImport.disabled = !canImport(); };
      const learnNote = el('p', { class: 'note warn', hidden: !P.learn },
        'Mail from an address that is not on the list is filed under MCR. On the Email tab of the MCR desk, MCR chooses the journalist once and can remember the sender: the next mail from that address goes to them by itself.');
      const learn = el('button', { class: 'btn', type: 'button', onclick: () => {
        P.learn = true; learnNote.hidden = false; refresh();
      } }, 'Learn them as mail arrives');
      queueMicrotask(() => { paint(); refreshButtons(); });
      return {
        title: 'Journalists',
        intro: 'Who sends links, so each video gets the right name in Dalet. Paste the list from Excel, choose a CSV file, or let the list grow as mail arrives.',
        body: [
          field('wz-people', 'Paste from Excel (surname, full name, email)', text,
            'One person per line. Several addresses go in further columns or in one cell separated by ";". Greek surnames are written in Latin capitals, as in the file names. A first line naming the columns is skipped.'),
          field('wz-people-file', 'Or choose a CSV file', file, 'Saved from Excel as "CSV UTF-8" or plain "CSV"; comma or semicolon.'),
          el('div', { class: 'row' }, preview, doImport, learn),
          out, learnNote,
          el('p', { class: 'note' }, 'People already on the list are never changed here; edit them on the desk. A copy of the list is kept before an import.'),
        ],
        valid: () => P.done || P.learn,
        next: async () => {},
      };
    },

    tools() {
      const box = el('div', { class: 'stack' }, el('p', { class: 'note' }, 'Checking the tools…'));
      const warn = el('p', { class: 'note warn', hidden: true },
        'Without these tools every video fails at the stage that needs one, and goes to review. Install them later from Maintenance → Toolchain.');
      const skip = el('button', { class: 'btn', type: 'button', onclick: () => {
        if (warn.hidden) { warn.hidden = false; skip.textContent = 'Skip anyway'; return; }
        S.skipNow();
      } }, 'Skip for now');
      let ready = false;
      let timer = 0;
      const load = async () => {
        clearTimeout(timer);
        if (!box.isConnected && S.step !== 'tools') return;
        let data;
        try { data = await api('/api/admin/tools'); } catch (err) { render(box, el('p', { class: 'note bad' }, err.message)); return; }
        const tools = data.tools || [];
        const busy = Boolean(data.running_tool);
        ready = tools.length > 0 && tools.every((t) => t.installed && !t.action);
        render(box, tools.map((t) => {
          const label = t.action === 'install' ? 'Install' : 'Repair';
          const state = !t.installed ? 'missing' : (t.action === 'repair' ? 'not the pinned build' : (t.version || 'installed'));
          const lines = [el('div', { class: 'spec-row' },
            el('span', {}, t.name),
            el('span', { class: t.installed && !t.action ? 'badge ok' : 'badge bad' }, state),
            t.pinned_version ? el('span', { class: 'note' }, `pinned ${t.pinned_version}`) : null,
            t.action ? el('button', { class: 'btn', type: 'button', disabled: busy, onclick: async () => {
              try { await api(`/api/admin/tools/${encodeURIComponent(t.name)}/install`, { method: 'POST' }); } catch (err) { toast(err.message, 'bad'); }
              load();
            } }, label) : null)];
          if (t.running) lines.push(el('p', { class: 'note' }, `${t.running.stage}…`));
          else if (t.last_result) lines.push(el('p', { class: t.last_result.ok ? 'note ok' : 'note bad' }, t.last_result.message));
          return el('div', { class: 'stack' }, lines);
        }), ready ? el('p', { class: 'note ok' }, '✓ Every tool is installed at the pinned build.') : null);
        refresh();
        // Polled only while an install runs, and only while this step is open.
        if (busy && S.step === 'tools') timer = setTimeout(load, 2000);
      };
      load();
      return {
        title: 'Tools',
        intro: 'The programs every video goes through: yt-dlp downloads, ffmpeg and ffprobe convert and check, bmxtranswrap writes the MXF. Each is installed at the build this version was tested with and checked against its fingerprint.',
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
        title: 'Network, archive and language model',
        intro: 'Network access and housekeeping. The defaults are right for a single machine.',
        body: [
          field('wz-port', 'Web port', port, 'The port the panels listen on. Needs a restart to change.', '8080'),
          field('wz-nets', 'MCR open networks (one per line)', nets,
            'Computers in these networks open the MCR desk without signing in. Leave empty to require a login for everyone.', '127.0.0.1/32 and ::1/128'),
          el('div', { class: 'field' },
            el('label', { class: 'checkbox-row' }, arch, el('span', {}, 'Keep delivered sources in the archive')),
            el('p', { class: 'note', style: 'margin:4px 0 0' }, 'A copy of each delivered source is kept for the days below. Default: on.')),
          field('wz-days', 'Archive retention (days)', days, 'How long a day\u2019s folder is kept.', '14'),
          el('div', { class: 'card', style: 'margin:0' },
            el('strong', {}, 'Language model (LLM assist)'),
            el('p', { class: 'note' }, 'Off, local (for example LM Studio or Ollama) or online: it only suggests a journalist or a keyword, and every suggestion is validated. It is set on the LLM card of the administration panel, which opens when you finish; the default is local assist.'),
            el('button', { class: 'btn', type: 'button', onclick: () => {
              // Same tab, so the session's "left the wizard" mark applies on the panel.
              sessionStorage.setItem('omni.setup.leave', '1');
              // Same page: changing only the fragment would not reload, and the wizard
              // has replaced the panel, so reload onto the LLM card.
              location.hash = 'llm';
              location.reload();
            } }, 'Leave the wizard and open the LLM card')),
          el('div', { class: 'card', style: 'margin:0' },
            el('strong', {}, 'Service account'),
            el('p', { class: 'note' }, 'A watchfolder on a NAS or an IP share needs the service to run as a Windows account that has Modify on that share. To change it: Windows Settings → Apps → Installed apps → OmniIngest → Modify (or OmniIngest-Setup.exe /change-account in the installation folder), then choose "A Windows account…". The Test button on the watchfolder step names the account to grant.')),
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
      const wide = el('p', { class: 'note warn', hidden: true },
        'Warning: a trusted-proxy range this wide lets every machine in it claim any client address. Name only the proxy itself (for example 10.0.0.5/32).');
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
      } }, 'Download answer file');
      return {
        title: 'TLS, reverse proxy and metrics',
        intro: 'For an IT-managed deployment. Leave everything as it is if you do not use these.',
        body: [
          field('wz-cert', 'TLS certificate (.pfx path)', cert, 'A PKCS#12 file holding certificate and key; empty serves plain HTTP. Needs a restart.', 'off'),
          field('wz-certpw', 'Certificate passphrase', pass, 'Stored encrypted; blank keeps the stored one.'),
          el('div', { class: 'field' },
            el('label', { class: 'checkbox-row' }, trust, el('span', {}, 'Trust the X-Forwarded-For header')),
            el('p', { class: 'note', style: 'margin:4px 0 0' }, 'Only from the proxies below; needs at least one. Default: off.')),
          field('wz-proxies', 'Trusted proxies (one network per line)', proxies, 'The reverse proxies whose forwarded address is believed.', 'none'),
          wide,
          field('wz-hosts', 'Allowed host names (one per line)', hosts, 'Extra DNS names the panel may be reached by. Needs a restart.', 'this machine\u2019s own names'),
          el('div', { class: 'field' },
            el('label', { class: 'checkbox-row' }, met, el('span', {}, 'Enable /metrics for monitoring')),
            el('p', { class: 'note', style: 'margin:4px 0 0' }, 'Prometheus text endpoint. Needs a restart. Default: off.')),
          field('wz-metnets', 'Networks allowed to scrape /metrics', metNets, 'An administrator session may always scrape.', '127.0.0.1/32 and ::1/128'),
          el('div', { class: 'row' }, download),
          el('p', { class: 'note' }, 'The answer file holds the settings chosen so far, and no secret. Download it after Next if you want the latest values.'),
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
      const list = el('div', { class: 'stack' }, el('p', { class: 'note' }, 'Checking\u2026'));
      const note = el('div', { class: 'stack' });
      (async () => {
        try {
          const status = await api('/api/system/status');
          const entries = Object.entries(status.checks || {});
          const cls = { ok: 'ok', degraded: 'warn', down: 'bad' };
          const word = { ok: 'green', degraded: 'amber', down: 'red' };
          render(list, entries.length === 0
            ? el('p', { class: 'note ok' }, 'All start-up checks passed.')
            : entries.map(([name, c]) => el('div', { class: 'spec-row' },
              el('span', { class: `badge ${cls[c.state] || ''}` }, word[c.state] || c.state),
              el('span', {}, name),
              el('span', { class: 'note' }, c.detail || ''))));
        } catch (err) { render(list, el('p', { class: 'note bad' }, err.message)); }
      })();
      if (S.restart) {
        note.append(el('p', { class: 'note warn' },
          'Restart required: the web port, TLS, allowed host names and metrics on/off are read once at start-up. After Finish, use «Επανεκκίνηση υπηρεσίας» on the Setup card (or restart the OmniIngest service in Services).'));
      }
      if (S.watchPending) {
        note.append(el('p', { class: 'note warn' }, S.watchPending));
      }
      // Repair runs in shadow on a new installation: it records what it would have
      // done and changes nothing (the Repair card has the mode).
      const repair = {
        off: 'off: a failed video goes to review with its reason.',
        shadow: 'shadow: when a video fails, repair works out what it would have done and records it, but changes nothing.',
        hold: 'hold: a video repair found is held for MCR’s approval before delivery.',
      };
      note.append(el('p', { class: 'note' }, `Repair mode (repair.mode) is ${repair[S.cfg.repair_mode] || S.cfg.repair_mode}`));
      return {
        title: 'Check',
        intro: 'The current state of the daemon. Amber and red lines say what to fix; the wizard can be run again from the Setup card. Next runs a test download.',
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
            status.className = 'note warn'; status.textContent = `«Επανασύνδεση…» (${err.message})`;
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
          status.textContent = `✓ Delivered: ${r.file_name}\nin ${r.watchfolder}\nCheck that Dalet shows it, then press Finish.`;
        } else if (failed) {
          status.className = 'note bad';
          status.textContent = `The test did not reach the watchfolder (${r.status}${r.error_code ? `, ${r.error_code}` : ''}).\n${r.error_message || ''}\nFix what it names (the earlier steps can be opened with Back) and start it again, or skip.`;
          start.disabled = false;
        } else {
          status.className = 'note';
          status.textContent = r.status === 'PENDING' ? 'Waiting for its turn in the queue…'
            : `${r.stage}${r.progress > 0 && r.progress < 100 ? ` · ${Math.round(r.progress)}%` : ''}…`;
          setTimeout(() => poll(gen), 1500);
        }
        refresh();
      };
      const start = el('button', { class: 'btn btn-primary', type: 'button', onclick: async () => {
        start.disabled = true;
        status.className = 'note'; status.textContent = 'Starting…';
        try {
          const r = await api('/api/admin/setup/test-download', { method: 'POST', body: { url: url.value.trim() } });
          T.jobId = r.job_id; T.status = null; S.cfg.setup_test_url = url.value.trim();
          T.poll += 1; poll(T.poll);
        } catch (err) { status.className = 'note bad'; status.textContent = err.message; start.disabled = false; }
      } }, 'Start the test download');
      const warn = el('p', { class: 'note warn', hidden: true },
        'Setup is then marked complete without proof that a video reaches playout. If the watchfolder, the share permissions or a tool is wrong, the first real video will show it instead. You can run this test later from the Setup card (Run setup wizard again).');
      const skip = el('button', { class: 'btn', type: 'button', onclick: () => {
        if (warn.hidden) { warn.hidden = false; skip.textContent = 'Skip and finish'; return; }
        S.finish({ skip_test: true }).catch((err) => toast(err.message, 'bad'));
      } }, 'Skip for now');
      paintStages(null, false);
      if (T.jobId) { start.disabled = true; T.poll += 1; poll(T.poll); }
      return {
        title: '«Δοκιμαστική λήψη»',
        intro: 'A known-good link goes through every stage, download to delivery, like any video. Setup is complete when its file is in the watchfolder.',
        body: [
          field('wz-test-url', 'Test link', url,
            'Saved as setup_test_url. Change it if this computer cannot reach YouTube.', 'YouTube’s first video, "Me at the zoo", 19 seconds'),
          el('p', { class: 'note warn' }, 'The file is delivered into the watchfolder like any video: Dalet imports it as 1_MCR_OMNIINGEST_TEST. Remove it in Dalet afterwards if you do not want to keep it.'),
          el('div', { class: 'row' }, start, skip),
          stages, status, events, warn,
        ],
        valid: () => Boolean(T.status && T.status.delivered),
        finishLabel: 'Finish',
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
    const back = el('button', { class: 'btn', type: 'button', onclick: () => { S.step = ids[idx - 1]; show(); } }, 'Back');
    if (idx === 0) back.disabled = true;
    const next = el('button', { class: 'btn btn-primary', type: 'button', onclick: async () => {
      next.disabled = true; msg.textContent = '';
      try {
        await screen.next();
        if (!last) { S.step = ids[idx + 1]; show(); }
      } catch (err) { msg.textContent = err.message; next.disabled = !screen.valid(); }
    } }, screen.finishLabel || 'Next');
    refresh = () => { next.disabled = !screen.valid(); };
    S.skipNow = async () => {
      // "Skip for now" on the mail step: nothing is saved, on to the next screen.
      S.step = ids[idx + 1]; show();
    };
    refresh();
    render(frame,
      el('div', { class: 'row', style: 'justify-content:space-between' },
        el('h2', { class: 'card-title', style: 'border:none;margin:0;padding:0' }, screen.title),
        el('span', { class: 'note' }, `Step ${idx + 1} of ${ids.length} \u00b7 ${MODES.find((m) => m[0] === S.mode)[1]}`)),
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
      } }, 'Open the admin panel'));
  };

  show();
}

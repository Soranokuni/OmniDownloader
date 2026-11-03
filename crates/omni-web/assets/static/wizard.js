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
    ids.push('watchfolder', 'mail');
    if (S.mode !== 'beginner') ids.push('network');
    if (S.mode === 'it') ids.push('it');
    ids.push('check');
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
        'Access denied usually means the service runs as an account the share does not accept (a NAS, or a share addressed by IP, does not know the computer account). Reinstall the service as a named account that has Modify on the share: omni-ingest.exe service install --account DOMAIN\\name (it asks for the password). Then test again. You can also skip this step for now and set the watchfolder later.');
      const paint = () => {
        result.className = `note ${S.watch.ok === true ? 'ok' : (S.watch.ok === false ? 'bad' : '')}`;
        result.textContent = S.watch.message;
        advice.hidden = !/access denied/i.test(S.watch.message || '');
      };
      const skipWarn = el('p', { class: 'note warn', hidden: true },
        `Files keep going to ${S.cfg.watchfolder_resolved || S.cfg.watchfolder_path} until you set the share. If that is not the folder Dalet watches, nothing reaches Dalet. Set it later from the Setup card.`);
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
      const result = el('p', { class: 'note', style: 'white-space:pre-line' });
      const filled = () => [tenant, client, mailbox].map((i) => i.value.trim());
      const complete = () => filled().every(Boolean) && (secret.value.trim() || S.cfg.graph.secret_set);
      for (const i of [tenant, client, mailbox, secret]) i.addEventListener('input', () => refresh());
      const test = el('button', { class: 'btn', type: 'button', onclick: async (e) => {
        const btn = e.currentTarget;
        btn.disabled = true; result.className = 'note'; result.textContent = 'Testing\u2026';
        try {
          const r = await api('/api/system/test-email', { method: 'POST', body: {
            tenant_id: tenant.value.trim(), client_id: client.value.trim(),
            mailbox: mailbox.value.trim(), client_secret: secret.value,
          } });
          result.className = 'note ok'; result.textContent = r.detail || 'The mailbox can be read.';
        } catch (err) { result.className = 'note bad'; result.textContent = err.message; }
        btn.disabled = false;
      } }, 'Test connection');
      const warn = el('p', { class: 'note warn', hidden: true },
        'Without mail, no email is ingested: links must be added by hand on the MCR desk. You can add the mailbox later from this wizard.');
      // Two clicks: the first shows what skipping costs, the second skips.
      const skip = el('button', { class: 'btn', type: 'button', onclick: () => {
        if (warn.hidden) { warn.hidden = false; skip.textContent = 'Skip anyway'; return; }
        S.skipNow();
      } }, 'Skip for now');
      return {
        title: 'Mail (Microsoft 365)',
        intro: 'The ingest mailbox is read through Microsoft Graph with an application registration (Mail.Read).',
        body: [
          field('wz-tenant', 'Tenant ID', tenant, 'From the app registration\u2019s overview page.'),
          field('wz-client', 'Client ID', client, 'The application (client) ID of the same registration.'),
          field('wz-mailbox', 'Mailbox', mailbox, 'The address the newsroom sends links to.'),
          field('wz-secret', 'Client secret', secret, 'Stored encrypted on this machine; never shown again and never written to config.json.'),
          el('div', { class: 'row' }, test,
            skip),
          result, warn,
        ],
        valid: () => complete() || filled().every((v) => !v),
        next: async () => {
          if (filled().every((v) => !v)) return;   // blank: no mail, nothing to save
          const r = await api('/api/admin/settings/mail', { method: 'POST', body: {
            tenant_id: tenant.value.trim(), client_id: client.value.trim(),
            mailbox: mailbox.value.trim(), client_secret: secret.value,
          } });
          secret.value = '';
          if (r.restart_required) S.restart = true;
          S.cfg.graph = { ...S.cfg.graph, tenant_id: tenant.value.trim(), client_id: client.value.trim(),
            mailbox: mailbox.value.trim(), secret_set: r.secret_set };
        },
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
            el('p', { class: 'note' }, 'A watchfolder on a NAS or an IP share needs the service to run as a named account that has Modify on that share. Reinstall the service with: omni-ingest.exe service install --account DOMAIN\\name (it asks for the password). The Test button on the watchfolder step names the account to grant.')),
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
          'Restart required: some settings you changed are read once at start-up. Restart the OmniIngest service (services.msc, or: omni-ingest.exe service stop, then start) for them to take effect.'));
      }
      return {
        title: 'Check and finish',
        intro: 'The current state of the daemon. Amber and red lines say what to fix; the wizard can be run again from the Setup card.',
        body: [list, note],
        valid: () => true,
        finishLabel: 'Finish',
        next: async () => {
          await api('/api/admin/settings/complete', { method: 'POST', body: { mode: S.mode } });
          location.reload();
        },
      };
    },
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

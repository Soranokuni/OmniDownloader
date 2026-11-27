/* Admin panel, «Συντήρηση» (P13.5): toolchain, download engine, database,
 * diagnostics, scheduled tasks, benchmarks, the audit log and the mail log.
 * Loaded when the section is first opened. Words by key from
 * admin-text.*.js (P13.6b); what the server reports in its own words (task
 * descriptions, audit messages, error codes) is shown as it comes.
 */

import { api, el, render, toast, fmtTime } from '/static/app.js?v=6';
import { sectionShown } from '/static/admin-common.js?v=6';
import { confirmDialog } from '/static/dialog.js?v=11';
import { t } from '/static/admin-text.js?v=6';

/* ------------------------------------------------------------------ *
 * Dependencies
 * ------------------------------------------------------------------ */

let toolsTimer = null;

const megabytes = (n) => `${Math.round((n || 0) / 1048576)} MB`;

function toolRow(tool, busy) {
  const pin = tool.matches_pin === true ? t('admin.tools.pinned')
    : (tool.installed && tool.matches_pin === false ? t('wizard.tools.not_pinned') : null);
  const label = tool.action === 'install' ? t('wizard.tools.install') : t('wizard.tools.repair');
  const lines = [
    el('div', { class: 'spec-row' },
      el('span', {}, tool.name),
      el('span', { class: tool.installed && tool.action !== 'repair' ? 'badge ok' : 'badge bad' },
        tool.installed ? (tool.version || t('admin.tools.does_not_run')) : t('wizard.tools.missing')),
      pin ? el('span', { class: 'note' }, pin) : null,
      tool.action
        ? el('button', {
            class: 'btn', type: 'button', disabled: busy,
            onclick: () => installTool(tool.name, label),
          }, label)
        : null),
  ];
  if (tool.note) lines.push(el('p', { class: 'note' }, tool.note));
  if (tool.running) {
    const r = tool.running;
    const sized = r.bytes_total > 0 && r.stage === 'downloading'
      ? ` ${t('admin.tools.of', { done: megabytes(r.bytes_done), total: megabytes(r.bytes_total) })}` : '';
    lines.push(el('p', { class: 'note' }, `${r.stage}…${sized}`));
  } else if (tool.last_result) {
    lines.push(el('p', { class: tool.last_result.ok ? 'note ok' : 'note bad' }, tool.last_result.message));
  }
  return el('div', { class: 'stack' }, lines);
}

async function loadTools() {
  clearTimeout(toolsTimer);
  let data;
  try {
    data = await api('/api/admin/tools');
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    render('dependencies', el('p', { class: 'note' }, t('admin.tools.none')));
    return;
  }
  const tools = data.tools || [];
  const busy = Boolean(data.running_tool);
  render('dependencies', tools.map((tool) => toolRow(tool, busy)));
  // Polled only while an install runs.
  if (busy) toolsTimer = setTimeout(loadTools, 2000);
}

async function installTool(name, label) {
  if (!await confirmDialog(t('admin.tools.ask', { action: label, name }), { okLabel: label })) return;
  try {
    await api(`/api/admin/tools/${encodeURIComponent(name)}/install`, { method: 'POST' });
    toast(t('admin.tools.started', { action: label, name }), 'ok');
  } catch (e) {
    toast(e.message, 'bad');
  }
  loadTools();
}

async function loadDependencies() {
  // The toolchain rows (with Install / Repair) come from /api/admin/tools.
  loadTools();

  let data;
  try {
    data = await api('/api/admin/dependencies');
  } catch (e) {
    if (e.code !== 'UNAUTHENTICATED') toast(e.message, 'bad');
    document.getElementById('ytdl-installed').textContent = t('admin.ytdl.unavailable');
    document.getElementById('ytdl-remote').textContent = t('admin.ytdl.unavailable');
    return;
  }

  document.getElementById('ytdl-installed').textContent = data.ytdl_installed || t('admin.ytdl.unknown');
  document.getElementById('ytdl-remote').textContent = data.ytdl_remote || t('admin.ytdl.unknown');

  const note = document.getElementById('ytdl-note');
  if (data.ytdl_update_available) {
    note.textContent = t('admin.ytdl.newer');
    note.className = 'note ok';
  } else {
    note.textContent = t('admin.ytdl.current');
    note.className = 'note';
  }

}

document.getElementById('ytdl-update').addEventListener('click', async (event) => {
  const button = event.currentTarget;
  button.disabled = true;
  button.textContent = t('admin.ytdl.updating');
  try {
    await api('/api/admin/update-ytdl', { method: 'POST' });
    toast(t('admin.ytdl.updated'), 'ok');
    loadDependencies();
  } catch (e) {
    toast(e.message, 'bad');
  } finally {
    button.disabled = false;
    button.textContent = t('admin.ytdl.update');
  }
});

document.getElementById('ytdl-rollback').addEventListener('click', async () => {
  if (!await confirmDialog(t('admin.ytdl.rollback_ask'), { okLabel: t('admin.ytdl.rollback_ok') })) return;
  try {
    await api('/api/admin/rollback-ytdl', { method: 'POST' });
    toast(t('admin.ytdl.rolled_back'), 'ok');
    loadDependencies();
  } catch (e) { toast(e.message, 'bad'); }
});

/* ------------------------------------------------------------------ *
 * Maintenance
 * ------------------------------------------------------------------ */

document.getElementById('purge-btn').addEventListener('click', async () => {
  const days = Number(document.getElementById('purge-days').value);
  if (!await confirmDialog(t('admin.db.purge_ask', { days }), { okLabel: t('admin.db.purge'), danger: true })) return;
  try {
    const result = await api('/api/admin/purge', {
      method: 'POST',
      body: { older_than_days: days },
    });
    toast(t('admin.db.purged', { n: result.purged_count }), 'ok');
    loadLogs();
  } catch (e) { toast(e.message, 'bad'); }
});

document.getElementById('vacuum-btn').addEventListener('click', async () => {
  try {
    await api('/api/admin/vacuum', { method: 'POST' });
    toast(t('admin.db.compacted'), 'ok');
  } catch (e) { toast(e.message, 'bad'); }
});

/* ------------------------------------------------------------------ *
 * Scheduled maintenance
 * ------------------------------------------------------------------ */

const OUTCOME_CLASS = { ok: 'ok', skipped: '', failed: 'bad' };
// Each outcome's word is `admin.tasks.outcome.<outcome>`.
const outcomeWord = (o) => (o in OUTCOME_CLASS ? t(`admin.tasks.outcome.${o}`) : o);

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
      el('td', { colspan: '5', class: 'empty' }, t('admin.tasks.none'))));
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
          }, outcomeWord(task.last_outcome))
        : el('span', { class: 'note' }, t('admin.tasks.never')),
    ),
    el('td', { class: 'right' },
      el('button', {
        class: 'btn',
        type: 'button',
        onClick: () => runTask(task.name),
      }, t('admin.sites.run_now')),
    ),
  )));
}

async function runTask(name) {
  try {
    const result = await api(`/api/admin/maintenance/${encodeURIComponent(name)}/run`, {
      method: 'POST',
    });
    toast(result.message || t('admin.tasks.scheduled'), 'ok');
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
      el('td', { colspan: '4', class: 'empty' }, t('admin.audit.none'))));
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
    ? el('tr', {}, el('td', { colspan: '6', class: 'empty' }, t('admin.mail_log.none')))
    : mails.map((m) => el('tr', {},
      el('td', { class: 'num' }, fmtTime(m.processed_at)),
      el('td', {}, m.subject || t('admin.mail_log.no_subject')),
      el('td', { class: 'mono' }, m.from_address || ''),
      el('td', {}, el('span', { class: m.outcome === 'JOBS' ? 'badge ok' : (m.outcome === 'FAILED' ? 'badge bad' : 'badge warn') }, m.outcome), ' ',
        // Read again once already (P11.11b) and still failed: the system leaves it to a person.
        m.read_again && !m.reprocess_pending
          ? el('span', { class: 'badge warn', title: t('admin.mail_log.person_title') }, t('admin.mail_log.person'))
          : ''),
      el('td', { class: 'num' }, String(m.jobs)),
      el('td', { class: 'right' }, m.reprocess_pending
        ? el('span', { class: 'badge info' }, t('admin.mail_log.next_poll'))
        : el('button', { class: 'btn', type: 'button', onClick: () => reprocessMail(m) }, t('admin.mail_log.reprocess'))),
    )));
}

async function reprocessMail(m) {
  try {
    await api('/api/admin/mail/reprocess', { method: 'POST', body: { internet_message_id: m.internet_message_id } });
    toast(t('admin.mail_log.queued', { subject: m.subject || t('admin.mail_log.no_subject') }), 'ok');
    loadMail();
  } catch (e) { toast(e.message, 'bad'); }
}

document.getElementById('mail-refresh').addEventListener('click', loadMail);

/* ------------------------------------------------------------------ *
 * Benchmarks (plan P6.4): the stored 65-link corpus, run on demand
 * ------------------------------------------------------------------ */

// The run whose report is open; the newest one until the admin picks another.
let benchSelected = null;
let benchBusy = false;
let benchTimer = 0;
const BENCH_POLL_MS = 10000;
const benchResult = document.getElementById('bench-result');

function showBench(message, ok) {
  benchResult.textContent = message;
  benchResult.className = ok ? 'note ok' : 'note bad';
}

const benchSecs = (ms) => (ms === null || ms === undefined ? '' : (ms / 1000).toFixed(1));
const benchPct = (v) => (v === null || v === undefined ? '' : `${v.toFixed(1)} %`);
const benchYes = (v) => (v === null || v === undefined ? '' : v ? t('admin.bench.yes') : t('admin.bench.no'));
const benchMode = (mode) => (mode === 'full' ? t('admin.bench.mode.full') : t('admin.bench.mode.extract'));

function benchVerdict(pass) {
  if (pass === true) return el('span', { class: 'badge ok' }, 'PASS');
  if (pass === false) return el('span', { class: 'badge bad' }, 'FAIL');
  return el('span', { class: 'badge' }, 'n/a');
}

function benchList(indices) {
  return indices && indices.length ? indices.map((i) => `#${i}`).join(', ') : t('admin.bench.list_none');
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
      `${t('admin.bench.run_title', { id: run.id, mode: benchMode(run.mode) })} `, benchVerdict(summary.pass)),
    el('p', { class: 'note' }, t('admin.bench.run_about', {
      state: run.state, started: fmtTime(run.started_at), finished: fmtTime(run.finished_at),
      run: summary.targets_run, counted: summary.counted,
    })),
    benchTable([t('admin.bench.threshold'), t('admin.bench.limit'), t('admin.bench.measured'), t('admin.bench.result')], summary.checks.map((c) => [
      c.name,
      `${c.op} ${c.limit}${c.unit ? ` ${c.unit}` : ''}`,
      c.value === null || c.value === undefined ? '' : `${c.value.toFixed(1)}${c.unit ? ` ${c.unit}` : ''}`,
      benchVerdict(c.pass),
    ])),
  ];
  const signed = (v, unit) => (v === null || v === undefined ? 'n/a' : `${v > 0 ? '+' : ''}${v.toFixed(1)}${unit}`);
  if (deltas) {
    parts.push(el('p', { class: 'note' }, t('admin.bench.deltas', {
      id: deltas.previous_run_id, mode: benchMode(run.mode),
      extract: signed(deltas.extract_pct, ''), delivery: signed(deltas.delivery_pct, ''),
      mean: signed(deltas.mean_extract_ms === null ? null : deltas.mean_extract_ms / 1000, ' s'),
      fails: deltas.compliance_fails === null ? 'n/a' : (deltas.compliance_fails > 0 ? '+' : '') + deltas.compliance_fails,
      worse: benchList(deltas.regressions), better: benchList(deltas.fixes),
    })));
  } else {
    parts.push(el('p', { class: 'note' }, t('admin.bench.no_earlier', { mode: benchMode(run.mode) })));
  }
  const tm = summary.timings;
  parts.push(
    el('h4', { class: 'note strong' }, t('admin.bench.mean_title')),
    benchTable([t('admin.bench.col.extract'), t('admin.bench.col.download'), t('admin.bench.col.transcode'), t('admin.bench.col.per_target')], [[
      benchSecs(tm.extract_mean_ms), benchSecs(tm.download_mean_ms), benchSecs(tm.transcode_mean_ms), benchSecs(tm.total_mean_ms),
    ]]),
    el('h4', { class: 'note strong' }, t('admin.bench.by_domain')),
    benchTable([t('admin.bench.col.domain'), t('admin.bench.col.targets'), t('admin.bench.col.extracted'), t('admin.bench.col.delivery'), t('admin.bench.col.mean_extract')],
      summary.by_domain.map((d) => [
        d.domain, String(d.targets), String(d.extract_ok), String(d.delivery_ok), benchSecs(d.mean_extract_ms),
      ])),
    summary.skipped && summary.skipped.length
      ? el('p', { class: 'note' }, t('admin.bench.skipped', { list: benchList(summary.skipped) }))
      : null,
    el('h4', { class: 'note strong' }, t('admin.bench.by_target')),
    benchTable(
      ['#', t('admin.bench.col.domain'), t('admin.bench.col.category'), t('admin.bench.col.expected'), t('admin.bench.col.extract'), 's',
        t('admin.bench.col.method'), t('admin.bench.col.streams'), t('admin.bench.col.download'), 's', t('admin.bench.col.transcode_s'),
        t('admin.bench.col.verify'), t('admin.bench.col.total_s'), t('admin.bench.col.error'), t('admin.bench.col.host')],
      targets.map((l) => [
        String(l.idx), l.domain, l.category, l.expected === 'expected_fail' ? t('admin.bench.expected_fail') : t('admin.bench.expected_pass'),
        benchYes(l.extract_ok), benchSecs(l.extract_ms), l.method || '', l.candidates === null || l.candidates === undefined ? '' : String(l.candidates),
        benchYes(l.download_ok), benchSecs(l.download_ms), benchSecs(l.transcode_ms), benchYes(l.verify_ok), benchSecs(l.total_ms),
        l.skipped ? t('admin.bench.too_long') : (l.error_code || ''), l.stream_host || '',
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

// Each run state's word is `admin.bench.state.<state>`.
const BENCH_STATES = ['running', 'done', 'failed', 'abandoned'];
const benchState = (r) => (r.state === 'running' && r.paused ? t('admin.bench.paused')
  : (BENCH_STATES.includes(r.state) ? t(`admin.bench.state.${r.state}`) : r.state));

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
      render('bench-runs', el('tr', {}, el('td', { colspan: '9', class: 'empty' }, t('admin.bench.none'))));
      render('bench-report', []);
      render('bench-progress', []);
      return;
    }
    render('bench-runs', runs.map((r) => {
      const open = el('button', { class: 'btn', type: 'button' }, t('admin.bench.report'));
      open.addEventListener('click', () => { benchSelected = r.id; loadBenchmarks(); });
      return el('tr', {},
        el('td', { class: 'strong' }, String(r.id)),
        el('td', {}, benchMode(r.mode)),
        el('td', {}, el('span', { class: r.state === 'done' ? 'badge ok' : r.state === 'running' ? 'badge' : 'badge bad' }, benchState(r))),
        el('td', {}, fmtTime(r.started_at)),
        el('td', { class: 'right' }, r.targets_run === null || r.targets_run === undefined ? '' : String(r.targets_run)),
        el('td', { class: 'right' }, benchPct(r.extract_pct)),
        el('td', { class: 'right' }, benchPct(r.delivery_pct)),
        el('td', {}, r.state === 'done' ? benchVerdict(r.pass) : ''),
        el('td', {}, open));
    }));
    // Progress of the run that is going: results are stored as each target finishes.
    const going = runs.find((r) => r.state === 'running');
    // P13.5: polled only while a run is going and this section is open, never per
    // server event (the panel used to refetch the whole card on every job change).
    clearTimeout(benchTimer);
    if (going && sectionShown('maintenance')) benchTimer = setTimeout(loadBenchmarks, BENCH_POLL_MS);
    if (going) {
      const of = going.params && going.params.targets ? going.params.targets.length : 0;
      try {
        const current = await api(`/api/admin/benchmarks/runs/${going.id}`);
        render('bench-progress', el('span', {},
          t('admin.bench.progress', { id: going.id, done: current.summary.targets_run, of })
          + (going.paused ? ` ${t('admin.bench.progress_paused')}` : '')));
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
    showBench(t('admin.bench.started', { id: out.run_id, n: out.targets, parallel: out.parallel }), true);
  } catch (e) {
    // 409 carries the reason (a job is running, or a run is going).
    showBench(e.message, false);
  } finally {
    button.disabled = false;
  }
  loadBenchmarks();
});


let logsTimer = 0;

/** The audit log refreshes every 15 s while this section is open. */
function pollLogs() {
  clearTimeout(logsTimer);
  if (!sectionShown('maintenance')) return;
  loadLogs();
  logsTimer = setTimeout(pollLogs, 15000);
}

/** The section opened (again): load every card; the pollers start. */
export function init() {
  loadDependencies();
  loadMaintenance();
  loadMail();
  loadBenchmarks();
  pollLogs();
}

/** The section was opened again: the pollers that stopped start once more. */
export function shown() {
  loadBenchmarks();
  pollLogs();
}

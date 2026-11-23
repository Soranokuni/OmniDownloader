/* Admin panel, «Συντήρηση» (P13.5): toolchain, download engine, database,
 * diagnostics, scheduled tasks, benchmarks, the audit log and the mail log.
 * Loaded when the section is first opened.
 */

import { api, el, render, toast, fmtTime } from '/static/app.js?v=6';
import { sectionShown } from '/static/admin-common.js?v=1';
import { confirmDialog } from '/static/dialog.js?v=10';

/* ------------------------------------------------------------------ *
 * Dependencies
 * ------------------------------------------------------------------ */

let toolsTimer = null;

const megabytes = (n) => `${Math.round((n || 0) / 1048576)} MB`;

function toolRow(tool, busy) {
  const pin = tool.matches_pin === true ? 'pinned build'
    : (tool.installed && tool.matches_pin === false ? 'not the pinned build' : null);
  const label = tool.action === 'install' ? 'Install' : 'Repair';
  const lines = [
    el('div', { class: 'spec-row' },
      el('span', {}, tool.name),
      el('span', { class: tool.installed && tool.action !== 'repair' ? 'badge ok' : 'badge bad' },
        tool.installed ? (tool.version || 'does not run') : 'missing'),
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
      ? ` ${megabytes(r.bytes_done)} of ${megabytes(r.bytes_total)}` : '';
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
    render('dependencies', el('p', { class: 'note' }, 'No toolchain information.'));
    return;
  }
  const tools = data.tools || [];
  const busy = Boolean(data.running_tool);
  render('dependencies', tools.map((tool) => toolRow(tool, busy)));
  // Polled only while an install runs.
  if (busy) toolsTimer = setTimeout(loadTools, 2000);
}

async function installTool(name, label) {
  if (!await confirmDialog(`${label} ${name}? Jobs wait while the file is swapped.`, { okLabel: label })) return;
  try {
    await api(`/api/admin/tools/${encodeURIComponent(name)}/install`, { method: 'POST' });
    toast(`${label} of ${name} started.`, 'ok');
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
  if (!await confirmDialog('Put the previous yt-dlp build back?', { okLabel: 'Roll back' })) return;
  try {
    await api('/api/admin/rollback-ytdl', { method: 'POST' });
    toast('Rolled back to the previous yt-dlp build.', 'ok');
    loadDependencies();
  } catch (e) { toast(e.message, 'bad'); }
});

/* ------------------------------------------------------------------ *
 * Maintenance
 * ------------------------------------------------------------------ */

document.getElementById('purge-btn').addEventListener('click', async () => {
  const days = Number(document.getElementById('purge-days').value);
  if (!await confirmDialog(`Permanently remove completed jobs older than ${days} days?`, { okLabel: 'Purge', danger: true })) return;
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
      el('td', {}, el('span', { class: m.outcome === 'JOBS' ? 'badge ok' : (m.outcome === 'FAILED' ? 'badge bad' : 'badge warn') }, m.outcome), ' ',
        // Read again once already (P11.11b) and still failed: the system leaves it to a person.
        m.read_again && !m.reprocess_pending
          ? el('span', { class: 'badge warn', title: 'Read again once (after a restart, a tool update or on request); it still failed. The system will not try it again on its own.' }, 'needs a person')
          : ''),
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
    // P13.5: polled only while a run is going and this section is open, never per
    // server event (the panel used to refetch the whole card on every job change).
    clearTimeout(benchTimer);
    if (going && sectionShown('maintenance')) benchTimer = setTimeout(loadBenchmarks, BENCH_POLL_MS);
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

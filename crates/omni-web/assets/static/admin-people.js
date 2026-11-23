/* Admin panel, «Άνθρωποι» (P13.5): accounts, journalists and groups.
 * Loaded when the section is first opened. Words by key from
 * admin-text.*.js (P13.6b).
 */

import { api, el, render, toast } from '/static/app.js?v=6';
import { saveJson } from '/static/admin-common.js?v=4';
import { confirmDialog, promptDialog } from '/static/dialog.js?v=10';
import { t } from '/static/admin-text.js?v=4';

/* ------------------------------------------------------------------ *
 * Users
 * ------------------------------------------------------------------ */

// Each role's name is `admin.role.<role>`.
const ROLES = ['admin', 'open_mcr', 'user'];
const roleLabel = (role) => (ROLES.includes(role) ? t(`admin.role.${role}`) : role);

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
      el('td', { colspan: '6', class: 'empty' }, t('admin.users.none'))));
    return;
  }

  render('users-body', users.map((user) => el('tr', {},
    el('td', { class: 'strong' }, user.email),
    el('td', {}, user.full_name),
    el('td', {}, el('span', { class: user.role === 'admin' ? 'badge bad' : 'badge' }, roleLabel(user.role))),
    el('td', {}, user.journalist_surname || '—'),
    el('td', {}, el('span', { class: user.is_active ? 'badge ok' : 'badge bad' },
      user.is_active ? t('admin.users.active') : t('admin.users.inactive'))),
    el('td', { class: 'right' },
      el('div', { class: 'row tight', style: 'justify-content:flex-end' },
        el('button', {
          class: 'btn',
          type: 'button',
          onClick: () => resetPassword(user.id, user.email),
        }, t('admin.users.reset')),
        el('button', {
          class: user.is_active ? 'btn btn-danger' : 'btn',
          type: 'button',
          onClick: () => setActive(user, !user.is_active),
        }, user.is_active ? t('admin.users.deactivate') : t('admin.users.reactivate')),
      ),
    ),
  )));
}

async function setActive(user, active) {
  if (!active && !await confirmDialog(t('admin.users.deactivate_ask', { email: user.email }), { okLabel: t('admin.users.deactivate'), danger: true })) return;
  try {
    await api(`/api/admin/users/${user.id}/active`, { method: 'POST', body: { active } });
    toast(t(active ? 'admin.users.reactivated' : 'admin.users.deactivated', { email: user.email }), 'ok');
    loadUsers();
  } catch (e) { toast(e.message, 'bad'); }
}

async function resetPassword(id, email) {
  const next = await promptDialog(t('admin.users.reset_ask', { email }), {
    type: 'password',
    okLabel: t('admin.users.reset'),
    validate: (v) => (v.length >= 12 ? '' : t('admin.users.reset_short')),
  });
  if (next === null) return;
  try {
    await api(`/api/admin/users/${id}/password`, { method: 'POST', body: { password: next } });
    // The server also ends that account's sessions, so the operator is not
    // left believing a reset took effect while the old session is still live.
    toast(t('admin.users.reset_done', { email }), 'ok');
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
    toast(t('admin.users.created'), 'ok');
    event.target.reset();
    userModal.hidden = true;
    loadUsers();
  } catch (e) { toast(e.message, 'bad'); }
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

// Each group kind's name is `admin.groups.kind.<kind>`.
const KINDS = ['news', 'show', 'desk', 'other'];
const kindLabel = (kind) => (KINDS.includes(kind) ? t(`admin.groups.kind.${kind}`) : kind);

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
    ? el('tr', {}, el('td', { colspan: '6', class: 'empty' }, t('admin.groups.none')))
    : groups.map((g) => el('tr', {},
      el('td', { class: 'mono strong' }, g.code),
      el('td', {}, g.name),
      el('td', {}, el('span', { class: 'badge info', title: g.kind }, kindLabel(g.kind))),
      el('td', {}, (g.keywords || []).join(', ')),
      el('td', {}, g.description || ''),
      el('td', { class: 'right' },
        el('div', { class: 'row tight', style: 'justify-content:flex-end' },
          el('button', { class: 'btn', type: 'button', onClick: () => editGroup(g) }, t('admin.edit')),
          el('button', { class: 'btn btn-danger', type: 'button', onClick: () => deleteGroup(g.code) }, t('admin.delete')),
        )),
    )));

  const staff = people.filter((p) => p.surname !== 'MCR');
  const known = new Set(groups.map((g) => g.code));
  render('people-body', staff.length === 0
    ? el('tr', {}, el('td', { colspan: '7', class: 'empty' }, t('admin.people.none')))
    : staff.map((p) => el('tr', {},
      el('td', { class: 'mono strong' }, p.surname),
      el('td', {}, p.full_name),
      el('td', { class: 'mono' }, (p.emails || []).join(', ') || '—'),
      el('td', {}, (p.aliases || []).join(', ') || '—'),
      el('td', {}, (p.groups || []).length
        ? (p.groups || []).map((code, i) => el('span', {
          class: known.has(code) ? (i === 0 ? 'badge info' : 'badge') : 'badge bad',
          style: 'margin-right:4px',
          title: i === 0 ? t('admin.people.default_group') : '',
        }, code))
        : '—'),
      el('td', { class: 'num' }, String(p.default_priority || 0)),
      el('td', { class: 'right' },
        el('div', { class: 'row tight', style: 'justify-content:flex-end' },
          el('button', { class: 'btn', type: 'button', onClick: () => editPerson(p) }, t('admin.edit')),
          el('button', { class: 'btn btn-danger', type: 'button', onClick: () => deletePerson(p.surname) }, t('admin.delete')),
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
    toast(t('admin.people.saved', { name: surname }), 'ok');
    pf('person-form').reset();
    loadTaxonomy();
  } catch (e) { toast(e.message, 'bad'); }
});

async function deletePerson(surname) {
  if (!await confirmDialog(t('admin.people.delete_ask', { name: surname }), { okLabel: t('admin.delete'), danger: true })) return;
  try {
    await api(`/api/journalists/${encodeURIComponent(surname)}`, { method: 'POST' });
    toast(t('admin.people.deleted', { name: surname }), 'ok');
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
    ? el('tr', {}, el('td', { colspan: '4', class: 'empty' }, t('admin.backups.none')))
    : backups.map((b) => el('tr', {},
      el('td', { class: 'mono' }, b.name),
      el('td', { class: 'num' }, String(b.groups)),
      el('td', { class: 'num' }, String(b.people)),
      el('td', { class: 'right' },
        el('div', { class: 'row tight', style: 'justify-content:flex-end' },
          el('button', { class: 'btn', type: 'button', onClick: () => downloadBackup(b.name) }, t('admin.download')),
          el('button', { class: 'btn btn-danger', type: 'button', onClick: () => restoreBackup(b.name) }, t('admin.backups.restore')),
        )),
    )));
}


async function downloadBackup(name) {
  try {
    saveJson(name, await api(`/api/admin/taxonomy/backups/${encodeURIComponent(name)}`));
  } catch (e) { toast(e.message, 'bad'); }
}

async function restoreBackup(name) {
  if (!await confirmDialog(t('admin.backups.restore_ask', { name }), { okLabel: t('admin.backups.restore'), danger: true })) return;
  try {
    const { report } = await api(`/api/admin/taxonomy/backups/${encodeURIComponent(name)}/restore`, { method: 'POST' });
    showTaxonomy(t('admin.backups.restored', {
      name, groups: report.groups_saved, people: report.people_saved,
      groups_removed: report.groups_removed, people_removed: report.people_removed,
    }), true);
    loadTaxonomy();
  } catch (e) { showTaxonomy(e.message, false); }
}

document.getElementById('taxonomy-backup-now').addEventListener('click', async () => {
  try {
    const { backup } = await api('/api/admin/taxonomy/backups', { method: 'POST' });
    toast(t('admin.backups.done', { name: backup.name }), 'ok');
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
    toast(t('admin.groups.saved'), 'ok');
    event.target.reset();
    loadTaxonomy();
  } catch (e) { toast(e.message, 'bad'); }
});

async function deleteGroup(code) {
  if (!await confirmDialog(t('admin.groups.delete_ask', { code }), { okLabel: t('admin.delete'), danger: true })) return;
  try {
    await api(`/api/groups/${encodeURIComponent(code)}`, { method: 'POST' });
    toast(t('admin.groups.deleted'), 'ok');
    loadTaxonomy();
  } catch (e) { toast(e.message, 'bad'); }
}


document.getElementById('taxonomy-export').addEventListener('click', async () => {
  try {
    saveJson('taxonomy.json', await api('/api/admin/taxonomy'));
    showTaxonomy(t('admin.taxonomy.exported'), true);
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
    showTaxonomy(t('admin.taxonomy.bad_json', { name: file.name }), false);
    return;
  }
  const replace = document.getElementById('taxonomy-replace').checked;
  if (replace && !await confirmDialog(t('admin.taxonomy.replace_ask'), { okLabel: t('admin.taxonomy.replace'), danger: true })) return;
  try {
    const { report } = await api('/api/admin/taxonomy', { method: 'POST', body: { taxonomy, replace } });
    const vars = {
      name: file.name, groups: report.groups_saved, people: report.people_saved,
      groups_removed: report.groups_removed, people_removed: report.people_removed,
    };
    showTaxonomy(t(replace ? 'admin.taxonomy.imported_replace' : 'admin.taxonomy.imported', vars), true);
    loadTaxonomy();
  } catch (e) { showTaxonomy(e.message, false); }
});


/** The section opened: load every card once; the forms reload after a save. */
export function init() {
  loadUsers();
  loadTaxonomy();
}

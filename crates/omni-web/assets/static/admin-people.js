/* Admin panel, «Άνθρωποι» (P13.5): accounts, journalists and groups.
 * Loaded when the section is first opened.
 */

import { api, el, render, toast } from '/static/app.js?v=6';
import { saveJson } from '/static/admin-common.js?v=1';
import { confirmDialog, promptDialog } from '/static/dialog.js?v=1';

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
  if (!active && !await confirmDialog(`Deactivate ${user.email}? They are signed out at once and cannot sign in until reactivated.`, { okLabel: 'Deactivate', danger: true })) return;
  try {
    await api(`/api/admin/users/${user.id}/active`, { method: 'POST', body: { active } });
    toast(`${user.email} ${active ? 'reactivated' : 'deactivated'}.`, 'ok');
    loadUsers();
  } catch (e) { toast(e.message, 'bad'); }
}

async function resetPassword(id, email) {
  const next = await promptDialog(`New password for ${email} (at least 12 characters):`, {
    type: 'password',
    okLabel: 'Reset password',
    validate: (v) => (v.length >= 12 ? '' : 'At least 12 characters.'),
  });
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
  if (!await confirmDialog(`Delete ${surname}? Their jobs keep their name; a backup is taken first.`, { okLabel: 'Delete', danger: true })) return;
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


async function downloadBackup(name) {
  try {
    saveJson(name, await api(`/api/admin/taxonomy/backups/${encodeURIComponent(name)}`));
  } catch (e) { toast(e.message, 'bad'); }
}

async function restoreBackup(name) {
  if (!await confirmDialog(`Restore ${name}? Groups and people are set back exactly as in that backup; what is there now is backed up first.`, { okLabel: 'Restore', danger: true })) return;
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
  if (!await confirmDialog(`Delete group ${code}? People lose this membership; jobs keep their label.`, { okLabel: 'Delete', danger: true })) return;
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
  if (replace && !await confirmDialog('Replace: groups and people not in the file will be removed. Continue?', { okLabel: 'Replace', danger: true })) return;
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


/** The section opened: load every card once; the forms reload after a save. */
export function init() {
  loadUsers();
  loadTaxonomy();
}

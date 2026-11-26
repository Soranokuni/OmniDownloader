/* Administration panel: the shell (P13.5).
 *
 * Five sections in a left sidebar, chosen by the address's #fragment:
 * Επισκόπηση, Ρυθμίσεις, Άνθρωποι, Ιστότοποι, Συντήρηση. Each section's code is
 * its own module, imported the first time the section is opened; the search box
 * over setting names opens the sections whose cards match.
 *
 * Audit messages contain operator- and email-derived text (job slugs, URLs,
 * journalist names). They are text nodes, never HTML.
 */

import { api, el, render, toast, logout } from '/static/app.js?v=6';
import { runWizard } from '/static/wizard.js?v=15';
import { t, mountLanguageSwitch } from '/static/admin-text.js?v=5';

// P13.6: the page's words in the chosen language (Greek by default) before
// anything reads them, the search index included.
mountLanguageSwitch();

/* ------------------------------------------------------------------ *
 * First-run account (P2.14) and the setup wizard (P7.12)
 *
 * While the first-run admin still has its default password the server refuses
 * every request but the wizard's account step and the password change, so the
 * page shows only the wizard (whose first real step replaces that account) and
 * the rest of the module never runs (the module stops at the await below).
 * The change-password form stays reachable from the wizard as a fallback.
 * ------------------------------------------------------------------ */

function showPasswordCard() {
  const field = (id, label, auto) => el('div', { class: 'field' },
    el('label', { for: id }, label),
    el('input', { type: 'password', id, required: true, autocomplete: auto }));
  const form = el('form', {
    class: 'stack',
    onsubmit: async (event) => {
      event.preventDefault();
      const fresh = document.getElementById('first-new').value;
      if (fresh !== document.getElementById('first-repeat').value) {
        toast(t('admin.pw.differ'), 'bad');
        return;
      }
      try {
        await api('/api/auth/password', {
          method: 'POST',
          body: {
            current_password: document.getElementById('first-current').value,
            new_password: fresh,
          },
        });
        // The server ended the session, so the only way on is to sign in again.
        toast(t('admin.pw.changed'), 'ok');
        setTimeout(() => { location.href = '/login'; }, 1200);
      } catch (e) { toast(e.message, 'bad'); }
    },
  },
    field('first-current', t('admin.pw.current'), 'current-password'),
    field('first-new', t('admin.pw.new'), 'new-password'),
    field('first-repeat', t('admin.pw.repeat'), 'new-password'),
    el('button', { class: 'btn btn-primary', type: 'submit' }, t('admin.pw.change')));
  render(document.querySelector('main'), el('section', { class: 'card' },
    el('h2', { class: 'card-title' }, t('admin.pw.title')),
    el('p', { class: 'note' }, t('admin.pw.first')),
    form));
}

const whoami = await api('/api/auth/me').catch(() => null);
if (whoami && whoami.password_change_required) {
  document.getElementById('logout-btn').addEventListener('click', logout);
  await runWizard({ defaultAdmin: true, showPasswordCard });
  await new Promise(() => {});
}

// A real administrator on an installation whose wizard was never finished
// (a config written by this version): the wizard takes the page until Finish.
const setupState = await api('/api/admin/settings').catch(() => null);
const leftWizard = sessionStorage.getItem('omni.setup.leave') === '1';
if (setupState && setupState.setup_completed === false && !leftWizard) {
  document.getElementById('logout-btn').addEventListener('click', logout);
  await runWizard({ defaultAdmin: false });
  await new Promise(() => {});
}

document.getElementById('logout-btn').addEventListener('click', logout);

/* ------------------------------------------------------------------ *
 * Sections (P13.5)
 * ------------------------------------------------------------------ */

// One URL per module; each is imported once, when its section is first opened.
const MODULES = {
  overview: () => import('/static/admin-overview.js?v=7'),
  settings: () => import('/static/admin-settings.js?v=7'),
  people: () => import('/static/admin-people.js?v=6'),
  sites: () => import('/static/admin-sites.js?v=7'),
  maintenance: () => import('/static/admin-maintenance.js?v=6'),
};
const SECTIONS = Object.keys(MODULES);
const loaded = new Map();

/** Import the section's module and run its init the first time; its `shown` after. */
async function ensure(name) {
  if (!loaded.has(name)) {
    const ready = MODULES[name]().then((m) => { m.init(); return m; });
    loaded.set(name, ready);
    try { await ready; } catch (e) {
      loaded.delete(name);
      toast(t('admin.section.failed', { message: e.message }), 'bad');
    }
    return;
  }
  const m = await loaded.get(name);
  if (m.shown) m.shown();
}

const sectionEl = (name) => document.querySelector(`[data-section="${name}"]`);

function showSection(name) {
  for (const s of SECTIONS) {
    sectionEl(s).hidden = s !== name;
    const link = document.querySelector(`[data-nav="${s}"]`);
    link.classList.toggle('active', s === name);
    link.setAttribute('aria-current', s === name ? 'page' : 'false');
  }
  for (const i of index) i.card.hidden = false;
  ensure(name);
}

function route() {
  const wanted = location.hash.replace(/^#/, '');
  // A card's own anchor (#llm, #cookies…) opens the section that holds it.
  const card = wanted && document.getElementById(wanted);
  const holder = card && card.closest('[data-section]');
  const name = SECTIONS.includes(wanted) ? wanted : (holder ? holder.dataset.section : 'overview');
  showSection(name);
  if (holder && !SECTIONS.includes(wanted)) card.scrollIntoView({ block: 'start' });
}

/* ------------------------------------------------------------------ *
 * Search over setting names: shows the matching cards of every section
 * ------------------------------------------------------------------ */

// The cards are the sections' direct children: nested `.card flush` tables are part of their card.
const cards = [...document.querySelectorAll('[data-section] > .card')];
const fold = (s) => s.normalize('NFD').replace(/[̀-ͯ]/g, '').toLowerCase();
const index = cards.map((card) => ({
  card,
  section: card.closest('[data-section]').dataset.section,
  text: fold([...card.querySelectorAll('h2, h3, label, summary, [data-keywords]')]
    .map((n) => `${n.textContent} ${n.dataset ? n.dataset.keywords || '' : ''}`).join(' ')),
}));

const search = document.getElementById('admin-search');
const searchNote = document.getElementById('admin-search-note');
search.addEventListener('input', () => {
  const q = fold(search.value.trim());
  if (!q) { searchNote.textContent = ''; route(); return; }
  const hits = index.filter((i) => q.split(/\s+/).every((w) => i.text.includes(w)));
  const sections = new Set(hits.map((h) => h.section));
  for (const s of SECTIONS) {
    sectionEl(s).hidden = !sections.has(s);
    document.querySelector(`[data-nav="${s}"]`).classList.remove('active');
    if (sections.has(s)) ensure(s);
  }
  for (const i of index) i.card.hidden = !hits.includes(i);
  searchNote.textContent = hits.length ? t('admin.search.hits', { n: hits.length }) : t('admin.search.none');
});

window.addEventListener('hashchange', () => { search.value = ''; searchNote.textContent = ''; route(); });
route();

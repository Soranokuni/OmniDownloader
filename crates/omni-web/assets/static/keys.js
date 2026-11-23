/* The desk's keyboard (plan P12.8).
 *
 *   J / ↓   next card of the list on screen       K / ↑   previous card
 *   Enter   open the card (its «Περισσότερα», or the held video's history)
 *   R       «Δοκιμή ξανά» on the card             A       «Έγκριση → Dalet» (asks first)
 *   /       the search of the tab                 ?       this list, in a window
 *
 * Nothing happens while a person types in a field, holds Ctrl/Alt/⌘, or a
 * dialog is open. The arrows move between cards only when a card already
 * has the focus, so they still scroll the page everywhere else.
 *
 * A list registers its cards: `navCard(node, { open, retry, approve })`
 * makes the node focusable and keeps its actions; mcr.js tells which list is
 * on screen and where its search field is.
 */

import { el } from '/static/app.js?v=6';
import { t } from '/static/text.js?v=9';
import { infoDialog, dialogOpen } from '/static/dialog.js?v=10';

const actions = new WeakMap();
let scope = () => null;
let searchOf = () => null;

/** Make `node` a card the keyboard can reach; `acts` are its shortcuts. */
export function navCard(node, acts = {}) {
  // A card is focusable by script only (Tab still goes through its buttons);
  // a button (a mail's head) already is, and stays in the Tab order.
  if (node.tagName !== 'BUTTON') node.setAttribute('tabindex', '-1');
  node.dataset.nav = '1';
  actions.set(node, acts);
  return node;
}

/** `listOf()` gives the element whose cards J/K walk; `searchFieldOf()` the field `/` focuses. */
export function initKeys({ listOf, searchFieldOf }) {
  scope = listOf;
  searchOf = searchFieldOf;
  document.addEventListener('keydown', onKey);
}

function typing(target) {
  if (!(target instanceof Element)) return false;
  return target.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(target.tagName);
}

function cards() {
  const root = scope();
  return root ? [...root.querySelectorAll('[data-nav]')].filter((n) => n.offsetParent !== null) : [];
}

function move(step) {
  const all = cards();
  if (!all.length) return;
  const at = all.indexOf(document.activeElement.closest && document.activeElement.closest('[data-nav]'));
  const next = at < 0 ? (step > 0 ? 0 : all.length - 1) : Math.min(all.length - 1, Math.max(0, at + step));
  all[next].focus();
  all[next].scrollIntoView({ block: 'nearest', behavior: 'auto' });
}

function current() {
  const node = document.activeElement && document.activeElement.closest ? document.activeElement.closest('[data-nav]') : null;
  return node ? { node, acts: actions.get(node) || {} } : null;
}

function onKey(ev) {
  if (ev.defaultPrevented || ev.ctrlKey || ev.metaKey || ev.altKey || dialogOpen() || typing(ev.target)) return;
  const key = ev.key;
  const here = current();
  if (key === 'j' || key === 'J' || (key === 'ArrowDown' && here)) { ev.preventDefault(); move(1); return; }
  if (key === 'k' || key === 'K' || (key === 'ArrowUp' && here)) { ev.preventDefault(); move(-1); return; }
  if (key === '/') {
    const field = searchOf();
    if (field) { ev.preventDefault(); field.focus(); field.select(); }
    return;
  }
  if (key === '?') { ev.preventDefault(); showShortcuts(); return; }
  if (!here || ev.target !== here.node) return; // the rest act on a focused card itself
  if (key === 'Enter' && here.acts.open) { ev.preventDefault(); here.acts.open(); return; }
  if ((key === 'r' || key === 'R') && here.acts.retry) { ev.preventDefault(); here.acts.retry(); return; }
  if ((key === 'a' || key === 'A') && here.acts.approve) { ev.preventDefault(); here.acts.approve(); }
}

/** The shortcut sheet («?»). */
export const SHORTCUTS = [
  ['J / ↓', 'keys.next'],
  ['K / ↑', 'keys.prev'],
  ['Enter', 'keys.open'],
  ['R', 'keys.retry'],
  ['A', 'keys.approve'],
  ['/', 'keys.search'],
  ['?', 'keys.sheet'],
  ['Esc', 'keys.escape'],
];

/** The shortcut sheet in a window; also «Βοήθεια» → «Συντομεύσεις πληκτρολογίου» (P12.10). */
export function showShortcuts() {
  infoDialog({
    title: t('keys.title'),
    content: el('dl', { class: 'keys-sheet' },
      SHORTCUTS.flatMap(([keyName, what]) => [el('dt', {}, el('kbd', {}, keyName)), el('dd', {}, t(what))])),
  });
}

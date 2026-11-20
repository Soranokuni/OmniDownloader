/* The one in-page dialog (plan P12.4), for what cannot be taken back: a video
 * sent to Dalet, a job closed for good, a wrong video recorded. It replaces
 * the browser's own confirm(), which blocks the page, cannot be styled or
 * read aloud well, and looks different in every browser.
 *
 *   if (!(await confirmDialog({ title, body, confirm: 'Έγκριση → Dalet' }))) return;
 *
 * Focus starts on «Άκυρο», so Enter alone never sends anything on air. Tab
 * stays inside the dialog, the page behind it is inert, Escape or a click
 * outside closes it as «Άκυρο», and the focus goes back to the button that
 * opened it. One dialog at a time: a second call while one is open answers
 * `false`.
 *
 * infoDialog (P12.8) is the same window with one «Κλείσιμο», for something
 * to read, such as the keyboard shortcuts.
 */

import { el } from '/static/app.js?v=6';
import { t } from '/static/text.js?v=4';

let openNow = false;
let seq = 0;

const FOCUSABLE = 'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])';

/** Whether a dialog is open (the desk's shortcuts wait while one is). */
export function dialogOpen() {
  return openNow;
}

/**
 * Show `box` in front of an inert page, with the focus kept inside it.
 * `done(answer)` closes it and gives the focus back to whatever had it.
 */
function present(box, onAnswer) {
  const opener = document.activeElement;
  const overlay = el('div', { class: 'modal dlg-overlay' }, box);

  // The page behind the dialog takes no focus and no clicks.
  const inerted = [];
  for (const node of document.body.children) {
    if (!node.inert) { node.inert = true; inerted.push(node); }
  }

  const close = (answer) => {
    document.removeEventListener('keydown', onKey, true);
    overlay.remove();
    for (const node of inerted) node.inert = false;
    openNow = false;
    if (opener && opener.isConnected && typeof opener.focus === 'function') opener.focus();
    onAnswer(answer);
  };
  const onKey = (ev) => {
    if (ev.key === 'Escape') {
      ev.preventDefault();
      ev.stopPropagation();
      close(false);
    } else if (ev.key === 'Tab') {
      const items = [...box.querySelectorAll(FOCUSABLE)].filter((n) => !n.disabled);
      if (!items.length) return;
      const first = items[0];
      const last = items[items.length - 1];
      if (ev.shiftKey && (document.activeElement === first || !box.contains(document.activeElement))) {
        ev.preventDefault();
        last.focus();
      } else if (!ev.shiftKey && (document.activeElement === last || !box.contains(document.activeElement))) {
        ev.preventDefault();
        first.focus();
      }
    }
  };
  overlay.addEventListener('click', (ev) => { if (ev.target === overlay) close(false); });
  document.addEventListener('keydown', onKey, true);
  document.body.append(overlay);
  return close;
}

/**
 * `body` is a string or an array of strings (one paragraph each). `danger`
 * colours the confirm button red. Resolves `true` only for the confirm button.
 */
export function confirmDialog({ title, body, confirm, cancel = t('dialog.cancel'), danger = false }) {
  if (openNow) return Promise.resolve(false);
  openNow = true;
  return new Promise((resolve) => {
    const id = ++seq;
    const paragraphs = (Array.isArray(body) ? body : [body]).filter(Boolean);
    const no = el('button', { class: 'btn', type: 'button' }, cancel);
    const yes = el('button', { class: danger ? 'btn btn-danger' : 'btn btn-primary', type: 'button' }, confirm);
    const box = el('div', {
      class: 'modal-body dlg',
      role: 'alertdialog',
      'aria-modal': 'true',
      'aria-labelledby': `dlg-title-${id}`,
      'aria-describedby': `dlg-body-${id}`,
    },
    el('h2', { class: 'dlg-title', id: `dlg-title-${id}` }, title),
    el('div', { id: `dlg-body-${id}` }, paragraphs.map((p) => el('p', { class: 'dlg-text' }, p))),
    el('div', { class: 'row dlg-actions' }, no, yes));
    const close = present(box, resolve);
    no.addEventListener('click', () => close(false));
    yes.addEventListener('click', () => close(true));
    no.focus();
  });
}

/** Something to read: `content` (nodes) under `title`, one «Κλείσιμο». */
export function infoDialog({ title, content }) {
  if (openNow) return Promise.resolve(false);
  openNow = true;
  return new Promise((resolve) => {
    const id = ++seq;
    const done = el('button', { class: 'btn btn-primary', type: 'button' }, t('page.close'));
    const box = el('div', { class: 'modal-body dlg', role: 'dialog', 'aria-modal': 'true', 'aria-labelledby': `dlg-title-${id}` },
      el('h2', { class: 'dlg-title', id: `dlg-title-${id}` }, title),
      content,
      el('div', { class: 'row dlg-actions' }, done));
    const close = present(box, resolve);
    done.addEventListener('click', () => close(true));
    done.focus();
  });
}

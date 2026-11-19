/* The desk's messages (plan P12.6).
 *
 * An error stays until the operator closes it («✕»), and is announced at
 * once by a screen reader (an assertive live region); at most three are on
 * screen, the oldest going first, and the same text twice moves to the end
 * instead of stacking. Any other message is announced politely and leaves
 * after 6 seconds.
 *
 * app.js's toast() (admin pages) still fades everything; the desk's modules
 * use this one. Nothing here builds HTML from a string.
 */

import { el } from '/static/app.js?v=6';
import { t } from '/static/text.js?v=1';

export const MAX_ERRORS = 3;
const PASSING_MS = 6000;

let host = null;
let errors = null;
let notes = null;

/** The live regions exist before the first message, or a screen reader may
 *  not announce what is put into a region that has just appeared. */
function regions() {
  if (host && host.isConnected) return;
  errors = el('div', { class: 'toast-stack', role: 'alert', 'aria-live': 'assertive' });
  notes = el('div', { class: 'toast-stack', role: 'status', 'aria-live': 'polite' });
  host = el('div', { class: 'toast-host desk-toasts' }, errors, notes);
  document.body.append(host);
}
regions();

/** A message. `kind` is 'ok' | 'warn' | 'bad' | ''. */
export function toast(message, kind = '') {
  regions();
  const text = String(message);
  if (kind === 'bad') {
    const same = [...errors.children].find((n) => n.dataset.text === text);
    if (same) same.remove();
    const close = el('button', { class: 'toast-close', type: 'button', 'aria-label': t('toast.close') }, '✕');
    const node = el('div', { class: 'toast bad sticky', 'data-text': text }, el('span', { class: 'toast-text' }, text), close);
    close.addEventListener('click', () => node.remove());
    errors.append(node);
    while (errors.children.length > MAX_ERRORS) errors.firstElementChild.remove();
    return;
  }
  const node = el('div', { class: `toast ${kind}`.trim() }, text);
  notes.append(node);
  setTimeout(() => node.remove(), PASSING_MS);
}

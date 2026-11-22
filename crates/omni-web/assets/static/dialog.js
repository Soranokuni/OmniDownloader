/* In-page dialogs for the admin panel (P13.5), in place of the browser's
 * confirm() and prompt(): they say what will happen in the page's own words,
 * can be styled as dangerous, and cannot be suppressed by the browser.
 *
 * Built with el() only. One dialog at a time; Escape or Cancel answers "no".
 * (Lane desk has its own dialog.js; the two are unified when the lanes merge.)
 */

import { el } from '/static/app.js?v=6';

function open(build) {
  return new Promise((resolve) => {
    const previous = document.activeElement;
    let done = false;
    const close = (answer) => {
      if (done) return;
      done = true;
      backdrop.remove();
      document.removeEventListener('keydown', onKey, true);
      if (previous && previous.focus) previous.focus();
      resolve(answer);
    };
    const onKey = (e) => { if (e.key === 'Escape') { e.preventDefault(); close(null); } };
    const { body, focus, ok } = build(close);
    const backdrop = el('div', { class: 'modal', role: 'presentation' },
      el('div', { class: 'modal-body stack', role: 'dialog', 'aria-modal': 'true' }, body));
    backdrop.addEventListener('click', (e) => { if (e.target === backdrop) close(null); });
    document.addEventListener('keydown', onKey, true);
    document.body.append(backdrop);
    (focus || ok).focus();
  });
}

/**
 * Ask a yes/no question. Resolves true for the action, false otherwise.
 *   message   what will happen, in full sentences
 *   okLabel   the button that does it (a verb: "Delete", "Purge")
 *   danger    style the action as destructive
 */
export function confirmDialog(message, { okLabel = 'OK', cancelLabel = 'Cancel', danger = false } = {}) {
  return open((close) => {
    const ok = el('button', { class: danger ? 'btn btn-danger' : 'btn btn-primary', type: 'button', onclick: () => close(true) }, okLabel);
    const cancel = el('button', { class: 'btn', type: 'button', onclick: () => close(false) }, cancelLabel);
    return {
      body: [el('p', { style: 'white-space:pre-line;margin:0' }, message), el('div', { class: 'row', style: 'justify-content:flex-end' }, cancel, ok)],
      focus: danger ? cancel : ok,
      ok,
    };
  }).then((a) => a === true);
}

/**
 * Ask for a value. Resolves the text, or null when cancelled.
 *   type        input type ('text', 'password')
 *   validate    (value) => message or '' : the action stays disabled with the message shown
 */
export function promptDialog(message, { okLabel = 'OK', cancelLabel = 'Cancel', type = 'text', value = '', placeholder = '', validate = () => '' } = {}) {
  return open((close) => {
    const input = el('input', { type, value, placeholder: placeholder || null, autocomplete: type === 'password' ? 'new-password' : 'off' });
    const why = el('p', { class: 'note bad', style: 'margin:0' });
    const ok = el('button', { class: 'btn btn-primary', type: 'submit' }, okLabel);
    const check = () => { const m = validate(input.value); why.textContent = m || ''; ok.disabled = Boolean(m); };
    input.addEventListener('input', check);
    queueMicrotask(check);
    const form = el('form', { class: 'stack', onsubmit: (e) => { e.preventDefault(); if (!ok.disabled) close(input.value); } },
      el('label', { style: 'white-space:pre-line' }, message), input, why,
      el('div', { class: 'row', style: 'justify-content:flex-end' },
        el('button', { class: 'btn', type: 'button', onclick: () => close(null) }, cancelLabel), ok));
    return { body: [form], focus: input, ok };
  });
}

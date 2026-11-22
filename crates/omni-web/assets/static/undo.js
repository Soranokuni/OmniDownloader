/* «Αναίρεση» instead of «Are you sure?» (plan P12.4).
 *
 * A reversible action acts at once and offers «Αναίρεση» for 8 seconds, in a
 * small strip at the bottom left. Two kinds:
 *
 *   deferAction: the change is not sent yet. What it hides disappears from
 *     the lists now (`hides(view, job)`), and the request goes out when the
 *     8 seconds are over. «Αναίρεση» drops it; nothing ever reached the server.
 *     Used where the server has no way back: removing a job, hiding old ones,
 *     clearing delivered ones, a journalist's own cancel.
 *
 *   undoToast: the change was already sent (a desk cancel must stop a running
 *     tool now, not in 8 s); «Αναίρεση» runs `undo`, which sends it back.
 *
 * A page left within the 8 seconds sends what is still waiting at once, best
 * effort. Nothing here builds HTML from a string.
 */

import { el } from '/static/app.js?v=6';
import { toast } from '/static/toast.js?v=9';
import { t } from '/static/text.js?v=8';

export const UNDO_MS = 8000;

let host = null;
function strip() {
  if (!host || !host.isConnected) {
    host = el('div', { class: 'undo-host', role: 'status', 'aria-live': 'polite' });
    document.body.append(host);
  }
  return host;
}

/** One line with its «Αναίρεση» button; `onUndo` runs once if pressed in time. */
function undoLine(text, onUndo, onExpire) {
  let settled = false;
  const button = el('button', { class: 'btn', type: 'button' }, t('undo.button'));
  const node = el('div', { class: 'toast undo' }, el('span', {}, text), button);
  const finish = () => { node.remove(); };
  const timer = setTimeout(() => {
    if (settled) return;
    settled = true;
    finish();
    if (onExpire) onExpire();
  }, UNDO_MS);
  button.addEventListener('click', () => {
    if (settled) return;
    settled = true;
    clearTimeout(timer);
    finish();
    onUndo();
  });
  strip().append(node);
  return {
    settle() {
      if (settled) return false;
      settled = true;
      clearTimeout(timer);
      finish();
      return true;
    },
  };
}

/** Actions waiting for their 8 seconds: token -> { hides, send } */
const waiting = new Map();
let nextToken = 1;

/** Whether a waiting action hides `job` from the list `view`
 *  ('live', 'review', 'completed', 'email', 'late', 'user'). */
export function hiddenNow(view, job) {
  for (const w of waiting.values()) {
    if (w.hides(view, job)) return true;
  }
  return false;
}

/** `jobs` without the ones a waiting action hides from `view`. */
export function visible(view, jobs) {
  return waiting.size ? jobs.filter((j) => !hiddenNow(view, j)) : jobs;
}

/**
 * Act now on screen, send in 8 s. `run()` makes the request; `after()`
 * redraws the lists (called at once, on undo, and when the request ended).
 * `done` is the message once it is sent (optional).
 */
export function deferAction({ text, hides, run, after, done }) {
  const token = nextToken++;
  const send = async () => {
    try {
      const result = await run();
      if (done) toast(typeof done === 'function' ? done(result) : done, 'ok');
    } catch (e) {
      toast(e.message, 'bad');
    } finally {
      waiting.delete(token);
      after();
    }
  };
  const line = undoLine(text, () => {
    waiting.delete(token);
    after();
  }, send);
  waiting.set(token, { hides, send: () => { if (line.settle()) send(); } });
  after();
}

/** The change was made; «Αναίρεση» runs `undo()` (which reports its own failure). */
export function undoToast(text, undo) {
  undoLine(text, undo, null);
}

// Leaving the page sends what is still waiting, so an action the person saw
// happen is not silently dropped. Best effort: a browser may cut it short.
window.addEventListener('pagehide', () => {
  for (const w of [...waiting.values()]) w.send();
});

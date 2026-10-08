/* Shared panel runtime (plan P2.3, P2.5).
 *
 * The panels used to build rows with template literals and assign them to
 * `innerHTML`. The values interpolated that way -- `url`, `slug`, `notes`,
 * `error_message`, `full_name` -- come from emails sent to the ingest address,
 * so anyone who could send mail to the newsroom could store script in the MCR
 * panel and have it run in an operator's browser (defect W-03).
 *
 * The fix is structural rather than a sanitiser: everything here builds DOM
 * nodes and sets `textContent`. There is no HTML-string path to get wrong, and
 * `tests/xss_tests.rs` fails the build if `innerHTML` reappears in an asset.
 */

/* ------------------------------------------------------------------ *
 * DOM building
 * ------------------------------------------------------------------ */

/** Escape for the rare case where a string must go into an attribute value
 *  that is built by hand. Prefer `el()` -- this exists so that any remaining
 *  string path has one obvious, tested helper. */
export function esc(value) {
  return String(value ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

/**
 * Build an element.
 *
 *   el('div', { class: 'card' }, 'plain text', el('b', {}, 'bold'))
 *
 * Strings in `children` become text nodes, never markup. Attributes whose
 * value is `null`/`undefined`/`false` are skipped; `onClick` style keys are
 * attached as listeners rather than as attributes, so no handler is ever
 * written into a string.
 */
export function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs || {})) {
    if (value === null || value === undefined || value === false) continue;
    if (key.startsWith('on') && typeof value === 'function') {
      node.addEventListener(key.slice(2).toLowerCase(), value);
    } else if (key === 'class') {
      node.className = value;
    } else if (key === 'text') {
      node.textContent = value;
    } else if (key === 'href') {
      // A `javascript:` or `data:` href is script with a different spelling.
      // Job URLs arrive from email, so this is the same attack surface the
      // innerHTML rewrite closed, one attribute along.
      const safe = safeHref(value);
      if (safe) node.setAttribute('href', safe);
    } else if (value === true) {
      node.setAttribute(key, '');
    } else {
      node.setAttribute(key, String(value));
    }
  }
  for (const child of children.flat()) {
    if (child === null || child === undefined || child === false) continue;
    node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return node;
}

/** `http(s)` URLs only; anything else yields null and no href is set. */
export function safeHref(value) {
  const raw = String(value ?? '').trim();
  if (/^https?:\/\//i.test(raw)) return raw;
  return null;
}

/** Replace an element's children with `nodes` (no HTML parsing involved). */
export function render(target, ...nodes) {
  const node = typeof target === 'string' ? document.getElementById(target) : target;
  if (!node) return;
  node.replaceChildren(...nodes.flat().filter((n) => n !== null && n !== undefined));
}

/** An `<svg><use>` reference into the sprite. */
export function icon(name, extraClass = '') {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('class', `icon ${extraClass}`.trim());
  svg.setAttribute('aria-hidden', 'true');
  const use = document.createElementNS('http://www.w3.org/2000/svg', 'use');
  use.setAttribute('href', `/static/icons.svg?v=${BUILD}#${name}`);
  svg.append(use);
  return svg;
}

/* ------------------------------------------------------------------ *
 * API
 * ------------------------------------------------------------------ */

/** Cache-busting stamp, replaced per release; also on the sprite URL. */
export const BUILD = '3';

/**
 * The single API entry point.
 *
 * It sets `X-Omni-Request`, which is what the server's CSRF guard looks for,
 * so a state-changing call made any other way will be refused -- deliberately:
 * one helper means one place where that header can be forgotten.
 *
 * Errors come back in the `{error:{code,message}}` shape and are thrown as an
 * `ApiError`, so callers can show `e.message` directly.
 */
export async function api(path, options = {}) {
  const init = {
    method: options.method || 'GET',
    headers: { 'X-Omni-Request': '1', ...(options.headers || {}) },
    credentials: 'same-origin',
  };
  if (options.body !== undefined) {
    init.headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(options.body);
  }

  let response;
  try {
    response = await fetch(path, init);
  } catch (networkError) {
    throw new ApiError('NETWORK', 'Η υπηρεσία λήψης δεν απαντά. Ελέγξτε ότι τρέχει και ξαναδοκιμάστε.');
  }

  let payload = null;
  const text = await response.text();
  if (text) {
    try { payload = JSON.parse(text); } catch { payload = null; }
  }

  if (response.status === 401) {
    // On a panel: the session ended (idle timeout, or an admin ended it), so
    // go to the login screen rather than leave a panel that silently stops
    // updating. On the login screen itself a 401 is the answer to a failed
    // sign-in, and the server's own message ("wrong email or password") is
    // the one to show: "Session expired." there sent people looking for a
    // session problem when the address was wrong.
    const onLogin = location.pathname.startsWith('/login');
    if (!onLogin) location.href = '/login';
    const serverMessage = payload && payload.error && payload.error.message;
    throw new ApiError('UNAUTHENTICATED', onLogin && serverMessage ? serverMessage : 'Η σύνδεση έληξε. Συνδεθείτε ξανά.');
  }

  if (!response.ok) {
    const err = payload && payload.error ? payload.error : {};
    let message = err.message || 'Το αίτημα απέτυχε.';
    // The server's internal errors are English; on a Greek page say it in Greek.
    if (document.documentElement.lang === 'el') {
      if (err.code === 'INTERNAL') {
        message = 'Σφάλμα στον διακομιστή. Δοκιμάστε ξανά· αν επαναλαμβάνεται, ενημερώστε τον διαχειριστή.';
      } else if (err.code === 'NOT_FOUND') {
        message = 'Δεν βρέθηκε· ίσως αφαιρέθηκε στο μεταξύ.';
      }
    }
    throw new ApiError(err.code || String(response.status), message, response.status);
  }
  return payload;
}

export class ApiError extends Error {
  constructor(code, message, status = 0) {
    super(message);
    this.code = code;
    this.status = status;
  }
}

/* ------------------------------------------------------------------ *
 * Feedback
 * ------------------------------------------------------------------ */

let toastHost = null;

/** A transient message. `kind` is 'ok' | 'bad' | ''. */
export function toast(message, kind = '') {
  if (!toastHost) {
    toastHost = el('div', { class: 'toast-host', role: 'status', 'aria-live': 'polite' });
    document.body.append(toastHost);
  }
  const node = el('div', { class: `toast ${kind}`.trim() }, String(message));
  toastHost.append(node);
  setTimeout(() => node.remove(), 6000);
}

/* ------------------------------------------------------------------ *
 * Live updates
 * ------------------------------------------------------------------ */

/**
 * Subscribe to the server's event stream, with polling as the fallback.
 *
 * `EventSource` reconnects on its own but says nothing when the server is
 * simply gone, so the interval underneath means a panel that lost the stream
 * still refreshes rather than showing a frozen queue that looks idle.
 */
export function live(onUpdate, intervalMs = 5000) {
  let source = null;
  try {
    source = new EventSource('/api/events');
    source.onmessage = () => onUpdate();
  } catch { /* fall through to polling */ }
  const timer = setInterval(onUpdate, intervalMs);
  onUpdate();
  return () => {
    if (source) source.close();
    clearInterval(timer);
  };
}

/* ------------------------------------------------------------------ *
 * Formatting
 * ------------------------------------------------------------------ */

/** Timestamps are `Option<DateTime>` on the wire. `null` renders blank -- it
 *  must never render as "now", which is what the archive used to show for
 *  every completed job. */
export function fmtTime(value) {
  if (!value) return '—';
  const d = new Date(value);
  // Greek date and 24-hour time, whatever the browser's own language is.
  return Number.isNaN(d.getTime())
    ? '—'
    : d.toLocaleString('el-GR', { day: '2-digit', month: '2-digit', year: 'numeric', hour: '2-digit', minute: '2-digit', hour12: false });
}

export function fmtDuration(seconds) {
  if (!seconds || seconds <= 0) return '—';
  // "4:25", "0:47": minutes and seconds, as on a clip's timecode.
  const s = Math.round(seconds);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

/** Map a job status to a badge colour class. */
export function statusClass(status) {
  switch (status) {
    case 'COMPLETED':
    case 'COMPLETED_MANUAL':
      return 'ok';
    case 'DOWNLOADING':
    case 'EXTRACTING':
      return 'info';
    case 'TRANSCODING':
    case 'REWRAPPING':
    case 'DELIVERING':
      return 'busy';
    case 'FAILED':
      return 'bad';
    case 'REQUIRES_REVIEW':
    case 'MANUAL_DOWNLOAD':
      return 'warn';
    default:
      return '';
  }
}

/** Log out, then go to the login page. */
export async function logout() {
  try { await api('/api/auth/logout', { method: 'POST' }); } catch { /* going anyway */ }
  location.href = '/login';
}

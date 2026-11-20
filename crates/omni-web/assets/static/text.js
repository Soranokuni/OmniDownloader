/* Every word the desk shows, looked up in one place (plan P12.7).
 *
 *   import { t, tn } from '/static/text.js?v=6';
 *   t('undo.button')                       -> «Αναίρεση»
 *   t('desk.retry.done', { name })         -> «Το 1_X_Y.mxf μπήκε ξανά στην ουρά.»
 *   tn('summary.failed', 3)                -> «3 απέτυχαν» (keys `….one` / `….other`)
 *
 * The words are plain dictionaries, one per language: text.el.js (Greek,
 * the default and the reference) and whatever another page adds with
 * `addLanguage('en', dict)`. A key missing in the chosen language falls back
 * to Greek; a key missing everywhere shows as the key itself, so a typo is
 * visible instead of a blank. `{name}` in a text is replaced by `vars.name`;
 * values are inserted as text, never as markup (el() makes the nodes).
 *
 * No framework and no build step: an object, a lookup, a switch.
 */

import EL from '/static/text.el.js?v=6';

const LANG_KEY = 'omni.lang';
const dictionaries = { el: EL };
let current = 'el';

/** Make a dictionary available as `code` (e.g. 'en'); the admin pages add theirs. */
export function addLanguage(code, dictionary) {
  dictionaries[code] = dictionary;
  if (storedLanguage() === code) current = code;
}

/** Switch the language, and remember it in this browser. Unknown codes are ignored. */
export function setLanguage(code) {
  if (!dictionaries[code]) return false;
  current = code;
  try { localStorage.setItem(LANG_KEY, code); } catch { /* private mode: for this page only */ }
  return true;
}

/** The language in use: 'el' unless another one was chosen and added. */
export function language() {
  return current;
}

function storedLanguage() {
  try { return localStorage.getItem(LANG_KEY); } catch { return null; }
}

/** The text of `key` in the current language, with `{name}` filled from `vars`. */
export function t(key, vars) {
  const dict = dictionaries[current] || EL;
  let text = Object.prototype.hasOwnProperty.call(dict, key) ? dict[key] : EL[key];
  if (text === undefined) return key;
  if (vars) text = text.replace(/\{(\w+)\}/g, (whole, name) => (name in vars ? String(vars[name]) : whole));
  return text;
}

/** Fill a page's fixed words: `data-t="key"` sets an element's text,
 *  `data-t-aria-label`, `data-t-title` and `data-t-placeholder` set those
 *  attributes. Run once the page is parsed, and again after a language switch. */
export function applyText(root = document) {
  for (const node of root.querySelectorAll('[data-t]')) node.textContent = t(node.dataset.t);
  for (const [attr, data] of [['aria-label', 'tAriaLabel'], ['title', 'tTitle'], ['placeholder', 'tPlaceholder']]) {
    const selector = `[data-t-${attr}]`;
    for (const node of root.querySelectorAll(selector)) node.setAttribute(attr, t(node.dataset[data]));
  }
}

/** Whether `key` has a text (in the current language or in Greek). */
export function has(key) {
  const dict = dictionaries[current] || EL;
  return Object.prototype.hasOwnProperty.call(dict, key) || Object.prototype.hasOwnProperty.call(EL, key);
}

/** The singular (`key.one`) or plural (`key.other`) text for `n`, with `{n}` filled. */
export function tn(key, n, vars = {}) {
  return t(`${key}.${Number(n) === 1 ? 'one' : 'other'}`, { n, ...vars });
}

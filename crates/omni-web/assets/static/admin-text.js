/* The administration panel's and the setup wizard's words (P13.6).
 *
 * Greek by default, English on request, through the desk's lookup (text.js,
 * P12.7): admin-text.el.js and admin-text.en.js hold the same keys. The choice
 * is kept in this browser (text.js); the desk and the journalist's page stay
 * Greek, since only these pages add an English dictionary.
 *
 *   import { t, applyText } from '/static/admin-text.js?v=5';
 */

import { t, tn, applyText, addLanguage, setLanguage, language } from '/static/text.js?v=10';
import DESK from '/static/text.el.js?v=10';
import ADMIN_EL from '/static/admin-text.el.js?v=4';
import ADMIN_EN from '/static/admin-text.en.js?v=4';

addLanguage('el', { ...DESK, ...ADMIN_EL });
addLanguage('en', ADMIN_EN);

export { t, tn, applyText, language };

// What a switch redraws: the panel reloads; the wizard redraws its step in place.
let redraw = () => location.reload();

/** Let the page redraw itself after a language switch instead of reloading. */
export function onLanguageChange(fn) {
  redraw = fn;
}

/** The page's fixed words in the chosen language, and the switch's own label. */
export function applyAdminText() {
  document.documentElement.lang = language();
  applyText();
  const button = document.getElementById('lang-btn');
  if (button) {
    // The switch names the other language in that language.
    const other = language() === 'en' ? 'el' : 'en';
    button.lang = other;
    button.textContent = t('lang.switch');
    button.title = t('lang.switch_title');
  }
}

/** Wire the topbar's language button (once per page). */
export function mountLanguageSwitch() {
  const button = document.getElementById('lang-btn');
  applyAdminText();
  if (!button || button.dataset.wired) return;
  button.dataset.wired = '1';
  button.addEventListener('click', () => {
    setLanguage(language() === 'en' ? 'el' : 'en');
    applyAdminText();
    redraw();
  });
}

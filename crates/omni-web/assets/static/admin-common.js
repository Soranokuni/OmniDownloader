/* Helpers shared by the admin panel's section modules (P13.5). */

import { el, toast } from '/static/app.js?v=6';
import { t } from '/static/admin-text.js?v=5';

/** Put `text` on the clipboard; plain HTTP on the LAN has no clipboard API. */
export function copyText(text, button) {
  const done = () => { button.textContent = t('wizard.copied'); setTimeout(() => { button.textContent = t('wizard.copy'); }, 1500); };
  if (navigator.clipboard && window.isSecureContext) {
    navigator.clipboard.writeText(text).then(done, () => toast(t('admin.copy_failed'), 'bad'));
    return;
  }
  const box = el('textarea', { readonly: '', style: 'position:fixed;left:-9999px' }, text);
  document.body.appendChild(box);
  box.select();
  try { document.execCommand('copy'); done(); } catch (e) { toast(t('admin.copy_failed'), 'bad'); }
  box.remove();
}

/** Download our own JSON as a file. */
export function saveJson(filename, data) {
  const blob = new Blob([JSON.stringify(data, null, 2) + '\n'], { type: 'application/json' });
  // Our own JSON as a local blob: a plain anchor, since el() only allows http(s) links.
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = filename;
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(a.href), 1000);
}

/** Whether the section `name` is the one on screen (pollers stop when it is not). */
export function sectionShown(name) {
  const s = document.querySelector(`[data-section="${name}"]`);
  return Boolean(s && !s.hidden);
}

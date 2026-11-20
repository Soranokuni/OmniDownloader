/* The desk's sounds and their settings (plan P12.9).
 *
 * Four kinds of news, each with its own sound and its own switch:
 *
 *   delivered  every video of a mail delivered      C5 E5 G5 C6, rising
 *   person     something needs a person (or is late) G5 E5, falling
 *   health     a health check got worse, or the service stopped answering
 *                                                     A4 A4 E4, low
 *   held       a video waits for «Έγκριση → Dalet»   E5 A5, a two-tone call
 *
 * and a reminder, repeated every N minutes while something has waited for a
 * person longer than N minutes (notify.js decides when; the sound is the
 * «person» one, twice). One volume for all of them.
 *
 * Everything is stored in this browser only (localStorage), so each desk
 * screen keeps its own: a monitor on the wall can stay quiet while the
 * operator's screen plays. The sound is synthesised with Web Audio: no file
 * to ship or cache. Browsers start audio only after a click on the page;
 * `unlock()` is called from one.
 */

import { el } from '/static/app.js?v=6';
import { t } from '/static/text.js?v=5';
import { infoDialog } from '/static/dialog.js?v=6';

const PREF_KEY = 'omni.mcr.sound';
// Before P12.9 there was only on/off: 'off' there still means silence.
const OLD_KEY = 'omni.mcr.chime';

export const EVENTS = ['delivered', 'person', 'health', 'held'];
export const REMIND_MIN = 1;
export const REMIND_MAX = 120;

const DEFAULTS = {
  on: true,
  volume: 70,
  delivered: true,
  person: true,
  health: true,
  held: true,
  remind: true,
  remindMins: 10,
};

/** Whatever was stored, made safe: a broken or old value never breaks the desk. */
export function sanitize(raw) {
  const p = { ...DEFAULTS };
  if (!raw || typeof raw !== 'object') return p;
  for (const key of ['on', 'remind', ...EVENTS]) {
    if (typeof raw[key] === 'boolean') p[key] = raw[key];
  }
  const volume = Math.round(Number(raw.volume));
  if (Number.isFinite(volume)) p.volume = Math.min(100, Math.max(0, volume));
  const mins = Math.round(Number(raw.remindMins));
  if (Number.isFinite(mins)) p.remindMins = Math.min(REMIND_MAX, Math.max(REMIND_MIN, mins));
  return p;
}

function load() {
  try {
    const raw = localStorage.getItem(PREF_KEY);
    if (raw) return sanitize(JSON.parse(raw));
    return sanitize({ on: localStorage.getItem(OLD_KEY) !== 'off' });
  } catch {
    return { ...DEFAULTS };
  }
}

let prefs = load();
let ctx = null;
let master = null;
const listeners = [];

/** The settings now (a copy). */
export function soundPrefs() {
  return { ...prefs };
}

function setPref(key, value) {
  prefs = sanitize({ ...prefs, [key]: value });
  try {
    localStorage.setItem(PREF_KEY, JSON.stringify(prefs));
  } catch { /* private mode: the choice lasts for this page only */ }
  applyVolume();
  for (const fn of listeners) fn();
}

/** `fn()` runs after every change of the settings or of the audio's state. */
export function onSoundChange(fn) {
  listeners.push(fn);
}

/** 0..100 to a gain: 70 (the default) is the level the desk always had; 100 is 1.5×. */
function gainOf(volume) {
  return Math.min(1.5, (volume / 70) ** 2);
}

function applyVolume() {
  if (master) master.gain.value = gainOf(prefs.volume);
}

/** The AudioContext, created and resumed inside a user gesture. */
export function unlock() {
  const Ctx = window.AudioContext || window.webkitAudioContext;
  if (!Ctx) return;
  if (!ctx) {
    ctx = new Ctx();
    master = ctx.createGain();
    master.connect(ctx.destination);
    applyVolume();
    ctx.addEventListener('statechange', () => { for (const fn of listeners) fn(); });
  }
  if (ctx.state === 'suspended') ctx.resume().catch(() => {});
  for (const fn of listeners) fn();
}

/** Sound is wanted but the browser has not let the page play yet. */
export function soundLocked() {
  return prefs.on && (!ctx || ctx.state !== 'running');
}

/** One soft bell-like note: a sine with a quiet octave, quick attack, long tail. */
function note(freq, at, length, gain) {
  for (const [mult, level] of [[1, 1], [2, 0.18]]) {
    const osc = ctx.createOscillator();
    const amp = ctx.createGain();
    osc.type = 'sine';
    osc.frequency.value = freq * mult;
    amp.gain.setValueAtTime(0.0001, at);
    amp.gain.exponentialRampToValueAtTime(gain * level, at + 0.015);
    amp.gain.exponentialRampToValueAtTime(0.0001, at + length);
    osc.connect(amp).connect(master);
    osc.start(at);
    osc.stop(at + length + 0.05);
  }
}

const SOUNDS = {
  delivered: (start) => {
    [523.25, 659.25, 783.99].forEach((f, i) => note(f, start + i * 0.12, 0.5, 0.22));
    note(1046.5, start + 0.36, 1.1, 0.25);
  },
  person: (start) => {
    note(783.99, start, 0.45, 0.2);
    note(659.25, start + 0.22, 0.9, 0.2);
  },
  health: (start) => {
    [[440, 0], [440, 0.2], [329.63, 0.4]].forEach(([f, d]) => note(f, start + d, 0.3, 0.24));
  },
  held: (start) => {
    note(659.25, start, 0.35, 0.21);
    note(880, start + 0.25, 0.8, 0.21);
  },
  remind: (start) => {
    SOUNDS.person(start);
    SOUNDS.person(start + 1.2);
  },
};

function sound(kind) {
  if (!ctx || ctx.state !== 'running' || prefs.volume === 0) return false;
  SOUNDS[kind](ctx.currentTime + 0.03);
  return true;
}

/**
 * Play the sound of `kind` (one of EVENTS, or 'remind') if the person wants
 * it: sound on, that switch on. Returns whether it played.
 */
export function play(kind) {
  if (!prefs.on) return false;
  if (kind === 'remind' ? !prefs.remind : !prefs[kind]) return false;
  return sound(kind);
}

/* ------------------------------------------------------------------ *
 * The settings window, opened from the header's «🔔 Ήχος».
 * ------------------------------------------------------------------ */

let seq = 0;

function check(key, label) {
  const id = `snd-${key}-${++seq}`;
  const box = el('input', { type: 'checkbox', id });
  box.checked = prefs[key];
  box.addEventListener('change', () => setPref(key, box.checked));
  return el('label', { class: 'snd-check', for: id }, box, label);
}

/** «Δοκιμή»: plays that sound now at the chosen volume, whatever the switches say. */
function testButton(kind) {
  return el('button', {
    class: 'btn snd-test',
    type: 'button',
    'aria-label': t('sound.test.label', { what: t(`sound.event.${kind}`) }),
    onClick: () => { unlock(); sound(kind); },
  }, t('sound.test'));
}

function settingsForm() {
  const volumeId = `snd-volume-${++seq}`;
  const volume = el('input', { type: 'range', id: volumeId, min: '0', max: '100', step: '5' });
  volume.value = String(prefs.volume);
  const volumeText = el('output', { for: volumeId, class: 'snd-value' }, `${prefs.volume}%`);
  volume.addEventListener('input', () => {
    setPref('volume', Number(volume.value));
    volumeText.textContent = `${prefs.volume}%`;
  });

  const minsId = `snd-mins-${++seq}`;
  const mins = el('input', {
    type: 'number', id: minsId, class: 'snd-mins', min: String(REMIND_MIN), max: String(REMIND_MAX), step: '1',
  });
  mins.value = String(prefs.remindMins);
  mins.addEventListener('change', () => {
    setPref('remindMins', Number(mins.value));
    mins.value = String(prefs.remindMins);
  });

  return el('div', { class: 'snd-form' },
    el('p', { class: 'dlg-text' }, t('sound.settings.intro')),
    check('on', t('sound.master')),
    el('div', { class: 'snd-row' },
      el('label', { for: volumeId }, t('sound.volume')), volume, volumeText),
    el('fieldset', { class: 'snd-events' },
      el('legend', {}, t('sound.events')),
      EVENTS.map((kind) => el('div', { class: 'snd-row' }, check(kind, t(`sound.event.${kind}`)), testButton(kind)))),
    el('div', { class: 'snd-row' },
      check('remind', t('sound.remind')),
      el('label', { for: minsId, class: 'snd-mins-label' }, t('sound.remind.after')), mins,
      el('span', {}, t('sound.remind.minutes')),
      testButton('remind')),
    el('p', { class: 'note' }, t('sound.settings.browser')));
}

/** The settings window. Called from a click, so the audio is unlocked first. */
export function openSoundSettings() {
  unlock();
  // Asked only from this click: browsers ignore an unprompted request.
  if (prefs.on && 'Notification' in window && Notification.permission === 'default') {
    Notification.requestPermission().catch(() => {});
  }
  return infoDialog({ title: t('sound.settings.title'), content: settingsForm() });
}

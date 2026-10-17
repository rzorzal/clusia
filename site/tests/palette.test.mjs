import { test } from 'node:test';
import assert from 'node:assert/strict';
import { KEYFRAMES, paletteAt, momentAt, schemeAt, hourFrom, skyAt, SKY_INK, contrast } from '../js/palette.js';

const channels = (hex) => [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16));
const keyframe = (hour) => KEYFRAMES.find((k) => k.hour === hour);

test('known colors at 12h and 0h, lowercase hex', () => {
  assert.equal(paletteAt(12)['--sky-top'], '#e9f5ec');
  assert.equal(paletteAt(0)['--sky-top'], '#0c0e0d');
});

test('keyframe hours are the approved table', () => {
  assert.deepEqual(KEYFRAMES.map((k) => k.hour), [0, 5, 6.5, 9, 16, 18, 20, 21, 24]);
});

test('paletteAt(18) equals the 18h keyframe for every color and alpha', () => {
  const p = paletteAt(18);
  const k = keyframe(18);
  for (const [name, value] of Object.entries(k.colors)) assert.equal(p[name], value, name);
  for (const [name, value] of Object.entries(k.alpha)) assert.equal(p[name], String(value), name);
});

test('paletteAt(17) sky-bottom is the midpoint of 16h and 18h', () => {
  const a = channels(keyframe(16).colors['--sky-bottom']);
  const b = channels(keyframe(18).colors['--sky-bottom']);
  const got = channels(paletteAt(17)['--sky-bottom']);
  got.forEach((v, i) => assert.ok(Math.abs(v - (a[i] + b[i]) / 2) <= 1, `channel ${i}`));
});

test('hours wrap around the day', () => {
  assert.deepEqual(paletteAt(24), paletteAt(0));
  assert.deepEqual(paletteAt(24.5), paletteAt(0.5));
  assert.deepEqual(paletteAt(-1), paletteAt(23));
});

test('alphas are strings', () => {
  assert.equal(paletteAt(0)['--sun-opacity'], '0');
  assert.equal(paletteAt(23)['--moon-opacity'], '1');
  assert.equal(paletteAt(20)['--stars-opacity'], '0.5');
});

test('sun-y: below the horizon at night, high by day, back down by 19h', () => {
  assert.equal(paletteAt(0)['--sun-y'], '1');
  assert.equal(paletteAt(9)['--sun-y'], '0.15');
  assert.equal(paletteAt(12)['--sun-y'], '0.15');
  assert.equal(paletteAt(16)['--sun-y'], '0.15');
  assert.equal(paletteAt(19)['--sun-y'], '1');
  assert.equal(paletteAt(22)['--sun-y'], '1');
  const mid = Number(paletteAt(7)['--sun-y']);
  assert.ok(mid > 0.15 && mid < 1);
});

test('momentAt', () => {
  assert.equal(momentAt(4.99), 'night');
  assert.equal(momentAt(5), 'dawn');
  assert.equal(momentAt(7), 'day');
  assert.equal(momentAt(15.99), 'day');
  assert.equal(momentAt(16), 'sunset');
  assert.equal(momentAt(19), 'dusk');
  assert.equal(momentAt(21), 'night');
});

test('schemeAt', () => {
  assert.equal(schemeAt(12), 'light');
  assert.equal(schemeAt(19.49), 'light');
  assert.equal(schemeAt(19.5), 'dark');
  assert.equal(schemeAt(23), 'dark');
  assert.equal(schemeAt(5.49), 'dark');
  assert.equal(schemeAt(5.5), 'light');
});

test('hourFrom: ?hour overrides the clock, bad values fall back', () => {
  const d = new Date(2026, 0, 1, 14, 30);
  assert.equal(hourFrom(d, ''), 14.5);
  assert.equal(hourFrom(d, '?hour=18.25'), 18.25);
  for (const s of ['?hour=abc', '?hour=25', '?hour=-1', '?hour=']) assert.equal(hourFrom(d, s), 14.5, s);
});

test('contrast is the WCAG ratio', () => {
  assert.equal(Math.round(contrast('#000000', '#ffffff') * 100) / 100, 21);
  assert.equal(contrast('#2f9e44', '#2f9e44'), 1);
  assert.equal(contrast('#1b1f1c', '#e9f5ec'), contrast('#e9f5ec', '#1b1f1c'));
});

test('skyAt: dark ink on a bright sky, light ink on a dark one', () => {
  assert.equal(skyAt(12), 'light');
  assert.equal(skyAt(17), 'light');
  assert.equal(skyAt(18), 'dark');
  assert.equal(skyAt(23), 'dark');
  assert.equal(skyAt(5), 'dark');
  assert.deepEqual(SKY_INK, { light: '#1B1F1C', dark: '#FFF4E6' });
});

test('the chosen sky ink keeps at least 4:1 on the upper sky at every quarter hour', () => {
  for (let h = 0; h < 24; h += 0.25) {
    const p = paletteAt(h);
    const top = channels(p['--sky-top']);
    const bottom = channels(p['--sky-bottom']);
    const sky = '#' + top.map((v, i) => Math.round(v + (bottom[i] - v) * 0.35).toString(16).padStart(2, '0')).join('');
    const ink = SKY_INK[skyAt(h)];
    const other = SKY_INK[skyAt(h) === 'light' ? 'dark' : 'light'];
    assert.ok(contrast(ink, sky) >= 4, `${h}h: ${contrast(ink, sky).toFixed(2)}`);
    assert.ok(contrast(ink, sky) >= contrast(other, sky), `${h}h picks the stronger ink`);
  }
});

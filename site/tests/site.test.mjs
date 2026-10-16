import { test } from 'node:test';
import assert from 'node:assert/strict';
import { paint, layerShift, PARALLAX_RANGE, driftShift, copyFrom } from '../js/site.js';
import { paletteAt, skyAt } from '../js/palette.js';

const fakeRoot = () => {
  const props = {};
  return { props, dataset: {}, style: { setProperty: (k, v) => { props[k] = v; } } };
};

test('paint sets every palette property, the scheme and the moment', () => {
  const root = fakeRoot();
  paint(root, 23);
  assert.deepEqual(root.props, paletteAt(23));
  assert.equal(root.dataset.scheme, 'dark');
  assert.equal(root.dataset.moment, 'night');

  paint(root, 18);
  assert.deepEqual(root.props, paletteAt(18));
  assert.equal(root.dataset.scheme, 'light');
  assert.equal(root.dataset.moment, 'sunset');
  assert.equal(root.dataset.sky, skyAt(18));

  paint(root, 12);
  assert.equal(root.dataset.sky, 'light');
});

test('layers rise by scroll times depth, up to the parallax range', () => {
  assert.equal(layerShift(0, 0.6), 0);
  assert.equal(layerShift(100, 0.25), -25);
  assert.equal(layerShift(-40, 0.6), 0);
  assert.equal(layerShift(PARALLAX_RANGE * 3, 0.1), -PARALLAX_RANGE * 0.1);
});

test('a drifting piece moves against its distance from the middle of the screen, scaled by its speed', () => {
  // Centred on screen it sits where the layout put it.
  assert.equal(driftShift(0, 0.2), 0);
  // Below the middle, a far piece (positive speed) is held back up; it lags behind the page.
  assert.equal(driftShift(400, 0.2), -80);
  assert.equal(driftShift(-400, 0.2), 80);
  // A near piece (negative speed) runs ahead of the page.
  assert.equal(driftShift(400, -0.1), 40);
  // Whole pixels, and never -0.
  assert.equal(driftShift(333, 0.1), -33);
  assert.ok(Object.is(driftShift(0, 0.3), 0));
  assert.ok(Object.is(driftShift(-0.4, 0.3), 0));
});

function fakeCopy({ clipboard, execCommand = () => true }) {
  const code = { textContent: 'curl x | bash' };
  const button = { textContent: 'Copy', parentElement: { querySelector: (sel) => (sel === '[data-copy]' ? code : null) } };
  const timers = [];
  const selected = [];
  const env = {
    navigator: clipboard ? { clipboard } : {},
    document: {
      createRange: () => ({ selectNodeContents: (n) => selected.push(n) }),
      execCommand,
    },
    getSelection: () => ({ removeAllRanges() {}, addRange() {} }),
    setTimeout: (fn, ms) => timers.push([fn, ms]),
    clearTimeout() {},
  };
  const status = { textContent: '' };
  return { code, button, env, timers, selected, status };
}

test('copy writes the code text to the clipboard and says Copied for 1.5 s', async () => {
  const written = [];
  const f = fakeCopy({ clipboard: { writeText: async (t) => written.push(t) } });
  await copyFrom(f.button, f.env, f.status);
  assert.deepEqual(written, ['curl x | bash']);
  assert.equal(f.button.textContent, 'Copied');
  assert.equal(f.status.textContent, 'Copied');
  assert.equal(f.timers.at(-1)[1], 1500);
  f.timers.at(-1)[0]();
  assert.equal(f.button.textContent, 'Copy');
});

test('without the clipboard API copy selects the code text and copies the selection', async () => {
  const f = fakeCopy({ clipboard: { writeText: async () => { throw new Error('denied'); } } });
  await copyFrom(f.button, f.env);
  assert.deepEqual(f.selected, [f.code]);
  assert.equal(f.button.textContent, 'Copied');

  const g = fakeCopy({ execCommand: () => false });
  await copyFrom(g.button, g.env, g.status);
  assert.deepEqual(g.selected, [g.code]);
  assert.equal(g.button.textContent, 'Press ⌘C');
  assert.equal(g.status.textContent, 'Selected. Press ⌘C to copy.');
});

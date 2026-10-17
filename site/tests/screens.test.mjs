import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { lightboxSource } from '../js/site.js';

const SITE = fileURLToPath(new URL('..', import.meta.url));
const html = readFileSync(SITE + 'screens.html', 'utf8');
const css = readFileSync(SITE + 'css/site.css', 'utf8');
const main = html.slice(html.indexOf('<main'), html.indexOf('</main>'));

const PAIRS = [
  'sp1-m5a-home', 'sp1-m5b-review', 'sp1-m5c-composer', 'sp2-m4-revalidated', 'sp2-m4-banner',
  'sp2-m1-agent-chat', 'sp2-m2-permission', 'sp2-m3-security', 'sp2-m3-audits', 'sp3-m1-diagrams',
  'sp2-m5-first-run-both', 'sp1-tray-readable',
];

test('screens.html is the Screens page, current in the nav', () => {
  assert.match(html, /<title>Screens — Clúsia<\/title>/);
  assert.match(main, /<h1[^>]*>Screens<\/h1>/);
  assert.match(html, /<a href="screens\.html" aria-current="page">Screens<\/a>/);
});

test('every cleared pair is a link to its light image holding both images, and nothing else', () => {
  for (const pair of PAIRS) {
    const link = main.match(new RegExp(`<a [^>]*href="assets/${pair}-light\\.png"[^>]*>[\\s\\S]*?</a>`));
    assert.ok(link, pair);
    assert.match(link[0], new RegExp(`data-dark="assets/${pair}-dark\\.png"`));
    assert.match(link[0], new RegExp(`<img class="shot-light" src="assets/${pair}-light\\.png" width="\\d+" height="\\d+" alt="[^"]+"`));
    assert.match(link[0], new RegExp(`<img class="shot-dark" src="assets/${pair}-dark\\.png" width="\\d+" height="\\d+" alt="[^"]+"`));
  }
  assert.equal([...main.matchAll(/<img\b[^>]*assets\/sp[^>]*>/g)].length, PAIRS.length * 2);
  assert.ok(!main.includes('sp1-m6-config-general'));
  assert.equal([...main.matchAll(/<figcaption>[^<]+<\/figcaption>/g)].length, PAIRS.length);
});

test('the note says the screens follow the clock', () => {
  assert.match(main, /Showing the (<span[^>]*>light<\/span>|light)(\/dark)? screens — they follow your clock|they follow your clock/);
});

test('the lightbox is a closed dialog with a close button', () => {
  const dialog = html.match(/<dialog class="lightbox"[^>]*>[\s\S]*?<\/dialog>/);
  assert.ok(dialog);
  assert.match(dialog[0], /<button[^>]*class="lightbox-close"[^>]*aria-label="Close"/);
  assert.match(css, /\.lightbox\s*{/);
  assert.match(css, /\.lightbox::backdrop/);
});

test('the lightbox shows the image of the current scheme', () => {
  const link = { dataset: { dark: 'assets/x-dark.png' }, getAttribute: (n) => (n === 'href' ? 'assets/x-light.png' : null) };
  assert.equal(lightboxSource(link, 'light'), 'assets/x-light.png');
  assert.equal(lightboxSource(link, 'dark'), 'assets/x-dark.png');
});
